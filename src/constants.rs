#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeDirection {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

pub const MIN_WIDTH: u16 = 10;
pub const MIN_HEIGHT: u16 = 10;

pub const DEFAULT_WIDTH: u16 = 80;
pub const DEFAULT_HEIGHT: u16 = 25;

pub const DEFAULT_X: u16 = 10;
pub const DEFAULT_Y: u16 = 5;

/// How many lines of history each pane keeps by default.
///
/// Both panes get the same treatment: a shell you cannot scroll back through is
/// the most jarring thing about running one inside another terminal. The number
/// is deliberately modest — `vt100` stores a row as a `Vec<Cell>`, so 2000 lines
/// of an 80-column grid is a few megabytes per pane.
///
/// `UNCL_SCROLLBACK`, from the environment uncl was launched in, overrides it,
/// and `0` turns scrollback off for setups that would rather have the memory.
pub const DEFAULT_SCROLLBACK: usize = 2_000;

/// How many lines one wheel notch scrolls.
pub const SCROLL_STEP: usize = 3;

/// The scrollback capacity to use, from `UNCL_SCROLLBACK` or the default.
///
/// Read once per pane (at parser construction), so exporting the variable in the
/// environment uncl is launched from is all it takes.
pub fn scrollback_lines() -> usize {
    match std::env::var("UNCL_SCROLLBACK") {
        Ok(spec) => parse_scrollback(&spec),
        Err(_) => DEFAULT_SCROLLBACK,
    }
}

/// `UNCL_SCROLLBACK`, kept separate so the parsing is testable without touching
/// the process environment (which is global and, in edition 2024, `unsafe`).
fn parse_scrollback(spec: &str) -> usize {
    let spec = spec.trim();
    if spec.is_empty() {
        return DEFAULT_SCROLLBACK;
    }
    // Plain digits only: `2k` is a guess, and a guess about memory is the wrong
    // thing to make silently.
    if !spec.bytes().all(|byte| byte.is_ascii_digit()) {
        return DEFAULT_SCROLLBACK;
    }
    spec.parse().unwrap_or(DEFAULT_SCROLLBACK)
}

/// Minimum time between frames (~120 Hz).
///
/// A child flooding output raises the reader signal far more often than a
/// terminal can usefully repaint; frames are coalesced to this interval. Input
/// is never delayed by it — the event loop still wakes for input immediately.
pub const FRAME_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// How often the event loop wakes up when nothing else is happening.
///
/// Frames are driven by PTY output and input events (see `app::signal::Signal`),
/// so this timer exists only for housekeeping — noticing that a child has
/// exited — and it is deliberately coarse: a 60 Hz idle wake-up would burn CPU
/// for nothing, which is exactly what S0 is fixing.
pub const IDLE_TICK_MS: u64 = 100;

/// How often the blocking crossterm poll in the input pump gives up and
/// re-checks whether it should shut down.
pub const INPUT_POLL_INTERVAL_MS: u64 = 50;

/// Maximum number of queued input events handled per loop iteration before we
/// yield back to rendering. A paste or a mouse drag can produce thousands of
/// events; without a budget we would starve the screen.
pub const INPUT_DRAIN_BUDGET: usize = 512;

/// How long to wait before trying to respawn the floating shell again after a
/// failure. `exec` can fail for reasons that need user action (a broken `$SHELL`,
/// a deleted cwd), and retrying every 100 ms tick would spin the event loop.
pub const TENANT_RESPAWN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Cap on how many bytes we coalesce into a single write to a child PTY.
pub const PTY_WRITE_BATCH: usize = 64 * 1024;

/// Cells the floating window's border eats on each side.
///
/// This used to be a flat `- 4` applied to the pty *and* the grid while the old
/// widget painted into `rect - 2`, so a full-screen app drew for a size we never
/// rendered. The border is one cell per side, so this is the only number that
/// keeps the pty, the parser and the painted area in agreement.
pub const TENANT_BORDER: u16 = 1;

/// The rectangle the float's child draws into, from the float's outer rect.
///
/// Used by the overlay (to size the pty and the parser) and by the renderer (to
/// paint). Keeping it in one place is what keeps those three numbers equal.
pub fn content_rect(rect: crate::geometry::Rect) -> crate::geometry::Rect {
    rect.inset(TENANT_BORDER)
}

/// `vt100` 0.18 requires non-zero dimensions (it used to panic on zero), so
/// every size that comes from the environment goes through here.
pub fn nz(value: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(value).unwrap_or(std::num::NonZeroU16::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrollback_spec_is_either_a_number_or_the_default() {
        assert_eq!(parse_scrollback("0"), 0);
        assert_eq!(parse_scrollback("500"), 500);
        assert_eq!(parse_scrollback(" 500 "), 500);
        // Guesses about memory are not made silently.
        assert_eq!(parse_scrollback("2k"), DEFAULT_SCROLLBACK);
        assert_eq!(parse_scrollback("-1"), DEFAULT_SCROLLBACK);
        assert_eq!(parse_scrollback(""), DEFAULT_SCROLLBACK);
        assert_eq!(parse_scrollback("lots"), DEFAULT_SCROLLBACK);
    }

    #[test]
    fn the_default_scrollback_is_useful() {
        // A pane that keeps no history cannot be scrolled, and one that keeps a
        // hundred thousand lines is a memory leak with a nice name.
        assert!((1_000..=10_000).contains(&DEFAULT_SCROLLBACK));
    }

    #[test]
    fn nz_never_returns_zero() {
        assert_eq!(nz(0).get(), 1);
        assert_eq!(nz(80).get(), 80);
    }
}
