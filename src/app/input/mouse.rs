//! Turning host mouse events into either *float geometry* or *child input*.
//!
//! This is the split that S0 got wrong: any mouse event while the float was
//! visible was swallowed by drag/resize, so a full-screen app inside the float
//! never saw a click. The rule now is:
//!
//! * `Alt` held → uncl owns the event (move the float).
//! * on the 1-cell border → uncl owns the event (resize from that edge/corner).
//! * inside the content area → the child owns it, encoded for its own protocol.
//! * outside the float → uncl owns it (click-away to hide) — unless a gesture
//!   started inside the child's grid, in which case the child keeps it until the
//!   button comes up. Losing that `Up` is what leaves a selection stuck.
//!
//! Everything here is pure geometry plus a little drag state, which keeps the
//! routing decisions testable.

use crate::app::ui::tenant::Overlay;
use crate::constants::{MIN_HEIGHT, MIN_WIDTH, ResizeDirection};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

/// What uncl decided to do with a mouse event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseRoute {
    /// The event landed inside the float's content area and must be encoded for
    /// the child, in coordinates relative to that area.
    Child { col: u16, row: u16 },
    /// uncl handled it (geometry, drag) — the child must not see it.
    Local,
    /// A click outside the float: hide it.
    HideFloat,
}

/// Decide the route for one event, without mutating anything.
pub fn route(overlay: &Overlay, m: MouseEvent) -> MouseRoute {
    let content = overlay.content_rect();
    let inside = content.contains(Position::new(m.column, m.row));
    let on_float = overlay.rect.contains(Position::new(m.column, m.row));
    let alt = m.modifiers.contains(KeyModifiers::ALT);

    // An in-progress gesture inside the child's grid outranks everything: the
    // child saw the press, so it must see the drag and the release even when the
    // pointer is over the border, over the owner pane, or off-screen (where the
    // coordinates clamp to the nearest cell, as a real terminal reports them).
    if overlay.child_grab
        && matches!(
            m.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::Moved
        )
    {
        let (col, row) = local_point(content, m.column, m.row);
        return MouseRoute::Child { col, row };
    }

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if alt && on_float {
                MouseRoute::Local
            } else if on_float && !inside {
                MouseRoute::Local
            } else if inside {
                let (col, row) = local_point(content, m.column, m.row);
                MouseRoute::Child { col, row }
            } else {
                MouseRoute::HideFloat
            }
        }

        // While a local drag is in progress it keeps owning the events, even if
        // the pointer leaves the float.
        MouseEventKind::Drag(_) | MouseEventKind::Up(_) if overlay.dragging || overlay.resizing => {
            MouseRoute::Local
        }

        MouseEventKind::Down(_)
        | MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {
            if inside && !alt {
                let (col, row) = local_point(content, m.column, m.row);
                MouseRoute::Child { col, row }
            } else {
                MouseRoute::Local
            }
        }

        MouseEventKind::Drag(_) | MouseEventKind::Up(_) | MouseEventKind::Moved => {
            if inside && !alt {
                let (col, row) = local_point(content, m.column, m.row);
                MouseRoute::Child { col, row }
            } else {
                MouseRoute::Local
            }
        }
    }
}

/// Convert a screen position into a zero-based cell inside `content`, clamping
/// the pointer to the grid so off-grid drags still land on the nearest cell.
fn local_point(content: Rect, column: u16, row: u16) -> (u16, u16) {
    let max_col = content.right().saturating_sub(1).max(content.x);
    let max_row = content.bottom().saturating_sub(1).max(content.y);
    (
        column.clamp(content.x, max_col) - content.x,
        row.clamp(content.y, max_row) - content.y,
    )
}

/// Start a drag, a resize, or nothing, based on where the press landed.
pub fn begin(overlay: &mut Overlay, m: MouseEvent) {
    if m.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }

    // The child owns the pointer until it releases; taking it away here would
    // strand the child's drag state.
    if overlay.child_grab {
        return;
    }

    let content = overlay.content_rect();
    let inside = content.contains(Position::new(m.column, m.row));
    let alt = m.modifiers.contains(KeyModifiers::ALT);

    if alt && overlay.rect.contains(Position::new(m.column, m.row)) {
        overlay.dragging = true;
        overlay.drag_offset = (
            m.column.saturating_sub(overlay.rect.x),
            m.row.saturating_sub(overlay.rect.y),
        );
        return;
    }

    if inside {
        return;
    }

    if let Some(direction) = direction_for(overlay.rect, m.column, m.row) {
        overlay.resizing = true;
        overlay.resize_direction = Some(direction);
    }
}

/// Which edge or corner of `rect` is the point `(x, y)` on?
///
/// Returns `None` when the point is not on the border at all (i.e. it is in the
/// content area or outside the float entirely).
pub fn direction_for(rect: Rect, x: u16, y: u16) -> Option<ResizeDirection> {
    if !rect.contains(Position::new(x, y)) {
        return None;
    }

    let left = x == rect.x;
    let right = x == rect.right().saturating_sub(1);
    let top = y == rect.y;
    let bottom = y == rect.bottom().saturating_sub(1);

    // A corner wins over an edge.
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeDirection::TopLeft),
        (_, true, true, _) => Some(ResizeDirection::TopRight),
        (true, _, _, true) => Some(ResizeDirection::BottomLeft),
        (_, true, _, true) => Some(ResizeDirection::BottomRight),
        // Edges: pick the corner nearest the pointer's other axis, so dragging
        // the left border feels like resizing from that side whichever way you
        // pull.
        (true, _, _, _) | (_, true, _, _) => {
            if y <= rect.y.saturating_add(rect.height / 2) {
                Some(if left {
                    ResizeDirection::TopLeft
                } else {
                    ResizeDirection::TopRight
                })
            } else {
                Some(if left {
                    ResizeDirection::BottomLeft
                } else {
                    ResizeDirection::BottomRight
                })
            }
        }
        (_, _, true, _) | (_, _, _, true) => {
            if x <= rect.x.saturating_add(rect.width / 2) {
                Some(if top {
                    ResizeDirection::TopLeft
                } else {
                    ResizeDirection::BottomLeft
                })
            } else {
                Some(if top {
                    ResizeDirection::TopRight
                } else {
                    ResizeDirection::BottomRight
                })
            }
        }
        _ => None,
    }
}

/// Apply an in-progress drag/resize. Returns whether anything changed.
pub fn drag(overlay: &mut Overlay, m: MouseEvent, bounds: (u16, u16)) -> bool {
    let MouseEventKind::Drag(MouseButton::Left) = m.kind else {
        // Motion without a button still moves a drag if one is active (some
        // terminals report plain motion during a grab).
        if overlay.dragging && matches!(m.kind, MouseEventKind::Moved) {
            let max_x = bounds.0.saturating_sub(overlay.rect.width);
            let max_y = bounds.1.saturating_sub(overlay.rect.height);
            let new_x = m.column.saturating_sub(overlay.drag_offset.0).min(max_x);
            let new_y = m.row.saturating_sub(overlay.drag_offset.1).min(max_y);
            overlay.rect.x = new_x;
            overlay.rect.y = new_y;
            return true;
        }
        return false;
    };

    if overlay.resizing {
        let Some(direction) = overlay.resize_direction else {
            return false;
        };
        let rect = overlay.rect;
        let right = rect.right();
        let bottom = rect.bottom();

        let (new_x, new_y, new_width, new_height) = match direction {
            ResizeDirection::TopLeft => {
                let new_x = m.column.min(right.saturating_sub(MIN_WIDTH));
                let new_y = m.row.min(bottom.saturating_sub(MIN_HEIGHT));
                (
                    new_x,
                    new_y,
                    right.saturating_sub(new_x).max(MIN_WIDTH),
                    bottom.saturating_sub(new_y).max(MIN_HEIGHT),
                )
            }
            ResizeDirection::TopRight => {
                let new_y = m.row.min(bottom.saturating_sub(MIN_HEIGHT));
                (
                    rect.x,
                    new_y,
                    m.column.saturating_sub(rect.x).max(MIN_WIDTH),
                    bottom.saturating_sub(new_y).max(MIN_HEIGHT),
                )
            }
            ResizeDirection::BottomLeft => {
                let new_x = m.column.min(right.saturating_sub(MIN_WIDTH));
                (
                    new_x,
                    rect.y,
                    right.saturating_sub(new_x).max(MIN_WIDTH),
                    m.row.saturating_sub(rect.y).max(MIN_HEIGHT),
                )
            }
            ResizeDirection::BottomRight => (
                rect.x,
                rect.y,
                m.column.saturating_sub(rect.x).max(MIN_WIDTH),
                m.row.saturating_sub(rect.y).max(MIN_HEIGHT),
            ),
        };

        overlay.resize_to_corner(new_x, new_y, new_width, new_height, direction, bounds);
        return true;
    }

    if overlay.dragging {
        let max_x = bounds.0.saturating_sub(overlay.rect.width);
        let max_y = bounds.1.saturating_sub(overlay.rect.height);
        overlay.rect.x = m.column.saturating_sub(overlay.drag_offset.0).min(max_x);
        overlay.rect.y = m.row.saturating_sub(overlay.drag_offset.1).min(max_y);
        return true;
    }

    false
}

/// Finish any drag/resize. Returns whether one was in progress.
pub fn end(overlay: &mut Overlay) -> bool {
    let was_active = overlay.dragging || overlay.resizing;
    overlay.dragging = false;
    overlay.resizing = false;
    overlay.resize_direction = None;
    was_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn overlay() -> Overlay {
        Overlay::new()
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn alt_mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::ALT,
        }
    }

    #[test]
    fn clicks_inside_the_content_go_to_the_child_in_local_coordinates() {
        let overlay = overlay();
        let content = overlay.content_rect();
        let route = route(&overlay, mouse(MouseEventKind::Down(MouseButton::Left), content.x + 3, content.y + 2));
        assert_eq!(route, MouseRoute::Child { col: 3, row: 2 });
    }

    #[test]
    fn clicks_on_the_border_are_local() {
        let overlay = overlay();
        let event = mouse(MouseEventKind::Down(MouseButton::Left), overlay.rect.x, overlay.rect.y);
        assert_eq!(route(&overlay, event), MouseRoute::Local);
    }

    #[test]
    fn alt_click_inside_moves_the_float_instead_of_reaching_the_child() {
        let overlay = overlay();
        let content = overlay.content_rect();
        let route = route(&overlay, alt_mouse(MouseEventKind::Down(MouseButton::Left), content.x + 1, content.y + 1));
        assert_eq!(route, MouseRoute::Local);
    }

    #[test]
    fn clicks_outside_hide_the_float() {
        let overlay = overlay();
        let route = route(&overlay, mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        assert_eq!(route, MouseRoute::HideFloat);
    }

    #[test]
    fn scroll_inside_reaches_the_child() {
        let overlay = overlay();
        let content = overlay.content_rect();
        let route = route(&overlay, mouse(MouseEventKind::ScrollDown, content.x, content.y));
        assert_eq!(route, MouseRoute::Child { col: 0, row: 0 });
    }

    #[test]
    fn drags_inside_are_forwarded_when_no_local_drag_is_active() {
        let overlay = overlay();
        let content = overlay.content_rect();
        let route = route(
            &overlay,
            mouse(MouseEventKind::Drag(MouseButton::Left), content.x + 1, content.y + 1),
        );
        assert_eq!(route, MouseRoute::Child { col: 1, row: 1 });
    }

    #[test]
    fn an_active_local_drag_keeps_the_events_even_outside() {
        let mut overlay = overlay();
        overlay.dragging = true;
        let dragging = route(&overlay, mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0));
        assert_eq!(dragging, MouseRoute::Local);
        let released = route(&overlay, mouse(MouseEventKind::Up(MouseButton::Left), 0, 0));
        assert_eq!(released, MouseRoute::Local);
    }

    #[test]
    fn corners_are_detected_before_edges() {
        let rect = Rect::new(2, 2, 10, 6);
        assert_eq!(direction_for(rect, 2, 2), Some(ResizeDirection::TopLeft));
        assert_eq!(direction_for(rect, 11, 2), Some(ResizeDirection::TopRight));
        assert_eq!(direction_for(rect, 2, 7), Some(ResizeDirection::BottomLeft));
        assert_eq!(direction_for(rect, 11, 7), Some(ResizeDirection::BottomRight));
        // middle of the left edge, upper half
        assert_eq!(direction_for(rect, 2, 3), Some(ResizeDirection::TopLeft));
        // middle of the top edge, right half
        assert_eq!(direction_for(rect, 9, 2), Some(ResizeDirection::TopRight));
        // interior and exterior are not handles
        assert_eq!(direction_for(rect, 5, 5), None);
        assert_eq!(direction_for(rect, 0, 0), None);
    }

    #[test]
    fn begin_starts_a_move_on_alt_and_a_resize_on_the_border() {
        let mut moving = overlay();
        let content = moving.content_rect();
        let press = alt_mouse(MouseEventKind::Down(MouseButton::Left), content.x + 2, content.y + 2);
        begin(&mut moving, press);
        assert!(moving.dragging);

        let mut resized = overlay();
        let corner_x = resized.rect.right() - 1;
        let corner_y = resized.rect.bottom() - 1;
        let press = mouse(MouseEventKind::Down(MouseButton::Left), corner_x, corner_y);
        begin(&mut resized, press);
        assert!(resized.resizing);
        assert_eq!(resized.resize_direction, Some(ResizeDirection::BottomRight));

        // A plain click inside starts nothing locally.
        let mut plain = overlay();
        let content = plain.content_rect();
        let press = mouse(MouseEventKind::Down(MouseButton::Left), content.x + 2, content.y + 2);
        begin(&mut plain, press);
        assert!(!plain.dragging && !plain.resizing);
    }

    #[test]
    fn dragging_the_bottom_right_corner_grows_the_float() {
        let mut overlay = overlay();
        let before = overlay.rect;
        overlay.resizing = true;
        overlay.resize_direction = Some(ResizeDirection::BottomRight);
        drag(
            &mut overlay,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                before.right() + 4,
                before.bottom() + 2,
            ),
            (120, 60),
        );
        assert!(overlay.rect.width > before.width);
        assert!(overlay.rect.height > before.height);
        assert_eq!(overlay.rect.x, before.x);
    }

    #[test]
    fn moving_with_alt_keeps_the_size() {
        let mut overlay = overlay();
        let size = (overlay.rect.width, overlay.rect.height);
        overlay.dragging = true;
        overlay.drag_offset = (5, 3);
        drag(
            &mut overlay,
            mouse(MouseEventKind::Drag(MouseButton::Left), 40, 20),
            (200, 80),
        );
        assert_eq!((overlay.rect.width, overlay.rect.height), size);
        assert_eq!(overlay.rect.x, 35);
        assert_eq!(overlay.rect.y, 17);
    }

    #[test]
    fn a_child_gesture_keeps_the_pointer_until_release() {
        let mut overlay = overlay();
        overlay.child_grab = true;

        // Outside the float entirely: still the child's, clamped to the grid.
        let dragged = route(&overlay, mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0));
        assert_eq!(dragged, MouseRoute::Child { col: 0, row: 0 });

        let released = route(&overlay, mouse(MouseEventKind::Up(MouseButton::Left), 0, 0));
        assert_eq!(released, MouseRoute::Child { col: 0, row: 0 });
    }

    #[test]
    fn off_grid_drag_coordinates_clamp_to_the_last_cell() {
        let mut overlay = overlay();
        overlay.child_grab = true;
        let content = overlay.content_rect();

        let route = route(
            &overlay,
            mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX, u16::MAX),
        );
        assert_eq!(
            route,
            MouseRoute::Child {
                col: content.width - 1,
                row: content.height - 1
            }
        );
    }

    #[test]
    fn scroll_reaches_the_child_during_a_gesture_only_when_inside() {
        let mut overlay = overlay();
        overlay.child_grab = true;
        let content = overlay.content_rect();

        let inside = route(&overlay, mouse(MouseEventKind::ScrollUp, content.x, content.y));
        assert_eq!(inside, MouseRoute::Child { col: 0, row: 0 });

        let outside = route(&overlay, mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(outside, MouseRoute::Local);
    }

    #[test]
    fn begin_does_nothing_while_the_child_owns_the_pointer() {
        let mut overlay = overlay();
        overlay.child_grab = true;
        let content = overlay.content_rect();

        // Even Alt+drag must not steal an in-flight child gesture...
        begin(&mut overlay, alt_mouse(MouseEventKind::Down(MouseButton::Left), content.x + 1, content.y + 1));
        assert!(!overlay.dragging && !overlay.resizing);

        // ...and neither must a border press.
        let corner_x = overlay.rect.right() - 1;
        let corner_y = overlay.rect.bottom() - 1;
        begin(&mut overlay, mouse(MouseEventKind::Down(MouseButton::Left), corner_x, corner_y));
        assert!(!overlay.resizing);
    }

    #[test]
    fn end_reports_whether_something_was_active() {
        let mut overlay = overlay();
        assert!(!end(&mut overlay));
        overlay.dragging = true;
        assert!(end(&mut overlay));
        assert!(!overlay.dragging);
    }
}
