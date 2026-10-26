//! Minimal replacement for `tui_term::widget::PseudoTerminal`.
//!
//! `tui-term` pins `vt100` 0.16, which blocks the 0.18 upgrade that lets us read
//! terminal modes (mouse protocol, cursor visibility, …) instead of sniffing the
//! byte stream. Rather than vendor 1,200 lines of widget plumbing for the ~50 we
//! use, this draws a `vt100::Screen` into a `ratatui::Buffer` directly.
//!
//! The cursor is deliberately *not* painted here. Painting it as a styled cell
//! is what broke the cursor in S0: a `RAPID_BLINK` modifier on a blank cell is
//! indistinguishable from no cursor, and it can never blink the way the user's
//! terminal blinks. The real hardware cursor is placed by the owner loop with
//! `Frame::set_cursor_position`, so it blinks, and takes the shape, that the
//! user's terminal is configured for.

use ratatui::{buffer::Buffer, layout::Rect, widgets::Block, widgets::Clear, widgets::Widget};
use vt100::Screen;

use super::adapter::apply_cell;

pub struct PseudoTerminal<'a> {
    screen: &'a Screen,
    block: Option<Block<'a>>,
}

impl<'a> PseudoTerminal<'a> {
    pub fn new(screen: &'a Screen) -> Self {
        Self {
            screen,
            block: None,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }
}

impl Widget for PseudoTerminal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // A float dragged or resized past the screen edge hands us an area that
        // extends outside the buffer. Clip up front instead of relying on every
        // primitive below to do it.
        let area = area.intersection(buf.area);
        if area.is_empty() {
            return;
        }

        Clear.render(area, buf);

        let inner = match self.block {
            Some(block) => {
                let inner = block.inner(area);
                block.render(area, buf);
                inner
            }
            None => area,
        };

        render_screen(self.screen, inner, buf);
    }
}

fn render_screen(screen: &Screen, area: Rect, buf: &mut Buffer) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(cell.style().fg, Some(ratatui::style::Color::Indexed(1)));
        assert_eq!(cell.style().bg, Some(ratatui::style::Color::Indexed(4)));
    }

    #[test]
    fn blank_cell_does_not_disturb_its_neighbour() {
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"a b");

        let mut buf = buffer(20, 2);
        PseudoTerminal::new(parser.screen()).render(buf.area, &mut buf);

        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "b");
    }

    #[test]
    fn grid_painter_never_paints_a_cursor() {
        // Regression guard: the widget must not style cells to fake a cursor.
        let mut parser = vt100::Parser::new(nz(2), nz(20), 0);
        parser.process(b"abc");

        let mut buf = buffer(20, 2);
        PseudoTerminal::new(parser.screen()).render(buf.area, &mut buf);

        for x in 0..4 {
            let cell = buf.cell((x, 0)).unwrap();
            assert!(
                !cell
                    .style()
                    .add_modifier
                    .contains(ratatui::style::Modifier::RAPID_BLINK)
            );
            assert!(
                !cell
                    .style()
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn block_shrinks_the_drawing_area() {
        use ratatui::widgets::{Block, Borders};

        let mut parser = vt100::Parser::new(nz(4), nz(20), 0);
        parser.process(b"xy");

        let mut buf = buffer(20, 4);
        let block = Block::default().borders(Borders::ALL);
        PseudoTerminal::new(parser.screen())
            .block(block)
            .render(buf.area, &mut buf);

        // "x" lands one cell right and down, inside the border.
        assert_eq!(buf.cell((1, 1)).unwrap().symbol(), "x");
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), "y");
    }
}
