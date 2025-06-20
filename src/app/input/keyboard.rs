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

    if key_event.code == KeyCode::Home {
        lease.tenant_visible = !lease.tenant_visible;
        return false;
    }

    if key_event.modifiers.contains(KeyModifiers::SHIFT) {
        match key_event.code {
            KeyCode::Left => {
                if lease.tenant_visible {
                    lease
                        .tenant
                        .resize_to(x, y, width.saturating_sub(1), height, term_size);
                    lease.resize_screen(height, width.saturating_sub(1)).await;
                    return false;
                } else {
                    // Send Shift+Left escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;2D".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Right => {
                if lease.tenant_visible {
                    lease.tenant.resize_to(x, y, width + 1, height, term_size);
                    lease.resize_screen(height, width + 1).await;
                    return false;
                } else {
                    // Send Shift+Right escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;2C".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Up => {
                if lease.tenant_visible {
                    lease
                        .tenant
                        .resize_to(x, y, width, height.saturating_sub(1), term_size);
                    lease.resize_screen(height.saturating_sub(1), width).await;
                    return false;
                } else {
                    // Send Shift+Up escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;2A".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Down => {
                if lease.tenant_visible {
                    lease.tenant.resize_to(x, y, width, height + 1, term_size);
                    lease.resize_screen(height + 1, width).await;
                    return false;
                } else {
                    // Send Shift+Down escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;2B".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            _ => {} // Pass other keys through
        }
    } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
        match key_event.code {
            KeyCode::Left => {
                if lease.tenant_visible {
                    lease.tenant.move_to(x.saturating_sub(1), y, term_size);
                    return false;
                } else {
                    // Send Ctrl+Left escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;5D".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Right => {
                if lease.tenant_visible {
                    lease.tenant.move_to(x + 1, y, term_size);
                    return false;
                } else {
                    // Send Ctrl+Right escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;5C".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Up => {
                if lease.tenant_visible {
                    lease.tenant.move_to(x, y.saturating_sub(1), term_size);
                    return false;
                } else {
                    // Send Ctrl+Up escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;5A".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            KeyCode::Down => {
                if lease.tenant_visible {
                    lease.tenant.move_to(x, y + 1, term_size);
                    return false;
                } else {
                    // Send Ctrl+Down escape sequence
                    sender
                        .send(Bytes::from(b"\x1b[1;5B".to_vec()))
                        .await
                        .unwrap();
                    return false;
                }
            }
            _ => {} // Pass other control keys through to the application
        }
    }

    // Handle regular characters
    match key_event.code {
        KeyCode::Char(c) => {
            if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                // Handle control characters (ASCII 1-26)
                let ctrl_char = (c as u8) & 0x1F;
                //TODO: try sync send
                sender.send(Bytes::from(vec![ctrl_char])).await.unwrap();
            } else if key_event.modifiers.contains(KeyModifiers::ALT) {
                sender.send(Bytes::from(vec![27, c as u8])).await.unwrap();
            } else {
                // Regular character
                sender
                    .send(Bytes::from(c.to_string().into_bytes()))
                    .await
                    .unwrap();
            }
        }

        KeyCode::Enter => {
            sender.send(Bytes::from(vec![b'\r'])).await.unwrap();
        }

        KeyCode::Backspace => {
            sender.send(Bytes::from(vec![8])).await.unwrap();
        }

        KeyCode::Delete => {
            // Send the standard escape sequence for Delete key
            sender
                .send(Bytes::from(vec![27, 91, 51, 126]))
                .await
                .unwrap();
        }

        KeyCode::Tab => {
            sender.send(Bytes::from(vec![9])).await.unwrap();
        }

        KeyCode::BackTab => {
            sender.send(Bytes::from(vec![27, 91, 90])).await.unwrap();
        }

        KeyCode::Left => {
            sender.send(Bytes::from(vec![27, 91, 68])).await.unwrap();
        }

        KeyCode::Right => {
            sender.send(Bytes::from(vec![27, 91, 67])).await.unwrap();
        }

        KeyCode::Up => {
            sender.send(Bytes::from(vec![27, 91, 65])).await.unwrap();
        }

        KeyCode::Down => {
            sender.send(Bytes::from(vec![27, 91, 66])).await.unwrap();
        }

        KeyCode::Esc => {
            sender.send(Bytes::from(vec![27])).await.unwrap();
        }

        KeyCode::End => {
            sender.send(Bytes::from(vec![27, 91, 70])).await.unwrap();
        }

        KeyCode::PageUp => {
            sender
                .send(Bytes::from(vec![27, 91, 53, 126]))
                .await
                .unwrap();
        }

        KeyCode::PageDown => {
            sender
                .send(Bytes::from(vec![27, 91, 54, 126]))
                .await
                .unwrap();
        }

        KeyCode::F(n) => {
            // Function keys
            match n {
                1 => sender.send(Bytes::from(vec![27, 79, 80])).await.unwrap(),
                2 => sender.send(Bytes::from(vec![27, 79, 81])).await.unwrap(),
                3 => sender.send(Bytes::from(vec![27, 79, 82])).await.unwrap(),
                4 => sender.send(Bytes::from(vec![27, 79, 83])).await.unwrap(),
                5 => sender
                    .send(Bytes::from(vec![27, 91, 49, 53, 126]))
                    .await
                    .unwrap(),
                6 => sender
                    .send(Bytes::from(vec![27, 91, 49, 55, 126]))
                    .await
                    .unwrap(),
                7 => sender
                    .send(Bytes::from(vec![27, 91, 49, 56, 126]))
                    .await
                    .unwrap(),
                8 => sender
                    .send(Bytes::from(vec![27, 91, 49, 57, 126]))
                    .await
                    .unwrap(),
                9 => sender
                    .send(Bytes::from(vec![27, 91, 50, 48, 126]))
                    .await
                    .unwrap(),
                10 => sender
                    .send(Bytes::from(vec![27, 91, 50, 49, 126]))
                    .await
                    .unwrap(),
                11 => sender
                    .send(Bytes::from(vec![27, 91, 50, 51, 126]))
                    .await
                    .unwrap(),
                12 => sender
                    .send(Bytes::from(vec![27, 91, 50, 52, 126]))
                    .await
                    .unwrap(),
                _ => {}
            }
        }

        _ => return false,
    }

    false
}
