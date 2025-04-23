use crate::app::ui::overlay::Overlay;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn handle_keyboard_input(
    overlay: &mut Overlay,
    key_event: KeyEvent,
    term_size: (u16, u16),
) -> bool {
    let x = overlay.rect.x;
    let y = overlay.rect.y;
    let width = overlay.rect.width;
    let height = overlay.rect.height;

    match key_event.code {
        KeyCode::Char('q') => return true,

        KeyCode::Left => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width.saturating_sub(1), height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x.saturating_sub(1), y, term_size);
            }
        }

        KeyCode::Right => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width + 1, height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x + 1, y, term_size);
            }
        }

        KeyCode::Up => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width, height.saturating_sub(1), term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x, y.saturating_sub(1), term_size);
            }
        }

        KeyCode::Down => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width, height + 1, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x, y + 1, term_size);
            }
        }

        _ => {}
    }

    false
}
