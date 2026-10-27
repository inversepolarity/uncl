//! `DCS` queries: `DECRQSS` (what setting is this?) and `XTGETTCAP` (what can you
//! do?).
//!
//! `vt100` implements no `DCS` at all. `vte` hands `hook`/`put`/`unhook` to it,
//! nothing there reaches [`vt100::Callbacks`], and the query disappears without a
//! trace — so a program that asks "what is the current SGR?" waits for its timeout
//! and carries on with a guess. This is the same class of bug the capability proxy
//! exists to fix (`super::caps`), one sequence family further in.
//!
//! The bytes are already carved out of the child's stream by the image tap
//! ([`crate::app::images::Splitter`], which has to find `DCS` payloads for sixel
//! anyway), so the answer is produced at the same place — before the parser looks
//! at them — and sent back through the child's own pty writer.
//!
//! Two rules, inherited from `caps`:
//!
//! * **Answer only what is true.** `DECRQSS` for a setting this engine does not
//!   track (scroll margins, `DECSCL`) is answered *invalid* (`DCS 0 $ r …`), which
//!   is a legal reply and tells the caller to stop asking. Claiming a value would
//!   be worse than silence, because the caller would believe it.
//! * **Answer, don't hang.** A query we recognise never goes unanswered.

use vt100::{Color, CursorStyle, Screen};

use super::caps::Capabilities;

/// The `DECSCUSR` parameter that means `style`.
///
/// Lives here because both callers need it from the same table: the UI sets the
/// *host's* cursor to match the focused child, and `DECRQSS` ` q` has to report
/// what the child asked for. Two tables would drift.
pub fn decscusr(style: CursorStyle) -> u16 {
    match style {
        CursorStyle::Default => 0,
        CursorStyle::BlinkBlock => 1,
        CursorStyle::SteadyBlock => 2,
        CursorStyle::BlinkUnderline => 3,
        CursorStyle::SteadyUnderline => 4,
        CursorStyle::BlinkVerticalBar => 5,
        CursorStyle::SteadyVerticalBar => 6,
    }
}

/// The reply to a `DCS` sequence, or `None` if it is not a query we answer.
///
/// `seq` is the whole sequence, introducer and terminator included, exactly as
/// [`crate::app::images::Piece::OutOfBand`] hands it over.
pub fn reply(screen: &Screen, caps: &Capabilities, seq: &[u8]) -> Option<Vec<u8>> {
    let body = body(seq)?;

    // `DCS $ q <setting> ST` — DECRQSS.
    if let Some(setting) = body.strip_prefix(b"$q") {
        return Some(decrqss(screen, setting));
    }

    // `DCS + q <hex name>[; <hex name>…] ST` — XTGETTCAP.
    if let Some(names) = body.strip_prefix(b"+q") {
        // A name that is not hex decodes to nothing, and a reply with nothing in
        // it is not a reply: staying silent is the only honest answer there.
        return Some(xtgettcap(caps, names)).filter(|out| !out.is_empty());
    }

    None
}

/// Strip `ESC P` and the terminator from a complete `DCS`.
fn body(seq: &[u8]) -> Option<&[u8]> {
    let rest = seq.strip_prefix(b"\x1bP")?;
    // `ESC \` is the terminator everywhere; `0x9c` (8-bit ST) is accepted too
    // because some senders emit it even though we never produce it.
    let rest = rest.strip_suffix(b"\x1b\\").or_else(|| rest.strip_suffix(b"\x9c"))?;
    Some(rest)
}

/// `DECRQSS`: `DCS 1 $ r <setting><value> ST` when known, `DCS 0 $ r <setting> ST`
/// when not.
fn decrqss(screen: &Screen, setting: &[u8]) -> Vec<u8> {
    match setting {
        // Current SGR. `vt100` tracks the pen, so this is a real answer rather
        // than the default palette: a program that queried it because *it* changed
        // the attributes gets what it set.
        b"m" => decrqss_value(setting, sgr(screen)),
        // DECSCUSR: the cursor shape the child asked for.
        b" q" => decrqss_value(setting, decscusr(screen.cursor_style()).to_string()),
        // Scroll margins, left/right margins, conformance level, and everything
        // else: `vt100` does not expose them, and a wrong number is worse than an
        // honest refusal.
        _ => {
            let mut out = Vec::with_capacity(setting.len() + 8);
            out.extend_from_slice(b"\x1bP0$r");
            out.extend_from_slice(setting);
            out.extend_from_slice(b"\x1b\\");
            out
        }
    }
}

fn decrqss_value(setting: &[u8], value: String) -> Vec<u8> {
    let mut out = Vec::with_capacity(setting.len() + value.len() + 8);
    out.extend_from_slice(b"\x1bP1$r");
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(setting);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The current SGR state as the parameters that would set it from a reset.
///
/// Starts from `0` so the answer is absolute: a caller applies it without having
/// to remember what it did before. Underline *style* (`4:3`) is not available in
/// `vt100`'s model — plain `4` is what this engine can honestly report.
fn sgr(screen: &Screen) -> String {
    let mut params: Vec<String> = vec!["0".to_string()];
    if screen.bold() {
        params.push("1".into());
    }
    if screen.dim() {
        params.push("2".into());
    }
    if screen.italic() {
        params.push("3".into());
    }
    if screen.underline() {
        params.push("4".into());
    }
    if screen.inverse() {
        params.push("7".into());
    }
    if let Some(colour) = colour_param(screen.fgcolor(), 30, 90) {
        params.push(colour);
    }
    if let Some(colour) = colour_param(screen.bgcolor(), 40, 100) {
        params.push(colour);
    }
    params.join(";")
}

/// One colour as SGR parameters: `38;5;n` / `38;2;r;g;b`, or the 16-colour form.
fn colour_param(colour: Color, base: u8, bright: u8) -> Option<String> {
    match colour {
        // The default foreground/background is what reset already gives us.
        Color::Default => None,
        Color::Idx(index) => Some(if index < 8 {
            format!("{}", base + index)
        } else if index < 16 {
            format!("{}", bright + index - 8)
        } else {
            format!("{};5;{index}", base + 8)
        }),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

/// `XTGETTCAP`: one `DCS 1 + r <name>=<value> ST` per name we know, and
/// `DCS 0 + r <name> ST` for the rest. Names and values are hex, because a
/// `DCS` cannot carry arbitrary bytes.
fn xtgettcap(caps: &Capabilities, names: &[u8]) -> Vec<u8> {
    let child_term = caps.child_term();
    let mut out = Vec::new();

    for name in names.split(|byte| *byte == b';') {
        let Some(decoded) = hex_decode(name) else {
            continue;
        };
        let value = match decoded.as_slice() {
            // The terminal's own name, which is what `TERM` says it is.
            b"TN" => Some(child_term.to_string()),
            b"Co" | b"colors" => Some("256".to_string()),
            // Bits per channel, slash separated — the form kitty-family tools read.
            b"RGB" => caps.truecolor.then(|| "8/8/8".to_string()),
            // Underline styles and colours are `alacritty_terminal`'s, not ours
            // (docs/rendering-report.md, Appendix I, item C1).
            _ => None,
        };

        match value {
            Some(value) => {
                out.extend_from_slice(b"\x1bP1+r");
                out.extend_from_slice(name);
                out.push(b'=');
                out.extend_from_slice(hex_encode(value.as_bytes()).as_bytes());
                out.extend_from_slice(b"\x1b\\");
            }
            _ => {
                out.extend_from_slice(b"\x1bP0+r");
                out.extend_from_slice(name);
                out.extend_from_slice(b"\x1b\\");
            }
        }
    }

    out
}

fn hex_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::nz;

    fn parser() -> vt100::Parser {
        vt100::Parser::new(nz(6), nz(20), 0)
    }

    fn caps(truecolor: bool) -> Capabilities {
        Capabilities {
            term: "xterm-256color".into(),
            term_program: Some("WezTerm".into()),
            colorterm: Some("truecolor".into()),
            kitty_graphics: true,
            sixel: false,
            kitty_keyboard: false,
            truecolor,
        }
    }

    /// `DCS $ q m ST`, the way a child writes it.
    fn decrqss_seq(setting: &[u8]) -> Vec<u8> {
        let mut seq = b"\x1bP$q".to_vec();
        seq.extend_from_slice(setting);
        seq.extend_from_slice(b"\x1b\\");
        seq
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn a_reset_terminal_reports_a_reset_pen() {
        let parser = parser();
        let reply = reply(parser.screen(), &caps(true), &decrqss_seq(b"m")).unwrap();
        // Absolute, so a caller can apply it without knowing what came before.
        assert_eq!(text(reply), "\x1bP1$r0m\x1b\\");
    }

    #[test]
    fn the_pen_a_child_set_is_the_pen_reported() {
        let mut parser = parser();
        parser.process(b"\x1b[1;3;4;7;31;44m");
        let reply = reply(parser.screen(), &caps(true), &decrqss_seq(b"m")).unwrap();
        assert_eq!(text(reply), "\x1bP1$r0;1;3;4;7;31;44m\x1b\\");
    }

    #[test]
    fn an_indexed_and_an_rgb_colour_are_both_spelled() {
        // 256-colour palette entry, then truecolor, then a bright basic one.
        let mut parser = parser();
        parser.process(b"\x1b[38;5;200;48;5;3m");
        assert!(text(reply(parser.screen(), &caps(true), &decrqss_seq(b"m")).unwrap())
            .contains("38;5;200;43"));

        parser.process(b"\x1b[38;2;10;20;30m");
        assert!(text(reply(parser.screen(), &caps(true), &decrqss_seq(b"m")).unwrap())
            .contains("38;2;10;20;30"));

        parser.process(b"\x1b[0m\x1b[91m");
        assert!(text(reply(parser.screen(), &caps(true), &decrqss_seq(b"m")).unwrap())
            .contains(";91m"));
    }

    #[test]
    fn the_cursor_shape_is_reported_as_the_child_asked_for_it() {
        let mut parser = parser();
        parser.process(b"\x1b[4 q");
        assert_eq!(
            text(reply(parser.screen(), &caps(true), &decrqss_seq(b" q")).unwrap()),
            "\x1bP1$r4 q\x1b\\"
        );
    }

    #[test]
    fn a_setting_we_do_not_track_is_refused_not_guessed() {
        let parser = parser();
        for setting in [&b"r"[..], &b"s"[..], &b"\"p"[..]] {
            assert_eq!(
                text(reply(parser.screen(), &caps(true), &decrqss_seq(setting)).unwrap()),
                format!("\x1bP0$r{}\x1b\\", String::from_utf8_lossy(setting)),
                "{}",
                String::from_utf8_lossy(setting)
            );
        }
    }

    #[test]
    fn xtgettcap_answers_what_it_can_and_refuses_the_rest() {
        // `TN` = 544e, `Co` = 436f, `Smulx` = 536d756c78.
        let seq = b"\x1bP+q544e;436f;536d756c78\x1b\\";
        let reply = text(reply(parser().screen(), &caps(true), seq).unwrap());

        // Each answer is its own DCS, and the value is hex: `xterm-256color` is
        // `787465726d2d323536636f6c6f72`.
        assert!(reply.contains("\x1bP1+r544e=787465726d2d323536636f6c6f72\x1b\\"), "{reply:?}");
        // `Co` = 256 → `323536`.
        assert!(reply.contains("\x1bP1+r436f=323536\x1b\\"), "{reply:?}");
        // Underline styles are not ours to claim until the engine swap.
        assert!(reply.contains("\x1bP0+r536d756c78\x1b\\"), "{reply:?}");
    }

    #[test]
    fn truecolor_is_only_claimed_when_it_is_true() {
        let seq = b"\x1bP+q524742\x1b\\"; // `RGB`
        assert!(text(reply(parser().screen(), &caps(true), seq).unwrap())
            .contains("\x1bP1+r524742=382f382f38\x1b\\"));
        assert!(text(reply(parser().screen(), &caps(false), seq).unwrap())
            .contains("\x1bP0+r524742\x1b\\"));
    }

    #[test]
    fn something_that_is_not_a_query_is_left_alone() {
        let parser = parser();
        // Sixel, a kitty reply, an unparseable name: not ours to answer, and a
        // malformed `XTGETTCAP` produces nothing at all rather than an empty DCS.
        assert!(reply(parser.screen(), &caps(true), b"\x1bPq#0;2;0;0;0\x1b\\").is_none());
        assert!(reply(parser.screen(), &caps(true), b"\x1bP0;1|17/ab\x1b\\").is_none());
        assert!(reply(parser.screen(), &caps(true), b"\x1bP+q5z\x1b\\").is_none());
        assert!(reply(parser.screen(), &caps(true), b"\x1bP$qm").is_none());
    }

    #[test]
    fn an_8_bit_string_terminator_is_accepted_too() {
        let parser = parser();
        let reply = reply(parser.screen(), &caps(true), b"\x1bP$qm\x9c").unwrap();
        assert_eq!(text(reply), "\x1bP1$r0m\x1b\\");
    }

    #[test]
    fn the_hex_codec_round_trips() {
        assert_eq!(hex_decode(b"544e"), Some(b"TN".to_vec()));
        assert_eq!(hex_decode(b"5"), None);
        assert_eq!(hex_decode(b""), None);
        assert_eq!(hex_decode(b"5z"), None);
        assert_eq!(hex_encode(b"TN"), "544e");
    }
}
