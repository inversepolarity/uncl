use crate::app::pty::{self, PtySession};
use crate::app::signal::Signal;
use crate::constants::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, DEFAULT_X, DEFAULT_Y, MIN_HEIGHT, MIN_WIDTH, ResizeDirection,
    TENANT_CHROME_COLS, TENANT_CHROME_ROWS, nz,
};

use anyhow::Result;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, block::Position},
};

use portable_pty::CommandBuilder;

use std::sync::{Arc, RwLock};

use vt100::Screen;

use super::widget::{Cursor, PseudoTerminal};

pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

pub struct Overlay {
    pub rect: Rect,
    pub dragging: bool,
    pub drag_offset: (u16, u16),
    pub resizing: bool,
    pub resize_direction: Option<ResizeDirection>,
    pub size: Size,
    pub is_dead: bool,
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            rect: Rect::new(DEFAULT_X, DEFAULT_Y, DEFAULT_WIDTH, DEFAULT_HEIGHT),
            dragging: false,
            drag_offset: (0, 0),
            resizing: false,
            resize_direction: None,
            size: Size {
                cols: DEFAULT_WIDTH,
                rows: DEFAULT_HEIGHT,
            },
            is_dead: true,
        }
    }

    /// The size the child is told it has, i.e. the area inside the border.
    ///
    /// S0 makes the PTY and the parser agree on this number (they used to
    /// disagree by two cells); unifying it with what the widget paints is S1.
    pub fn content_size(&self) -> (u16, u16) {
        (
            self.size.rows.saturating_sub(TENANT_CHROME_ROWS).max(1),
            self.size.cols.saturating_sub(TENANT_CHROME_COLS).max(1),
        )
    }

    /// Spawn the floating shell. No terminal state is touched here: the owner
    /// screen is already in raw mode and owns the alternate screen, so flipping
    /// raw mode on and off again (the old behaviour) only caused flicker.
    pub fn spawn(
        &mut self,
        parser: Arc<RwLock<vt100::Parser>>,
        signal: Arc<Signal>,
    ) -> Result<PtySession> {
        let (rows, cols) = self.content_size();

        // The grid must match the size the child is told, from the first byte.
        parser.write().unwrap().set_size(nz(rows), nz(cols));

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(std::env::current_dir()?);

        let session = pty::spawn(cmd, rows, cols, parser, signal)?;
        self.is_dead = false;
        Ok(session)
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        let (rows, cols) = self.content_size();
        let block = Block::default()
            .borders(Borders::ALL)
            .title_position(Position::Bottom)
            .title_alignment(ratatui::layout::Alignment::Right)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Color::Green)
            .title(format!("s:{rows}:{cols}"))
            .style(Style::default().bg(Color::Reset));

        let pseudo_term = PseudoTerminal::new(screen)
            .block(block)
            .cursor(Cursor::default().visibility(true));

        f.render_widget(pseudo_term, self.rect);
    }

    pub fn resize_to(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        bounds: (u16, u16),
    ) {
        //FIX: there are more ways to resize than handled here
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            return;
        }

        x = x.min(bounds.0.saturating_sub(1));
        y = y.min(bounds.1.saturating_sub(1));

        let max_width = bounds.0.saturating_sub(x);
        let max_height = bounds.1.saturating_sub(y);

        width = width.min(max_width).max(MIN_WIDTH);
        height = height.min(max_height).max(MIN_HEIGHT);

        if x > bounds.0.saturating_sub(width) {
            x = bounds.0.saturating_sub(width);
        }
        if y > bounds.1.saturating_sub(height) {
            y = bounds.1.saturating_sub(height);
        }

        self.rect.x = x;
        self.rect.y = y;
        self.rect.width = width;
        self.rect.height = height;
        self.size.cols = width;
        self.size.rows = height;
    }

    pub fn move_to(&mut self, target_x: u16, target_y: u16, bounds: (u16, u16)) {
        let max_x = bounds.0.saturating_sub(self.rect.width);
        let max_y = bounds.1.saturating_sub(self.rect.height);

        self.rect.x = target_x.min(max_x);
        self.rect.y = target_y.min(max_y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_size_leaves_room_for_the_border() {
        let overlay = Overlay::new();
        let (rows, cols) = overlay.content_size();
        assert_eq!(rows, DEFAULT_HEIGHT - TENANT_CHROME_ROWS);
        assert_eq!(cols, DEFAULT_WIDTH - TENANT_CHROME_COLS);
    }

    #[test]
    fn content_size_never_reaches_zero() {
        let mut overlay = Overlay::new();
        overlay.size = Size { cols: 1, rows: 1 };
        assert_eq!(overlay.content_size(), (1, 1));
    }

    #[test]
    fn resize_keeps_the_float_inside_the_screen() {
        let mut overlay = Overlay::new();
        // ask for something absurd; it must be clamped, not overflow
        overlay.resize_to(100, 100, 500, 500, (120, 40));
        assert!(overlay.rect.x + overlay.rect.width <= 120);
        assert!(overlay.rect.y + overlay.rect.height <= 40);
    }

    #[test]
    fn resize_refuses_sizes_below_the_minimum() {
        let mut overlay = Overlay::new();
        let before = overlay.rect;
        overlay.resize_to(0, 0, MIN_WIDTH - 1, MIN_HEIGHT - 1, (80, 25));
        assert_eq!(overlay.rect, before);
    }

    #[test]
    fn move_to_clamps_at_the_edges() {
        let mut overlay = Overlay::new();
        overlay.move_to(1000, 1000, (80, 25));
        assert_eq!(overlay.rect.x, 80 - overlay.rect.width);
        assert_eq!(overlay.rect.y, 25 - overlay.rect.height);
    }
}
