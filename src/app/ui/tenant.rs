use crate::app::pty::{self, PtySession};
use crate::app::signal::Signal;
use crate::constants::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, DEFAULT_X, DEFAULT_Y, MIN_HEIGHT, MIN_WIDTH, ResizeDirection,
    TENANT_BORDER, nz,
};

use anyhow::Result;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders},
};

use portable_pty::CommandBuilder;

use std::sync::{Arc, RwLock};

use vt100::Screen;

use super::widget::PseudoTerminal;

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

    /// The area the child actually draws into: the rect minus its border.
    ///
    /// This is the single source of truth for three numbers that used to
    /// disagree by two cells: the size the pty is told, the size the parser
    /// grid is set to, and the area the widget paints into (`Block::inner`).
    pub fn content_rect(&self) -> Rect {
        let inset = TENANT_BORDER;
        Rect {
            x: self.rect.x.saturating_add(inset),
            y: self.rect.y.saturating_add(inset),
            width: self.rect.width.saturating_sub(inset * 2).max(1),
            height: self.rect.height.saturating_sub(inset * 2).max(1),
        }
    }

    /// `(rows, cols)` of [`Self::content_rect`].
    pub fn content_size(&self) -> (u16, u16) {
        let content = self.content_rect();
        (content.height, content.width)
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
        // ratatui 0.30 dropped `title_alignment`: the title carries its own
        // alignment now, and `title_bottom` replaces `title_position(Bottom)`.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Color::Green)
            .title_bottom(Line::from(format!("s:{rows}:{cols}")).right_aligned())
            .style(Style::default().bg(Color::Reset));

        f.render_widget(PseudoTerminal::new(screen).block(block), self.rect);
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
    fn content_rect_matches_the_border_the_widget_paints() {
        let overlay = Overlay::new();
        let content = overlay.content_rect();
        assert_eq!(content.x, overlay.rect.x + TENANT_BORDER);
        assert_eq!(content.y, overlay.rect.y + TENANT_BORDER);
        assert_eq!(content.width, overlay.rect.width - TENANT_BORDER * 2);
        assert_eq!(content.height, overlay.rect.height - TENANT_BORDER * 2);
    }

    #[test]
    fn content_size_is_derived_from_content_rect() {
        let overlay = Overlay::new();
        let (rows, cols) = overlay.content_size();
        assert_eq!((cols, rows), (overlay.content_rect().width, overlay.content_rect().height));
    }

    #[test]
    fn content_size_never_reaches_zero() {
        let mut overlay = Overlay::new();
        overlay.rect = Rect::new(0, 0, 1, 1);
        assert_eq!(overlay.content_size(), (1, 1));
    }

    #[test]
    fn resize_keeps_the_float_inside_the_screen() {
        let mut overlay = Overlay::new();
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
