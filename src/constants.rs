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

/// Cap on how many bytes we coalesce into a single write to a child PTY.
pub const PTY_WRITE_BATCH: usize = 64 * 1024;

/// Cells the floating window's border eats on each side.
///
/// This used to be a flat `- 4` applied to the pty *and* the grid while the
/// widget painted into `rect - 2`, so a full-screen app drew for a size we never
/// rendered. The border is one cell per side, so this is the only number that
/// keeps the pty, the parser and the painted area in agreement.
pub const TENANT_BORDER: u16 = 1;

/// `vt100` 0.18 requires non-zero dimensions (it used to panic on zero), so
/// every size that comes from the environment goes through here.
pub fn nz(value: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(value).unwrap_or(std::num::NonZeroU16::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nz_never_returns_zero() {
        assert_eq!(nz(0).get(), 1);
        assert_eq!(nz(80).get(), 80);
    }
}
