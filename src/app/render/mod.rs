//! The frame emitter: uncl's own ANSI writer.
//!
//! Until S2 a frame was built like this: `vt100` grid → `ratatui::Buffer` of
//! styled cells → ratatui's diff → its crossterm backend → bytes. Every one of
//! those steps walks the whole screen and allocates, and the middle one is a
//! second copy of a grid we already have.
//!
//! This module writes the frame directly from the parser's screens:
//!
//! * **The owner pane** is painted with `vt100`'s row diff, so an unchanged row
//!   costs nothing and a scrolling shell only repaints what moved.
//! * **The float** is painted *over* it into its content rectangle. Because S1
//!   made the child's PTY exactly as wide as that rectangle, no horizontal
//!   clipping is needed — every row the child produces is exactly one row here.
//! * **One buffer per frame**: the caller does a single `write_all` + `flush`.
//! * **The cursor** is hidden while painting and placed once at the end, so it
//!   blinks at the position and in the shape the child asked for, with no cell
//!   ever styled to imitate it.
//!
//! Two things go wrong when a `vt100` row encoder is reused for a pane that is
//! *not* at the top-left of the screen, and both are handled here:
//!
//! 1. `vt100` emits **pane-local absolute** cursor moves (`CSI r;cH`) for
//!    backwards jumps within a row. Inside the float those addresses are wrong,
//!    so `append_row` rewrites them to screen coordinates.
//! 2. `vt100` ends a row with `EL` (`CSI K`) when the row's tail is styled but
//!    empty (a coloured status bar, a `btop` pane border). On a full-width pane
//!    that is only wasteful; inside a float it erases the float's border *and*
//!    everything to the right of it. Such rows are followed by `repair_tail`,
//!    which repaints the owner's tail segment for that row.
//!
//! Input modes (`?1000`, `?2004`, `DECCKM`) are deliberately **never** replayed
//! from the child's screen: uncl owns the host terminal's modes (see
//! `ui::owner::HostMouse`), and replaying `state_diff` would make the host start
//! reporting in a protocol the child never asked for.

use vt100::Screen;

use crate::app::images::Store;
use crate::app::term::links::{Link, Links, OSC8_CLOSE};
use crate::constants::content_rect;
use crate::geometry::Rect;

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const RESET: &[u8] = b"\x1b[0m";
const BORDER_COLOR: &[u8] = b"\x1b[32m";
const BORDER_VERTICAL: &str = "│";
/// kitty: delete every placement (not the image data). Sent when the float goes
/// away, because kitty images live in a layer that our own repaint cannot touch.
const KITTY_DELETE_ALL: &[u8] = b"\x1b_Ga=d,d=A\x1b\\";

/// Everything one frame needs: the two snapshots and where they go.
///
/// The screens are **owned**: the caller takes one clone per pane under the
/// parser lock (so the lock is never held while a frame is encoded) and hands the
/// snapshots over, which becomes the `prev` state for the next frame. Taking
/// ownership is what keeps this at one clone per pane per frame instead of two.
pub struct Panes<'a> {
    pub owner: Screen,
    pub owner_area: Rect,
    /// The float's screen and its rectangle *including* the border.
    pub float: Option<(Screen, Rect)>,
    pub float_focused: bool,
    /// Graphics the float's child has drawn, in the child's own coordinates.
    pub images: Option<&'a Store>,
    /// Hyperlinks both children drew, in their own coordinates.
    pub links: FrameLinks,
}

/// The hyperlinks each pane drew this frame (`OSC 8`, S3.5).
///
/// Owned snapshots rather than borrows, for the same reason the screens are
/// owned: the caller takes them under the parser's lock and the frame is encoded
/// afterwards. The common case — no links at all — is a `Vec::new()` and costs
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct FrameLinks {
    pub owner: Links,
    pub float: Links,
}

/// Per-frame state: what the host terminal was last told to display.
pub struct Renderer {
    out: Vec<u8>,
    owner_prev: Option<Screen>,
    float_prev: Option<Screen>,
    /// Is the cursor currently shown, as far as we have told the terminal?
    cursor_shown: bool,
    /// Where we last put it, or `None` when painting has moved it since.
    cursor_at: Option<(u16, u16)>,
    force_full: bool,
    /// The content rect the images were last drawn into. A change means every
    /// placement has to move.
    image_layout: Option<Rect>,
    /// The newest image sequence the terminal has been told about.
    last_image_seq: u64,
    /// Have we drawn anything that lives in the terminal's image layer?
    images_shown: bool,
    /// Did we delete the images (float hidden)? Then the next show has to send
    /// the payloads again instead of re-placing by id.
    images_forgotten: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            owner_prev: None,
            float_prev: None,
            // Terminals start with a visible cursor, and the guard shows it again
            // on the way out.
            cursor_shown: true,
            cursor_at: None,
            force_full: true,
            image_layout: None,
            last_image_seq: 0,
            images_shown: false,
            images_forgotten: false,
        }
    }

    /// Repaint everything on the next frame.
    ///
    /// Required whenever the answer to "what is already on the screen?" is no
    /// longer the previous frame: show/hide, a resize, or a new child.
    pub fn invalidate(&mut self) {
        self.force_full = true;
        self.owner_prev = None;
        self.float_prev = None;
        // The float may be somewhere else now, so every placement has to move.
        // The image *data* does not: the terminal still holds it, and a kitty
        // image is one `a=p,i=<id>` away rather than a re-send of the payload.
        self.image_layout = None;
    }

    /// The float's child was replaced: its images belong to a process that no
    /// longer exists, so drop the terminal's whole image layer and let the next
    /// frame start again.
    pub fn forget_images(&mut self) {
        self.images_forgotten = true;
        self.images_shown = true;
        self.image_layout = None;
        self.last_image_seq = 0;
    }

    /// Build one frame. The returned bytes are empty when nothing changed.
    pub fn render(&mut self, panes: Panes<'_>) -> &[u8] {
        self.out.clear();

        let Panes {
            owner,
            owner_area,
            float,
            float_focused,
            images,
            links,
        } = panes;
        let float_at = float.as_ref().map(|(screen, rect)| (screen, *rect));

        let owner_full = self.force_full || self.owner_prev.is_none();
        let float_full = self.force_full || self.float_prev.is_none();

        let mut owner_rows = if owner_area.is_empty() {
            Vec::new()
        } else {
            dirty_rows(&owner, self.owner_prev.as_ref(), owner_area, owner_full)
        };

        // A row that gained a hyperlink has to be repainted even when its cells
        // did not change: the link is not part of the cell, so the diff cannot
        // see it.
        if !links.owner.is_empty() {
            for row in links.owner.rows() {
                if row < owner_area.height && !owner_rows.contains(&row) {
                    owner_rows.push(row);
                }
            }
        }

        let (float_plan, borders) = match float_at {
            Some((screen, rect)) => {
                let plan = plan_float(
                    screen,
                    self.float_prev.as_ref(),
                    content_rect(rect),
                    &owner_rows,
                    owner_area,
                    float_full,
                    &links.float,
                );

                // The owner painted over the float's top/bottom border rows, or
                // the whole float is new: both borders have to be redrawn.
                let borders = float_full
                    || owner_rows
                        .iter()
                        .any(|row| *row == rect.y || *row == rect.bottom().saturating_sub(1));

                (plan, borders)
            }
            None => (Vec::new(), false),
        };

        if !owner_rows.is_empty() || !float_plan.is_empty() || borders {
            self.begin_paint();
        }

        if !owner_rows.is_empty() {
            emit_owner_rows(
                &mut self.out,
                &owner,
                self.owner_prev.as_ref(),
                owner_area,
                &owner_rows,
                owner_full,
                &links.owner,
            );
        }

        if let Some((screen, rect)) = float_at {
            emit_float(
                &mut self.out,
                &owner,
                screen,
                self.float_prev.as_ref(),
                content_rect(rect),
                rect,
                owner_area,
                &float_plan,
                borders,
                &links.float,
            );
        }

        // Graphics go last, after the grid: a pixel-based image is destroyed by
        // any text painted over it, and a kitty image would be covered by the
        // grid rows above it. Each image is sized to the room it has left inside
        // the float, so none of them can spill over the border.
        let repainted = !float_plan.is_empty() || borders;
        let content = float_at.map(|(_, rect)| content_rect(rect));
        self.emit_images(images, content, repainted);

        let (cursor_screen, cursor_area) = match float_at {
            Some((screen, rect)) if float_focused => (screen, content_rect(rect)),
            _ => (&owner, owner_area),
        };
        self.place_cursor(cursor_screen, cursor_area);

        // The snapshots become the reference for the next frame.
        self.owner_prev = Some(owner);
        self.float_prev = float.map(|(screen, _)| screen);
        self.force_full = false;

        &self.out
    }

    /// Hide the cursor before painting: rows are drawn with `CUP` jumps, and a
    /// visible cursor would run along with them.
    fn begin_paint(&mut self) {
        if self.cursor_shown {
            self.out.extend_from_slice(HIDE_CURSOR);
            self.cursor_shown = false;
        }
        // Painting leaves the terminal's cursor wherever the last row ended.
        self.cursor_at = None;
    }

    /// Re-emit the float's graphics at their (possibly new) cells.
    ///
    /// Three reasons to emit anything at all:
    ///
    /// * the image is **new** — the terminal has never seen it;
    /// * the float **moved** (show, drag, resize) — every placement has to follow,
    ///   which for kitty is a one-line `a=p,i=<id>` rather than the payload;
    /// * a **pixel-based** image (sixel, iTerm2) had its rows repainted, which
    ///   erased it, because those protocols draw into the same plane as text.
    ///
    /// A still screen with a kitty image therefore costs nothing per frame, which
    /// is the whole point of putting this after the grid diff.
    fn emit_images(&mut self, store: Option<&Store>, content: Option<Rect>, repainted: bool) {
        let Some(content) = content else {
            // The float is gone. Its kitty images live in a layer our repaint
            // cannot reach, so they have to be deleted explicitly, or they stay
            // stuck on the owner's screen.
            if self.images_shown {
                self.out.extend_from_slice(KITTY_DELETE_ALL);
                self.images_shown = false;
                self.images_forgotten = true;
                self.cursor_at = None;
            }
            self.image_layout = None;
            return;
        };

        let layout_changed = self.image_layout != Some(content);
        let mut emitted = false;

        // Everything we thought the terminal held is gone (the float came back,
        // or its child was replaced): start from an empty image layer, so a dead
        // child's pictures cannot survive into the next one.
        if self.images_forgotten {
            self.out.extend_from_slice(KITTY_DELETE_ALL);
            emitted = true;
        }

        if let Some(store) = store {
            for image in store.images() {
                // `vt100` reports the cursor as (row, column).
                let (row, col) = image.anchor();

                // Anything anchored outside the child's grid is not ours to draw:
                // it would land on the border or on the owner's cells.
                if col >= content.width || row >= content.height {
                    self.last_image_seq = self.last_image_seq.max(image.seq());
                    continue;
                }

                let fresh = self.images_forgotten || image.seq() > self.last_image_seq;
                let moved = layout_changed && image.replay().is_some();
                let erased = repainted && image.pixel_based();

                // Everything from the anchor cell to the float's bottom-right
                // corner is the room an image is allowed to take. The child's
                // grid is the content rect (S1's geometry law), so a child that
                // drew at its own origin lands inside the float, instead of
                // across the border and over the owner's screen.
                let avail = (
                    content.width.saturating_sub(col),
                    content.height.saturating_sub(row),
                );

                if fresh || moved || erased {
                    // Re-placing an image the terminal already holds costs one
                    // line; anything else is the payload, sized to `avail`.
                    let bytes = match image.replay() {
                        Some(_) if !fresh => image
                            .replace(Some(avail))
                            .unwrap_or_else(|| image.render(Some(avail))),
                        _ => image.render(Some(avail)),
                    };
                    cup(&mut self.out, content.y.saturating_add(row), content.x.saturating_add(col));
                    self.out.extend_from_slice(&bytes);
                    emitted = true;
                }

                self.last_image_seq = self.last_image_seq.max(image.seq());
            }
        }

        if emitted {
            self.images_shown = true;
            // The last image left the cursor somewhere in its own pixels.
            self.cursor_at = None;
        }
        self.images_forgotten = false;
        self.image_layout = Some(content);
    }

    fn place_cursor(&mut self, screen: &Screen, area: Rect) {
        if screen.hide_cursor() {
            if self.cursor_shown {
                self.out.extend_from_slice(HIDE_CURSOR);
                self.cursor_shown = false;
                self.cursor_at = None;
            }
            return;
        }

        let (row, col) = screen.cursor_position();
        let x = area
            .x
            .saturating_add(col)
            .min(area.right().saturating_sub(1));
        let y = area
            .y
            .saturating_add(row)
            .min(area.bottom().saturating_sub(1));

        if self.cursor_at != Some((x, y)) {
            cup(&mut self.out, y, x);
            self.cursor_at = Some((x, y));
        }
        if !self.cursor_shown {
            self.out.extend_from_slice(SHOW_CURSOR);
            self.cursor_shown = true;
        }
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Do the two screens show the same row?
fn rows_equal(cur: &Screen, prev: &Screen, row: u16, width: u16) -> bool {
    for col in 0..width {
        if cur.cell(row, col) != prev.cell(row, col) {
            return false;
        }
    }
    true
}

fn dirty_rows(
    cur: &Screen,
    prev: Option<&Screen>,
    area: Rect,
    full: bool,
) -> Vec<u16> {
    if full {
        return (0..area.height).collect();
    }
    let Some(prev) = prev else {
        return (0..area.height).collect();
    };

    let mut rows = Vec::new();
    for row in 0..area.height {
        if !rows_equal(cur, prev, row, area.width) {
            rows.push(row);
        }
    }
    rows
}

/// How one float row has to be painted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FloatRow {
    /// Only the cells that changed: the screen still matches the previous frame,
    /// so `vt100`'s row diff applies (`rows_diff` costs nothing for a row that
    /// did not change).
    Diff(u16),
    /// The whole row: the float is new, or the owner painted over it.
    Full(u16),
}

fn plan_float(
    cur: &Screen,
    prev: Option<&Screen>,
    content: Rect,
    owner_rows: &[u16],
    owner_area: Rect,
    full: bool,
    links: &Links,
) -> Vec<FloatRow> {
    let mut plan = Vec::new();

    for row in 0..content.height {
        // The owner pane is painted first and covers the whole screen, so a row
        // it repainted under the float has to be restored in full. (`owner_rows`
        // is pane-relative; the float's rect is in screen coordinates.)
        let screen_row = content.y.saturating_add(row);
        let covered = owner_rows.contains(&screen_row.saturating_sub(owner_area.y));

        // A hyperlinked row is painted whole: `rows_diff` only carries the cells
        // that changed, and there is no honest way to splice a span into that.
        let linked = links.covers(cur, row);

        let entry = if full || covered || linked {
            Some(FloatRow::Full(row))
        } else {
            match prev {
                None => Some(FloatRow::Full(row)),
                Some(prev) if !rows_equal(cur, prev, row, content.width) => {
                    Some(FloatRow::Diff(row))
                }
                Some(_) => None,
            }
        };

        if let Some(entry) = entry {
            plan.push(entry);
        }
    }

    plan
}

fn emit_owner_rows(
    out: &mut Vec<u8>,
    cur: &Screen,
    prev: Option<&Screen>,
    area: Rect,
    rows: &[u16],
    full: bool,
    links: &Links,
) {
    let width = area.width;

    if full || prev.is_none() {
        // A full repaint has to blank each row first: `rows_formatted` skips
        // default cells and relies on what is already on screen.
        for (row, text) in cur.rows_formatted(0, width).enumerate() {
            let row = row as u16;
            if !rows.contains(&row) {
                continue;
            }
            cup(out, area.y.saturating_add(row), area.x);
            reset(out);
            erase_chars(out, width);
            if links.covers(cur, row) {
                emit_row_segments(out, cur, links, row, area.y, area.x, width);
            } else {
                append_row(out, &text, area.y, area.x);
            }
        }
        return;
    }

    let prev = prev.unwrap_or(cur);
    for (row, text) in cur.rows_diff(prev, 0, width).enumerate() {
        let row = row as u16;
        if !rows.contains(&row) {
            continue;
        }

        // A row that carries a hyperlink is painted whole, so the span can be
        // wrapped: a diff is a sparse set of cells and has no span to wrap.
        if links.covers(cur, row) {
            cup(out, area.y.saturating_add(row), area.x);
            reset(out);
            erase_chars(out, width);
            emit_row_segments(out, cur, links, row, area.y, area.x, width);
            continue;
        }

        if text.is_empty() {
            continue;
        }
        cup(out, area.y.saturating_add(row), area.x);
        reset(out);
        append_row(out, &text, area.y, area.x);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_float(
    out: &mut Vec<u8>,
    owner: &Screen,
    cur: &Screen,
    prev: Option<&Screen>,
    content: Rect,
    rect: Rect,
    owner_area: Rect,
    plan: &[FloatRow],
    borders: bool,
    links: &Links,
) {
    let width = content.width;

    // Diffs are what keep a busy TUI cheap: `rows_diff` returns an empty string
    // (and allocates nothing) for a row that did not change.
    let mut diffs = prev.map(|prev| cur.rows_diff(prev, 0, width));
    let mut diff_at: u16 = 0;

    for entry in plan {
        match *entry {
            FloatRow::Diff(row) => {
                let Some(diffs) = diffs.as_mut() else {
                    continue;
                };
                let text = diffs
                    .nth(usize::from(row.saturating_sub(diff_at)))
                    .unwrap_or_default();
                diff_at = row.saturating_add(1);

                if !text.is_empty() {
                    emit_float_row(out, owner, cur, content, rect, owner_area, row, &text, false, links);
                }
            }
            FloatRow::Full(row) => {
                let text = cur
                    .rows_formatted(0, width)
                    .nth(usize::from(row))
                    .unwrap_or_default();
                emit_float_row(out, owner, cur, content, rect, owner_area, row, &text, true, links);
            }
        }
    }

    if borders {
        emit_horizontal_border(out, rect, rect.y, Corner::TopLeft, Corner::TopRight);
        emit_horizontal_border(
            out,
            rect,
            rect.bottom().saturating_sub(1),
            Corner::BottomLeft,
            Corner::BottomRight,
        );
        emit_title(out, rect, content);
    }
}

/// Paint one row of the float: both border cells, the child's cells, and — when
/// the child's row erased to the end of the line — the owner's tail beyond it.
#[allow(clippy::too_many_arguments)]
fn emit_float_row(
    out: &mut Vec<u8>,
    owner: &Screen,
    cur: &Screen,
    content: Rect,
    rect: Rect,
    owner_area: Rect,
    row: u16,
    text: &str,
    blank_first: bool,
    links: &Links,
) {
    let screen_row = content.y.saturating_add(row);

    cup(out, screen_row, rect.x);
    border_char(out, BORDER_VERTICAL);

    cup(out, screen_row, content.x);
    reset(out);
    if blank_first {
        // Bounded to the float's width, so it can never reach the border or the
        // owner pane. Needed only when the cells are not known to be the ones we
        // last painted.
        erase_chars(out, content.width);
    }
    let erased = if links.covers(cur, row) {
        emit_row_segments(out, cur, links, row, content.y, content.x, content.width)
    } else {
        append_row(out, text, content.y, content.x)
    };

    // After the row: an `EL` inside it would have erased the border cell.
    cup(out, screen_row, rect.right().saturating_sub(1));
    border_char(out, BORDER_VERTICAL);

    if erased {
        repair_tail(out, owner, screen_row, rect, owner_area);
    }
}

/// Paint one row in segments, wrapping each hyperlinked span in `OSC 8`.
///
/// The row is written as up to three pieces — plain, linked, plain — because the
/// span's columns are known while a byte offset *inside* `vt100`'s encoded row is
/// not. Each piece is asked for by column range, which is the same call
/// `repair_tail` uses, so wide characters and styles stay correct.
///
/// Returns whether any piece ended with `EL` (which the float's caller has to
/// repair beyond the border).
fn emit_row_segments(
    out: &mut Vec<u8>,
    screen: &Screen,
    links: &Links,
    row: u16,
    origin_y: u16,
    origin_x: u16,
    width: u16,
) -> bool {
    let found = links.for_row(screen, row);
    if found.is_empty() {
        return false;
    }

    let mut erased = false;

    for (start, len, open) in segments(&found, width) {
        let Some(text) = screen.rows_formatted(start, len).nth(usize::from(row)) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }

        cup(
            out,
            origin_y.saturating_add(row),
            origin_x.saturating_add(start),
        );
        reset(out);
        if let Some(open) = open.as_ref() {
            out.extend_from_slice(open);
        }
        // Absolute moves inside the text are pane-local, so only the pane's
        // origin is added; the segment's own column was set by the `CUP` above.
        erased |= append_row(out, &text, origin_y, origin_x);
        if open.is_some() {
            out.extend_from_slice(OSC8_CLOSE);
        }
    }

    erased
}

/// The row, split at link boundaries: `(start, len, opening sequence)`.
fn segments(links: &[&Link], width: u16) -> Vec<(u16, u16, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut cursor = 0u16;

    for link in links {
        let (start, end) = link.span();
        // Overlapping spans cannot both be honoured: the first one wins, which
        // keeps the painted row a faithful copy of the cells.
        if start < cursor || start >= width {
            continue;
        }
        let end = end.min(width);
        if end <= start {
            continue;
        }
        if start > cursor {
            out.push((cursor, start - cursor, None));
        }
        out.push((start, end - start, Some(link.open())));
        cursor = end;
    }

    if cursor < width {
        out.push((cursor, width - cursor, None));
    }
    out
}

/// Repaint what a float row's `EL` erased beyond the float: the owner's tail.
///
/// The float's own right border is redrawn by the caller; this restores the
/// cells to the right of the float, which the child's `EL` could not know about.
fn repair_tail(out: &mut Vec<u8>, owner: &Screen, row: u16, rect: Rect, owner_area: Rect) {
    // `rows_formatted` indexes into the row, so a rect that is (briefly) wider than
    // the parser's grid must not become an out-of-range start column.
    let cols = owner.size().1.get();
    let start = rect.right().min(cols);
    let end = owner_area.right().min(cols);
    if start >= end {
        return;
    }

    let width = end - start;
    let owner_row = usize::from(row.saturating_sub(owner_area.y));
    if let Some(text) = owner.rows_formatted(start, width).nth(owner_row) {
        // The sub-row's origin is the first column to the right of the float, so
        // its pane-local jumps shift by exactly that.
        cup(out, row, start);
        reset(out);
        append_row(out, &text, row, start);
    }
}

#[derive(Clone, Copy)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    fn ch(self) -> char {
        match self {
            Corner::TopLeft => '┌',
            Corner::TopRight => '┐',
            Corner::BottomLeft => '└',
            Corner::BottomRight => '┘',
        }
    }
}

fn emit_horizontal_border(
    out: &mut Vec<u8>,
    rect: Rect,
    row: u16,
    left: Corner,
    right: Corner,
) {
    let width = usize::from(rect.width);
    if width < 2 {
        return;
    }

    let mut line = String::with_capacity(width * 3);
    line.push(left.ch());
    for _ in 0..width - 2 {
        line.push('─');
    }
    line.push(right.ch());

    cup(out, row, rect.x);
    out.extend_from_slice(BORDER_COLOR);
    out.extend_from_slice(line.as_bytes());
    reset(out);
}

/// The child's size, printed in the bottom border. ASCII only, so `len()` is the
/// cell count.
fn emit_title(out: &mut Vec<u8>, rect: Rect, content: Rect) {
    let title = format!("s:{}:{}", content.height, content.width);
    let available = usize::from(rect.width.saturating_sub(2));
    if title.len() > available {
        return;
    }

    let start = rect
        .x
        .saturating_add(1)
        .saturating_add(available as u16 - title.len() as u16);

    cup(out, rect.bottom().saturating_sub(1), start);
    out.extend_from_slice(BORDER_COLOR);
    out.extend_from_slice(title.as_bytes());
    reset(out);
}

fn border_char(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(BORDER_COLOR);
    out.extend_from_slice(text.as_bytes());
    reset(out);
}

/// `CSI row ; col H`, one-based on the wire, zero-based in the arguments.
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(b"\x1b[");
    push_u16(out, row.saturating_add(1));
    out.push(b';');
    push_u16(out, col.saturating_add(1));
    out.push(b'H');
}

/// `CSI n X`: erase `n` cells in place, without moving the cursor and without
/// touching anything past them.
fn erase_chars(out: &mut Vec<u8>, cells: u16) {
    if cells == 0 {
        return;
    }
    out.extend_from_slice(b"\x1b[");
    push_u16(out, cells);
    out.push(b'X');
}

fn reset(out: &mut Vec<u8>) {
    out.extend_from_slice(RESET);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    let mut buf = [0u8; 5];
    let mut at = buf.len();
    let mut value = value;

    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }

    out.extend_from_slice(&buf[at..]);
}

/// Append one row emitted by `vt100` for a pane whose origin (row 0, column 0) is
/// at `(dy, dx)` on the real screen.
///
/// Everything `vt100` emits inside a row is relative — except `CSI r;cH` for a
/// backwards jump (used when clearing a run of styled-but-empty cells), which is
/// relative to the *pane's* origin and therefore has to be shifted.
///
/// Returns whether the row ended with `EL`, i.e. whether the caller has to repair
/// whatever lies to the right of this pane.
fn append_row(out: &mut Vec<u8>, text: &str, dy: u16, dx: u16) -> bool {
    if dy == 0 && dx == 0 {
        out.extend_from_slice(text.as_bytes());
        return text.contains("\x1b[K");
    }

    let bytes = text.as_bytes();
    let mut at = 0;
    let mut erased = false;

    while at < bytes.len() {
        if bytes[at] == 0x1b && bytes.get(at + 1) == Some(&b'[') {
            let start = at;
            let mut end = at + 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end >= bytes.len() {
                out.extend_from_slice(&bytes[start..]);
                return erased;
            }

            let final_byte = bytes[end];
            let params = &bytes[start + 2..end];

            if final_byte == b'H' || final_byte == b'f' {
                match parse_cup(params) {
                    // `CSI H` (vt100 emits it for its own origin) means 1;1, so it
                    // has to be shifted like any other absolute move.
                    Some((row, col)) => cup(
                        out,
                        row.saturating_sub(1).saturating_add(dy),
                        col.saturating_sub(1).saturating_add(dx),
                    ),
                    None => out.extend_from_slice(&bytes[start..=end]),
                }
            } else {
                if final_byte == b'K' {
                    erased = true;
                }
                out.extend_from_slice(&bytes[start..=end]);
            }

            at = end + 1;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }

    erased
}

/// Parse the parameters of a `CUP`/`HVP`, one-based on the wire.
///
/// `CSI H` (no parameters at all) and an empty or zero field both mean 1, which is
/// how `vt100` writes its own origin.
fn parse_cup(params: &[u8]) -> Option<(u16, u16)> {
    let mut parts = params.split(|byte| *byte == b';');

    let row = parse_cup_field(parts.next()?)?;
    let col = match parts.next() {
        Some(field) => parse_cup_field(field)?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((row, col))
}

/// One `CUP` field: empty or zero means 1, per ECMA-48.
fn parse_cup_field(field: &[u8]) -> Option<u16> {
    if field.is_empty() {
        return Some(1);
    }
    Some(parse_u16(field)?.max(1))
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }

    let mut value: u32 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
        if value > u32::from(u16::MAX) {
            return None;
        }
    }
    Some(value as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::images::Ingest;
    use crate::constants::nz;

    fn parser(rows: u16, cols: u16) -> vt100::Parser {
        vt100::Parser::new(nz(rows), nz(cols), 0)
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn cup_and_erase_are_one_based_and_padded() {
        let mut out = Vec::new();
        cup(&mut out, 0, 0);
        cup(&mut out, 11, 4);
        erase_chars(&mut out, 7);
        assert_eq!(text(&out), "\x1b[1;1H\x1b[12;5H\x1b[7X");
    }

    #[test]
    fn push_u16_writes_every_digit() {
        let mut out = Vec::new();
        for value in [0u16, 9, 10, 65535] {
            push_u16(&mut out, value);
            out.push(b',');
        }
        assert_eq!(text(&out), "0,9,10,65535,");
    }

    #[test]
    fn append_row_shifts_absolute_moves_and_leaves_relative_ones_alone() {
        let mut out = Vec::new();
        // A backwards jump (pane-local) followed by a relative move.
        let erased = append_row(&mut out, "ab\x1b[2;5Hcd\x1b[3C", 10, 20);
        assert!(!erased);
        // `CSI 2;5H` was pane row 2, column 5 → screen 12, 25.
        assert_eq!(text(&out), "ab\x1b[12;25Hcd\x1b[3C");
    }

    #[test]
    fn append_row_reports_and_keeps_el() {
        let mut out = Vec::new();
        let erased = append_row(&mut out, "x\x1b[41m\x1b[K", 0, 0);
        assert!(erased);
        assert_eq!(text(&out), "x\x1b[41m\x1b[K");
    }

    #[test]
    fn append_row_does_not_shift_other_csi_sequences() {
        let mut out = Vec::new();
        append_row(&mut out, "\x1b[0m\x1b[?25l\x1b[2X", 5, 5);
        assert_eq!(text(&out), "\x1b[0m\x1b[?25l\x1b[2X");
    }

    #[test]
    fn parse_cup_defaults_missing_and_empty_fields_to_one() {
        assert_eq!(parse_cup(b"3;7"), Some((3, 7)));
        assert_eq!(parse_cup(b""), Some((1, 1)));
        assert_eq!(parse_cup(b"3"), Some((3, 1)));
        assert_eq!(parse_cup(b";7"), Some((1, 7)));
        assert_eq!(parse_cup(b"0;0"), Some((1, 1)));
        assert_eq!(parse_cup(b"3;7;9"), None);
        assert_eq!(parse_cup(b"a;7"), None);
        assert_eq!(parse_u16(b"65536"), None);
    }

    #[test]
    fn a_bare_cup_is_the_panes_own_origin() {
        // `vt100` writes `CSI H` for its pane's (0, 0).
        let mut out = Vec::new();
        let erased = append_row(&mut out, "ab\x1b[Hcd", 10, 20);
        assert!(!erased);
        assert_eq!(text(&out), "ab\x1b[11;21Hcd");
    }

    #[test]
    fn a_new_renderer_paints_everything_once() {
        let mut parser = parser(4, 20);
        parser.process(b"hello");

        let area = Rect::new(0, 0, 20, 4);
        let mut renderer = Renderer::new();
        let frame = renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });

        let frame = text(frame);
        assert!(frame.starts_with("\x1b[?25l"), "frame: {frame:?}");
        assert!(frame.contains("hello"), "frame: {frame:?}");
        assert!(frame.contains("\x1b[1;1H"), "frame: {frame:?}");
        assert!(frame.ends_with("\x1b[?25h"), "frame: {frame:?}");
    }

    #[test]
    fn an_unchanged_screen_costs_nothing() {
        let mut parser = parser(4, 20);
        parser.process(b"hello");

        let area = Rect::new(0, 0, 20, 4);
        let mut renderer = Renderer::new();
        renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });

        let second = renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });
        assert!(second.is_empty(), "second frame: {:?}", text(second));
    }

    #[test]
    fn one_changed_row_repaints_one_row() {
        let mut parser = parser(4, 20);
        parser.process(b"one\r\ntwo");

        let area = Rect::new(0, 0, 20, 4);
        let mut renderer = Renderer::new();
        renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });

        parser.process(b"\r\nchanged");
        let frame = text(renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("changed"), "frame: {frame:?}");
        // Row 0 did not change, so it must not be repainted.
        assert!(!frame.contains("\x1b[1;1H"), "frame: {frame:?}");
    }

    #[test]
    fn the_float_is_drawn_inside_its_border() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(4, 12);
        float.process(b"floating");

        let rect = Rect::new(2, 2, 14, 6);
        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("floating"), "frame: {frame:?}");
        // Top-left corner, and content one cell in.
        assert!(frame.contains(&format!("\x1b[{};{}H", rect.y + 1, rect.x + 1)));
        assert!(frame.contains(&format!("\x1b[{};{}H", rect.y + 2, rect.x + 2)));
        assert!(frame.contains('┌') && frame.contains('┘'));
        // The child's size is printed in the bottom border.
        assert!(frame.contains("s:4:12"), "frame: {frame:?}");
    }

    #[test]
    fn a_hidden_float_leaves_the_owner_alone() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");

        let area = Rect::new(0, 0, 40, 10);
        let mut renderer = Renderer::new();
        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });
        // The float disappearing is a repaint, but the owner's cells are intact.
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        }));
        assert!(frame.is_empty(), "frame: {frame:?}");
    }

    #[test]
    fn hiding_the_cursor_emits_dectcem() {
        let mut parser = parser(4, 20);
        parser.process(b"\x1b[?25l");

        let area = Rect::new(0, 0, 20, 4);
        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: parser.screen().clone(),
            owner_area: area,
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("\x1b[?25l"), "frame: {frame:?}");
        assert!(!frame.contains("\x1b[?25h"), "frame: {frame:?}");
    }

    #[test]
    fn the_owner_tail_is_repaired_after_a_float_el() {
        let rect = Rect::new(2, 0, 12, 3);
        let content = content_rect(rect);

        // Owner text to the right of the float, on its first content row. Rows and
        // columns are derived from the rect: a hand-written row here is exactly the
        // kind of off-by-one this test is supposed to catch.
        let mut owner = parser(10, 40);
        let place = format!("\x1b[{};20HTAIL", content.y + 1);
        owner.process(place.as_bytes());

        // One row of a full-width styled bar: vt100 emits `EL` for the styled
        // empty tail, which is what would erase the owner's text.
        let mut float = parser(content.height, content.width);
        float.process(b"\x1b[44m\x1b[K");

        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        }));

        let el_at = frame.find("\x1b[K").expect("the float row should erase");
        // The repair starts by addressing the first column to the right of the float
        // on that same row…
        let repair_cup = format!("\x1b[{};{}H", content.y + 1, rect.right() + 1);
        assert!(frame.contains(&repair_cup), "frame: {frame:?}");
        let cup_at = frame.find(&repair_cup).unwrap();
        assert!(cup_at > el_at, "frame: {frame:?}");
        // …and the owner's text follows it (rfind: the first TAIL is the owner's own
        // full repaint, which happens before the float is drawn).
        let tail_at = frame.rfind("TAIL").expect("the owner tail should be repainted");
        assert!(tail_at > cup_at, "frame: {frame:?}");
    }

    #[test]
    fn an_unchanged_float_costs_nothing() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"abc");

        let panes = |owner: &vt100::Parser, float: &vt100::Parser| Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), Rect::new(2, 2, 12, 5))),
            float_focused: true,
            images: None,
            links: FrameLinks::default(),
        };

        let mut renderer = Renderer::new();
        let first = renderer.render(panes(&owner, &float)).to_vec();
        assert!(text(&first).contains('┌'));

        let second = renderer.render(panes(&owner, &float));
        assert!(second.is_empty(), "second frame: {:?}", text(second));
    }

    #[test]
    fn a_changed_float_row_is_a_diff_and_leaves_the_border_alone() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"abc");

        let panes = |owner: &vt100::Parser, float: &vt100::Parser| Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), Rect::new(2, 2, 12, 5))),
            float_focused: true,
            images: None,
            links: FrameLinks::default(),
        };

        let mut renderer = Renderer::new();
        renderer.render(panes(&owner, &float));

        float.process(b"\r\ntwo");
        let frame = text(renderer.render(panes(&owner, &float)));

        assert!(frame.contains("two"), "frame: {frame:?}");
        // Row 0 of the float did not change, so it is not repainted…
        assert!(!frame.contains("abc"), "frame: {frame:?}");
        // …and neither is the border: a diff row is not blanked first.
        assert!(!frame.contains('┌'), "frame: {frame:?}");
        assert!(!frame.contains("\x1b[10X"), "frame: {frame:?}");
    }

    #[test]
    fn invalidate_forces_the_next_frame_to_be_full() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");

        let mut renderer = Renderer::new();
        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });

        // Nothing changed, so nothing is painted…
        let quiet = renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        });
        assert!(quiet.is_empty());

        // …until the caller says the screen no longer matches what we think.
        renderer.invalidate();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("owner"), "frame: {frame:?}");
        // A full repaint blanks each row before writing it.
        assert!(frame.contains("\x1b[40X"), "frame: {frame:?}");
    }

    // ---- S3: graphics ----------------------------------------------------

    const KITTY: &[u8] = b"\x1b_Gf=24,a=T,i=7;QUJD\x1b\\";
    const SIXEL: &[u8] = b"\x1bPq\"1;1;2;2#0~~\x1b\\";

    fn store(area: Rect) -> Store {
        let mut store = Store::new();
        let size = (area.width, area.height);
        assert!(matches!(
            store.ingest(crate::app::images::Kind::Apc, KITTY, (1, 2), size),
            Ingest::Stored
        ));
        store
    }

    #[test]
    fn a_new_image_is_drawn_after_the_grid() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let store = store(content_rect(rect));

        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));

        let row_at = frame.find("hi").expect("the float row is painted");
        let image_at = frame.find("QUJD").expect("the image is emitted");
        assert!(image_at > row_at, "grid first, then graphics: {frame:?}");

        // Anchored at (row 1, col 2) inside the child's grid, translated.
        let content = content_rect(rect);
        let cup_at = frame.find(&format!("\x1b[{};{}H", content.y + 2, content.x + 3));
        assert!(cup_at.is_some_and(|cup_at| cup_at < image_at), "frame: {frame:?}");
    }

    #[test]
    fn an_unchanged_image_costs_nothing() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let store = store(content_rect(rect));
        let mut renderer = Renderer::new();

        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        });

        let second = renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        });
        assert!(second.is_empty(), "frame: {:?}", text(second));
    }

    #[test]
    fn moving_the_float_re_places_the_image_by_id() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let store = store(content_rect(rect));
        let mut renderer = Renderer::new();

        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        });

        // The float moved: same image, one line of placement, no payload.
        renderer.invalidate();
        let moved = Rect::new(20, 6, 12, 5);
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), moved)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("\x1b_Ga=p,i=7\x1b\\"), "frame: {frame:?}");
        assert!(!frame.contains("QUJD"), "the payload must not be re-sent: {frame:?}");
    }

    #[test]
    fn hiding_the_float_deletes_the_image_layer() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let store = store(content_rect(rect));
        let mut renderer = Renderer::new();

        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        });

        // `Home`: the kitty image lives in a layer our repaint cannot reach.
        renderer.invalidate();
        let hidden = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: None,
            float_focused: false,
            images: Some(&store),
            links: FrameLinks::default(),
        }));
        assert!(hidden.contains("\x1b_Ga=d,d=A\x1b\\"), "frame: {hidden:?}");

        // And showing it again has to send the payload, because we deleted it.
        renderer.invalidate();
        let shown = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));
        assert!(shown.contains("QUJD"), "frame: {shown:?}");
    }

    #[test]
    fn a_sixel_is_redrawn_when_its_rows_are_repainted() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let mut store = Store::new();
        assert!(matches!(
            store.ingest(crate::app::images::Kind::Dcs, SIXEL, (0, 0), (10, 3)),
            Ingest::Stored
        ));

        let mut renderer = Renderer::new();
        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        });

        // Text painted over sixel destroys it, so any repaint has to be followed
        // by the blit again.
        float.process(b"\r\nmore");
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));
        assert!(frame.contains("more"), "frame: {frame:?}");
        assert!(frame.contains("\x1bPq"), "frame: {frame:?}");
    }

    #[test]
    fn an_image_anchored_outside_the_grid_is_not_drawn() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        let rect = Rect::new(2, 2, 12, 5);
        let mut store = Store::new();
        // Row 99 does not exist in a 3-row child.
        assert!(matches!(
            store.ingest(crate::app::images::Kind::Apc, KITTY, (99, 0), (10, 3)),
            Ingest::Stored
        ));

        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));
        assert!(!frame.contains("QUJD"), "frame: {frame:?}");
    }

    #[test]
    fn an_oversized_inline_image_is_fitted_to_the_float() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let mut float = parser(3, 10);
        float.process(b"hi");

        // A 2000x1000 PNG, drawn by a child that never said how big to make it —
        // exactly what `wezterm imgcat uncl.png` sends.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2000u32.to_be_bytes());
        png.extend_from_slice(&1000u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let payload = {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in png.chunks(3) {
                let mut block = [0u8; 3];
                block[..chunk.len()].copy_from_slice(chunk);
                let value = (u32::from(block[0]) << 16)
                    | (u32::from(block[1]) << 8)
                    | u32::from(block[2]);
                for index in 0..4 {
                    if index <= chunk.len() {
                        out.push(char::from(ALPHABET[((value >> (18 - index * 6)) & 0x3f) as usize]));
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        };
        let sequence = format!("\x1b]1337;File=inline=1:{payload}\x07");

        let mut store = Store::new();
        assert!(matches!(
            store.ingest(
                crate::app::images::Kind::Osc,
                sequence.as_bytes(),
                (0, 0),
                (10, 3)
            ),
            Ingest::Stored
        ));

        let rect = Rect::new(2, 2, 12, 5);
        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: Some(&store),
            links: FrameLinks::default(),
        }));

        assert!(frame.contains("1337;File="), "frame: {frame:?}");
        // The header now asks for a box instead of the image's native 2000 px.
        assert!(frame.contains("preserveAspectRatio=1"), "frame: {frame:?}");
        assert!(!frame.contains("width=2000px"), "must not draw at native size: {frame:?}");
        assert!(frame.contains("px;height="), "frame: {frame:?}");
    }

    /// A parser whose first row carries a hyperlink over `text`.
    fn linked_parser(rows: u16, cols: u16, text: &[u8]) -> (vt100::Parser, Links) {
        let mut parser = parser(rows, cols);
        let mut links = Links::new();
        links.osc(parser.screen(), b"", b"https://example.com");
        parser.process(text);
        links.osc(parser.screen(), b"", b"");
        (parser, links)
    }

    #[test]
    fn a_hyperlinked_row_is_wrapped_in_osc8() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");
        let (float, float_links) = linked_parser(3, 10, b"docs");

        let rect = Rect::new(2, 2, 12, 5);
        let content = content_rect(rect);
        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks {
                owner: Links::new(),
                float: float_links,
            },
        }));

        let open = frame.find("\x1b]8;;https://example.com\x1b\\");
        let word = frame.find("docs");
        let close = frame.find("\x1b]8;;\x1b\\");
        assert!(open.is_some() && word.is_some() && close.is_some(), "frame: {frame:?}");
        let (open, word, close) = (open.unwrap(), word.unwrap(), close.unwrap());
        assert!(open < word && word < close, "the link must wrap the cells: {frame:?}");

        // And the span starts at the child's first cell, inside the float.
        let cup_at = frame.find(&format!("\x1b[{};{}H", content.y + 1, content.x + 1));
        assert!(cup_at.is_some_and(|cup_at| cup_at < open), "frame: {frame:?}");
    }

    #[test]
    fn a_hyperlink_costs_a_full_row_not_a_diff() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");

        let rect = Rect::new(2, 2, 12, 5);
        let mut renderer = Renderer::new();

        // First frame: the text is there, with no link on it.
        let mut float = parser(3, 10);
        float.process(b"docs");
        renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks::default(),
        });

        // The cells do not change; only the link is added. A diff would show
        // nothing at all, so the row has to be painted whole. The cursor is
        // walked back over the same words: that is a link with no new text.
        let mut links = Links::new();
        float.process(b"\x1b[1;1H");
        links.osc(float.screen(), b"", b"https://example.com");
        float.process(b"\x1b[1;5H");
        links.osc(float.screen(), b"", b"");

        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks {
                owner: Links::new(),
                float: links,
            },
        }));
        assert!(
            frame.contains("\x1b]8;;https://example.com\x1b\\"),
            "frame: {frame:?}"
        );
        assert!(frame.contains("docs"), "frame: {frame:?}");
    }

    #[test]
    fn a_stale_hyperlink_is_not_drawn() {
        let mut owner = parser(10, 40);
        owner.process(b"owner");

        let (mut float, float_links) = linked_parser(3, 10, b"docs");
        // The words are replaced; the span no longer points at them.
        float.process(b"\rOTHER");

        let rect = Rect::new(2, 2, 12, 5);
        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: Some((float.screen().clone(), rect)),
            float_focused: true,
            images: None,
            links: FrameLinks {
                owner: Links::new(),
                float: float_links,
            },
        }));
        assert!(!frame.contains("\x1b]8;"), "no link over the wrong words: {frame:?}");
    }

    #[test]
    fn the_owner_row_is_wrapped_too() {
        let mut owner = parser(10, 40);
        owner.process(b"before ");
        let mut links = Links::new();
        links.osc(owner.screen(), b"", b"file:///tmp");
        owner.process(b"log");
        links.osc(owner.screen(), b"", b"");

        let mut renderer = Renderer::new();
        let frame = text(renderer.render(Panes {
            owner: owner.screen().clone(),
            owner_area: Rect::new(0, 0, 40, 10),
            float: None,
            float_focused: false,
            images: None,
            links: FrameLinks {
                owner: links,
                float: Links::new(),
            },
        }));

        let open = frame.find("\x1b]8;;file:///tmp\x1b\\").expect("opened");
        let word = frame.find("log").expect("the word is painted");
        assert!(open < word, "frame: {frame:?}");
        assert!(frame[word..].contains("\x1b]8;;\x1b\\"), "frame: {frame:?}");
    }

    #[test]
    fn segments_cover_the_row_and_never_overlap() {
        let mut parser = parser(2, 20);
        let mut links = Links::new();
        links.osc(parser.screen(), b"", b"a");
        parser.process(b"abc");
        links.osc(parser.screen(), b"", b"");
        parser.process(b"\r\x1b[4C");
        links.osc(parser.screen(), b"", b"b");
        parser.process(b"de");
        links.osc(parser.screen(), b"", b"");

        let screen = parser.screen();
        let found = links.for_row(screen, 0);
        assert_eq!(found.len(), 2, "two spans: {found:?}");

        let parts = segments(&found, 10);
        // (0,3) linked, (3,1) plain, (4,2) linked, (6,4) plain.
        assert_eq!(
            parts.iter().map(|(start, len, _)| (*start, *len)).collect::<Vec<_>>(),
            vec![(0, 3), (3, 1), (4, 2), (6, 4)]
        );
        assert!(parts[0].2.is_some() && parts[1].2.is_none() && parts[2].2.is_some());
    }
}
