//! Minimal replacement for `tui_term::widget::PseudoTerminal`.
//!
//! `tui-term` pins `vt100` 0.16, which blocks the 0.18 upgrade that lets us read
//! terminal modes (mouse protocol, cursor visibility, …) instead of sniffing the
//! byte stream. Rather than vendor 1,200 lines of widget plumbing for the ~50 we
//! use, this draws a `vt100::Screen` into a `ratatui::Buffer` directly.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Clear, Widget},
};
use vt100::Screen;

use super::adapter::apply_cell;

/// How to draw the cursor on top of the grid.
#[derive(Clone, Debug)]
pub struct Cursor {
    /// Whether this pane is allowed to show a cursor at all. The parser's own
    /// "hide cursor" mode always wins (`vt100::Screen::hide_cursor`).
    pub show: bool,
    pub style: Style,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            show: true,
            style: Style::default().add_modifier(Modifier::REVERSED),
        }
    }
}

impl Cursor {
    pub fn visibility(mut self, show: bool) -> Self {
        self.show = show;
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

pub struct PseudoTerminal<'a> {
    screen: &'a Screen,
    block: Option<Block<'a>>,
    cursor: Cursor,
}

impl<'a> PseudoTerminal<'a> {
    pub fn new(screen: &'a Screen) -> Self {
        Self {
            screen,
            block: None,
            cursor: Cursor {
                show: true,
                style: Style::new(),
            },
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    pub fn cursor(mut self, cursor: Cursor) -> Self {
        self.cursor = cursor;
        self
    }
}

impl Widget for PseudoTerminal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);

        let inner = match self.block {
            Some(block) => {
                let inner = block.inner(area);
                block.render(area, buf);
                inner
            }
            None => area,
        };

        render_screen(self.screen, inner, buf, &self.cursor);
    }
}

fn render_screen(screen: &Screen, area: Rect, buf: &mut Buffer, cursor: &Cursor) {
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(screen_cell) = screen.cell(row, col) else {
                continue;
            };
            let x = area.x.saturating_add(col);
            let y = area.y.saturating_add(row);
            if let Some(buf_cell) = buf.cell_mut((x, y)) {
                apply_cell(screen_cell, buf_cell);
            }
        }
    }

    if !cursor.show || screen.hide_cursor() {
        return;
    }

    let (row, col) = screen.cursor_position();
    if row >= area.height || col >= area.width {
        return;
    }
    let x = area.x.saturating_add(col);
    let y = area.y.saturating_add(row);
    if let Some(buf_cell) = buf.cell_mut((x, y)) {
        // Patch rather than replace, so the character under the cursor keeps
        // its colours and only gains the cursor's modifiers.
        let style = buf_cell.style().patch(cursor.style);
        buf_cell.set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};
    use crate::constants::nz;

    fn buffer(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    #[test]
    fn renders_text_at_the_right_cells() {
        let mut parser = vt100::Parser::new(nz(4), nz(20), 0);
        parser.process(b"hi\r\nyo");

        let mut buf = buffer(20, 4);
        PseudoTerminal::new(parser.screen()).render(buf.area, &mut buf);

        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "h");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "i");
        assert_eq!(buf.cell((0, 1)).unwrap().symbol(), "y");
    }

    #[test]
    fn maps_sgr_colours() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"\x1b[31;44mX");

        let mut buf = buffer(20, 2);
        PseudoTerminal::new(parser.screen()).render(buf.area, &mut buf);

        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.style().fg, Some(Color::Indexed(1)));
        assert_eq!(cell.style().bg, Some(Color::Indexed(4)));
    }

    #[test]
    fn blank_cell_is_not_painted_over_its_neighbour() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"a b");

        let mut buf = buffer(20, 2);
        PseudoTerminal::new(parser.screen()).render(buf.area, &mut buf);

        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "b");
    }

    #[test]
    fn hidden_cursor_is_not_drawn() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"\x1b[?25labc");

        let mut buf = buffer(20, 2);
        let cursor = Cursor::default().style(Style::default().add_modifier(Modifier::REVERSED));
        PseudoTerminal::new(parser.screen())
            .cursor(cursor)
            .render(buf.area, &mut buf);

        for x in 0..3 {
            let cell = buf.cell((x, 0)).unwrap();
            assert!(
                !cell.style().add_modifier.contains(Modifier::REVERSED),
                "cell {x} should not carry the cursor style"
            );
        }
    }

    #[test]
    fn shown_cursor_patches_the_cell_under_it() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        // red text, cursor left after it (column 2)
        parser.process(b"\x1b[31mab");

        let mut buf = buffer(20, 2);
        let cursor = Cursor::default().style(Style::default().add_modifier(Modifier::REVERSED));
        PseudoTerminal::new(parser.screen())
            .cursor(cursor)
            .render(buf.area, &mut buf);

        let cell = buf.cell((2, 0)).unwrap();
        assert!(cell.style().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn cursor_is_suppressed_when_the_pane_is_not_focused() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"ab");

        let mut buf = buffer(20, 2);
        PseudoTerminal::new(parser.screen())
            .cursor(Cursor::default().visibility(false))
            .render(buf.area, &mut buf);

        let cell = buf.cell((2, 0)).unwrap();
        assert!(!cell.style().add_modifier.contains(Modifier::REVERSED));
    }
}
