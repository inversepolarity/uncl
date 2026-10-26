use crate::app::lease::Lease;
use crate::constants::{MIN_HEIGHT, MIN_WIDTH, ResizeDirection};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

pub fn handle_mouse(lease: &mut Lease, m: MouseEvent, bounds: (u16, u16)) {
    let overlay = &mut lease.tenant;
    let rect = overlay.rect;
    let x = m.column;
    let y = m.row;

    // Saturating arithmetic throughout: a one-cell terminal used to underflow
    // here (`rect.x + rect.width - MIN_WIDTH`), which panics in debug builds.
    let right_edge = rect.x.saturating_add(rect.width);
    let bottom_edge = rect.y.saturating_add(rect.height);

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if is_within_overlay(m, rect) {
                if lease.tenant_visible {
                    let near_left = x <= rect.x.saturating_add(1);
                    let near_right = x >= right_edge.saturating_sub(2);
                    let near_top = y <= rect.y.saturating_add(1);
                    let near_bottom = y >= bottom_edge.saturating_sub(2);

                    if near_left && near_top {
                        overlay.resizing = true;
                        overlay.resize_direction = Some(ResizeDirection::TopLeft);
                    } else if near_right && near_top {
                        overlay.resizing = true;
                        overlay.resize_direction = Some(ResizeDirection::TopRight);
                    } else if near_left && near_bottom {
                        overlay.resizing = true;
                        overlay.resize_direction = Some(ResizeDirection::BottomLeft);
                    } else if near_right && near_bottom {
                        overlay.resizing = true;
                        overlay.resize_direction = Some(ResizeDirection::BottomRight);
                    } else {
                        overlay.dragging = true;
                        overlay.drag_offset = (x.saturating_sub(rect.x), y.saturating_sub(rect.y));
                    }
                }
            } else if lease.tenant_visible {
                lease.tenant_visible = false;
            }
        }

        MouseEventKind::Drag(MouseButton::Left) => {
            if overlay.resizing {
                if let Some(direction) = &overlay.resize_direction {
                    let rect = overlay.rect;
                    let (new_x, new_y, new_width, new_height) = match direction {
                        ResizeDirection::TopLeft => {
                            let new_x = m
                                .column
                                .min(rect.x.saturating_add(rect.width).saturating_sub(MIN_WIDTH));
                            let new_y = m
                                .row
                                .min(rect.y.saturating_add(rect.height).saturating_sub(MIN_HEIGHT));
                            let new_width = (rect.x.saturating_add(rect.width).saturating_sub(new_x))
                                .max(MIN_WIDTH);
                            let new_height = (rect.y.saturating_add(rect.height).saturating_sub(new_y))
                                .max(MIN_HEIGHT);
                            (new_x, new_y, new_width, new_height)
                        }

                        ResizeDirection::TopRight => {
                            let new_y = m
                                .row
                                .min(rect.y.saturating_add(rect.height).saturating_sub(MIN_HEIGHT));
                            let new_width = m.column.saturating_sub(rect.x).max(MIN_WIDTH);
                            let new_height = (rect.y.saturating_add(rect.height).saturating_sub(new_y))
                                .max(MIN_HEIGHT);
                            (rect.x, new_y, new_width, new_height)
                        }

                        ResizeDirection::BottomLeft => {
                            let new_x = m
                                .column
                                .min(rect.x.saturating_add(rect.width).saturating_sub(MIN_WIDTH));
                            let new_width = (rect.x.saturating_add(rect.width).saturating_sub(new_x))
                                .max(MIN_WIDTH);
                            let new_height = m.row.saturating_sub(rect.y).max(MIN_HEIGHT);
                            (new_x, rect.y, new_width, new_height)
                        }

                        ResizeDirection::BottomRight => {
                            let new_width = m.column.saturating_sub(rect.x).max(MIN_WIDTH);
                            let new_height = m.row.saturating_sub(rect.y).max(MIN_HEIGHT);
                            (rect.x, rect.y, new_width, new_height)
                        }
                    };

                    if new_width >= MIN_WIDTH && new_height >= MIN_HEIGHT {
                        overlay.resize_to(new_x, new_y, new_width, new_height, bounds);
                        lease.resize_screen(new_height, new_width);
                    }
                }
            } else if overlay.dragging {
                let max_x = bounds.0.saturating_sub(overlay.rect.width);
                let max_y = bounds.1.saturating_sub(overlay.rect.height);
                let new_x = m.column.saturating_sub(overlay.drag_offset.0).min(max_x);
                let new_y = m.row.saturating_sub(overlay.drag_offset.1).min(max_y);
                overlay.rect.x = new_x;
                overlay.rect.y = new_y;
            }
        }

        MouseEventKind::Up(MouseButton::Left) => {
            overlay.dragging = false;
            overlay.resizing = false;
            overlay.resize_direction = None;
        }

        _ => {}
    }
}

pub fn is_within_overlay(m: MouseEvent, r: Rect) -> bool {
    let x = m.column;
    let y = m.row;
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
