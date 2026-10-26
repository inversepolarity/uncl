//! Out-of-band graphics: the tap, the store, and the bytes we re-emit.
//!
//! The grid (`vt100`) has no idea what an image is. Upstream `vte` throws DCS
//! payloads away, routes `SOS`/`PM`/`APC` bytes into `anywhere()` (which discards
//! them) and only reports `OSC` strings, so pixels never survive the trip through
//! a parser. That is why an image a child draws today simply never appears.
//!
//! This module takes those sequences out of the byte stream **before** the parser
//! sees it, and keeps them verbatim for the renderer to re-emit inside the float
//! after the grid has been painted. Three protocols, all passthrough — no decode,
//! no scaling, no re-encoding, no alpha:
//!
//! * **kitty graphics** — `ESC _ G <controls> ; <payload> ESC \`. The only one with
//!   an image id and a placement, so it is the only one that can be *moved*
//!   cheaply: once the terminal holds the image, re-placing it is
//!   `ESC _ G a=p,i=<id> ESC \` instead of the whole payload.
//! * **sixel** — `ESC P … q … ESC \`. One opaque blit at the cursor: no id, no
//!   crop, no way to move it. Gated behind `UNCL_SIXEL` (see `term::caps`).
//! * **iTerm2 inline images** — `OSC 1337 ; File=… : <base64> BEL`. Also one blit
//!   at the cursor. It costs nothing on top of the same store, and the terminals
//!   most likely to be hosting uncl (WezTerm, iTerm2, kitty) all implement it —
//!   `wezterm imgcat` works through it.
//!
//! Everything else (`OSC 52`, colour queries, `DECRQSS`…) is handed back to the
//! parser exactly as before, so this module is invisible to the rest of the
//! terminal.
//!
//! ## Why a splitter and not a parser hook
//!
//! `vt100` does not implement `vte::Perform::hook/put/unhook`, so a DCS payload is
//! gone before a `Callbacks` method could see it, and APC never reaches
//! `Callbacks` at all. Splitting the stream ourselves is ~150 lines, has no
//! version coupling, and is the same trick every multiplexer uses.

use std::sync::atomic::{AtomicU64, Ordering};

/// Sequence numbers are global and monotonic, so a renderer keeps working across
/// a float restart: a new child's first image is always "newer" than anything the
/// previous one drew.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The three out-of-band sequence families the tap removes from the stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// `ESC P … ST`
    Dcs,
    /// `ESC _ … ST`
    Apc,
    /// `ESC ] … BEL|ST`
    Osc,
}

/// One ordered piece of a read chunk.
pub enum Piece {
    /// Ordinary terminal output: the parser must see this.
    Text(Vec<u8>),
    /// An out-of-band sequence, complete, including its introducer and
    /// terminator. [`Store::ingest`] decides whether it is graphics.
    OutOfBand(Kind, Vec<u8>),
}

/// The largest single sequence we will buffer. An image that does not fit in this
/// is dropped rather than allowed to grow without bound — a child can send an
/// arbitrarily large base64 payload, and we are not a decoder queue.
pub const MAX_BLOB: usize = 8 * 1024 * 1024;

/// How many images one pane remembers.
pub const MAX_IMAGES: usize = 8;

/// Total bytes of image payload one pane remembers.
pub const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;

/// Pull out-of-band sequences out of a child's output, across chunk boundaries.
#[derive(Debug, Default)]
pub struct Splitter {
    /// A sequence that started in an earlier chunk and has not terminated yet.
    pending: Option<(Kind, Vec<u8>)>,
    /// The chunk ended on a lone `ESC`: hold it back, because it may be the
    /// introducer of a sequence that starts in the next read. Handing it to the
    /// parser would let `vte` swallow the whole APC and lose the image.
    carry_esc: bool,
}

impl Splitter {
    pub fn new() -> Self {
        Self {
            pending: None,
            carry_esc: false,
        }
    }

    /// Split one read into parser text and out-of-band sequences, in order.
    ///
    /// The order matters: the caller feeds text to the parser *first*, so the
    /// cursor position it records for a graphics sequence is the one the child
    /// had when it emitted it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Piece> {
        // Put back the lone `ESC` of the previous read, so everything below sees
        // whole sequences. Rare enough that the copy does not matter.
        let mut rejoined;
        let chunk = if self.carry_esc {
            self.carry_esc = false;
            rejoined = Vec::with_capacity(chunk.len() + 1);
            rejoined.push(0x1b);
            rejoined.extend_from_slice(chunk);
            rejoined.as_slice()
        } else {
            chunk
        };

        let mut pieces = Vec::new();
        let mut text: Vec<u8> = Vec::new();
        let mut at = 0usize;

        // 1. Finish a sequence carried over from the previous chunk.
        if let Some((kind, mut buf)) = self.pending.take() {
            let split_terminator = buf.last() == Some(&0x1b) && chunk.first() == Some(&b'\\');

            if split_terminator {
                // `ESC` ended the last chunk and `\\` starts this one.
                buf.push(b'\\');
                pieces.push(Piece::OutOfBand(kind, buf));
                at = 1;
            } else if let Some((end, next)) = terminator_at(chunk, 0, kind) {
                buf.extend_from_slice(&chunk[..end]);
                pieces.push(Piece::OutOfBand(kind, buf));
                at = next;
            } else {
                buf.extend_from_slice(chunk);
                // A sequence this large is not going to terminate inside anything
                // we can afford to hold: drop it.
                if buf.len() <= MAX_BLOB {
                    self.pending = Some((kind, buf));
                }
                return pieces;
            }
        }

        // 2. Walk the rest, splitting at each introducer.
        while at < chunk.len() {
            let Some((start, kind, body)) = introducer_at(chunk, at) else {
                text.extend_from_slice(&chunk[at..]);
                break;
            };

            text.extend_from_slice(&chunk[at..start]);
            if !text.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut text)));
            }

            match terminator_at(chunk, body, kind) {
                Some((end, next)) => {
                    pieces.push(Piece::OutOfBand(kind, chunk[start..end].to_vec()));
                    at = next;
                }
                None => {
                    let mut buf = Vec::with_capacity(chunk.len() - start);
                    buf.extend_from_slice(&chunk[start..]);
                    self.pending = Some((kind, buf));
                    return pieces;
                }
            }
        }

        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }

        // A lone `ESC` at the very end may belong with the next read.
        if chunk.last() == Some(&0x1b) {
            self.carry_esc = true;
            if let Some(Piece::Text(text)) = pieces.last_mut() {
                text.pop();
                if text.is_empty() {
                    pieces.pop();
                }
            }
        }

        pieces
    }
}

/// Find the next introducer at or after `from`: `(start, kind, body start)`.
fn introducer_at(chunk: &[u8], from: usize) -> Option<(usize, Kind, usize)> {
    let mut at = from;
    while at < chunk.len() {
        match chunk[at] {
            0x1b => match chunk.get(at + 1) {
                Some(b'P') => return Some((at, Kind::Dcs, at + 2)),
                Some(b'_') => return Some((at, Kind::Apc, at + 2)),
                Some(b']') => return Some((at, Kind::Osc, at + 2)),
                // Any other escape (CSI, keypad mode, ST) is ordinary output.
                _ => at += 1,
            },
            // The 8-bit C1 forms (`0x90`, `0x9d`, `0x9f`) are deliberately *not*
            // introducers here. Those byte values occur inside ordinary UTF-8
            // text (`✓` is `e2 9c 93`, `ʝ` is `ca 9d`), and a terminal that is
            // decoding UTF-8 — which `vte` is — treats them as text. Every sender
            // of an image uses the `ESC` forms anyway, so accepting the C1 forms
            // would buy nothing and could swallow a line of someone's output.
            _ => at += 1,
        }
    }
    None
}

/// Find the terminator of a sequence: `(end of sequence, next byte to read)`.
fn terminator_at(chunk: &[u8], from: usize, kind: Kind) -> Option<(usize, usize)> {
    let mut at = from;
    while at < chunk.len() {
        match chunk[at] {
            // `ESC \` is ST. By here we already know we are inside DCS/APC/OSC.
            0x1b if chunk.get(at + 1) == Some(&b'\\') => return Some((at + 2, at + 2)),
            0x1b => at += 1,
            // 8-bit ST.
            0x9c => return Some((at + 1, at + 1)),
            // BEL terminates an OSC string only.
            0x07 if kind == Kind::Osc => return Some((at + 1, at + 1)),
            _ => at += 1,
        }
    }
    None
}

/// What the caller should do with an out-of-band sequence.
#[derive(Debug)]
pub enum Ingest {
    /// Graphics: kept for re-emission, do not give it to the parser.
    Stored,
    /// Not graphics: hand it to the parser like any other output.
    Passthrough,
    /// A kitty capability query: these bytes go back to the child's pty.
    Reply(Vec<u8>),
}

/// One image, kept verbatim.
#[derive(Debug)]
pub struct Image {
    seq: u64,
    bytes: Vec<u8>,
    /// The kitty image id, when the payload had one: the only handle that lets us
    /// re-place an image instead of re-sending it.
    id: Option<u32>,
    /// A position-only replay, when the protocol has ids (kitty).
    replay: Option<Vec<u8>>,
    /// The child's cursor cell when the sequence arrived, pane-local, in
    /// `vt100`'s own `(row, column)` order.
    anchor: (u16, u16),
    /// Pixel-based protocols are destroyed by text painted over them, so they
    /// have to be re-emitted whenever the frame repaints their rows. Kitty keeps
    /// images in a separate layer and does not.
    pixel_based: bool,
}

impl Image {
    /// Monotonic id, so the renderer knows what it has already emitted.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The complete sequence, ready to write to the host terminal.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The placement-only replay, if the protocol can re-place by id.
    pub fn replay(&self) -> Option<&[u8]> {
        self.replay.as_deref()
    }

    /// Where the child's cursor was, in the child's own grid: `(row, column)`.
    pub fn anchor(&self) -> (u16, u16) {
        self.anchor
    }

    /// Does repainting the grid destroy this image? (`sixel`/iTerm2: yes.)
    pub fn pixel_based(&self) -> bool {
        self.pixel_based
    }
}

/// The images one pane remembers, oldest first.
#[derive(Debug, Default)]
pub struct Store {
    images: Vec<Image>,
    bytes: usize,
    /// A kitty transmission that arrives in several APCs (`m=1` … `m=0`). The
    /// chunks have to be re-emitted in order and as a unit, so they are joined
    /// here instead of each becoming an "image" of its own. The trailing byte is
    /// the action of the *first* chunk: kitty only states `a=` there, and whether
    /// that action placed the image decides whether it can be re-placed later.
    partial: Option<(Vec<u8>, (u16, u16), u8)>,
    /// Set once per skipped-oversized image, so a sixel that cannot fit does not
    /// print a line of log per frame.
    warned: bool,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything still worth drawing, oldest first.
    pub fn images(&self) -> &[Image] {
        &self.images
    }

    /// Classify and, if it is graphics, keep an out-of-band sequence.
    ///
    /// `area` is the child's grid size — which S1 made identical to the float's
    /// content rect — and decides whether a pixel-sized image can fit at all.
    pub fn ingest(
        &mut self,
        kind: Kind,
        bytes: &[u8],
        anchor: (u16, u16),
        area: (u16, u16),
    ) -> Ingest {
        match kind {
            Kind::Apc => self.ingest_apc(bytes, anchor, area),
            Kind::Dcs => self.ingest_dcs(bytes, anchor, area),
            Kind::Osc => self.ingest_osc(bytes, anchor, area),
        }
    }

    fn ingest_apc(&mut self, bytes: &[u8], anchor: (u16, u16), area: (u16, u16)) -> Ingest {
        // kitty graphics: `ESC _ G <controls> [; <payload>] ST`
        let Some(body) = payload_of(bytes, b"\x1b_G") else {
            return Ingest::Passthrough;
        };
        let Some(controls) = KittyControls::parse(body) else {
            return Ingest::Passthrough;
        };

        match controls.action {
            // `a=q` asks "do you speak graphics at all?" — the one probe that
            // makes children start sending images.
            b'q' => Ingest::Reply(controls.query_reply()),
            // `a=d` deletes. Honour it against our store, and pass the deletion
            // through once so the host forgets the image too. It is never
            // re-placed: a replay would resurrect what the child just removed.
            b'd' => {
                // `d=A`/`d=a` carry no id: the child is deleting every image it
                // ever sent, so everything we hold is stale.
                match controls.id {
                    Some(_) => self.forget(controls.id),
                    None => {
                        self.images.clear();
                        self.bytes = 0;
                    }
                }
                self.push(Image {
                    seq: 0,
                    bytes: bytes.to_vec(),
                    id: None,
                    replay: None,
                    anchor,
                    pixel_based: false,
                });
                Ingest::Stored
            }
            _ => {
                // `m=1` says more chunks follow. A PNG is a few dozen of them, so
                // they are joined into one image: emitting a fragment on its own
                // would both break the image and evict the earlier fragments.
                if controls.more || self.partial.is_some() {
                    let overlong = {
                        let entry = self
                            .partial
                            .get_or_insert_with(|| (Vec::new(), anchor, controls.action));
                        entry.0.extend_from_slice(bytes);
                        entry.0.len() > MAX_BLOB
                    };
                    if overlong {
                        self.partial = None;
                        return Ingest::Stored;
                    }
                    if controls.more {
                        return Ingest::Stored;
                    }

                    let Some((joined, first_anchor, first_action)) = self.partial.take() else {
                        return Ingest::Stored;
                    };
                    // `a=` is stated on the first chunk only; the last one still
                    // carries the placement parameters.
                    let mut controls = controls;
                    controls.action = first_action;
                    self.push(Image {
                        seq: 0,
                        bytes: joined,
                        id: controls.id,
                        replay: controls.replay(),
                        anchor: first_anchor,
                        pixel_based: false,
                    });
                    return Ingest::Stored;
                }

                if !fits(controls.footprint(), area) {
                    self.warn_once("image is larger than the float; skipping");
                    return Ingest::Stored;
                }
                self.push(Image {
                    seq: 0,
                    bytes: bytes.to_vec(),
                    id: controls.id,
                    replay: controls.replay(),
                    anchor,
                    // Kitty images live in the terminal's image layer, so our
                    // repaints do not erase them.
                    pixel_based: false,
                });
                Ingest::Stored
            }
        }
    }

    fn ingest_dcs(&mut self, bytes: &[u8], anchor: (u16, u16), area: (u16, u16)) -> Ingest {
        let Some(body) = payload_of(bytes, b"\x1bP") else {
            return Ingest::Passthrough;
        };

        // sixel: `<params> q <data>`. Anything else in a DCS is not ours.
        let Some(q) = body.iter().take(16).position(|byte| *byte == b'q') else {
            return Ingest::Passthrough;
        };
        if !body[..q]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
        {
            return Ingest::Passthrough;
        }

        if !fits(raster_attributes(&body[q + 1..]), area) {
            self.warn_once("sixel image is larger than the float; skipping");
            return Ingest::Stored;
        }

        self.push(Image {
            seq: 0,
            bytes: bytes.to_vec(),
            id: None,
            // No id and no placement: sixel can only be blitted again.
            replay: None,
            anchor,
            pixel_based: true,
        });
        Ingest::Stored
    }

    fn ingest_osc(&mut self, bytes: &[u8], anchor: (u16, u16), area: (u16, u16)) -> Ingest {
        // iTerm2 inline image: `OSC 1337 ; File=<args> : <base64> BEL|ST`
        let Some(body) = payload_of(bytes, b"\x1b]") else {
            return Ingest::Passthrough;
        };
        let Some(rest) = body.strip_prefix(b"1337;File=") else {
            return Ingest::Passthrough;
        };
        let Some(colon) = rest.iter().position(|byte| *byte == b':') else {
            return Ingest::Passthrough;
        };

        let args = &rest[..colon];
        // Only *inline* files are ours to draw; a download request is not.
        if !args
            .split(|byte| *byte == b';')
            .any(|arg| arg == &b"inline=1"[..])
        {
            return Ingest::Passthrough;
        }

        if !fits(iterm_footprint(args), area) {
            self.warn_once("inline image is larger than the float; skipping");
            return Ingest::Stored;
        }

        self.push(Image {
            seq: 0,
            bytes: bytes.to_vec(),
            id: None,
            replay: None,
            anchor,
            pixel_based: true,
        });
        Ingest::Stored
    }

    fn warn_once(&mut self, message: &str) {
        if !self.warned {
            self.warned = true;
            eprintln!("uncl: {message} (reported once)");
        }
    }

    /// Drop every image with this kitty id.
    fn forget(&mut self, id: Option<u32>) {
        let Some(id) = id else {
            return;
        };
        self.images.retain(|image| image.id != Some(id));
        self.bytes = self.images.iter().map(|image| image.bytes.len()).sum();
    }

    fn push(&mut self, mut image: Image) {
        image.seq = next_seq();

        self.bytes = self.bytes.saturating_add(image.bytes.len());
        self.images.push(image);

        // Bound both the count and the bytes: a viewer that scrolls a hundred
        // images must not turn uncl into the memory hog it is meant to replace.
        while self.images.len() > MAX_IMAGES || self.bytes > MAX_STORE_BYTES {
            let dropped = self.images.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.bytes.len());
        }
    }
}

/// Would something this size fit inside the child's grid?
fn fits(footprint: Footprint, area: (u16, u16)) -> bool {
    match footprint {
        Footprint::Unknown => true,
        Footprint::Cells(cols, rows) => cols <= area.0 && rows <= area.1,
        Footprint::Pixels(width, height) => {
            let (cell_w, cell_h) = cell_px();
            let cols = width.div_ceil(u32::from(cell_w.max(1)));
            let rows = height.div_ceil(u32::from(cell_h.max(1)));
            cols <= u32::from(area.0) && rows <= u32::from(area.1)
        }
    }
}

/// What a payload says about its own size, before any decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Footprint {
    Unknown,
    Cells(u16, u16),
    Pixels(u32, u32),
}

/// Cell size in pixels, for protocols that only talk in pixels.
///
/// uncl cannot ask the terminal (that needs a reply probe we do not run), so this
/// is an assumption: `UNCL_CELL_PX=WxH`, defaulting to a 10x20 cell. It is only
/// ever used to decide whether an image *could* fit; the terminal does the
/// drawing.
fn cell_px() -> (u16, u16) {
    let Ok(spec) = std::env::var("UNCL_CELL_PX") else {
        return (10, 20);
    };
    let mut parts = spec
        .split(['x', 'X', ','])
        .filter_map(|part| part.parse::<u16>().ok());
    match (parts.next(), parts.next()) {
        (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
        _ => (10, 20),
    }
}

/// The bytes between an introducer and its terminator.
fn payload_of<'a>(bytes: &'a [u8], introducer: &[u8]) -> Option<&'a [u8]> {
    let body = bytes.strip_prefix(introducer)?;
    for terminator in [&b"\x1b\\"[..], &b"\x9c"[..], &b"\x07"[..]] {
        if let Some(stripped) = body.strip_suffix(terminator) {
            return Some(stripped);
        }
    }
    Some(body)
}

/// The kitty control block, reduced to what uncl needs.
#[derive(Debug, Default)]
struct KittyControls {
    action: u8,
    id: Option<u32>,
    placement: Option<u32>,
    /// `m=1`: more chunks of this transmission follow.
    more: bool,
    /// `c`/`r` (size in cells), `x`/`y`/`w`/`h` (source crop), `z` (z-index),
    /// `q` (quiet). All of it replays verbatim; we never interpret the pixels.
    replay_keys: Vec<(u8, String)>,
}

impl KittyControls {
    fn parse(body: &[u8]) -> Option<Self> {
        let controls = body.split(|byte| *byte == b';').next()?;
        let mut out = KittyControls {
            // kitty's default action is `t` (transmit only).
            action: b't',
            ..KittyControls::default()
        };

        for item in controls.split(|byte| *byte == b',') {
            if item.is_empty() {
                continue;
            }
            let at = item.iter().position(|byte| *byte == b'=')?;
            let key = *item.first()?;
            let value = std::str::from_utf8(&item[at + 1..]).ok()?;

            match key {
                b'a' => out.action = value.as_bytes().first().copied().unwrap_or(b't'),
                b'i' => out.id = value.parse().ok(),
                b'p' => out.placement = value.parse().ok(),
                b'm' => out.more = value == "1",
                b'c' | b'r' | b'x' | b'y' | b'w' | b'h' | b'z' | b'q' | b'C' | b'X' | b'Y' => {
                    out.replay_keys.push((key, value.to_owned()));
                }
                _ => {}
            }
        }

        Some(out)
    }

    /// How big the image will be, if the controls say.
    fn footprint(&self) -> Footprint {
        let mut cols = None;
        let mut rows = None;
        for (key, value) in &self.replay_keys {
            match (key, value.parse::<u16>()) {
                (b'c', Ok(parsed)) => cols = Some(parsed),
                (b'r', Ok(parsed)) => rows = Some(parsed),
                _ => {}
            }
        }
        match (cols, rows) {
            (Some(cols), Some(rows)) => Footprint::Cells(cols, rows),
            _ => Footprint::Unknown,
        }
    }

    /// The answer to `a=q`: kitty's "graphics are available" reply.
    fn query_reply(&self) -> Vec<u8> {
        let mut reply = String::from("\x1b_G");
        if let Some(id) = self.id {
            reply.push_str(&format!("i={id}"));
        }
        reply.push_str(";OK\x1b\\");
        reply.into_bytes()
    }

    /// `a=p,i=<id>[,p=<placement>][,c=…,r=…,x=…]`: the same placement without the
    /// payload, which is all a move needs.
    fn replay(&self) -> Option<Vec<u8>> {
        // `a=t` transmits without placing: replaying it as a placement would draw
        // a second copy the child never asked for, so it can only be re-sent.
        if self.more || !matches!(self.action, b'T' | b'p') {
            return None;
        }
        let id = self.id?;
        let mut replay = format!("\x1b_Ga=p,i={id}");
        if let Some(placement) = self.placement {
            replay.push_str(&format!(",p={placement}"));
        }
        for (key, value) in &self.replay_keys {
            replay.push(',');
            replay.push(char::from(*key));
            replay.push('=');
            replay.push_str(value);
        }
        replay.push_str("\x1b\\");
        Some(replay.into_bytes())
    }
}

/// sixel raster attributes: `"Pan;Pad;Ph;Pv` — the last two are pixels.
fn raster_attributes(data: &[u8]) -> Footprint {
    let Some(quote) = data.iter().position(|byte| *byte == b'"') else {
        return Footprint::Unknown;
    };

    let mut fields = data[quote + 1..]
        .split(|byte| *byte == b';')
        .take(4)
        .filter_map(parse_u32);

    let _pan = fields.next();
    let _pad = fields.next();
    match (fields.next(), fields.next()) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Footprint::Pixels(width, height),
        _ => Footprint::Unknown,
    }
}

/// iTerm2 `width=`/`height=`: bare numbers are cells, `Npx` is pixels.
fn iterm_footprint(args: &[u8]) -> Footprint {
    let mut cols = None;
    let mut rows = None;

    for arg in args.split(|byte| *byte == b';') {
        if let Some(value) = arg.strip_prefix(b"width=") {
            cols = iterm_cells(value);
        }
        if let Some(value) = arg.strip_prefix(b"height=") {
            rows = iterm_cells(value);
        }
    }

    match (cols, rows) {
        (Some(cols), Some(rows)) => Footprint::Cells(cols, rows),
        _ => Footprint::Unknown,
    }
}

fn iterm_cells(value: &[u8]) -> Option<u16> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete kitty transmission+placement.
    const APC: &[u8] = b"\x1b_Gf=24,s=1,v=1,a=T,i=7;AAAA\x1b\\";

    fn pieces(splitter: &mut Splitter, chunk: &[u8]) -> (Vec<u8>, Vec<(Kind, Vec<u8>)>) {
        let mut text = Vec::new();
        let mut out = Vec::new();
        for piece in splitter.feed(chunk) {
            match piece {
                Piece::Text(bytes) => text.extend_from_slice(&bytes),
                Piece::OutOfBand(kind, bytes) => out.push((kind, bytes)),
            }
        }
        (text, out)
    }

    fn ingest(store: &mut Store, kind: Kind, bytes: &[u8]) -> Ingest {
        store.ingest(kind, bytes, (0, 0), (80, 24))
    }

    #[test]
    fn ordinary_output_is_passed_through_untouched() {
        let mut splitter = Splitter::new();
        let (text, out) = pieces(&mut splitter, b"hello\r\n\x1b[1;31mred\x1b[0m");
        assert_eq!(text.as_slice(), b"hello\r\n\x1b[1;31mred\x1b[0m");
        assert!(out.is_empty());
    }

    #[test]
    fn an_apc_is_taken_out_of_the_stream() {
        let mut splitter = Splitter::new();
        let mut chunk = b"before".to_vec();
        chunk.extend_from_slice(APC);
        chunk.extend_from_slice(b"after");

        let (text, out) = pieces(&mut splitter, &chunk);
        assert_eq!(text.as_slice(), b"beforeafter");
        assert_eq!(out, vec![(Kind::Apc, APC.to_vec())]);
    }

    #[test]
    fn a_sequence_split_across_chunks_is_reassembled() {
        let mut splitter = Splitter::new();

        // Bytes before the sequence are emitted at once; the sequence itself
        // waits for its terminator.
        let (text, out) = pieces(&mut splitter, b"before\x1b_Gf=24,i=7;AA");
        assert_eq!(text.as_slice(), b"before");
        assert!(out.is_empty());

        let (text, out) = pieces(&mut splitter, b"AA\x1b\\after");
        assert_eq!(text.as_slice(), b"after");
        assert_eq!(out, vec![(Kind::Apc, b"\x1b_Gf=24,i=7;AAAA\x1b\\".to_vec())]);
    }

    #[test]
    fn a_terminator_split_across_chunks_still_ends_the_sequence() {
        let mut splitter = Splitter::new();

        let (text, out) = pieces(&mut splitter, b"\x1b_Gi=7;AA\x1b");
        assert!(text.is_empty());
        assert!(out.is_empty());

        let (text, out) = pieces(&mut splitter, b"\\after");
        assert_eq!(text.as_slice(), b"after");
        assert_eq!(out, vec![(Kind::Apc, b"\x1b_Gi=7;AA\x1b\\".to_vec())]);
    }

    #[test]
    fn an_introducer_split_between_esc_and_its_letter_is_rejoined() {
        let mut splitter = Splitter::new();

        // The read ends between `ESC` and `_`, which is where a 8 KiB buffer
        // lands one time in eight thousand.
        let (text, out) = pieces(&mut splitter, b"before\x1b");
        assert_eq!(text.as_slice(), b"before");
        assert!(out.is_empty());

        let (text, out) = pieces(&mut splitter, b"_Gi=1,a=q\x1b\\");
        assert!(text.is_empty());
        assert_eq!(out, vec![(Kind::Apc, b"\x1b_Gi=1,a=q\x1b\\".to_vec())]);
    }

    #[test]
    fn a_sequence_starting_exactly_at_a_chunk_boundary_survives() {
        let mut splitter = Splitter::new();
        let (text, out) = pieces(&mut splitter, b"\x1b_");
        assert!(text.is_empty());
        assert!(out.is_empty());

        let (text, out) = pieces(&mut splitter, b"Gi=3,a=q\x1b\\!");
        assert_eq!(text.as_slice(), b"!");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dcs_sixel_and_osc_inline_images_are_found() {
        let mut splitter = Splitter::new();
        let mut chunk = b"\x1bPq\"1;1;2;2#0;2;0;0;0#0~~\x1b\\".to_vec();
        chunk.extend_from_slice(b"\x1b]1337;File=inline=1:AAAA\x07");

        let (text, out) = pieces(&mut splitter, &chunk);
        assert!(text.is_empty());
        assert_eq!(out[0].0, Kind::Dcs);
        assert_eq!(out[1].0, Kind::Osc);
    }

    #[test]
    fn a_non_image_osc_is_ours_to_pass_on() {
        let mut splitter = Splitter::new();
        let (text, out) = pieces(&mut splitter, b"\x1b]52;c;aGk=\x07");
        // The splitter is protocol-agnostic: it reports the OSC, the store
        // decides it is not an image and hands it back to the parser.
        assert!(text.is_empty());
        assert_eq!(out, vec![(Kind::Osc, b"\x1b]52;c;aGk=\x07".to_vec())]);

        let mut store = Store::new();
        assert!(matches!(ingest(&mut store, out[0].0, &out[0].1), Ingest::Passthrough));
    }

    #[test]
    fn utf8_text_containing_c1_bytes_is_not_mistaken_for_a_sequence() {
        // `✓` is e2 9c 93 and `ʝ` is ca 9d: both contain bytes that a naive
        // scanner would read as 8-bit DCS/OSC/APC introducers or ST.
        let mut splitter = Splitter::new();
        let line = "ok ✓ ʝ done\r\n".as_bytes();
        let (text, out) = pieces(&mut splitter, line);
        assert_eq!(text.as_slice(), line);
        assert!(out.is_empty());
    }

    #[test]
    fn an_endless_sequence_is_dropped_rather_than_buffered() {
        let mut splitter = Splitter::new();
        let (_, out) = pieces(&mut splitter, b"\x1b_Gi=1;");
        assert!(out.is_empty());

        // Pretend the buffer has already exceeded the cap.
        splitter.pending = Some((Kind::Apc, vec![0u8; MAX_BLOB + 1]));
        let (text, out) = pieces(&mut splitter, b"more");
        assert!(text.is_empty() && out.is_empty());
        assert!(splitter.pending.is_none());
    }

    #[test]
    fn kitty_controls_keep_what_a_move_needs() {
        let controls =
            KittyControls::parse(b"a=T,f=100,i=7,p=2,c=10,r=4,x=1,y=2,z=3,q=2,;payload").unwrap();
        assert_eq!(controls.action, b'T');
        assert_eq!(controls.id, Some(7));
        assert_eq!(controls.placement, Some(2));
        assert_eq!(controls.footprint(), Footprint::Cells(10, 4));

        let replay = String::from_utf8(controls.replay().unwrap()).unwrap();
        assert_eq!(
            replay,
            "\x1b_Ga=p,i=7,p=2,c=10,r=4,x=1,y=2,z=3,q=2\x1b\\"
        );
    }

    #[test]
    fn kitty_needs_an_id_to_be_moved_but_not_to_be_drawn() {
        let mut store = Store::new();
        let plain = b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\";
        assert!(matches!(
            store.ingest(Kind::Apc, plain, (3, 4), (80, 24)),
            Ingest::Stored
        ));

        let image = &store.images()[0];
        assert_eq!(image.anchor(), (3, 4));
        assert!(image.replay().is_none(), "no id, so it cannot be re-placed");
        assert!(!image.pixel_based());
        assert!(image.seq() > 0);
    }

    #[test]
    fn a_chunked_transmission_becomes_one_image() {
        let mut store = Store::new();

        // Two fragments (`m=1`, then `m=0`), exactly as a real sender writes
        // them: the action is on the first chunk only.
        let first = b"\x1b_Ga=T,f=100,i=5,m=1;AAAA\x1b\\";
        let second = b"\x1b_Gi=5,m=0;BBBB\x1b\\";

        assert!(matches!(
            store.ingest(Kind::Apc, first, (1, 1), (80, 24)),
            Ingest::Stored
        ));
        assert!(
            store.images().is_empty(),
            "a fragment is not an image yet"
        );

        assert!(matches!(
            store.ingest(Kind::Apc, second, (9, 9), (80, 24)),
            Ingest::Stored
        ));
        assert_eq!(store.images().len(), 1);

        let image = &store.images()[0];
        // Both fragments, in order, anchored where the image started.
        assert_eq!(image.bytes().len(), first.len() + second.len());
        assert_eq!(&image.bytes()[..3], &b"\x1b_G"[..]);
        assert_eq!(image.anchor(), (1, 1));
        // And it can be moved by id afterwards.
        assert_eq!(
            image.replay().map(|bytes| bytes.to_vec()),
            Some(b"\x1b_Ga=p,i=5\x1b\\".to_vec())
        );
    }

    #[test]
    fn a_transmit_only_sequence_is_never_replayed_as_a_placement() {
        let mut store = Store::new();
        // What a careful sender does: transmit once, place once.
        ingest(&mut store, Kind::Apc, b"\x1b_Ga=t,f=24,i=3;AAAA\x1b\\");
        ingest(&mut store, Kind::Apc, b"\x1b_Ga=p,i=3\x1b\\");

        let images = store.images();
        assert_eq!(images.len(), 2);
        assert!(
            images[0].replay().is_none(),
            "transmitting again is fine, placing it again is a second copy"
        );
        assert_eq!(
            images[1].replay().map(|bytes| bytes.to_vec()),
            Some(b"\x1b_Ga=p,i=3\x1b\\".to_vec())
        );
    }

    #[test]
    fn deleting_everything_empties_the_store() {
        let mut store = Store::new();
        ingest(&mut store, Kind::Apc, APC);
        assert!(matches!(
            ingest(&mut store, Kind::Apc, b"\x1b_Ga=d,d=A\x1b\\"),
            Ingest::Stored
        ));
        // Only the deletion itself is left: it still has to reach the terminal.
        assert_eq!(store.images().len(), 1);
        assert!(store.images()[0].replay().is_none());
    }

    #[test]
    fn a_kitty_query_is_answered_instead_of_stored() {
        let mut store = Store::new();
        match store.ingest(Kind::Apc, b"\x1b_Gi=9,a=q\x1b\\", (0, 0), (80, 24)) {
            Ingest::Reply(reply) => assert_eq!(reply.as_slice(), b"\x1b_Gi=9;OK\x1b\\"),
            other => panic!("a query must be answered, got {other:?}"),
        }
        assert!(store.images().is_empty());
    }

    #[test]
    fn a_kitty_delete_forgets_the_image_and_is_passed_through_once() {
        let mut store = Store::new();
        ingest(&mut store, Kind::Apc, APC);
        assert_eq!(store.images().len(), 1);

        assert!(matches!(
            ingest(&mut store, Kind::Apc, b"\x1b_Ga=d,i=7\x1b\\"),
            Ingest::Stored
        ));
        // The image is gone; the deletion itself is kept so the host forgets it
        // too, but it must never be re-placed, and it is newer than the image it
        // removed (so the renderer sends it).
        assert_eq!(store.images().len(), 1);
        assert!(store.images()[0].replay().is_none());
    }

    #[test]
    fn a_sixel_blit_is_pixel_based_and_anchored() {
        let mut store = Store::new();
        let sixel = b"\x1bPq\"1;1;2;2#0;2;0;0;0#0~~\x1b\\";
        assert!(matches!(
            store.ingest(Kind::Dcs, sixel, (5, 6), (80, 24)),
            Ingest::Stored
        ));

        let image = &store.images()[0];
        assert!(image.pixel_based(), "text painted over sixel erases it");
        assert_eq!(image.anchor(), (5, 6));
    }

    #[test]
    fn a_sixel_too_large_for_the_float_is_skipped() {
        let mut store = Store::new();
        // 1000x1000 pixels at the default 10x20 cell is 100x50 cells.
        let sixel = b"\x1bPq\"1;1;1000;1000#0~~\x1b\\";
        assert!(matches!(
            store.ingest(Kind::Dcs, sixel, (0, 0), (80, 24)),
            Ingest::Stored
        ));
        assert!(
            store.images().is_empty(),
            "an image that cannot fit must not be stored"
        );
    }

    #[test]
    fn an_inline_image_needs_inline_and_gets_a_cell_footprint() {
        let mut store = Store::new();
        let osc = b"\x1b]1337;File=inline=1;width=10;height=4:AAAA\x07";
        assert!(matches!(
            store.ingest(Kind::Osc, osc, (1, 2), (80, 24)),
            Ingest::Stored
        ));
        let download = b"\x1b]1337;File=name=aGk=:AAAA\x07";
        assert!(matches!(
            store.ingest(Kind::Osc, download, (1, 2), (80, 24)),
            Ingest::Passthrough
        ));
    }

    #[test]
    fn a_plain_dcs_is_not_graphics() {
        let mut store = Store::new();
        assert!(matches!(
            store.ingest(Kind::Dcs, b"\x1bP$qm\x1b\\", (0, 0), (80, 24)),
            Ingest::Passthrough
        ));
    }

    #[test]
    fn the_store_is_bounded_in_count_and_bytes() {
        let mut store = Store::new();
        for _ in 0..(MAX_IMAGES + 4) {
            ingest(&mut store, Kind::Apc, APC);
        }
        assert_eq!(store.images().len(), MAX_IMAGES);
        // The newest are the ones that survive, in order.
        let seqs: Vec<u64> = store.images().iter().map(Image::seq).collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "seqs: {seqs:?}");

        let huge = vec![b'A'; MAX_STORE_BYTES / 2 + 1];
        let mut blob = b"\x1b_Gf=24,i=1;".to_vec();
        blob.extend_from_slice(&huge);
        blob.extend_from_slice(b"\x1b\\");
        ingest(&mut store, Kind::Apc, &blob);
        assert!(store.bytes <= MAX_STORE_BYTES);
    }
}
