//! What uncl tells its children about the terminal they are running in.
//!
//! A child cannot talk to the real terminal — it talks to us. Everything a
//! terminal application discovers about its environment (device attributes,
//! colour queries, window size, sixel support) has to be answered by uncl, from
//! a record we build once at startup.
//!
//! Two rules:
//!
//! * **Never advertise something we cannot transport.** Answering "yes" to
//!   sixel makes the child emit sixel; if our own layer drops it, the child has
//!   no way to find out and the user sees garbage. Since S3 the tap in
//!   `app::images` does transport DCS/APC/OSC payloads, so sixel is advertised
//!   when the host can draw it *and* `UNCL_SIXEL` is set (the sixel path is
//!   passthrough-only and cannot crop, so it stays opt-in).
//! * **Answer, don't stay silent.** Applications block on these queries with a
//!   timeout; silence is the bug this module exists to fix.

use std::env;
use std::sync::OnceLock;

/// The cell size uncl falls back to, in pixels.
///
/// Nothing in the terminal protocol tells a program its cell size: the program
/// has to ask (`CSI 16 t`), and the answer comes from whoever is pretending to
/// be the terminal — us. So we have to have one: [`probe_cell_px`] measures the
/// host's tty at startup when it can, `UNCL_CELL_PX=WxH` overrides it, and this
/// is the last resort.
///
/// It matters twice:
///
/// * children that size images in pixels ask `CSI 16 t`; answering zero makes
///   them divide by zero (`wezterm imgcat` panics on exactly that),
/// * sixel only states its size in pixels, so this is what decides whether it
///   could fit inside the float.
pub const DEFAULT_CELL_PX: (u16, u16) = (10, 20);

/// The cell size the host tty reported at startup, if it reported one.
///
/// `TIOCGWINSZ` carries the window's pixel size and most terminals fill it in
/// (WezTerm, kitty, iTerm2, foot); the ones that do not leave it zero, which is
/// what [`DEFAULT_CELL_PX`] is for.
static PROBED_CELL_PX: OnceLock<(u16, u16)> = OnceLock::new();

/// Ask the host tty for its window size in pixels and derive the cell size.
///
/// Called once, before any child exists. Costs one `ioctl` and cannot fail
/// loudly: a terminal that does not answer leaves the assumption in place.
pub fn probe_cell_px() {
    let Ok(window) = crossterm::terminal::window_size() else {
        return;
    };
    if window.columns == 0 || window.rows == 0 || window.width == 0 || window.height == 0 {
        return;
    }
    // `WindowSize`'s pixel fields are `u16` (an ioctl `winsize` is), so this is
    // integer division on small numbers; a terminal that reports nonsense gets a
    // zero cell and is ignored below.
    let cell_w = window.width / window.columns;
    let cell_h = window.height / window.rows;
    if cell_w > 0 && cell_h > 0 {
        let _ = PROBED_CELL_PX.set((cell_w, cell_h));
    }
}

/// The cell size in effect: `UNCL_CELL_PX=WxH`, else the measured one, else
/// [`DEFAULT_CELL_PX`].
///
/// Never zero — a caller may divide by it, and we may be the one telling a
/// child where to divide.
pub fn cell_px() -> (u16, u16) {
    if let Ok(spec) = env::var("UNCL_CELL_PX") {
        if let Some(measured) = parse_cell_px(&spec) {
            return measured;
        }
    }
    PROBED_CELL_PX.get().copied().unwrap_or(DEFAULT_CELL_PX)
}

/// `"12x26"` (also `12,26`), or `None` for anything unusable — including a zero,
/// because a zero cell size is precisely the answer that panics other people's
/// programs.
fn parse_cell_px(spec: &str) -> Option<(u16, u16)> {
    let mut parts = spec
        .split(['x', 'X', ','])
        .filter_map(|part| part.trim().parse::<u16>().ok());
    match (parts.next(), parts.next()) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        _ => None,
    }
}

/// One word for where [`cell_px`] came from, for `--doctor`.
fn cell_px_source() -> &'static str {
    // Report what was actually used: a malformed spec falls through to the
    // measured value, and the report should say so.
    if env::var("UNCL_CELL_PX")
        .ok()
        .is_some_and(|spec| parse_cell_px(&spec).is_some())
    {
        "UNCL_CELL_PX"
    } else if PROBED_CELL_PX.get().is_some() {
        "measured"
    } else {
        "assumed"
    }
}

/// Capabilities of the *host* terminal, as far as uncl is willing to relay them.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// `TERM` of the host, kept for diagnostics only.
    pub term: String,
    pub term_program: Option<String>,
    pub colorterm: Option<String>,
    /// Does the host have a kitty graphics capable terminal attached?
    pub kitty_graphics: bool,
    /// Does the host support sixel?
    pub sixel: bool,
    /// Is the host in a kitty keyboard protocol capable terminal? (uncl does not
    /// implement the protocol, so this is informational.)
    pub kitty_keyboard: bool,
    /// 24-bit colour possible?
    pub truecolor: bool,
}

impl Capabilities {
    /// Best-effort detection from the environment.
    ///
    /// `TERM` is a weak signal on its own; the reliable ones are the
    /// terminfo-independent variables emulators set (`TERM_PROGRAM`,
    /// `COLORTERM`, `KITTY_WINDOW_ID`, `WEZTERM_EXECUTABLE`). What children end up
    /// seeing is decided by [`Self::child_term`], not by this.
    pub fn from_env() -> Self {
        let term = env::var("TERM").unwrap_or_default();
        let term_lower = term.to_ascii_lowercase();
        let term_program = env::var("TERM_PROGRAM").ok();
        let program = term_program.as_deref().unwrap_or_default().to_ascii_lowercase();
        let colorterm = env::var("COLORTERM").ok();

        let is_kitty = env::var("KITTY_WINDOW_ID").is_ok()
            || term_lower.contains("kitty")
            || program == "kitty";
        let is_wezterm = program == "wezterm" || env::var("WEZTERM_EXECUTABLE").is_ok();
        let is_ghostty = term_lower.contains("ghostty") || program == "ghostty";
        let is_foot = term_lower.contains("foot");
        let is_mlterm = term_lower.contains("mlterm");
        let is_contour = term_lower.contains("contour");

        let truecolor = colorterm
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit"))
            || is_kitty
            || is_wezterm
            || is_ghostty;

        // Sixel: only terminals known to render it *and* only if the switch is
        // on. The switch flips to true for good when the DCS tap (S3) lands.
        let host_sixel = is_wezterm || is_foot || is_mlterm || is_contour;
        let sixel = host_sixel && env_flag("UNCL_SIXEL");

        // Windows: images are off, and not because of a bug we could fix here.
        // The child sits under uncl's *own* ConPTY, and ConPTY v1 (the in-box
        // implementation on Windows 10/11) parses the child's VT into a text
        // buffer, swallows DCS payloads, and answers DA1 itself *without* the
        // sixel attribute — so a child never even asks us, and a sixel we did
        // forward would be destroyed one layer down. ConPTY v2 forwards bytes and
        // would make this possible, but shipping it means bundling `conpty.dll`.
        //
        // Reporting `false` here is what keeps the promise in this module's header:
        // never advertise what we cannot transport (see the "Windows and Docker"
        // section of docs/rendering-report.md).
        let images = !cfg!(windows);
        let sixel = sixel && images;
        let kitty_graphics = (is_kitty || is_wezterm || is_ghostty) && images;

        Self {
            term,
            term_program,
            colorterm,
            kitty_graphics,
            sixel,
            kitty_keyboard: is_kitty,
            truecolor,
        }
    }

    /// Primary device attributes.
    ///
    /// Attributes: 1 = 132 columns, 4 = sixel, 6 = selective erase, 9 = national
    /// replacement charsets, 15 = technical characters, 22 = ANSI colour.
    pub fn da1(&self) -> Vec<u8> {
        let mut attrs: Vec<u16> = vec![1, 6, 9, 15, 22];
        if self.sixel {
            attrs.push(4);
        }
        attrs.sort_unstable();
        attrs.dedup();

        let mut reply = String::from("\x1b[?");
        for (index, attr) in attrs.iter().enumerate() {
            if index > 0 {
                reply.push(';');
            }
            reply.push_str(&attr.to_string());
        }
        reply.push('c');
        reply.into_bytes()
    }

    /// Secondary device attributes: terminal type, firmware version, ROM.
    pub fn da2(&self) -> Vec<u8> {
        // Deliberately unambitious: children mostly use this to detect "a real
        // terminal answered" rather than to switch on the type.
        b"\x1b[>0;1;0c".to_vec()
    }

    /// The `TERM` children see.
    ///
    /// The host's family is passed through, because which terminal you are running
    /// in is real information: `alacritty`, `foot`, `wezterm` and `ghostty` all
    /// imply capabilities uncl transports faithfully (24-bit colour, and for the
    /// image protocols the S3 tap carries the bytes through untouched). Three cases
    /// are downgraded instead:
    ///
    /// * **passthrough wrappers** (`screen*`, `tmux*`): their `TERM` describes a
    ///   multiplexer's own escape dialect, which we do not speak,
    /// * **`xterm-kitty`**: kitty's `TERM` is what makes applications enable the
    ///   kitty *keyboard* protocol, and uncl speaks the legacy key encodings — so
    ///   the claim would be false about input. Kitty's *graphics* are advertised
    ///   separately (`UNCL_GRAPHICS`, and the image tap carries them), which is the
    ///   half we can honestly deliver,
    /// * **nothing usable** (`dumb`, `linux`, unset): `xterm-256color` is the lowest
    ///   honest description of what this layer implements.
    ///
    /// `--doctor` prints both this and the host's own `TERM`, so the mapping is
    /// never a mystery.
    pub fn child_term(&self) -> &str {
        let term = self.term.as_str();
        let lower = term.to_ascii_lowercase();
        let downgrade = term.is_empty()
            || matches!(lower.as_str(), "dumb" | "linux")
            || lower.starts_with("screen")
            || lower.starts_with("tmux")
            || lower.contains("kitty");
        if downgrade { "xterm-256color" } else { term }
    }

    /// Apply the environment children should see.
    ///
    /// `TERM` follows [`Self::child_term`]: the host's family where we can carry
    /// what it implies, `xterm-256color` where we cannot. `COLORTERM` is inherited
    /// because 24-bit colour survives our own re-encoding intact, and
    /// `UNCL_GRAPHICS` says what the S3 tap carries.
    pub fn apply_child_env(&self, cmd: &mut portable_pty::CommandBuilder) {
        cmd.env("TERM", self.child_term());
        if self.truecolor {
            cmd.env("COLORTERM", "truecolor");
        }
        // Shells and tools use this to decide between "dumb" and interactive
        // output; uncl is a real terminal as far as the child is concerned.
        cmd.env("TERM_PROGRAM", "uncl");
        // Our own namespace, so a script (or the acceptance tests in
        // docs/rendering-report.md) can ask "will uncl carry my images?" without
        // guessing from TERM. Nothing outside uncl reads this.
        cmd.env("UNCL_GRAPHICS", "kitty,iterm2");
        // `FORCE_HYPERLINK` is the convention tools use to mean "this terminal
        // renders `OSC 8`", and since S3.5 uncl carries them (`app::term::links`),
        // so saying so is the truth. Without it, most programs fall back to
        // plain text and nothing is clickable.
        cmd.env("FORCE_HYPERLINK", "1");
        if let Some(program) = &self.term_program {
            cmd.env("UNCL_HOST_TERM_PROGRAM", program);
        }
    }

    /// A few lines for `uncl doctor`.
    pub fn summary(&self) -> String {
        format!(
            "TERM={} TERM_PROGRAM={} COLORTERM={}\n  truecolor={} sixel={} kitty_graphics={} kitty_keyboard={}\n  image tap: kitty + iTerm2 always, sixel {} (UNCL_SIXEL)\n  cell size: {}x{} px ({}), reported to children as CSI 16t (UNCL_CELL_PX)\n  hyperlinks: OSC 8 carried inside the float; FORCE_HYPERLINK=1 for children\n  scrollback: {} lines per pane, in both panes (UNCL_SCROLLBACK)\n  child TERM={} ({})\n  images: {} ({}), over {}",
            if self.term.is_empty() { "<unset>" } else { &self.term },
            self.term_program.as_deref().unwrap_or("<unset>"),
            self.colorterm.as_deref().unwrap_or("<unset>"),
            self.truecolor,
            self.sixel,
            self.kitty_graphics,
            self.kitty_keyboard,
            if self.sixel { "advertised" } else { "not advertised" },
            cell_px().0,
            cell_px().1,
            cell_px_source(),
            crate::constants::scrollback_lines(),
            self.child_term(),
            if self.child_term() == self.term.as_str() {
                "the host's own"
            } else {
                "downgraded: uncl does not carry what that TERM implies"
            },
            if self.kitty_graphics || self.sixel {
                "on"
            } else {
                "off"
            },
            if cfg!(windows) {
                "ConPTY v1 swallows DCS: Windows has no images until conpty.dll v2 is bundled"
            } else if self.kitty_graphics || self.sixel {
                "the image tap carries kitty/iTerm2; sixel only with UNCL_SIXEL=1"
            } else {
                "this host advertises no image protocol"
            },
            std::env::consts::OS,
        )
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// The 16 ANSI colours, used to answer `OSC 4` palette queries.
///
/// These are the xterm defaults. A user on a customised palette gets slightly
/// wrong answers, which is strictly better than the silence that used to make
/// `OSC 4;n;?` callers hang.
pub const ANSI_PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// Default foreground / background, used to answer `OSC 10/11` queries until we
/// probe the real values from the host (S3, where it starts to matter).
pub const DEFAULT_FG: (u8, u8, u8) = (0xe5, 0xe5, 0xe5);
pub const DEFAULT_BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

/// Format a colour the way xterm does: `rgb:rr/gg/bb`.
pub fn rgb_spec((r, g, b): (u8, u8, u8)) -> String {
    format!("rgb:{r:02x}/{g:02x}/{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(sixel: bool) -> Capabilities {
        Capabilities {
            term: "xterm-256color".into(),
            term_program: None,
            colorterm: Some("truecolor".into()),
            kitty_graphics: false,
            sixel,
            kitty_keyboard: false,
            truecolor: true,
        }
    }

    #[test]
    fn da1_only_claims_sixel_when_usable() {
        let without = String::from_utf8(caps(false).da1()).unwrap();
        assert!(without.starts_with("\x1b[?"));
        assert!(without.ends_with('c'));
        assert!(!without.contains(";4;") && !without.contains("?4"));

        let with = String::from_utf8(caps(true).da1()).unwrap();
        assert_eq!(with, "\x1b[?1;4;6;9;15;22c");
        assert_eq!(without, "\x1b[?1;6;9;15;22c");
    }

    #[test]
    fn da1_attributes_are_ascending() {
        let reply = String::from_utf8(caps(true).da1()).unwrap();
        let body = reply.trim_start_matches("\x1b[?").trim_end_matches('c');
        let attrs: Vec<u32> = body.split(';').map(|a| a.parse().unwrap()).collect();
        let mut sorted = attrs.clone();
        sorted.sort_unstable();
        assert_eq!(attrs, sorted);
    }

    #[test]
    fn da2_is_a_well_formed_reply() {
        assert_eq!(caps(false).da2(), b"\x1b[>0;1;0c".to_vec());
    }

    #[test]
    fn a_cell_size_spec_is_either_usable_or_ignored() {
        assert_eq!(parse_cell_px("12x26"), Some((12, 26)));
        assert_eq!(parse_cell_px("12,26"), Some((12, 26)));
        assert_eq!(parse_cell_px(" 12 X 26 "), Some((12, 26)));
        // Never zero: a zero cell size is what makes a child divide by zero.
        assert_eq!(parse_cell_px("0x26"), None);
        assert_eq!(parse_cell_px("12x0"), None);
        assert_eq!(parse_cell_px("12"), None);
        assert_eq!(parse_cell_px("auto"), None);
        assert_eq!(parse_cell_px(""), None);
    }

    #[test]
    fn the_child_gets_the_hosts_terminal_unless_we_cannot_carry_it() {
        let with = |term: &str| Capabilities {
            term: term.into(),
            term_program: None,
            colorterm: None,
            kitty_graphics: false,
            sixel: false,
            kitty_keyboard: false,
            truecolor: false,
        };

        // Faithful families pass through: the claim is true of this layer too.
        for term in ["alacritty", "foot", "wezterm", "xterm-256color", "ghostty"] {
            assert_eq!(with(term).child_term(), term);
        }

        // A multiplexer's dialect we do not speak, kitty's keyboard protocol we do
        // not implement, and consoles that cannot do any of it.
        for term in [
            "screen-256color",
            "tmux-256color",
            "xterm-kitty",
            "dumb",
            "linux",
            "",
        ] {
            assert_eq!(with(term).child_term(), "xterm-256color", "{term:?}");
        }
    }

    #[test]
    fn the_fallback_cell_size_is_never_zero() {
        let (width, height) = DEFAULT_CELL_PX;
        assert!(width > 0 && height > 0);
    }

    #[test]
    fn palette_entry_stops_at_the_ansi_range() {
        assert_eq!(ANSI_PALETTE.len(), 16);
        assert_eq!(rgb_spec(ANSI_PALETTE[1]), "rgb:cd/00/00");
    }
}
