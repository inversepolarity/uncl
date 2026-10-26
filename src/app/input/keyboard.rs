use crate::app::lease::Lease;

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;

/// Push bytes at the focused child.
///
/// Sends are non-blocking (unbounded channel) and never panic: a closed channel
/// means the child is gone, which the owner loop notices on its own.
#[inline]
fn send(tx: &UnboundedSender<Bytes>, bytes: Bytes) {
    let _ = tx.send(bytes);
}

#[inline]
fn send_static(tx: &UnboundedSender<Bytes>, bytes: &'static [u8]) {
    send(tx, Bytes::from_static(bytes));
}

#[inline]
fn send_bytes(tx: &UnboundedSender<Bytes>, bytes: Vec<u8>) {
    send(tx, Bytes::from(bytes));
}

/// Translate one key event for the focused child.
///
/// Returns `true` when uncl itself should exit. Local chords (float resize and
/// move) are handled here when the float is visible, otherwise the same key
/// combination is forwarded to the child.
pub fn handle_keyboard_input(
    lease: &mut Lease,
    sender: &UnboundedSender<Bytes>,
    key_event: KeyEvent,
    term_size: (u16, u16),
) -> bool {
    let rect = lease.tenant.rect;

    if key_event.modifiers.contains(KeyModifiers::SHIFT) {
        match key_event.code {
            KeyCode::Left => {
                if lease.tenant_visible {
                    let width = rect.width.saturating_sub(1);
                    lease.tenant.resize_to(rect.x, rect.y, width, rect.height, term_size);
                    lease.resize_screen(rect.height, width);
                } else {
                    send_static(sender, b"\x1b[1;2D");
                }
                return false;
            }
            KeyCode::Right => {
                if lease.tenant_visible {
                    let width = rect.width + 1;
                    lease.tenant.resize_to(rect.x, rect.y, width, rect.height, term_size);
                    lease.resize_screen(rect.height, width);
                } else {
                    send_static(sender, b"\x1b[1;2C");
                }
                return false;
            }
            KeyCode::Up => {
                if lease.tenant_visible {
                    let height = rect.height.saturating_sub(1);
                    lease.tenant.resize_to(rect.x, rect.y, rect.width, height, term_size);
                    lease.resize_screen(height, rect.width);
                } else {
                    send_static(sender, b"\x1b[1;2A");
                }
                return false;
            }
            KeyCode::Down => {
                if lease.tenant_visible {
                    let height = rect.height + 1;
                    lease.tenant.resize_to(rect.x, rect.y, rect.width, height, term_size);
                    lease.resize_screen(height, rect.width);
                } else {
                    send_static(sender, b"\x1b[1;2B");
                }
                return false;
            }
            _ => {}
        }
    } else if key_event.modifiers.contains(KeyModifiers::CONTROL) {
        match key_event.code {
            KeyCode::Left => {
                if lease.tenant_visible {
                    lease.tenant.move_to(rect.x.saturating_sub(1), rect.y, term_size);
                } else {
                    send_static(sender, b"\x1b[1;5D");
                }
                return false;
            }
            KeyCode::Right => {
                if lease.tenant_visible {
                    lease.tenant.move_to(rect.x + 1, rect.y, term_size);
                } else {
                    send_static(sender, b"\x1b[1;5C");
                }
                return false;
            }
            KeyCode::Up => {
                if lease.tenant_visible {
                    lease.tenant.move_to(rect.x, rect.y.saturating_sub(1), term_size);
                } else {
                    send_static(sender, b"\x1b[1;5A");
                }
                return false;
            }
            KeyCode::Down => {
                if lease.tenant_visible {
                    lease.tenant.move_to(rect.x, rect.y + 1, term_size);
                } else {
                    send_static(sender, b"\x1b[1;5B");
                }
                return false;
            }
            _ => {}
        }
    }

    match key_event.code {
        KeyCode::Char(c) => {
            if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                // Control characters are ASCII 1-26 for the alphabetic range.
                let ctrl_char = (c as u32 & 0x1f) as u8;
                send_bytes(sender, vec![ctrl_char]);
            } else if key_event.modifiers.contains(KeyModifiers::ALT) {
                // ALT is transmitted as an ESC prefix; the character keeps its
                // UTF-8 encoding so non-ASCII binds survive.
                let mut buf = [0u8; 4];
                let mut bytes = Vec::with_capacity(5);
                bytes.push(27);
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                send_bytes(sender, bytes);
            } else {
                let mut buf = [0u8; 4];
                send_bytes(sender, c.encode_utf8(&mut buf).as_bytes().to_vec());
            }
            false
        }

        KeyCode::Enter => {
            send_static(sender, b"\r");
            false
        }

        KeyCode::Backspace => {
            send_bytes(sender, vec![8]);
            false
        }

        KeyCode::Delete => {
            send_static(sender, b"\x1b[3~");
            false
        }

        KeyCode::Insert => {
            send_static(sender, b"\x1b[2~");
            false
        }

        KeyCode::Tab => {
            send_bytes(sender, vec![9]);
            false
        }

        KeyCode::BackTab => {
            send_static(sender, b"\x1b[Z");
            false
        }

        KeyCode::Left => {
            send_static(sender, b"\x1b[D");
            false
        }

        KeyCode::Right => {
            send_static(sender, b"\x1b[C");
            false
        }

        KeyCode::Up => {
            send_static(sender, b"\x1b[A");
            false
        }

        KeyCode::Down => {
            send_static(sender, b"\x1b[B");
            false
        }

        KeyCode::Esc => {
            send_static(sender, b"\x1b");
            false
        }

        KeyCode::End => {
            send_static(sender, b"\x1b[F");
            false
        }

        KeyCode::PageUp => {
            send_static(sender, b"\x1b[5~");
            false
        }

        KeyCode::PageDown => {
            send_static(sender, b"\x1b[6~");
            false
        }

        KeyCode::F(n) => {
            match n {
                1 => send_static(sender, b"\x1bOP"),
                2 => send_static(sender, b"\x1bOQ"),
                3 => send_static(sender, b"\x1bOR"),
                4 => send_static(sender, b"\x1bOS"),
                5 => send_static(sender, b"\x1b[15~"),
                6 => send_static(sender, b"\x1b[17~"),
                7 => send_static(sender, b"\x1b[18~"),
                8 => send_static(sender, b"\x1b[19~"),
                9 => send_static(sender, b"\x1b[20~"),
                10 => send_static(sender, b"\x1b[21~"),
                11 => send_static(sender, b"\x1b[23~"),
                12 => send_static(sender, b"\x1b[24~"),
                _ => {}
            }
            false
        }

        // Home is consumed by the owner loop (it toggles the float) and every
        // other key we do not know about is left alone.
        _ => false,
    }
}
