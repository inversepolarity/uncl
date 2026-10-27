//! The one rectangle type uncl has.
//!
//! S1 borrowed `ratatui::layout::Rect` because ratatui owned the frame. Now that
//! `app::render` writes the frame itself (S2), the only geometry left is this:
//! a screen rectangle in cells, with the handful of operations the float, the
//! mouse router and the encoder actually use.

/// A rectangle in cells, origin top-left.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// One past the last column.
    pub const fn right(&self) -> u16 {
        self.x.saturating_add(self.width)
    }

    /// One past the last row.
    pub const fn bottom(&self) -> u16 {
        self.y.saturating_add(self.height)
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Shrink by `n` cells on every side, never below 1x1.
    ///
    /// This is the float's border: the child's PTY is sized to the result, the
    /// parser is sized to the result, and the renderer paints into the result.
    /// One definition, three call sites — which is the whole point.
    pub fn inset(self, n: u16) -> Self {
        Self {
            x: self.x.saturating_add(n),
            y: self.y.saturating_add(n),
            width: self.width.saturating_sub(n.saturating_mul(2)).max(1),
            height: self.height.saturating_sub(n.saturating_mul(2)).max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_and_bottom_are_exclusive() {
        let rect = Rect::new(2, 3, 10, 5);
        assert_eq!(rect.right(), 12);
        assert_eq!(rect.bottom(), 8);
    }

    #[test]
    fn contains_uses_the_half_open_interval() {
        let rect = Rect::new(2, 3, 10, 5);
        assert!(rect.contains(2, 3));
        assert!(rect.contains(11, 7));
        assert!(!rect.contains(12, 7));
        assert!(!rect.contains(11, 8));
        assert!(!rect.contains(1, 3));
    }

    #[test]
    fn inset_never_collapses_to_nothing() {
        assert_eq!(Rect::new(0, 0, 12, 8).inset(1), Rect::new(1, 1, 10, 6));
        assert_eq!(Rect::new(0, 0, 2, 2).inset(1), Rect::new(1, 1, 1, 1));
        assert_eq!(Rect::new(0, 0, 1, 1).inset(1), Rect::new(1, 1, 1, 1));
        assert_eq!(Rect::new(0, 0, 0, 0).inset(1), Rect::new(1, 1, 1, 1));
    }

    #[test]
    fn right_and_bottom_saturate() {
        let rect = Rect::new(u16::MAX - 1, u16::MAX - 1, 10, 10);
        assert_eq!(rect.right(), u16::MAX);
        assert_eq!(rect.bottom(), u16::MAX);
    }

    #[test]
    fn empty_is_about_zero_extent() {
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert!(Rect::new(0, 0, 5, 0).is_empty());
        assert!(!Rect::new(0, 0, 1, 1).is_empty());
    }
}
