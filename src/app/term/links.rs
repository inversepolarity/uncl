//! `OSC 8` hyperlinks: which cells a link covers, so the frame can re-emit it.
//!
//! `vt100` has no notion of a hyperlink. `vte` hands the sequence to
//! [`Callbacks::unhandled_osc`](vt100::Callbacks::unhandled_osc) and the cells
//! it applied to become perfectly ordinary cells. Since S2 re-encodes every row
//! from those cells, a link a child emits would otherwise be lost: the text
//! arrives, the click does not.
//!
//! So the callback records the **span**: the cursor cell when the link opened,
//! the cursor cell when it closed, the target, and the text those cells held.
//! The renderer wraps that span in `OSC 8 ; <params> ; <url> ST` … `OSC 8 ; ; ST`
//! when it paints the row.
//!
//! This is the cheap tier, and the compromises come with it:
//!
//! * **one row per link.** A link the child wraps across a line break is not
//!   recorded — splitting the span needs to know where the grid wrapped, which
//!   `vt100` does not tell us.
//! * **self-healing.** The covered text is stored with the span; if the cells no
//!   longer say the same thing when the row is painted, the link is stale and
//!   nothing is drawn. Hyperlinks live in the text's coordinate space, and die
//!   with it.
//! * **bounded.** [`MAX_LINKS`] spans are remembered, oldest dropped first.
//!
//! Nothing here draws anything: the renderer owns the encoder, and a stale or
//! unknown link is simply not emitted, which every terminal ignores.

use std::sync::Mutex;

use vt100::Screen;

/// How many link spans one pane remembers.
pub const MAX_LINKS: usize = 64;

/// `OSC 8 ; ; ST` — the sequence that ends a hyperlink.
pub const OSC8_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

/// One hyperlinked span of one row of the child's grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    row: u16,
    start: u16,
    end: u16,
    /// The `id=`/other parameters the child sent, verbatim (often empty).
    params: Vec<u8>,
    /// The URI, verbatim.
    target: Vec<u8>,
    /// What the covered cells said, so a reused row can be detected.
    text: Vec<u8>,
}

impl Link {
    /// The pane-local row this link sits on.
    pub fn row(&self) -> u16 {
        self.row
    }

    /// The covered cells, `[start, end)`.
    pub fn span(&self) -> (u16, u16) {
        (self.start, self.end)
    }

    /// The URI, as the child wrote it.
    pub fn target(&self) -> &[u8] {
        &self.target
    }

    /// The opening sequence, ready to write before the span's cells.
    pub fn open(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.params.len() + self.target.len() + 8);
        out.extend_from_slice(b"\x1b]8;");
        out.extend_from_slice(&self.params);
        out.push(b';');
        out.extend_from_slice(&self.target);
        out.extend_from_slice(b"\x1b\\");
        out
    }

    /// Do the cells still say what they said when the link closed?
    fn matches(&self, screen: &Screen) -> bool {
        let mut text = Vec::with_capacity(self.text.len());
        for col in self.start..self.end {
            let Some(cell) = screen.cell(self.row, col) else {
                return false;
            };
            text.extend_from_slice(cell.contents().as_bytes());
        }
        text == self.text
    }
}

/// A link the child opened and has not closed yet.
#[derive(Clone, Debug)]
struct Open {
    row: u16,
    col: u16,
    params: Vec<u8>,
    target: Vec<u8>,
}

/// The hyperlinks one pane knows about, plus the one currently open.
#[derive(Clone, Debug, Default)]
pub struct Links {
    links: Vec<Link>,
    open: Option<Open>,
}

impl Links {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one `OSC 8 ; <params> ; <target>` from a child.
    ///
    /// An empty target closes the open link; anything else opens one. The span
    /// is only known when it closes, because that is when the cursor has moved
    /// past the last covered cell.
    pub fn osc(&mut self, screen: &Screen, params: &[u8], target: &[u8]) {
        if target.is_empty() {
            self.close(screen);
            return;
        }
        let (row, col) = screen.cursor_position();
        self.open = Some(Open {
            row,
            col,
            params: params.to_vec(),
            target: target.to_vec(),
        });
    }

    fn close(&mut self, screen: &Screen) {
        let Some(open) = self.open.take() else {
            return;
        };

        let (row, col) = screen.cursor_position();
        // A link that wrapped to another row cannot be expressed as one span,
        // and an empty one covers nothing.
        if row != open.row || col <= open.col {
            return;
        }

        let width = screen.size().1.get();
        let end = col.min(width);
        if end <= open.col {
            return;
        }

        let mut text = Vec::new();
        for col in open.col..end {
            let Some(cell) = screen.cell(open.row, col) else {
                return;
            };
            text.extend_from_slice(cell.contents().as_bytes());
        }

        self.links.push(Link {
            row: open.row,
            start: open.col,
            end,
            params: open.params,
            target: open.target,
            text,
        });

        while self.links.len() > MAX_LINKS {
            self.links.remove(0);
        }
    }

    /// Everything a frame needs, without holding the pane's lock while drawing.
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// The links to draw on `row`, left to right, skipping the ones whose cells
    /// have since changed.
    pub fn for_row(&self, screen: &Screen, row: u16) -> Vec<&Link> {
        let mut links: Vec<&Link> = self
            .links
            .iter()
            .filter(|link| link.row == row && link.matches(screen))
            .collect();
        links.sort_by_key(|link| link.start);
        links.dedup_by_key(|link| link.start);
        links
    }

    /// The rows any link sits on, so a caller can repaint them even when their
    /// cells have not changed. May repeat a row; cheap either way.
    pub fn rows(&self) -> impl Iterator<Item = u16> + '_ {
        self.links.iter().map(Link::row)
    }

    /// Does this row have anything to draw? (Cheaper than [`Self::for_row`] when
    /// all the caller needs is a decision.)
    pub fn covers(&self, screen: &Screen, row: u16) -> bool {
        self.links
            .iter()
            .any(|link| link.row == row && link.start < link.end && link.matches(screen))
    }
}

/// A pane's links behind the lock the reader thread writes them through.
#[derive(Debug, Default)]
pub struct SharedLinks(Mutex<Links>);

impl SharedLinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one `OSC 8` from the child.
    pub fn osc(&self, screen: &Screen, params: &[u8], target: &[u8]) {
        if let Ok(mut links) = self.0.lock() {
            links.osc(screen, params, target);
        }
    }

    /// Copy the links for this frame.
    pub fn snapshot(&self) -> Links {
        match self.0.lock() {
            Ok(links) => links.snapshot(),
            Err(_) => Links::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::nz;

    fn parser() -> vt100::Parser {
        vt100::Parser::new(nz(6), nz(40), 0)
    }

    fn links() -> Links {
        Links::new()
    }

    #[test]
    fn a_link_records_the_cells_it_covers() {
        let mut parser = parser();
        let mut links = links();

        // Exactly the order the callbacks see: the OSC, then the text, then the
        // closing OSC.
        links.osc(&parser.screen().clone(), b"", b"https://example.com");
        parser.process(b"click me");
        links.osc(&parser.screen().clone(), b"", b"");
        let screen = parser.screen().clone();

        assert_eq!(links.links.len(), 1);
        let link = &links.links[0];
        assert_eq!(link.row(), 0);
        assert_eq!(link.span(), (0, 8));
        assert_eq!(link.target(), b"https://example.com");
        assert_eq!(
            link.open(),
            b"\x1b]8;;https://example.com\x1b\\".to_vec()
        );

        let found = links.for_row(&screen, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].span(), (0, 8));
    }

    #[test]
    fn parameters_survive_and_start_where_the_cursor_was() {
        let mut parser = parser();
        parser.process(b"ab");
        let start = parser.screen().clone();

        let mut links = links();
        links.osc(&start, b"id=42;foo=bar", b"file:///etc");
        parser.process(b"cd");
        let end = parser.screen().clone();
        links.osc(&end, b"", b"");

        let link = &links.links[0];
        assert_eq!(link.span(), (2, 4));
        assert_eq!(
            link.open(),
            b"\x1b]8;id=42;foo=bar;file:///etc\x1b\\".to_vec()
        );
    }

    #[test]
    fn a_link_that_wrapped_to_another_row_is_dropped() {
        let mut parser = parser();
        let mut links = links();

        links.osc(&parser.screen().clone(), b"", b"https://example.com");
        // Enough text to wrap in a 40-column grid.
        parser.process(&vec![b'x'; 60]);
        links.osc(&parser.screen().clone(), b"", b"");

        assert!(
            links.is_empty(),
            "one span cannot express a wrapped link"
        );
    }

    #[test]
    fn an_empty_link_covers_nothing() {
        let mut parser = parser();
        let mut links = links();
        let screen = parser.screen().clone();
        links.osc(&screen, b"", b"https://example.com");
        links.osc(&screen, b"", b"");
        assert!(links.is_empty());
    }

    #[test]
    fn a_closing_sequence_without_an_open_link_is_harmless() {
        let mut parser = parser();
        let mut links = links();
        links.osc(&parser.screen().clone(), b"", b"");
        assert!(links.is_empty());
        parser.process(b"text");
    }

    #[test]
    fn a_link_whose_cells_changed_is_not_drawn_any_more() {
        let mut parser = parser();
        let mut links = links();

        links.osc(&parser.screen().clone(), b"", b"https://a.example");
        parser.process(b"first");
        links.osc(&parser.screen().clone(), b"", b"");
        assert_eq!(links.for_row(parser.screen(), 0).len(), 1);

        // The line is repainted with different text: the span is stale, and a
        // clickable region over the wrong words would be worse than none.
        parser.process(b"\rsecond");
        assert_eq!(links.for_row(parser.screen(), 0).len(), 0);
        assert!(!links.covers(parser.screen(), 0));
    }

    #[test]
    fn the_store_is_bounded() {
        let mut parser = parser();
        let mut links = links();

        for _ in 0..(MAX_LINKS + 5) {
            links.osc(&parser.screen().clone(), b"", b"https://example.com");
            parser.process(b"x");
            links.osc(&parser.screen().clone(), b"", b"");
            parser.process(b"\r");
        }

        assert_eq!(links.links.len(), MAX_LINKS);
    }

    #[test]
    fn the_shared_store_round_trips_a_snapshot() {
        let mut parser = parser();
        let shared = SharedLinks::new();

        shared.osc(&parser.screen().clone(), b"", b"https://example.com");
        parser.process(b"ok");
        shared.osc(&parser.screen().clone(), b"", b"");

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.links.len(), 1);
        assert_eq!(snapshot.links[0].target(), b"https://example.com");
        // And an empty store costs nothing to snapshot: no allocation at all.
        assert!(SharedLinks::new().snapshot().is_empty());
    }
}
