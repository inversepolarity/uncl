//! Turning host input events into the byte sequences a *child* expects.
//!
//! A terminal multiplexer cannot forward what it receives: the host terminal
//! reports in its own mode, the child expects its own. Everything here is a pure
//! function of (event, child modes) so the whole table is unit-testable, which
//! matters because these are exactly the bytes nobody can eyeball.
//!
//! S1 covers keys and mouse. Kitty keyboard protocol is deliberately not
//! advertised (see `term::caps`), so a child never asks us for it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use crate::geometry::Rect;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// The input-relevant modes of one child, read from its parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChildModes {
    /// DECCKM: arrows and Home/End use SS3 instead of CSI.
    pub application_cursor: bool,
    /// DECAWM-ish: bracketed paste wrapping of pasted text.
    pub bracketed_paste: bool,
    pub mouse_mode: MouseProtocolMode,
    pub mouse_encoding: MouseProtocolEncoding,
}

impl ChildModes {
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        Self {
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            mouse_mode: screen.mouse_protocol_mode(),
            mouse_encoding: screen.mouse_protocol_encoding(),
        }
    }

    /// Did the child ask for mouse reporting at all?
    pub fn wants_mouse(&self) -> bool {
        !matches!(self.mouse_mode, MouseProtocolMode::None)
    }
}

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_code(modifiers: KeyModifiers) -> u16 {
    let mut code = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 4;
    }
    code
}

/// Encode one key press for a child in `modes`.
///
/// Returns `None` when the key has no terminal representation (media keys,
/// keypad keys without application-keypad support, …).
pub fn encode_key(key: KeyEvent, modes: ChildModes) -> Option<Vec<u8>> {
    let mods = key.modifiers;

    match key.code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                // Ctrl+<char> is the ASCII control range. Ctrl+Shift+<char> must
                // not add anything: the terminal has no separate encoding.
                let control = (c.to_ascii_lowercase() as u32 & 0x1f) as u8;
                let mut out = Vec::with_capacity(2);
                if mods.contains(KeyModifiers::ALT) {
                    out.push(0x1b);
                }
                out.push(control);
                Some(out)
            } else {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf).as_bytes();
                let mut out = Vec::with_capacity(encoded.len() + 1);
                if mods.contains(KeyModifiers::ALT) {
                    out.push(0x1b);
                }
                out.extend_from_slice(encoded);
                Some(out)
            }
        }

        KeyCode::Enter => Some(vec![b'\r']),
        // DEL, which is what terminals send for backspace unless the child asked
        // for DECBKM; shells and editors both understand it.
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),

        // Letter keys: SS3 only when the child is in application-cursor mode and
        // no modifier is held; xterm uses the CSI form as soon as one is.
        KeyCode::Up => cursor_key(b'A', modes.application_cursor, mods),
        KeyCode::Down => cursor_key(b'B', modes.application_cursor, mods),
        KeyCode::Right => cursor_key(b'C', modes.application_cursor, mods),
        KeyCode::Left => cursor_key(b'D', modes.application_cursor, mods),
        KeyCode::Home => cursor_key(b'H', modes.application_cursor, mods),
        KeyCode::End => cursor_key(b'F', modes.application_cursor, mods),

        // Numbered keys: CSI <n> ~ , with a modifier parameter when modified.
        KeyCode::Insert => tilde(2, mods),
        KeyCode::Delete => tilde(3, mods),
        KeyCode::PageUp => tilde(5, mods),
        KeyCode::PageDown => tilde(6, mods),
        KeyCode::F(n) => function_key(n, mods),

        _ => None,
    }
}

fn cursor_key(final_byte: u8, application_cursor: bool, mods: KeyModifiers) -> Option<Vec<u8>> {
    let code = modifier_code(mods);
    if application_cursor && code == 1 {
        Some(vec![0x1b, b'O', final_byte])
    } else if code == 1 {
        Some(vec![0x1b, b'[', final_byte])
    } else {
        Some(format!("\x1b[1;{code}{}", final_byte as char).into_bytes())
    }
}

fn tilde(number: u8, mods: KeyModifiers) -> Option<Vec<u8>> {
    let code = modifier_code(mods);
    if code == 1 {
        Some(format!("\x1b[{number}~").into_bytes())
    } else {
        Some(format!("\x1b[{number};{code}~").into_bytes())
    }
}

fn function_key(n: u8, mods: KeyModifiers) -> Option<Vec<u8>> {
    let code = modifier_code(mods);
    match n {
        // F1-F4 have SS3 forms; xterm uses CSI 1;<mod> P when modified.
        1..=4 => {
            let final_byte = b'P' + (n - 1);
            if code == 1 {
                Some(vec![0x1b, b'O', final_byte])
            } else {
                Some(format!("\x1b[1;{code}{}", final_byte as char).into_bytes())
            }
        }
        5 => tilde(15, mods),
        6 => tilde(17, mods),
        7 => tilde(18, mods),
        8 => tilde(19, mods),
        9 => tilde(20, mods),
        10 => tilde(21, mods),
        11 => tilde(23, mods),
        12 => tilde(24, mods),
        _ => None,
    }
}

/// SGR mouse button numbering: the standard wheel and button codes, plus the
/// modifier bits the host reported.
fn sgr_button(kind: MouseEventKind, modifiers: KeyModifiers) -> Option<(u16, bool)> {
    let shift = 4 * u16::from(modifiers.contains(KeyModifiers::SHIFT));
    let alt = 8 * u16::from(modifiers.contains(KeyModifiers::ALT));
    let ctrl = 16 * u16::from(modifiers.contains(KeyModifiers::CONTROL));
    let extra = shift + alt + ctrl;

    let (base, release) = match kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => (0, false),
        MouseEventKind::Down(crossterm::event::MouseButton::Middle) => (1, false),
        MouseEventKind::Down(crossterm::event::MouseButton::Right) => (2, false),
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => (0, true),
        MouseEventKind::Up(crossterm::event::MouseButton::Middle) => (1, true),
        MouseEventKind::Up(crossterm::event::MouseButton::Right) => (2, true),
        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => (32, false),
        MouseEventKind::Drag(crossterm::event::MouseButton::Middle) => (33, false),
        MouseEventKind::Drag(crossterm::event::MouseButton::Right) => (34, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };

    Some((base + extra, release))
}

/// Does `modes` want this event kind at all?
fn reports(mode: MouseProtocolMode, kind: MouseEventKind) -> bool {
    use MouseEventKind as K;
    // Motion *with* a button held is `Drag`; motion without one is `Moved`.
    // Only `?1003` reports the latter — conflating them is how a `?1002` child
    // ends up being told the pointer moved when nothing was pressed.
    let is_drag = matches!(kind, K::Drag(_));
    let is_down = matches!(kind, K::Down(_));
    let is_up = matches!(kind, K::Up(_));
    let is_scroll = matches!(
        kind,
        K::ScrollUp | K::ScrollDown | K::ScrollLeft | K::ScrollRight
    );
    let is_button = is_down || is_up || is_scroll;

    match mode {
        MouseProtocolMode::None => false,
        // X10: presses only (the wheel is reported as buttons 4/5).
        MouseProtocolMode::Press => is_down || is_scroll,
        // VT200: press and release.
        MouseProtocolMode::PressRelease => is_button,
        // Button-event: presses, releases, wheel, and drags only.
        MouseProtocolMode::ButtonMotion => is_button || is_drag,
        // Any-event: the lot.
        MouseProtocolMode::AnyMotion => true,
    }
}

/// Encode a mouse event for a child, in coordinates relative to its own grid.
///
/// `col`/`row` are zero-based inside the child's area; `area` is that grid's
/// size, used only to drop events outside it.
pub fn encode_mouse(
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    col: u16,
    row: u16,
    area: Rect,
    modes: ChildModes,
) -> Option<Vec<u8>> {
    if !reports(modes.mouse_mode, kind) {
        return None;
    }
    if col >= area.width || row >= area.height {
        return None;
    }

    // Legacy X10 encoding cannot express motion without a button held (35), and
    // a child that asked for it is in a mode that predates it anyway.
    if matches!(kind, MouseEventKind::Moved)
        && matches!(
            modes.mouse_encoding,
            MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8
        )
    {
        return None;
    }

    let (button, release) = sgr_button(kind, modifiers)?;

    match modes.mouse_encoding {
        // SGR: unambiguous, so releases get their own final byte.
        MouseProtocolEncoding::Sgr => {
            let final_byte = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, final_byte).into_bytes())
        }

        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            // Legacy X10 encoding is one byte per field and saturates at 223
            // columns; a release is reported as button 3.
            let encoded_button = if release { 3 } else { button };
            let values = [
                32 + encoded_button.min(255) as u16,
                32 + col as u16 + 1,
                32 + row as u16 + 1,
            ];
            if values.iter().any(|value| *value > 255) {
                return None;
            }

            let mut out = vec![0x1b, b'[', b'M'];
            for value in values {
                if matches!(modes.mouse_encoding, MouseProtocolEncoding::Utf8) {
                    // UTF-8 mouse: the field value *is* the codepoint.
                    let ch = char::from_u32(u32::from(value))?;
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                } else {
                    out.push(value as u8);
                }
            }
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn modes() -> ChildModes {
        ChildModes::default()
    }

    fn app_cursor() -> ChildModes {
        ChildModes {
            application_cursor: true,
            ..ChildModes::default()
        }
    }

    fn sgr_mouse(mode: MouseProtocolMode) -> ChildModes {
        ChildModes {
            mouse_mode: mode,
            mouse_encoding: MouseProtocolEncoding::Sgr,
            ..ChildModes::default()
        }
    }

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    #[test]
    fn plain_character_is_its_utf8() {
        assert_eq!(encode_key(key(KeyCode::Char('a'), KeyModifiers::NONE), modes()).unwrap(), b"a");
        assert_eq!(
            encode_key(key(KeyCode::Char('é'), KeyModifiers::NONE), modes()).unwrap(),
            "é".as_bytes()
        );
    }

    #[test]
    fn alt_prefixes_escape_and_ctrl_masks() {
        assert_eq!(
            encode_key(key(KeyCode::Char('b'), KeyModifiers::ALT), modes()).unwrap(),
            vec![0x1b, b'b']
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), modes()).unwrap(),
            vec![0x03]
        );
        // Ctrl+Shift is the same byte as Ctrl: the terminal has no encoding for
        // the shift bit on a control character.
        assert_eq!(
            encode_key(
                key(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                modes()
            )
            .unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL), modes()).unwrap(),
            vec![0x01]
        );
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), modes()).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::NONE), app_cursor()).unwrap(),
            b"\x1bOA"
        );
        // A modifier always wins, in both modes (xterm behaviour).
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::SHIFT), modes()).unwrap(),
            b"\x1b[1;2A"
        );
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::SHIFT), app_cursor()).unwrap(),
            b"\x1b[1;2A"
        );
    }

    #[test]
    fn home_and_end_follow_application_cursor_mode() {
        assert_eq!(
            encode_key(key(KeyCode::Home, KeyModifiers::NONE), modes()).unwrap(),
            b"\x1b[H"
        );
        assert_eq!(
            encode_key(key(KeyCode::End, KeyModifiers::NONE), app_cursor()).unwrap(),
            b"\x1bOF"
        );
    }

    #[test]
    fn numbered_keys_and_function_keys() {
        assert_eq!(
            encode_key(key(KeyCode::Delete, KeyModifiers::NONE), modes()).unwrap(),
            b"\x1b[3~"
        );
        assert_eq!(
            encode_key(key(KeyCode::PageUp, KeyModifiers::CONTROL), modes()).unwrap(),
            b"\x1b[5;5~"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(1), KeyModifiers::NONE), modes()).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(5), KeyModifiers::NONE), modes()).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            encode_key(key(KeyCode::F(12), KeyModifiers::ALT), modes()).unwrap(),
            b"\x1b[24;3~"
        );
        assert!(encode_key(key(KeyCode::F(20), KeyModifiers::NONE), modes()).is_none());
    }

    #[test]
    fn control_keys_have_their_classic_bytes() {
        assert_eq!(encode_key(key(KeyCode::Enter, KeyModifiers::NONE), modes()).unwrap(), b"\r");
        assert_eq!(encode_key(key(KeyCode::Tab, KeyModifiers::NONE), modes()).unwrap(), b"\t");
        assert_eq!(encode_key(key(KeyCode::Backspace, KeyModifiers::NONE), modes()).unwrap(), vec![0x7f]);
        assert_eq!(encode_key(key(KeyCode::Esc, KeyModifiers::NONE), modes()).unwrap(), vec![0x1b]);
        assert_eq!(encode_key(key(KeyCode::BackTab, KeyModifiers::NONE), modes()).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn mouse_is_not_sent_when_disabled() {
        assert!(encode_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            AREA,
            sgr_mouse(MouseProtocolMode::None),
        )
        .is_none());
    }

    #[test]
    fn sgr_mouse_reports_press_release_and_wheel() {
        let modes = sgr_mouse(MouseProtocolMode::PressRelease);
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                KeyModifiers::NONE,
                4,
                2,
                AREA,
                modes
            )
            .unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(crossterm::event::MouseButton::Left),
                KeyModifiers::NONE,
                4,
                2,
                AREA,
                modes
            )
            .unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE, 0, 0, AREA, modes).unwrap(),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollDown, KeyModifiers::NONE, 0, 0, AREA, modes).unwrap(),
            b"\x1b[<65;1;1M"
        );
    }

    #[test]
    fn modifiers_are_folded_into_the_button_code() {
        let modes = sgr_mouse(MouseProtocolMode::PressRelease);
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                KeyModifiers::CONTROL,
                0,
                0,
                AREA,
                modes
            )
            .unwrap(),
            b"\x1b[<16;1;1M"
        );
    }

    #[test]
    fn x10_mode_reports_presses_only() {
        let modes = sgr_mouse(MouseProtocolMode::Press);
        assert!(encode_mouse(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            AREA,
            modes
        )
        .is_none());
        assert!(encode_mouse(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            AREA,
            modes
        )
        .is_none());
        assert!(encode_mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE, 0, 0, AREA, modes).is_some());
    }

    #[test]
    fn button_motion_mode_reports_drags() {
        assert!(encode_mouse(
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            1,
            1,
            AREA,
            sgr_mouse(MouseProtocolMode::ButtonMotion)
        )
        .is_some());
        assert!(encode_mouse(
            MouseEventKind::Moved,
            KeyModifiers::NONE,
            1,
            1,
            AREA,
            sgr_mouse(MouseProtocolMode::ButtonMotion)
        )
        .is_none());
        assert!(encode_mouse(
            MouseEventKind::Moved,
            KeyModifiers::NONE,
            1,
            1,
            AREA,
            sgr_mouse(MouseProtocolMode::AnyMotion)
        )
        .is_some());
    }

    #[test]
    fn legacy_encoding_is_three_bytes() {
        let modes = ChildModes {
            mouse_mode: MouseProtocolMode::PressRelease,
            mouse_encoding: MouseProtocolEncoding::Default,
            ..ChildModes::default()
        };
        let encoded = encode_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            4,
            2,
            AREA,
            modes,
        )
        .unwrap();
        assert_eq!(encoded, vec![0x1b, b'[', b'M', 32, 37, 35]);
    }

    #[test]
    fn legacy_release_uses_button_three() {
        let modes = ChildModes {
            mouse_mode: MouseProtocolMode::PressRelease,
            mouse_encoding: MouseProtocolEncoding::Default,
            ..ChildModes::default()
        };
        let encoded = encode_mouse(
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            AREA,
            modes,
        )
        .unwrap();
        assert_eq!(encoded[3], 32 + 3);
    }

    #[test]
    fn utf8_encoding_uses_codepoints() {
        let modes = ChildModes {
            mouse_mode: MouseProtocolMode::PressRelease,
            mouse_encoding: MouseProtocolEncoding::Utf8,
            ..ChildModes::default()
        };
        let encoded = encode_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            AREA,
            modes,
        )
        .unwrap();
        // 32+0, 32+1, 32+1 as codepoints, all single-byte, so identical bytes.
        assert_eq!(encoded, vec![0x1b, b'[', b'M', 32, 33, 33]);
    }

    #[test]
    fn events_outside_the_grid_are_dropped() {
        let modes = sgr_mouse(MouseProtocolMode::PressRelease);
        assert!(encode_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            80,
            0,
            AREA,
            modes
        )
        .is_none());
        assert!(encode_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            KeyModifiers::NONE,
            0,
            24,
            AREA,
            modes
        )
        .is_none());
    }

    #[test]
    fn modes_are_read_from_the_screen() {
        let mut parser = vt100::Parser::new(
            crate::constants::nz(4),
            crate::constants::nz(20),
            0,
        );
        parser.process(b"\x1b[?1h\x1b[?1002h\x1b[?1006h\x1b[?2004h");
        let modes = ChildModes::from_screen(parser.screen());
        assert!(modes.application_cursor);
        assert!(modes.bracketed_paste);
        assert!(modes.wants_mouse());
        assert_eq!(modes.mouse_encoding, MouseProtocolEncoding::Sgr);
    }
}
