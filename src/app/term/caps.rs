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
    /// `TERM` itself is a poor signal here because uncl deliberately does not
    /// pass the host's `TERM` to children (see `owner.rs`); the useful signals
    /// are the terminfo-independent env vars that emulators set.
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

        Self {
            term,
            term_program,
            colorterm,
            kitty_graphics: is_kitty || is_wezterm || is_ghostty,
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

    /// Apply the environment children should see.
    ///
    /// `TERM` is *not* inherited: we cannot transport images yet (S3), so
    /// claiming `xterm-kitty` or a sixel-capable `TERM` would make children emit
    /// bytes this layer drops. `COLORTERM` is inherited because 24-bit colour
    /// survives our own re-encoding intact.
    pub fn apply_child_env(&self, cmd: &mut portable_pty::CommandBuilder) {
        cmd.env("TERM", "xterm-256color");
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
        if let Some(program) = &self.term_program {
            cmd.env("UNCL_HOST_TERM_PROGRAM", program);
        }
    }

    /// A few lines for `uncl doctor`.
    pub fn summary(&self) -> String {
        format!(
            "TERM={} TERM_PROGRAM={} COLORTERM={}\n  truecolor={} sixel={} kitty_graphics={} kitty_keyboard={}\n  image tap: kitty + iTerm2 always, sixel {} (UNCL_SIXEL)",
            if self.term.is_empty() { "<unset>" } else { &self.term },
            self.term_program.as_deref().unwrap_or("<unset>"),
            self.colorterm.as_deref().unwrap_or("<unset>"),
            self.truecolor,
            self.sixel,
            self.kitty_graphics,
            self.kitty_keyboard,
            if self.sixel { "advertised" } else { "not advertised" },
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
    fn palette_entry_stops_at_the_ansi_range() {
        assert_eq!(ANSI_PALETTE.len(), 16);
        assert_eq!(rgb_spec(ANSI_PALETTE[1]), "rgb:cd/00/00");
    }
}
