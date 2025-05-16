use crate::app::lease::Lease;

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::Sender;

pub async fn handle_keyboard_input(
    lease: &mut Lease,
    sender: &Sender<Bytes>,
    key_event: KeyEvent,
    term_size: (u16, u16),
) -> bool {
    let x = lease.tenant.rect.x;
    let y = lease.tenant.rect.y;
    let width = lease.tenant.rect.width;
    let height = lease.tenant.rect.height;

    let target_sender = sender;

    match key_event.code {
        KeyCode::Char(input) => target_sender
            .send(Bytes::from(input.to_string().into_bytes()))
            .await
            .unwrap(),

        KeyCode::Backspace => {
            target_sender.send(Bytes::from(vec![8])).await.unwrap();
        }

        KeyCode::Home => {
            lease.tenant_visible = !lease.tenant_visible;
            return false;
        }

        KeyCode::Enter => {
            target_sender.send(Bytes::from(vec![b'\n'])).await.unwrap();
        }

        KeyCode::Left => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                lease
                    .tenant
                    .resize_to(x, y, width.saturating_sub(1), height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                lease.tenant.move_to(x.saturating_sub(1), y, term_size);
            }

            target_sender
                .send(Bytes::from(vec![27, 91, 68]))
                .await
                .unwrap()
        }

        KeyCode::Right => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                lease.tenant.resize_to(x, y, width + 1, height, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                lease.tenant.move_to(x + 1, y, term_size);
            }

            target_sender
                .send(Bytes::from(vec![27, 91, 67]))
                .await
                .unwrap()
        }

        KeyCode::Up => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                lease
                    .tenant
                    .resize_to(x, y, width, height.saturating_sub(1), term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                lease.tenant.move_to(x, y.saturating_sub(1), term_size);
            }

            target_sender
                .send(Bytes::from(vec![27, 91, 65]))
                .await
                .unwrap()
        }

        KeyCode::Down => {
            if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                lease.tenant.resize_to(x, y, width, height + 1, term_size);
            } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                lease.tenant.move_to(x, y + 1, term_size);
            }

            target_sender
                .send(Bytes::from(vec![27, 91, 66]))
                .await
                .unwrap()
        }

        _ => return false,
    }

    false
}
