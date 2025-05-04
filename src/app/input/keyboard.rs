use crate::app::ui::overlay::Overlay;
use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::Sender;

pub async fn handle_keyboard_input(
    overlay: &mut Overlay,
    sender: &Sender<Bytes>,
    key_event: KeyEvent,
    term_size: (u16, u16),
) -> bool {
    let x = overlay.rect.x;
    let y = overlay.rect.y;
    let width = overlay.rect.width;
    let height = overlay.rect.height;

    match key_event.code {
        KeyCode::Char('q') => return true,

        KeyCode::Char(input) => sender
            .send(Bytes::from(input.to_string().into_bytes()))
            .await
            .unwrap(),

        KeyCode::Backspace => {
            sender.send(Bytes::from(vec![8])).await.unwrap();
        }

        KeyCode::Enter => sender.send(Bytes::from(vec![b'\n'])).await.unwrap(),

        KeyCode::Left => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width.saturating_sub(1), height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x.saturating_sub(1), y, term_size);
            }

            //move cursor left
            sender.send(Bytes::from(vec![27, 91, 68])).await.unwrap()
        }

        KeyCode::Right => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width + 1, height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x + 1, y, term_size);
            }
            sender.send(Bytes::from(vec![27, 91, 67])).await.unwrap()
        }

        KeyCode::Up => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width, height.saturating_sub(1), term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x, y.saturating_sub(1), term_size);
            }
            sender.send(Bytes::from(vec![27, 91, 65])).await.unwrap()
        }

        KeyCode::Down => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                overlay.resize_to(x, y, width, height + 1, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.move_to(x, y + 1, term_size);
            }
            sender.send(Bytes::from(vec![27, 91, 66])).await.unwrap()
        }

        _ => {}
    }

    false
}
