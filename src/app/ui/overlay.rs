use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders},
};

pub struct Overlay {
    pub rect: Rect,
    pub dragging: bool,
    pub drag_offset: (u16, u16),
    pub resizing: bool,
    pub resize_direction: Option<ResizeDirection>,
}

use crate::constants::*;

impl Overlay {
    pub fn new() -> Self {
        Self {
            rect: Rect::new(DEFAULT_X, DEFAULT_Y, DEFAULT_WIDTH, DEFAULT_HEIGHT),
            dragging: false,
            drag_offset: (0, 0),
            resizing: false,
            resize_direction: None,
        }
    }

    pub fn render(&self, f: &mut Frame) {
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White).bg(Color::DarkGray));
        f.render_widget(block, self.rect);
    }

    pub fn resize_to(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        bounds: (u16, u16),
    ) {
        // Ignore any resize attempts that fall below the minimum constraints
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            return;
        }

        // Safeguard: Ensure x and y are within bounds (can't move beyond the bounds of the screen)
        x = x.max(0).min(bounds.0.saturating_sub(1)); // Prevent x from exceeding bounds width
        y = y.max(0).min(bounds.1.saturating_sub(1)); // Prevent y from exceeding bounds height

        // Calculate the max width and height that are available for resizing
        let max_width = bounds.0.saturating_sub(x);
        let max_height = bounds.1.saturating_sub(y);

        // Safeguard against overflow by ensuring we do not resize past bounds or minimum sizes
        width = width.min(max_width).max(MIN_WIDTH);
        height = height.min(max_height).max(MIN_HEIGHT);

        // Ensure the x and y positions are within bounds based on the new size
        // This ensures the new window does not go out of bounds when resizing
        if x > bounds.0.saturating_sub(width) {
            x = bounds.0.saturating_sub(width); // Prevent x from going past bounds
        }
        if y > bounds.1.saturating_sub(height) {
            y = bounds.1.saturating_sub(height); // Prevent y from going past bounds
        }

        // Set the new window dimensions (x, y, width, height)
        self.rect.x = x;
        self.rect.y = y;
        self.rect.width = width;
        self.rect.height = height;
    }

    pub fn move_to(&mut self, target_x: u16, target_y: u16, bounds: (u16, u16)) {
        let max_x = bounds.0.saturating_sub(self.rect.width);
        let max_y = bounds.1.saturating_sub(self.rect.height);

        self.rect.x = target_x.min(max_x);
        self.rect.y = target_y.min(max_y);
    }
}
