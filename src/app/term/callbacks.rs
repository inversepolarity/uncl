//! The child's side of the wire: answering its terminal queries.
//!
//! `vt100` hands every escape sequence it does not model to
//! [`vt100::Callbacks`]. We use that to answer the queries a terminal
//! application asks before it draws anything — device attributes, cursor
//! position, window size, colour queries — and to notice modes it switches on
//! that `vt100` does not track (currently focus reporting).
//!
//! The reply bytes go back through the *same* pty writer the user's keystrokes
//! use, so they are coalesced and ordered with input.
//!
//! A few things a child asks for are not for the child at all — an `OSC 52`
//! clipboard copy has to reach the *host* terminal. Those are queued on a host
//! channel and written by the one thread that owns stdout, so a child cannot
//! interleave its bytes with a frame.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use tokio::sync::mpsc::UnboundedSender;
use vt100::{Callbacks, Screen};

use super::caps::{ANSI_PALETTE, Capabilities, DEFAULT_BG, DEFAULT_FG, cell_px, rgb_spec};
use super::links::SharedLinks;

/// Write half of a child's pty, installed once the pty writer exists.
///
/// The callbacks are constructed before the pty is opened (the parser needs them
/// at construction time), so the channel is filled in afterwards.
#[derive(Clone, Debug, Default)]
pub struct ReplySink(Arc<OnceLock<UnboundedSender<Bytes>>>);

impl ReplySink {
    pub fn install(&self, tx: &UnboundedSender<Bytes>) {
        let _ = self.0.set(tx.clone());
    }

    pub fn send(&self, bytes: Vec<u8>) {
        if let Some(tx) = self.0.get() {
            let _ = tx.send(Bytes::from(bytes));
        }
    }
}

/// Modes a child switched on that `vt100` does not model for us, and the
/// hyperlinks it drew (see [`super::links`]).
#[derive(Debug, Default)]
pub struct ChildState {
    focus_reporting: AtomicBool,
    links: SharedLinks,
}

impl ChildState {
    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting.load(Ordering::Relaxed)
    }

    fn set_focus_reporting(&self, enabled: bool) {
        self.focus_reporting.store(enabled, Ordering::Relaxed);
    }

    /// The pane's hyperlinks, for the renderer.
    pub fn links(&self) -> &SharedLinks {
        &self.links
    }
}

#[derive(Clone, Debug)]
pub struct ChildCallbacks {
    sink: ReplySink,
    caps: Arc<Capabilities>,
    state: Arc<ChildState>,
    /// Effects destined for the host terminal, drained by the owner loop.
    host: UnboundedSender<Vec<u8>>,
}

impl ChildCallbacks {
    pub fn new(
        caps: Arc<Capabilities>,
        host: UnboundedSender<Vec<u8>>,
    ) -> (Self, ReplySink, Arc<ChildState>) {
        let sink = ReplySink::default();
        let state = Arc::new(ChildState::default());
        let callbacks = Self {
            sink: sink.clone(),
            caps,
            state: state.clone(),
            host,
        };
        (callbacks, sink, state)
    }

    pub fn state(&self) -> &Arc<ChildState> {
        &self.state
    }
}

impl Callbacks for ChildCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Focus reporting is the one mode we have to track ourselves.
        if i1 == Some(b'?') && (c == 'h' || c == 'l') {
            for param in params {
                if *param == [1004] {
                    self.state.set_focus_reporting(c == 'h');
                }
            }
        }

        if let Some(reply) = reply_for_csi(screen, &self.caps, i1, i2, params, c) {
            self.sink.send(reply);
        }
    }

    fn unhandled_osc(&mut self, screen: &mut Screen, params: &[&[u8]]) {
        // `OSC 8 ; <params> ; <target>`: a hyperlink. This is the only place the
        // cursor still knows which cells the link covers, so the span is
        // recorded here and re-emitted by the renderer.
        if let Some((id, target)) = osc8(params) {
            self.state.links.osc(screen, id, &target);
            return;
        }
        if let Some(reply) = reply_for_osc(params) {
            self.sink.send(reply);
        }
    }

    /// `OSC 52 ; <selector> ; <base64>` — the child copying to the clipboard.
    ///
    /// This is not a query about the child's terminal, it is a request the real
    /// terminal has to fulfil, so it goes to the host. `vt100` has already
    /// validated both the selector and the payload as base64, which means we can
    /// relay it byte for byte without decoding.
    fn copy_to_clipboard(&mut self, _screen: &mut Screen, ty: &[u8], data: &[u8]) {
        let _ = self.host.send(osc52(ty, data));
    }
}

/// Wrap a clipboard payload in the host-side `OSC 52` sequence.
///
/// Terminals that do not implement `OSC 52` ignore it, which is the correct
/// failure mode: the copy silently does nothing rather than corrupting output.
pub fn osc52(ty: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ty.len() + data.len() + 8);
    out.extend_from_slice(b"\x1b]52;");
    out.extend_from_slice(ty);
    out.push(b';');
    out.extend_from_slice(data);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Build the reply to a CSI sequence `vt100` did not handle.
///
/// Pure, so the whole query/response table is unit-testable.
pub fn reply_for_csi(
    screen: &Screen,
    caps: &Capabilities,
    i1: Option<u8>,
    i2: Option<u8>,
    params: &[&[u16]],
    c: char,
) -> Option<Vec<u8>> {
    let first = params.first().and_then(|param| param.first().copied());
    let _ = i2; // no query we answer uses a second intermediate

    match (i1, c) {
        // Primary device attributes.
        (None, 'c') => Some(caps.da1()),
        // Secondary device attributes.
        (Some(b'>'), 'c') => Some(caps.da2()),

        // Device status report.
        (None, 'n') => match first {
            Some(5) => Some(b"\x1b[0n".to_vec()),
            // Cursor position report.
            Some(6) => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes())
            }
            _ => None,
        },
        // Private cursor position report.
        (Some(b'?'), 'n') if first == Some(6) => {
            let (row, col) = screen.cursor_position();
            Some(format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes())
        }

        // Window reports (CSI n t). A child asking for geometry is asking *us*,
        // because we are its terminal — so every one of these has to be answered
        // consistently and none of the numbers may be zero.
        //
        // Zero is not a harmless placeholder here: `wezterm imgcat` asks for the
        // text area (`18t`) and the cell size (`16t`), and divides by the cell
        // size to turn pixels into cells. With `16t` unanswered it panicked with
        // "attempt to divide by zero" inside the float.
        (None, 't') => window_report(screen, first),

        // `XTSMGRAPHICS` (`CSI ? … S`) is answered by DA1 attribute 4 instead,
        // so children that ask for sixel geometry fall back to what we
        // advertised.
        _ => None,
    }
}

/// Answer a `CSI n t` window report, or `None` for the ones we cannot know.
///
/// The replies are derived from one pair of numbers — the child's grid and
/// [`cell_px`] — so a tool that cross-checks them (text area in cells times cell
/// size equals text area in pixels) gets a consistent story.
fn window_report(screen: &Screen, first: Option<u16>) -> Option<Vec<u8>> {
    let (rows, cols) = screen.size();
    let (cell_w, cell_h) = cell_px();

    // Pixel area, clamped: these are u16 fields in the reply and an 8K-wide
    // terminal at 30px a cell would overflow before it could reach the wire.
    let pixel_width = (u32::from(cols.get()) * u32::from(cell_w)).min(u32::from(u16::MAX));
    let pixel_height = (u32::from(rows.get()) * u32::from(cell_h)).min(u32::from(u16::MAX));

    // Every reply is `CSI <code> ; height ; width t` — height first, which is
    // the opposite of how it reads.
    let (code, height, width) = match first? {
        14 => (4, pixel_height, pixel_width),          // text area, pixels
        15 => (5, pixel_height, pixel_width),          // screen, pixels
        16 => (6, u32::from(cell_h), u32::from(cell_w)), // one cell, pixels
        18 => (8, u32::from(rows.get()), u32::from(cols.get())), // text area, cells
        19 => (9, u32::from(rows.get()), u32::from(cols.get())), // screen, cells
        _ => return None,
    };

    Some(format!("\x1b[{code};{height};{width}t").into_bytes())
}

/// Split an `OSC 8` into `(params, target)`, or `None` if it is another OSC.
///
/// `vte` splits the payload on `;`, and a URI may legitimately contain one
/// (`?a=1;b=2`), so everything after the second field is glued back together.
/// An empty target is the close of the previous link.
fn osc8(params: &[&[u8]]) -> Option<(&[u8], Vec<u8>)> {
    let (first, rest) = params.split_first()?;
    if *first != b"8" {
        return None;
    }

    match rest.split_first() {
        // `OSC 8 ; ; url` and `OSC 8 ; id=1 ; url`
        Some((id, target)) if !target.is_empty() => {
            let mut url = Vec::new();
            for (index, piece) in target.iter().enumerate() {
                if index > 0 {
                    url.push(b';');
                }
                url.extend_from_slice(piece);
            }
            Some((*id, url))
        }
        // `OSC 8 ; url` — no parameter field at all. Rare, but unambiguous.
        Some((only, _)) if !only.is_empty() => Some((b"", only.to_vec())),
        // `OSC 8 ; ;` closes the link.
        _ => Some((b"", Vec::new())),
    }
}

/// Build the reply to an OSC sequence `vt100` did not handle.
pub fn reply_for_osc(params: &[&[u8]]) -> Option<Vec<u8>> {
    match params {
        // Colour queries. `ST`-terminated, which every caller accepts.
        [b"10", b"?"] => Some(osc(&format!("10;{}", rgb_spec(DEFAULT_FG)))),
        [b"11", b"?"] => Some(osc(&format!("11;{}", rgb_spec(DEFAULT_BG)))),
        [b"4", index, b"?"] => {
            let index: usize = std::str::from_utf8(index).ok()?.parse().ok()?;
            let colour = ANSI_PALETTE.get(index)?;
            Some(osc(&format!("4;{index};{}", rgb_spec(*colour))))
        }
        _ => None,
    }
}

fn osc(body: &str) -> Vec<u8> {
    format!("\x1b]{body}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::nz;

    fn caps() -> Arc<Capabilities> {
        Arc::new(Capabilities {
            term: "xterm-256color".into(),
            term_program: None,
            colorterm: None,
            kitty_graphics: false,
            sixel: false,
            kitty_keyboard: false,
            truecolor: false,
        })
    }

    fn host() -> (tokio::sync::mpsc::UnboundedSender<Vec<u8>>, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn parser() -> vt100::Parser<ChildCallbacks> {
        let (callbacks, _sink, _state) = ChildCallbacks::new(caps(), host().0);
        vt100::Parser::new_with_callbacks(nz(4), nz(20), 0, callbacks)
    }

    #[test]
    fn answers_primary_da() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[], 'c').unwrap();
        assert_eq!(reply, b"\x1b[?1;6;9;15;22c".to_vec());
    }

    #[test]
    fn answers_secondary_da() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), Some(b'>'), None, &[], 'c').unwrap();
        assert_eq!(reply, b"\x1b[>0;1;0c".to_vec());
    }

    #[test]
    fn answers_cursor_position_one_based() {
        let mut parser = parser();
        parser.process(b"\x1b[3;7H");
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[&[6u16][..]], 'n').unwrap();
        assert_eq!(reply, b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn answers_window_size_in_characters() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[&[18u16][..]], 't').unwrap();
        assert_eq!(reply, b"\x1b[8;4;20t".to_vec());
    }

    #[test]
    fn answers_the_cell_size() {
        // `wezterm imgcat` divides by this: a silent `16t` is a divide by zero
        // in the child, which is what this test exists to prevent.
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[&[16u16][..]], 't').unwrap();
        let (cell_w, cell_h) = cell_px();
        assert_eq!(reply, format!("\x1b[6;{cell_h};{cell_w}t").into_bytes());
    }

    #[test]
    fn window_reports_are_internally_consistent() {
        // A 4-row, 20-column child. The cell size is whatever this machine
        // reports (`UNCL_CELL_PX` in the environment would legitimately change
        // it), so the expectations are derived from the same source.
        let (cell_w, cell_h) = cell_px();
        let (rows, cols) = (4u32, 20u32);

        let parser = parser();
        let screen = parser.screen().clone();
        let ask = |code: u16| {
            reply_for_csi(&screen, &caps(), None, None, &[&[code][..]], 't')
                .map(|reply| String::from_utf8(reply).unwrap())
        };

        assert_eq!(ask(18).unwrap(), format!("\x1b[8;{rows};{cols}t"));
        assert_eq!(ask(19).unwrap(), format!("\x1b[9;{rows};{cols}t"));
        assert_eq!(
            ask(14).unwrap(),
            format!("\x1b[4;{};{}t", rows * u32::from(cell_h), cols * u32::from(cell_w))
        );
        assert_eq!(
            ask(15).unwrap(),
            format!("\x1b[5;{};{}t", rows * u32::from(cell_h), cols * u32::from(cell_w))
        );
        assert_eq!(ask(16).unwrap(), format!("\x1b[6;{cell_h};{cell_w}t"));

        // The cross-check the terminal protocol promises: the pixel report is
        // exactly the cell report multiplied by the cell size.
        let pixel_area: Vec<u32> = ask(14).unwrap()[2..].trim_end_matches('t')
            .split(';').skip(1).map(|f| f.parse().unwrap()).collect();
        assert_eq!(pixel_area, vec![rows * u32::from(cell_h), cols * u32::from(cell_w)]);
    }

    #[test]
    fn no_window_report_is_ever_zero() {
        // Whatever the environment says, a zero dimension must never reach a
        // child: that is the shape of the bug this whole report exists for.
        for code in [14u16, 15, 16, 18, 19] {
            let parser = parser();
            let screen = parser.screen().clone();
            let reply = reply_for_csi(&screen, &caps(), None, None, &[&[code][..]], 't').unwrap();
            let text = String::from_utf8(reply).unwrap();
            let numbers: Vec<u32> = text[2..text.len() - 1]
                .split(';')
                .filter_map(|field| field.parse().ok())
                .collect();
            assert!(
                numbers.iter().all(|value| *value > 0),
                "code {code} answered {text:?}"
            );
        }
    }

    #[test]
    fn stays_silent_about_things_it_cannot_answer() {
        let parser = parser();
        let screen = parser.screen().clone();
        assert!(reply_for_csi(&screen, &caps(), Some(b'?'), None, &[&[1u16][..], &[0u16][..]], 'S').is_none());
        assert!(reply_for_csi(&screen, &caps(), Some(b'?'), None, &[&[0u16][..]], 'u').is_none());
        // Window *title* reports are the host's business, not ours.
        assert!(reply_for_csi(&screen, &caps(), None, None, &[&[20u16][..]], 't').is_none());
        assert!(reply_for_csi(&screen, &caps(), None, None, &[&[21u16][..]], 't').is_none());
    }

    #[test]
    fn answers_colour_queries() {
        assert_eq!(
            reply_for_osc(&[&b"11"[..], &b"?"[..]]).unwrap(),
            b"\x1b]11;rgb:00/00/00\x1b\\".to_vec()
        );
        assert_eq!(
            reply_for_osc(&[&b"4"[..], &b"1"[..], &b"?"[..]]).unwrap(),
            b"\x1b]4;1;rgb:cd/00/00\x1b\\".to_vec()
        );
        // out of range / not a query
        assert!(reply_for_osc(&[&b"4"[..], &b"99"[..], &b"?"[..]]).is_none());
        assert!(reply_for_osc(&[&b"52"[..], &b"c"[..], &b"?"[..]]).is_none());
        assert!(reply_for_osc(&[&b"8"[..], &b";;"[..], &b"https://x"[..]]).is_none());
    }

    #[test]
    fn tracks_focus_reporting_enable_and_disable() {
        let (mut callbacks, _sink, state) = ChildCallbacks::new(caps(), host().0);
        let mut screen_parser = vt100::Parser::new(nz(4), nz(20), 0);

        // 1004 is not modelled by vt100, so it reaches our callback.
        callbacks.unhandled_csi(
            screen_parser.screen_mut(),
            Some(b'?'),
            None,
            &[&[1004u16][..]],
            'h',
        );
        assert!(state.focus_reporting());

        callbacks.unhandled_csi(
            screen_parser.screen_mut(),
            Some(b'?'),
            None,
            &[&[1004u16][..]],
            'l',
        );
        assert!(!state.focus_reporting());
    }

    #[test]
    fn sink_delivers_to_the_writer_once_installed() {
        let (mut callbacks, sink, _state) = ChildCallbacks::new(caps(), host().0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        sink.install(&tx);

        let mut screen_parser = vt100::Parser::new(nz(4), nz(20), 0);
        callbacks.unhandled_csi(screen_parser.screen_mut(), None, None, &[], 'c');

        let sent = rx.try_recv().expect("a reply should have been queued");
        assert!(sent.starts_with(b"\x1b[?"));
    }

    #[test]
    fn an_osc8_is_split_into_parameters_and_a_target() {
        assert_eq!(
            osc8(&[&b"8"[..], &b""[..], &b"https://x.example"[..]]),
            Some((&b""[..], b"https://x.example".to_vec()))
        );
        assert_eq!(
            osc8(&[&b"8"[..], &b"id=1"[..], &b"file:///tmp"[..]]),
            Some((&b"id=1"[..], b"file:///tmp".to_vec()))
        );
        // A URI containing `;` is glued back together, because `vte` split it.
        assert_eq!(
            osc8(&[&b"8"[..], &b""[..], &b"https://x.example/?a=1"[..], &b"b=2"[..]]),
            Some((&b""[..], b"https://x.example/?a=1;b=2".to_vec()))
        );
        // Closing forms, and the two-field variant.
        assert_eq!(osc8(&[&b"8"[..], &b""[..], &b""[..]]), Some((&b""[..], Vec::new())));
        assert_eq!(osc8(&[&b"8"[..], &b"x"[..]]), Some((&b""[..], b"x".to_vec())));
        // Anything that is not a hyperlink is left alone.
        assert_eq!(osc8(&[&b"52"[..], &b"c"[..], &b"aGk="[..]]), None);
        assert_eq!(osc8(&[]), None);
    }

    #[test]
    fn a_hyperlink_is_recorded_and_drawn_later() {
        let (mut callbacks, _sink, state) = ChildCallbacks::new(caps(), host().0);
        let mut parser = vt100::Parser::new(nz(4), nz(20), 0);

        callbacks.unhandled_osc(parser.screen_mut(), &[&b"8"[..], &b""[..], &b"https://x"[..]]);
        parser.process(b"link");
        callbacks.unhandled_osc(parser.screen_mut(), &[&b"8"[..], &b""[..], &b""[..]]);

        let links = state.links().snapshot();
        let found = links.for_row(parser.screen(), 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].span(), (0, 4));
        assert_eq!(found[0].open(), b"\x1b]8;;https://x\x1b\\".to_vec());
    }

    #[test]
    fn an_osc8_is_not_answered() {
        // A hyperlink is not a query: replying would put bytes on the child's
        // stdin that it never asked for.
        let (mut callbacks, sink, _state) = ChildCallbacks::new(caps(), host().0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        sink.install(&tx);

        let mut parser = vt100::Parser::new(nz(4), nz(20), 0);
        callbacks.unhandled_osc(parser.screen_mut(), &[&b"8"[..], &b""[..], &b"https://x"[..]]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc52_is_wrapped_verbatim() {
        assert_eq!(
            osc52(b"c", b"aGVsbG8="),
            b"\x1b]52;c;aGVsbG8=\x1b\\".to_vec()
        );
    }

    #[test]
    fn clipboard_copies_are_sent_to_the_host_not_the_child() {
        let (host_tx, mut host_rx) = host();
        let (mut callbacks, sink, _state) = ChildCallbacks::new(caps(), host_tx);

        // The child's own writer must not see this at all.
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        sink.install(&child_tx);

        let mut screen = vt100::Parser::new(nz(4), nz(20), 0);
        callbacks.copy_to_clipboard(screen.screen_mut(), b"c", b"aGk=");

        let queued = host_rx.try_recv().expect("the copy should have been queued");
        assert_eq!(queued, b"\x1b]52;c;aGk=\x1b\\".to_vec());
        assert!(child_rx.try_recv().is_err());
    }
}
