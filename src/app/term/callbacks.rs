//! The child's side of the wire: answering its terminal queries.
//!
//! `vt100` hands every escape sequence it does not model to
//! [`vt100::Callbacks`]. We use that to answer the queries a terminal
//! application asks before it draws anything — device attributes, cursor
//! position, window size, colour queries — and to notice modes it switches on
//! that `vt100` does not track (currently focus reporting).
//!
//! The reply bytes go back through the *same* pty writer the user's keystrokes
//! use, so they are coalesced and ordered with input.
//!
//! A few things a child asks for are not for the child at all — an `OSC 52`
//! clipboard copy has to reach the *host* terminal. Those are queued on a host
//! channel and written by the one thread that owns stdout, so a child cannot
//! interleave its bytes with a frame.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use tokio::sync::mpsc::UnboundedSender;
use vt100::{Callbacks, Screen};

use super::caps::{ANSI_PALETTE, Capabilities, DEFAULT_BG, DEFAULT_FG, rgb_spec};

/// Write half of a child's pty, installed once the pty writer exists.
///
/// The callbacks are constructed before the pty is opened (the parser needs them
/// at construction time), so the channel is filled in afterwards.
#[derive(Clone, Debug, Default)]
pub struct ReplySink(Arc<OnceLock<UnboundedSender<Bytes>>>);

impl ReplySink {
    pub fn install(&self, tx: &UnboundedSender<Bytes>) {
        let _ = self.0.set(tx.clone());
    }

    pub fn send(&self, bytes: Vec<u8>) {
        if let Some(tx) = self.0.get() {
            let _ = tx.send(Bytes::from(bytes));
        }
    }
}

/// Modes a child switched on that `vt100` does not model for us.
#[derive(Debug, Default)]
pub struct ChildState {
    focus_reporting: AtomicBool,
}

impl ChildState {
    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting.load(Ordering::Relaxed)
    }

    fn set_focus_reporting(&self, enabled: bool) {
        self.focus_reporting.store(enabled, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
pub struct ChildCallbacks {
    sink: ReplySink,
    caps: Arc<Capabilities>,
    state: Arc<ChildState>,
    /// Effects destined for the host terminal, drained by the owner loop.
    host: UnboundedSender<Vec<u8>>,
}

impl ChildCallbacks {
    pub fn new(
        caps: Arc<Capabilities>,
        host: UnboundedSender<Vec<u8>>,
    ) -> (Self, ReplySink, Arc<ChildState>) {
        let sink = ReplySink::default();
        let state = Arc::new(ChildState::default());
        let callbacks = Self {
            sink: sink.clone(),
            caps,
            state: state.clone(),
            host,
        };
        (callbacks, sink, state)
    }

    pub fn state(&self) -> &Arc<ChildState> {
        &self.state
    }
}

impl Callbacks for ChildCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Focus reporting is the one mode we have to track ourselves.
        if i1 == Some(b'?') && (c == 'h' || c == 'l') {
            for param in params {
                if *param == [1004] {
                    self.state.set_focus_reporting(c == 'h');
                }
            }
        }

        if let Some(reply) = reply_for_csi(screen, &self.caps, i1, i2, params, c) {
            self.sink.send(reply);
        }
    }

    fn unhandled_osc(&mut self, _screen: &mut Screen, params: &[&[u8]]) {
        if let Some(reply) = reply_for_osc(params) {
            self.sink.send(reply);
        }
    }

    /// `OSC 52 ; <selector> ; <base64>` — the child copying to the clipboard.
    ///
    /// This is not a query about the child's terminal, it is a request the real
    /// terminal has to fulfil, so it goes to the host. `vt100` has already
    /// validated both the selector and the payload as base64, which means we can
    /// relay it byte for byte without decoding.
    fn copy_to_clipboard(&mut self, _screen: &mut Screen, ty: &[u8], data: &[u8]) {
        let _ = self.host.send(osc52(ty, data));
    }
}

/// Wrap a clipboard payload in the host-side `OSC 52` sequence.
///
/// Terminals that do not implement `OSC 52` ignore it, which is the correct
/// failure mode: the copy silently does nothing rather than corrupting output.
pub fn osc52(ty: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ty.len() + data.len() + 8);
    out.extend_from_slice(b"\x1b]52;");
    out.extend_from_slice(ty);
    out.push(b';');
    out.extend_from_slice(data);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Build the reply to a CSI sequence `vt100` did not handle.
///
/// Pure, so the whole query/response table is unit-testable.
pub fn reply_for_csi(
    screen: &Screen,
    caps: &Capabilities,
    i1: Option<u8>,
    i2: Option<u8>,
    params: &[&[u16]],
    c: char,
) -> Option<Vec<u8>> {
    let first = params.first().and_then(|param| param.first().copied());
    let _ = i2; // no query we answer uses a second intermediate

    match (i1, c) {
        // Primary device attributes.
        (None, 'c') => Some(caps.da1()),
        // Secondary device attributes.
        (Some(b'>'), 'c') => Some(caps.da2()),

        // Device status report.
        (None, 'n') => match first {
            Some(5) => Some(b"\x1b[0n".to_vec()),
            // Cursor position report.
            Some(6) => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes())
            }
            _ => None,
        },
        // Private cursor position report.
        (Some(b'?'), 'n') if first == Some(6) => {
            let (row, col) = screen.cursor_position();
            Some(format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes())
        }

        // Window size in characters (CSI 18 t).
        (None, 't') if first == Some(18) => {
            let (rows, cols) = screen.size();
            Some(format!("\x1b[8;{};{}t", rows.get(), cols.get()).into_bytes())
        }

        // Text area size in characters (CSI 19 t) is not answerable without a
        // pixel size, and `XTSMGRAPHICS` (`CSI ? … S`) is answered by DA1
        // attribute 4 instead, so children that ask for sixel geometry fall back
        // to what we advertised.
        _ => None,
    }
}

/// Build the reply to an OSC sequence `vt100` did not handle.
pub fn reply_for_osc(params: &[&[u8]]) -> Option<Vec<u8>> {
    match params {
        // Colour queries. `ST`-terminated, which every caller accepts.
        [b"10", b"?"] => Some(osc(&format!("10;{}", rgb_spec(DEFAULT_FG)))),
        [b"11", b"?"] => Some(osc(&format!("11;{}", rgb_spec(DEFAULT_BG)))),
        [b"4", index, b"?"] => {
            let index: usize = std::str::from_utf8(index).ok()?.parse().ok()?;
            let colour = ANSI_PALETTE.get(index)?;
            Some(osc(&format!("4;{index};{}", rgb_spec(*colour))))
        }
        _ => None,
    }
}

fn osc(body: &str) -> Vec<u8> {
    format!("\x1b]{body}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::nz;

    fn caps() -> Arc<Capabilities> {
        Arc::new(Capabilities {
            term: "xterm-256color".into(),
            term_program: None,
            colorterm: None,
            kitty_graphics: false,
            sixel: false,
            kitty_keyboard: false,
            truecolor: false,
        })
    }

    fn host() -> (tokio::sync::mpsc::UnboundedSender<Vec<u8>>, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn parser() -> vt100::Parser<ChildCallbacks> {
        let (callbacks, _sink, _state) = ChildCallbacks::new(caps(), host().0);
        vt100::Parser::new_with_callbacks(nz(4), nz(20), 0, callbacks)
    }

    #[test]
    fn answers_primary_da() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[], 'c').unwrap();
        assert_eq!(reply, b"\x1b[?1;6;9;15;22c".to_vec());
    }

    #[test]
    fn answers_secondary_da() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), Some(b'>'), None, &[], 'c').unwrap();
        assert_eq!(reply, b"\x1b[>0;1;0c".to_vec());
    }

    #[test]
    fn answers_cursor_position_one_based() {
        let mut parser = parser();
        parser.process(b"\x1b[3;7H");
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[&[6u16][..]], 'n').unwrap();
        assert_eq!(reply, b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn answers_window_size_in_characters() {
        let parser = parser();
        let screen = parser.screen().clone();
        let reply = reply_for_csi(&screen, &caps(), None, None, &[&[18u16][..]], 't').unwrap();
        assert_eq!(reply, b"\x1b[8;4;20t".to_vec());
    }

    #[test]
    fn stays_silent_about_things_it_cannot_answer() {
        let parser = parser();
        let screen = parser.screen().clone();
        assert!(reply_for_csi(&screen, &caps(), Some(b'?'), None, &[&[1u16][..], &[0u16][..]], 'S').is_none());
        assert!(reply_for_csi(&screen, &caps(), Some(b'?'), None, &[&[0u16][..]], 'u').is_none());
        assert!(reply_for_csi(&screen, &caps(), None, None, &[&[14u16][..]], 't').is_none());
    }

    #[test]
    fn answers_colour_queries() {
        assert_eq!(
            reply_for_osc(&[&b"11"[..], &b"?"[..]]).unwrap(),
            b"\x1b]11;rgb:00/00/00\x1b\\".to_vec()
        );
        assert_eq!(
            reply_for_osc(&[&b"4"[..], &b"1"[..], &b"?"[..]]).unwrap(),
            b"\x1b]4;1;rgb:cd/00/00\x1b\\".to_vec()
        );
        // out of range / not a query
        assert!(reply_for_osc(&[&b"4"[..], &b"99"[..], &b"?"[..]]).is_none());
        assert!(reply_for_osc(&[&b"52"[..], &b"c"[..], &b"?"[..]]).is_none());
        assert!(reply_for_osc(&[&b"8"[..], &b";;"[..], &b"https://x"[..]]).is_none());
    }

    #[test]
    fn tracks_focus_reporting_enable_and_disable() {
        let (mut callbacks, _sink, state) = ChildCallbacks::new(caps(), host().0);
        let mut screen_parser = vt100::Parser::new(nz(4), nz(20), 0);

        // 1004 is not modelled by vt100, so it reaches our callback.
        callbacks.unhandled_csi(
            screen_parser.screen_mut(),
            Some(b'?'),
            None,
            &[&[1004u16][..]],
            'h',
        );
        assert!(state.focus_reporting());

        callbacks.unhandled_csi(
            screen_parser.screen_mut(),
            Some(b'?'),
            None,
            &[&[1004u16][..]],
            'l',
        );
        assert!(!state.focus_reporting());
    }

    #[test]
    fn sink_delivers_to_the_writer_once_installed() {
        let (mut callbacks, sink, _state) = ChildCallbacks::new(caps(), host().0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        sink.install(&tx);

        let mut screen_parser = vt100::Parser::new(nz(4), nz(20), 0);
        callbacks.unhandled_csi(screen_parser.screen_mut(), None, None, &[], 'c');

        let sent = rx.try_recv().expect("a reply should have been queued");
        assert!(sent.starts_with(b"\x1b[?"));
    }

    #[test]
    fn osc52_is_wrapped_verbatim() {
        assert_eq!(
            osc52(b"c", b"aGVsbG8="),
            b"\x1b]52;c;aGVsbG8=\x1b\\".to_vec()
        );
    }

    #[test]
    fn clipboard_copies_are_sent_to_the_host_not_the_child() {
        let (host_tx, mut host_rx) = host();
        let (mut callbacks, sink, _state) = ChildCallbacks::new(caps(), host_tx);

        // The child's own writer must not see this at all.
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        sink.install(&child_tx);

        let mut screen = vt100::Parser::new(nz(4), nz(20), 0);
        callbacks.copy_to_clipboard(screen.screen_mut(), b"c", b"aGk=");

        let queued = host_rx.try_recv().expect("the copy should have been queued");
        assert_eq!(queued, b"\x1b]52;c;aGk=\x1b\\".to_vec());
        assert!(child_rx.try_recv().is_err());
    }
}
