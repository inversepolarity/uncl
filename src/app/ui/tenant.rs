use crate::app::pty::{self, ChildParser, PtySession};
use crate::app::term::callbacks::ReplySink;
use crate::app::signal::Signal;
use crate::constants::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, DEFAULT_X, DEFAULT_Y, MIN_HEIGHT, MIN_WIDTH, ResizeDirection,
    TENANT_BORDER, nz,
};

use anyhow::Result;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    /// A gesture started inside the child's grid: it owns the pointer until the
    /// button comes up, even if the pointer leaves the float.
    pub child_grab: bool,
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
            child_grab: false,
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
        parser: Arc<RwLock<ChildParser>>,
        sink: ReplySink,
        signal: Arc<Signal>,
        caps: &crate::app::term::caps::Capabilities,
    ) -> Result<PtySession> {
        let (rows, cols) = self.content_size();

        // The grid must match the size the child is told, from the first byte.
        parser.write().unwrap().set_size(nz(rows), nz(cols));

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(std::env::current_dir()?);
        caps.apply_child_env(&mut cmd);

        let session = pty::spawn(cmd, rows, cols, parser, sink, signal)?;
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

    /// Resize the float, clamped to the terminal and to [`MIN_WIDTH`] x
    /// [`MIN_HEIGHT`].
    ///
    /// The `MIN_*` guard is on the *result*, not on the request: while dragging
    /// an edge, the requested height passes through values below the minimum
    /// (dragging up past the top), and the old code bailed out completely, which
    /// made the float stop responding mid-drag.
    pub fn resize_to(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        bounds: (u16, u16),
    ) {
        let max_width = bounds.0.max(1);
        let max_height = bounds.1.max(1);

        width = width.clamp(MIN_WIDTH.min(max_width), max_width);
        height = height.clamp(MIN_HEIGHT.min(max_height), max_height);

        x = x.min(max_width.saturating_sub(width));
        y = y.min(max_height.saturating_sub(height));

        self.rect.x = x;
        self.rect.y = y;
        self.rect.width = width;
        self.rect.height = height;
        self.size.cols = width;
        self.size.rows = height;
    }

    /// Keep a resize anchored at the opposite corner: `resize_to` clamps the
    /// origin to keep the rect on screen, which is wrong when the caller is
    /// dragging the top-left corner *outwards*.
    pub fn resize_to_corner(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        direction: ResizeDirection,
        bounds: (u16, u16),
    ) {
        let max_width = bounds.0.max(1);
        let max_height = bounds.1.max(1);
        let width = width.clamp(MIN_WIDTH.min(max_width), max_width);
        let height = height.clamp(MIN_HEIGHT.min(max_height), max_height);

        let (x, y) = match direction {
            ResizeDirection::TopLeft => (x.min(bounds.0.saturating_sub(width)), y.min(bounds.1.saturating_sub(height))),
            ResizeDirection::TopRight => (x, y.min(bounds.1.saturating_sub(height))),
            ResizeDirection::BottomLeft => (x.min(bounds.0.saturating_sub(width)), y),
            ResizeDirection::BottomRight => (x, y),
        };

        self.resize_to(x, y, width, height, bounds);
    }

    /// Keep the float inside a terminal that just changed size.
    pub fn clamp_to(&mut self, bounds: (u16, u16)) {
        let width = self.rect.width.min(bounds.0).max(MIN_WIDTH.min(bounds.0));
        let height = self.rect.height.min(bounds.1).max(MIN_HEIGHT.min(bounds.1));
        let x = self.rect.x.min(bounds.0.saturating_sub(width));
        let y = self.rect.y.min(bounds.1.saturating_sub(height));

        self.rect = Rect::new(x, y, width, height);
        self.size = Size {
            cols: width,
            rows: height,
        };
    }

    /// Handle uncl's own geometry chords.
    ///
    /// `Alt`+arrows move the float, `Alt`+`Shift`+arrows resize it. Everything
    /// else belongs to the child: `Shift`/`Ctrl`+arrows used to be stolen for
    /// geometry, which broke text selection and word motion in every editor we
    /// put in the float.
    ///
    /// Returns `true` when the chord was consumed.
    pub fn geometry_key(&mut self, key: KeyEvent, bounds: (u16, u16)) -> bool {
        if !key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }

        let resize = key.modifiers.contains(KeyModifiers::SHIFT);
        let rect = self.rect;

        match (resize, key.code) {
            (false, KeyCode::Left) => self.move_to(rect.x.saturating_sub(1), rect.y, bounds),
            (false, KeyCode::Right) => self.move_to(rect.x + 1, rect.y, bounds),
            (false, KeyCode::Up) => self.move_to(rect.x, rect.y.saturating_sub(1), bounds),
            (false, KeyCode::Down) => self.move_to(rect.x, rect.y + 1, bounds),

            (true, KeyCode::Left) => self.resize_to(
                rect.x,
                rect.y,
                rect.width.saturating_sub(1),
                rect.height,
                bounds,
            ),
            (true, KeyCode::Right) => self.resize_to(rect.x, rect.y, rect.width + 1, rect.height, bounds),
            (true, KeyCode::Up) => self.resize_to(
                rect.x,
                rect.y,
                rect.width,
                rect.height.saturating_sub(1),
                bounds,
            ),
            (true, KeyCode::Down) => self.resize_to(rect.x, rect.y, rect.width, rect.height + 1, bounds),

            _ => return false,
        }

        true
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
    fn resizing_never_goes_below_the_minimum() {
        let mut overlay = Overlay::new();
        overlay.resize_to(0, 0, 1, 1, (80, 25));
        assert_eq!(overlay.rect.width, MIN_WIDTH);
        assert_eq!(overlay.rect.height, MIN_HEIGHT);
    }

    #[test]
    fn resizing_below_the_minimum_does_not_freeze_the_drag() {
        // The old code returned early for any request under MIN_HEIGHT, so
        // dragging the bottom edge upwards appeared to stop responding. The
        // width must still track the pointer.
        let mut overlay = Overlay::new();
        let before = overlay.rect;
        // Bounds larger than the float, so the clamp is not what is under test.
        overlay.resize_to(before.x, before.y, before.width + 5, 2, (200, 60));
        assert_eq!(overlay.rect.width, before.width + 5);
        assert_eq!(overlay.rect.height, MIN_HEIGHT);
    }

    #[test]
    fn corner_resize_keeps_the_anchor_corner_in_place() {
        let mut overlay = Overlay::new();
        let anchor_right = overlay.rect.right();
        let anchor_bottom = overlay.rect.bottom();
        let (x, y) = (overlay.rect.x + 4, overlay.rect.y + 2);
        let (width, height) = (overlay.rect.width - 4, overlay.rect.height - 2);
        // Bounds are generous so the clamp cannot move the anchor; that is what
        // resize_to_corner exists for.
        overlay.resize_to_corner(x, y, width, height, ResizeDirection::TopLeft, (200, 80));
        assert_eq!(overlay.rect.right(), anchor_right);
        assert_eq!(overlay.rect.bottom(), anchor_bottom);
    }

    #[test]
    fn clamp_to_pulls_the_float_back_on_screen() {
        let mut overlay = Overlay::new();
        overlay.resize_to(30, 15, 40, 20, (200, 80));
        overlay.clamp_to((50, 20));
        assert!(overlay.rect.right() <= 50);
        assert!(overlay.rect.bottom() <= 20);
        assert!(overlay.rect.width >= MIN_WIDTH.min(50));
        assert!(overlay.rect.height >= MIN_HEIGHT.min(20));
    }

    #[test]
    fn modifiers_alone_are_not_geometry_chords() {
        let mut overlay = Overlay::new();
        let before = overlay.rect;
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
        assert!(!overlay.geometry_key(key, (120, 60)));
        assert_eq!(overlay.rect, before);
    }

    #[test]
    fn alt_arrows_move_and_alt_shift_arrows_resize() {
        let mut overlay = Overlay::new();
        let before = overlay.rect;

        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        assert!(overlay.geometry_key(key, (120, 60)));
        assert_eq!(overlay.rect.x, before.x + 1);
        assert_eq!(overlay.rect.width, before.width);

        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert!(overlay.geometry_key(key, (120, 60)));
        assert_eq!(overlay.rect.height, before.height + 1);
        assert_eq!(overlay.rect.width, before.width);
    }

    #[test]
    fn move_to_clamps_at_the_edges() {
        let mut overlay = Overlay::new();
        overlay.move_to(1000, 1000, (80, 25));
        assert_eq!(overlay.rect.x, 80 - overlay.rect.width);
        assert_eq!(overlay.rect.y, 25 - overlay.rect.height);
    }
}
