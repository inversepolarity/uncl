//! Reading the *system* clipboard, for `OSC 52 ; <sel> ; ?`.
//!
//! A child asking to paste is asking the terminal, and the terminal's clipboard is
//! the desktop's. uncl cannot ask the host terminal for it: the reply travels back
//! as *input*, uncl's input is `crossterm`'s event stream (which parses stdin into
//! key events and would drop or misparse an `OSC 52` reply), and taking over raw
//! stdin is a change to the input path rather than a callback.
//!
//! So it asks the desktop instead, through the same small programs every Wayland
//! and X11 tool already uses: `wl-paste`, `xclip`, `xsel`, `pbpaste`, or
//! PowerShell's `Get-Clipboard`. One process per paste *query*, not per keystroke,
//! and none at all if the tools are missing — in which case the request stays
//! unanswered, which is the truthful reply for a terminal with no clipboard.
//!
//! Everything here that can be pure is pure, so the decisions are testable without
//! a desktop: which selection a selector means, which programs to try, and the
//! base64 the reply needs.

use std::process::{Command, Stdio};

/// The largest clipboard this will hand a child.
///
/// The reply is base64 in a pty write: a 1 MiB selection becomes ~1.4 MiB of
/// traffic, which is already generous for a paste. Anything bigger is far more
/// likely to be a copied file than text, and a terminal that streams it into a
/// shell is not doing the user a favour.
pub const MAX_CLIPBOARD: usize = 1024 * 1024;

/// Which clipboard an `OSC 52` selector asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selection {
    /// `c` — the clipboard.
    Clipboard,
    /// `p`, `s` — the primary selection (X11/Wayland only).
    Primary,
}

/// The selection an `OSC 52` selector names, or `None` if it is one we will not
/// answer.
///
/// Only the three single-letter selectors are served. The numbered cut buffers
/// (`0`–`7`) and the multi-letter forms carry no meaning here, and a selector like
/// `cp` asks for two clipboards at once, which no paste reply can express.
pub fn selection(ty: &[u8]) -> Option<Selection> {
    match ty {
        b"c" => Some(Selection::Clipboard),
        b"p" | b"s" => Some(Selection::Primary),
        _ => None,
    }
}

/// The programs to try, in order, for one selection.
///
/// Wayland first, then X11: on a Wayland session `xclip` may exist and still fail
/// (it talks to XWayland, if anything), so the order matters rather than being
/// cosmetic. A missing program is not an error — the next candidate is tried.
#[cfg(target_os = "linux")]
fn candidates(selection: Selection) -> &'static [&'static [&'static str]] {
    const CLIPBOARD: &[&[&str]] = &[
        &["wl-paste", "--no-newline", "--type", "text"],
        &["xclip", "-selection", "clipboard", "-o"],
        &["xsel", "--clipboard", "--output"],
    ];
    const PRIMARY: &[&[&str]] = &[
        &["wl-paste", "--no-newline", "--primary", "--type", "text"],
        &["xclip", "-selection", "primary", "-o"],
        &["xsel", "--primary", "--output"],
    ];
    match selection {
        Selection::Clipboard => CLIPBOARD,
        Selection::Primary => PRIMARY,
    }
}

#[cfg(target_os = "macos")]
fn candidates(selection: Selection) -> &'static [&'static [&'static str]] {
    // macOS has one clipboard. The primary selection does not exist, so a request
    // for it is honestly unanswerable rather than silently served from the
    // clipboard.
    const CLIPBOARD: &[&[&str]] = &[&["pbpaste"]];
    const PRIMARY: &[&[&str]] = &[];
    match selection {
        Selection::Clipboard => CLIPBOARD,
        Selection::Primary => PRIMARY,
    }
}

#[cfg(windows)]
fn candidates(selection: Selection) -> &'static [&'static [&'static str]] {
    // `-Raw` keeps the text as one string instead of an array of lines. Windows
    // has no primary selection either.
    const CLIPBOARD: &[&[&str]] = &[
        &["pwsh", "-NoProfile", "-Command", "Get-Clipboard -Raw"],
        &["powershell", "-NoProfile", "-Command", "Get-Clipboard -Raw"],
    ];
    const PRIMARY: &[&[&str]] = &[];
    match selection {
        Selection::Clipboard => CLIPBOARD,
        Selection::Primary => PRIMARY,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn candidates(selection: Selection) -> &'static [&'static [&'static str]] {
    // BSDs and anything else: `pbpaste` is the common name, and if it is not
    // there the request goes unanswered.
    const CLIPBOARD: &[&[&str]] = &[&["pbpaste"]];
    const PRIMARY: &[&[&str]] = &[];
    match selection {
        Selection::Clipboard => CLIPBOARD,
        Selection::Primary => PRIMARY,
    }
}

/// The system clipboard's contents for one selector, or `None`.
///
/// Blocking: it starts a process and waits for it. Callers run it on a thread of
/// its own (`app::term::callbacks`), never on the reader that feeds the parser.
pub fn read(ty: &[u8]) -> Option<Vec<u8>> {
    let selection = selection(ty)?;

    for argv in candidates(selection) {
        let (program, args) = argv.split_first()?;
        let Ok(output) = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            // Not installed, or it failed to start: the next candidate may work.
            continue;
        };

        if !output.status.success() {
            continue;
        }
        // Empty is a clipboard with nothing in it, which is still an answer — but
        // only for the program that owned the selection. An empty `wl-paste` on a
        // Wayland session where nothing was ever copied means "nothing", and
        // falling through to `xclip` would ask a different clipboard.
        if output.stdout.is_empty() {
            return Some(Vec::new());
        }
        if output.stdout.len() > MAX_CLIPBOARD {
            return None;
        }
        return Some(output.stdout);
    }

    None
}

/// Standard base64, as `OSC 52` wants it.
///
/// Hand-rolled for the same reason the decoder in `app::images` is: it is twenty
/// lines, it cannot panic, and a clipboard is not worth a dependency.
pub fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let value = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);

        // Four sextets per three bytes; the tail is padded, never truncated.
        for index in 0..4 {
            if index <= chunk.len() {
                let shift = 18 - index * 6;
                out.push(ALPHABET[((value >> shift) & 0x3f) as usize]);
            } else {
                out.push(b'=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert!(base64(b"").is_empty());
        // The RFC 4648 examples, one per tail length.
        assert_eq!(base64(b"f"), b"Zg==".to_vec());
        assert_eq!(base64(b"fo"), b"Zm8=".to_vec());
        assert_eq!(base64(b"foo"), b"Zm9v".to_vec());
        assert_eq!(base64(b"foob"), b"Zm9vYg==".to_vec());
        assert_eq!(base64(b"fooba"), b"Zm9vYmE=".to_vec());
        assert_eq!(base64(b"foobar"), b"Zm9vYmFy".to_vec());
        // A newline is data, not padding: clipboards hold it constantly.
        assert_eq!(base64(b"hi\n"), b"aGkK".to_vec());
        // Every byte value, so the high bits are covered.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(base64(&all).len(), 344);
    }

    #[test]
    fn only_the_three_single_letter_selectors_are_served() {
        assert_eq!(selection(b"c"), Some(Selection::Clipboard));
        assert_eq!(selection(b"p"), Some(Selection::Primary));
        assert_eq!(selection(b"s"), Some(Selection::Primary));
        // Cut buffers, nonsense, empty and compound selectors: not ours to answer.
        for ty in [&b"0"[..], b"7", b"", b"cp", b"C", b"clipboard"] {
            assert_eq!(selection(ty), None, "{:?}", String::from_utf8_lossy(ty));
        }
    }

    #[test]
    fn the_clipboard_always_has_a_program_to_try() {
        // If this were empty, every paste request would be silently unanswered and
        // the feature would be dead on the platform it was built for.
        assert!(!candidates(Selection::Clipboard).is_empty());
        for argv in candidates(Selection::Clipboard) {
            assert!(!argv.is_empty(), "a candidate with no program name");
            assert!(!argv[0].is_empty());
        }
    }

    #[test]
    fn an_unknown_selector_never_starts_a_process() {
        // Cheap guard: `read` refuses before it looks at the platform tables, so a
        // child cannot make uncl spawn anything by guessing selectors.
        assert!(read(b"nonsense").is_none());
        assert!(read(b"").is_none());
    }
}
