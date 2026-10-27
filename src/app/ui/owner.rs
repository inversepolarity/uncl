use anyhow::Result;
use portable_pty::CommandBuilder;

use bytes::Bytes;

use std::{
    io::{self, Write},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use vt100::{MouseProtocolMode, Screen};

use crate::app::input::encode::{ChildModes, encode_key, encode_mouse};
use crate::app::input::mouse::{self, MouseRoute, Wheel};
use crate::app::lease::{Lease, new_parser};
use crate::app::pty::{self, ChildParser, PtySession};
use crate::app::term::callbacks::ChildState;
use crate::app::term::dcs;
use crate::app::signal::Signal;
use crate::app::render::{FrameLinks, Panes, Renderer};
use crate::app::term::callbacks::ReplySink;
use crate::app::term::caps::Capabilities;
use crate::constants::*;
use crate::geometry::Rect;

pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// What the input handler wants the loop to do next.
enum Flow {
    Continue,
    /// Pane geometry, visibility or terminal size changed: repaint.
    Redraw,
    /// Owner shell is gone.
    Quit,
}

/// Restores the terminal on every exit path, including `?` early returns and
/// panics.
///
/// This replaces the old "restore at the end of initialize_pty" code, which was
/// skipped whenever `run()` returned early or the process was torn down from
/// under us — leaving the user's terminal in raw mode on the alternate screen
/// with no cursor, which reads as "uncl ate my prompt".
struct TerminalGuard;

impl TerminalGuard {
    fn engage() -> Self {
        Self
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // `CSI 0 SP q`: put the host's cursor shape back, since a child may have
        // changed it through us and it outlives the alternate screen.
        let _ = io::stdout().write_all(b"\x1b[0 q");
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show,
            SetAttribute(Attribute::Reset),
            ResetColor
        );
    }
}

/// What we ask the *host* terminal to report.
///
/// The host's reporting mode is not the child's: uncl has to receive enough to
/// route events (and to see a click outside the float), but not so much that a
/// mouse move becomes thousands of events per second. Specifically, always
/// request SGR encoding so coordinates past 223 columns work, and only request
/// any-motion (`?1003`) when the focused child actually asked for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostMouse {
    Off,
    Press,
    ButtonMotion,
    AnyMotion,
}

impl HostMouse {
    fn for_modes(modes: ChildModes) -> Self {
        match modes.mouse_mode {
            MouseProtocolMode::None => HostMouse::Off,
            MouseProtocolMode::Press | MouseProtocolMode::PressRelease => HostMouse::Press,
            MouseProtocolMode::ButtonMotion => HostMouse::ButtonMotion,
            MouseProtocolMode::AnyMotion => HostMouse::AnyMotion,
        }
    }

    /// uncl always needs press events while the float is up, so it can drag it
    /// and hide it on a click outside; motion only if the child asked.
    fn at_least_press(self) -> Self {
        match self {
            HostMouse::Off => HostMouse::Press,
            other => other,
        }
    }

    fn sequence(self) -> &'static [u8] {
        match self {
            // Disable everything first: turning ?1000 off does not turn ?1002 or
            // ?1003 off, which is how multiplexers end up receiving motion they
            // never asked for.
            HostMouse::Off => b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l",
            HostMouse::Press => b"\x1b[?1003l\x1b[?1002l\x1b[?1000h\x1b[?1006h",
            HostMouse::ButtonMotion => {
                b"\x1b[?1003l\x1b[?1002h\x1b[?1000h\x1b[?1006h"
            }
            HostMouse::AnyMotion => b"\x1b[?1003h\x1b[?1002h\x1b[?1000h\x1b[?1006h",
        }
    }
}

pub struct Container {
    pub rect: Rect,
    pub size: Size,
    pub caps: Arc<Capabilities>,
    pub parser: Arc<RwLock<ChildParser>>,
    pub parser_state: Arc<ChildState>,
    pub parser_sink: ReplySink,
    pub session: Option<PtySession>,
    pub lease: Lease,
    pub signal: Arc<Signal>,
    /// Set on the way out so the input pump thread stops polling forever.
    stop: Arc<AtomicBool>,
    /// The mouse mode we last asked the host for.
    host_mouse: HostMouse,
    /// The cursor style we last asked the host for (`DECSCUSR` parameter).
    cursor_style: u16,
    /// When the float's shell may be spawned again after a failure.
    tenant_retry_after: Option<Instant>,
    /// Effects children addressed to the *host* terminal (clipboard copies).
    host_tx: UnboundedSender<Vec<u8>>,
    host_rx: UnboundedReceiver<Vec<u8>>,
    /// The frame emitter (S2: uncl writes the ANSI stream itself).
    renderer: Renderer,
    /// Lines scrolled up in the owner pane (0 = live).
    owner_scroll: usize,
    /// Lines scrolled up in the float (0 = live). Reset with its lease.
    tenant_scroll: usize,
    /// The cell a press landed on, in whichever pane, so a click on a hyperlink
    /// can be told from a drag. `None` when the press was not on a link.
    link_press: Option<LinkPress>,
}

/// A press that landed on a hyperlink, so its release can be matched to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinkPress {
    /// Which pane: the float, or the shell behind it.
    tenant: bool,
    /// The cell, pane-local.
    cell: (u16, u16),
}

impl Container {
    pub fn new() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT));

        // Ask the host for its pixel geometry once, before any child can ask us
        // for ours. Children that size images (and sixel, which only knows
        // pixels) need it, and "no answer" makes them divide by zero.
        crate::app::term::caps::probe_cell_px();

        let caps = Arc::new(Capabilities::from_env());
        let (host_tx, host_rx) = unbounded_channel::<Vec<u8>>();
        let (parser, parser_sink, parser_state) = new_parser(caps.clone(), host_tx.clone());

        Self {
            rect: Rect::new(0, 0, cols, rows),
            size: Size { cols, rows },
            caps: caps.clone(),
            parser,
            parser_state,
            parser_sink,
            session: None,
            lease: Lease::new(caps, host_tx.clone()),
            signal: Signal::new(),
            stop: Arc::new(AtomicBool::new(false)),
            host_mouse: HostMouse::Off,
            cursor_style: 0,
            tenant_retry_after: None,
            host_tx,
            host_rx,
            renderer: Renderer::new(),
            owner_scroll: 0,
            tenant_scroll: 0,
            link_press: None,
        }
    }

    /// Spawn the floating shell and store its session on the lease.
    fn init_tenant(&mut self) -> Result<()> {
        let parser = self.lease.tenant_parser.clone();
        let sink = self.lease.tenant_sink.clone();
        let signal = self.signal.clone();
        let images = self.lease.tenant_images.clone();
        let session = self
            .lease
            .tenant
            .spawn(parser, sink, signal, self.caps.clone(), images)?;
        self.lease.attach(session);
        Ok(())
    }

    fn tenant_running(&self) -> bool {
        self.lease.running()
    }

    pub async fn initialize_pty(&mut self) -> Result<(), anyhow::Error> {
        // The float's default geometry (80x25 at 10,5) can be larger than the
        // real terminal; clamp before spawning so the child's first frame is
        // already inside the screen.
        self.lease.tenant.clamp_to((self.size.cols, self.size.rows));
        self.init_tenant()?;

        // Size the owner's grid to the real terminal before spawning, so the
        // shell never starts at 80x25 on a 200x60 screen.
        self.parser
            .write()
            .unwrap()
            .set_size(nz(self.size.rows), nz(self.size.cols));

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.args(["-y", "-i", "--login"]);
        cmd.cwd(std::env::current_dir()?);
        self.caps.apply_child_env(&mut cmd);

        let session = pty::spawn(
            cmd,
            self.size.rows,
            self.size.cols,
            self.parser.clone(),
            self.parser_sink.clone(),
            self.signal.clone(),
            // The owner pane is the real terminal: its images are as invisible as
            // they were before S3, which is honest — we never claim otherwise.
            None,
            self.caps.clone(),
        )?;
        self.session = Some(session);

        // Engage the guard *before* the first state change it has to undo, so
        // every exit path (including `?` and a panic) restores the terminal.
        enable_raw_mode()?;
        let _guard = TerminalGuard::engage();

        // Mouse reporting is *not* enabled here: it is driven per focused
        // child by `sync_host_mouse`, which is what stops a `?1003`-less child
        // from drowning us in motion events (see `HostMouse`).
        execute!(
            io::stdout(),
            EnableBracketedPaste,
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        self.sync_host_mouse();
        // Nobody has the mouse or the bell until the toggle says so: the float is
        // visible at startup, so it is the focused pane (`refocus` is what makes
        // the two mechanisms agree).
        self.refocus(self.float_focused());

        // The first frame paints every cell; nothing is inherited from the
        // screen that was there before.
        self.renderer.invalidate();

        let result = self.run().await;

        // Stop the input pump and the children while the alternate screen is
        // still up, so their teardown cannot smear the user's real screen.
        self.stop.store(true, Ordering::Release);
        self.shutdown_children();

        // `_guard` restores raw mode / mouse capture / alt screen / cursor here.
        result
    }

    /// Kill both shells. Dropping a session SIGHUPs its child and unblocks the
    /// reader thread, which is what lets the process exit instead of hanging on
    /// runtime shutdown.
    fn shutdown_children(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.shutdown();
        }
        self.session = None;
        self.lease.shutdown();
    }

    /// Paint the whole screen with one `write_all`.
    ///
    /// Both grids are cloned under their locks first (a `Screen` clone is a
    /// `memcpy` of ~32 bytes per cell), so the parser is never blocked while the
    /// frame is encoded — a reader thread that has to wait for the lock is a
    /// reader thread that is not draining the child's pty. The snapshots are then
    /// handed to the renderer, which keeps them as the next frame's `prev`.
    fn draw(&mut self) -> Result<()> {
        let mut owner: Screen = self.parser.read().unwrap().screen().clone();
        let visible = self.lease.tenant_visible && self.tenant_running();
        let mut float: Option<(Screen, Rect)> = if visible {
            let screen = self.lease.tenant_parser.read().unwrap().screen().clone();
            Some((screen, self.lease.tenant.rect))
        } else {
            None
        };

        // Scrolling back is a property of the *view*, not of the parser, so it is
        // applied to the snapshot: the renderer diffs view against view and
        // notices nothing unusual. `set_scrollback` clamps, which is also how an
        // over-scrolled offset heals itself.
        if self.owner_scroll > 0 {
            owner.set_scrollback(self.owner_scroll);
            self.owner_scroll = owner.scrollback();
        }
        if let Some((screen, _)) = float.as_mut() {
            if self.tenant_scroll > 0 {
                screen.set_scrollback(self.tenant_scroll);
                self.tenant_scroll = screen.scrollback();
            }
        }
        let focused_scrolled = if visible {
            self.tenant_scroll > 0
        } else {
            self.owner_scroll > 0
        };

        // Bind what the frame needs before borrowing the renderer mutably.
        let owner_area = self.rect;

        // Images belong to the live view: they are anchored to cells, and a
        // scrolled view is showing older ones. Handing the renderer no store at
        // all while scrolled is what suppresses them; the renderer clears the
        // host's layer and re-sends the payloads when the store comes back.
        let scrolled_away = visible && self.tenant_scroll > 0;

        // The image store is read under its lock for the duration of the frame.
        // It is never held while the parser runs (the reader takes the parser
        // lock first, then this one), so a frame cannot deadlock a reader.
        let images = self.lease.tenant_images.lock().unwrap();

        // Hyperlinks are snapshotted, not borrowed: the frames own their inputs,
        // and an empty snapshot allocates nothing.
        let links = FrameLinks {
            owner: self.parser_state.links().snapshot(),
            float: self.lease.tenant_state.links().snapshot(),
        };

        // The float's child may have named itself (`OSC 0/2`); the border is the
        // only place a floating window can say what is running in it.
        let float_title = self.lease.tenant_state.title();

        let bytes = self.renderer.render(Panes {
            owner,
            owner_area,
            float,
            float_focused: visible,
            images: if scrolled_away { None } else { Some(&*images) },
            links,
            // Shown in the float's bottom border while scrolled back.
            scroll: self.tenant_scroll,
            focused_scrolled,
            float_title: float_title.as_deref(),
        });

        if !bytes.is_empty() {
            let mut stdout = io::stdout().lock();
            stdout.write_all(bytes)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// Is the float focused (visible and alive)?
    fn float_focused(&self) -> bool {
        self.lease.tenant_visible && self.tenant_running()
    }

    /// The modes of whichever child currently owns the keyboard.
    fn focused_modes(&self) -> ChildModes {
        if self.float_focused() {
            let parser = self.lease.tenant_parser.read().unwrap();
            ChildModes::from_screen(parser.screen())
        } else {
            let parser = self.parser.read().unwrap();
            ChildModes::from_screen(parser.screen())
        }
    }

    /// Send bytes to whichever pane currently owns the keyboard.
    fn active_sender(&self) -> Option<UnboundedSender<Bytes>> {
        if self.float_focused() {
            self.lease.writer()
        } else {
            self.session.as_ref().map(|session| session.writer.clone())
        }
    }

    /// Ask the host for exactly the reporting the focused child needs.
    ///
    /// Re-sends only on change, so the steady state costs one comparison.
    fn sync_host_mouse(&mut self) {
        // Always at least press: uncl has to see the wheel to give either pane a
        // scrollback (a host that never reports the wheel is a host uncl cannot
        // scroll), and while the float is up it has to see the press to drag it
        // and to resolve its own hyperlinks. The cost is real and deliberate: the
        // *host's* scrollback and mouse selection are unavailable unless the host
        // overrides the grab (Shift+drag in most of them), because uncl now
        // provides both itself.
        let desired = HostMouse::for_modes(self.focused_modes()).at_least_press();

        if desired == self.host_mouse {
            return;
        }
        self.host_mouse = desired;

        let mut stdout = io::stdout();
        let _ = stdout.write_all(desired.sequence());
        let _ = stdout.flush();
    }

    /// Scroll one pane back or forward by `step` lines.
    ///
    /// Driven by the wheel (`SCROLL_STEP`, and only when the child did *not* ask
    /// for the mouse: a TUI that wants the wheel gets it, a shell does not) and by
    /// `Ctrl+Alt`+arrows/page keys, which need no mouse at all. The offset is
    /// clamped to the pane's scrollback here rather than at draw time, so
    /// scrolling up at the top does not build up phantom offset that has to be
    /// unwound.
    fn scroll(&mut self, tenant: bool, wheel: Wheel, step: usize) {
        let capacity = self.scrollback_len(tenant);
        if capacity == 0 {
            return;
        }

        let current = if tenant {
            self.tenant_scroll
        } else {
            self.owner_scroll
        };
        let next = match wheel {
            Wheel::Up => current.saturating_add(step).min(capacity),
            Wheel::Down => current.saturating_sub(step),
        };
        if next == current {
            return;
        }

        if tenant {
            self.tenant_scroll = next;
        } else {
            self.owner_scroll = next;
        }
    }

    /// How far back a pane can be scrolled.
    fn scrollback_len(&self, tenant: bool) -> usize {
        if tenant {
            match self.lease.tenant_parser.read() {
                Ok(parser) => parser.screen().scrollback_len(),
                Err(_) => 0,
            }
        } else {
            match self.parser.read() {
                Ok(parser) => parser.screen().scrollback_len(),
                Err(_) => 0,
            }
        }
    }

    /// Back to the live view in both panes.
    fn reset_scroll(&mut self) {
        self.owner_scroll = 0;
        self.tenant_scroll = 0;
    }

    /// Follow a click inside the float, so a press and its release on the same
    /// hyperlinked cell open the link.
    ///
    /// This exists because uncl holds the mouse the whole time it runs — it has
    /// to, to keep the float draggable, to hide it on a click outside, and to have
    /// a scrollback at all — which means the *host* terminal never sees the click
    /// and can never resolve a hyperlink of its own. So the click is ours to
    /// interpret, in either pane, and only when the child did not ask for it.
    fn link_click(&mut self, tenant: bool, kind: MouseEventKind, cell: (u16, u16)) {
        // Link spans live in the *live* grid's coordinates. While the view is
        // scrolled back every visible row is some other row's text, so there is
        // nothing here to click that uncl could point at correctly.
        if if tenant {
            self.tenant_scroll > 0
        } else {
            self.owner_scroll > 0
        } {
            self.link_press = None;
            return;
        }

        let press = LinkPress { tenant, cell };
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.link_press = self
                    .link_target(tenant, cell.1, cell.0)
                    .map(|_| press);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.link_press.take() == Some(press) {
                    if let Some(url) = self.link_target(tenant, cell.1, cell.0) {
                        open_url(&url);
                    }
                }
            }
            // Movement or another button: this is a drag, not a click.
            _ => self.link_press = None,
        }
    }

    /// The URI of a pane's link covering `(row, column)`, if there is one.
    fn link_target(&self, tenant: bool, row: u16, col: u16) -> Option<Vec<u8>> {
        let (state, parser) = if tenant {
            (&self.lease.tenant_state, &self.lease.tenant_parser)
        } else {
            (&self.parser_state, &self.parser)
        };
        let links = state.links().snapshot();
        let parser = parser.read().ok()?;
        let link = links
            .for_row(parser.screen(), row)
            .into_iter()
            .find(|link| {
                let (start, end) = link.span();
                col >= start && col < end
            })?;
        Some(link.target().to_vec())
    }

    /// Write everything children asked the *host* terminal for.
    ///
    /// Only this thread writes to stdout, so a child's clipboard copy can never
    /// land in the middle of a frame.
    fn flush_host_effects(&mut self) {
        let mut out: Vec<u8> = Vec::new();
        while let Ok(bytes) = self.host_rx.try_recv() {
            out.extend_from_slice(&bytes);
        }
        if out.is_empty() {
            return;
        }
        let mut stdout = io::stdout();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }

    /// Relay the focused pane's cursor shape (`DECSCUSR`).
    ///
    /// Without this, nvim's beam cursor in insert mode is a block in the float:
    /// the child set the shape on *its* terminal and the request never left it.
    fn sync_cursor_style(&mut self) {
        let style = if self.float_focused() {
            self.lease.tenant_parser.read().unwrap().screen().cursor_style()
        } else {
            self.parser.read().unwrap().screen().cursor_style()
        };

        let code = dcs::decscusr(style);
        if code == self.cursor_style {
            return;
        }
        self.cursor_style = code;

        let mut stdout = io::stdout();
        let _ = stdout.write_all(format!("\x1b[{code} q").as_bytes());
        let _ = stdout.flush();
    }

    /// Tell one pane that it gained or lost the keyboard.
    ///
    /// `vt100` does not model `CSI ?1004h`, so the mode is tracked by the
    /// callbacks (see `term::callbacks`). A child that never asked for focus
    /// reports must not receive them: `CSI I` would be typed into its input.
    fn send_focus(&self, to_float: bool, gained: bool) {
        let wants = if to_float {
            let parser = self.lease.tenant_parser.read().unwrap();
            parser.callbacks().state().focus_reporting()
        } else {
            let parser = self.parser.read().unwrap();
            parser.callbacks().state().focus_reporting()
        };

        if !wants {
            return;
        }

        let sender = if to_float {
            self.lease.writer()
        } else {
            self.session.as_ref().map(|session| session.writer.clone())
        };

        if let Some(sender) = sender {
            let sequence: &'static [u8] = if gained { b"\x1b[I" } else { b"\x1b[O" };
            let _ = sender.send(Bytes::from_static(sequence));
        }
    }

    /// Move keyboard focus to whichever pane is about to own it, and take it
    /// away from the other. Called on the toggle and on terminal focus changes.
    fn refocus(&self, focus_float: bool) {
        self.send_focus(!focus_float, false);
        self.send_focus(focus_float, true);
        // uncl's own copy of "who has the keyboard": the bell only rings from the
        // focused pane, so a hidden float cannot interrupt the owner's shell.
        self.parser_state.set_focused(!focus_float);
        self.lease.tenant_state.set_focused(focus_float);
    }

    fn forward_paste(&self, text: &str) {
        let Some(sender) = self.active_sender() else {
            return;
        };

        let modes = self.focused_modes();
        let mut payload = Vec::with_capacity(text.len() + 12);
        if modes.bracketed_paste {
            payload.extend_from_slice(b"\x1b[200~");
        }
        payload.extend_from_slice(text.as_bytes());
        if modes.bracketed_paste {
            payload.extend_from_slice(b"\x1b[201~");
        }

        let _ = sender.send(Bytes::from(payload));
    }

    /// Route one key press: uncl's own chords first, then the child's encoder.
    fn handle_key(&mut self, key: KeyEvent) -> Flow {
        // Windows reports release/repeat events as well; sending all of them
        // made every keystroke fire twice there.
        if key.kind != KeyEventKind::Press {
            return Flow::Continue;
        }

        // Typing means "I am done reading history": every shell in every terminal
        // behaves this way, and the key is still delivered to the child below.
        self.reset_scroll();

        // Escape hatch: Ctrl+Alt+q quits uncl without having to type `exit`
        // into whichever shell happens to have focus.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('q'))
        {
            return Flow::Quit;
        }

        // The one chord uncl keeps for itself is the toggle.
        if key.code == KeyCode::Home && key.modifiers.is_empty() {
            self.lease.tenant_visible = !self.lease.tenant_visible;
            self.lease.tenant.child_grab = false;
            if self.lease.tenant_visible {
                self.lease.sync_size();
            } else {
                // Hiding the float puts it back on the live view, so it does not
                // reappear showing history nobody asked to see.
                self.tenant_scroll = 0;
                self.link_press = None;
            }
            // Children that asked for `?1004` must see the focus move, or every
            // TUI in the float keeps painting its "unfocused" cursor style.
            self.refocus(self.float_focused());
            self.renderer.invalidate();
            return Flow::Redraw;
        }

        // Keyboard scrollback, for terminals without a wheel and for the wheels a
        // host swallows anyway: Ctrl+Alt+Up/Down scroll the focused pane a notch,
        // Ctrl+Alt+PageUp/PageDown a screenful. Typing anything returns to live.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            let wheel = match key.code {
                KeyCode::Up | KeyCode::PageUp => Some(Wheel::Up),
                KeyCode::Down | KeyCode::PageDown => Some(Wheel::Down),
                _ => None,
            };
            if let Some(wheel) = wheel {
                let page = usize::from(self.size.rows).max(1);
                let step = match key.code {
                    KeyCode::PageUp | KeyCode::PageDown => page,
                    _ => SCROLL_STEP,
                };
                let tenant = self.float_focused();
                self.scroll(tenant, wheel, step);
                return Flow::Redraw;
            }
        }

        let term_size = (self.size.cols, self.size.rows);

        // Float geometry: Alt+arrows move, Alt+Shift+arrows resize.
        if self.float_focused() && self.lease.tenant.geometry_key(key, term_size) {
            self.lease.sync_size();
            self.renderer.invalidate();
            return Flow::Redraw;
        }

        let Some(sender) = self.active_sender() else {
            return Flow::Continue;
        };
        let modes = self.focused_modes();

        if let Some(bytes) = encode_key(key, modes) {
            let _ = sender.send(Bytes::from(bytes));
        }

        Flow::Continue
    }

    /// Route one mouse event: float geometry, child input, or click-away.
    fn handle_mouse(&mut self, m: MouseEvent) -> Flow {
        let term_size = (self.size.cols, self.size.rows);

        if !self.float_focused() {
            return self.forward_owner_mouse(m);
        }

        match mouse::route(&self.lease.tenant, m) {
            MouseRoute::Child { col, row } => {
                let content = self.lease.tenant.content_rect();
                let modes = self.lease.modes();
                let forwarded = encode_mouse(m.kind, m.modifiers, col, row, content, modes);

                if let Some(bytes) = forwarded {
                    if let Some(sender) = self.lease.writer() {
                        let _ = sender.send(Bytes::from(bytes));
                    }
                } else {
                    // The child did not ask for this event, so it is ours:
                    // scrollback on a wheel, and a hyperlink on a click. (A
                    // plain `Down` lands here whenever the child has mouse
                    // reporting off — which is every shell.)
                    if let Some(wheel) = Wheel::from_kind(m.kind) {
                        self.scroll(true, wheel, SCROLL_STEP);
                        return Flow::Redraw;
                    }
                    self.link_click(true, m.kind, (col, row));
                }

                // Hold the gesture for the child until the button comes up, so a
                // drag that leaves the float still delivers its release.
                match m.kind {
                    MouseEventKind::Down(_) => self.lease.tenant.child_grab = modes.wants_mouse(),
                    MouseEventKind::Up(_) => self.lease.tenant.child_grab = false,
                    _ => {}
                }

                Flow::Continue
            }

            MouseRoute::Local => {
                if let Some(wheel) = Wheel::from_kind(m.kind) {
                    // Outside the float (or on its border): the owner pane.
                    self.scroll(false, wheel, SCROLL_STEP);
                    return Flow::Redraw;
                }

                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    mouse::begin(&mut self.lease.tenant, m);
                    // A press that starts a drag or a resize is not a link click.
                    self.link_press = None;
                } else if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) {
                    // The release did not land on the cell the press did (it left
                    // the float), so it is not a click on a link either.
                    self.link_press = None;
                    if mouse::end(&mut self.lease.tenant) {
                        // A finished resize changes the child's grid size.
                        self.lease.sync_size();
                        self.renderer.invalidate();
                    }
                } else if mouse::drag(&mut self.lease.tenant, m, term_size) {
                    // Live-resize the child while the pointer moves, so a
                    // full-screen app reflows during the drag.
                    self.lease.sync_size();
                    self.renderer.invalidate();
                }
                Flow::Redraw
            }

            MouseRoute::HideFloat => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.lease.tenant_visible = false;
                    self.lease.tenant.child_grab = false;
                    self.renderer.invalidate();
                    return Flow::Redraw;
                }
                Flow::Continue
            }
        }
    }

    /// Mouse events for the background shell, encoded in *its* protocol.
    fn forward_owner_mouse(&mut self, m: MouseEvent) -> Flow {
        let (modes, area) = {
            let parser = self.parser.read().unwrap();
            (ChildModes::from_screen(parser.screen()), self.rect)
        };

        // Most shells never enable mouse reporting; skip the encode entirely.
        // Either way the child is asked first: only if it declined does the event
        // become uncl's — the wheel scrolls its history, and a click opens a
        // hyperlink (`Wheel` is never encoded when the child wants the mouse).
        let forwarded = if modes.wants_mouse() {
            encode_mouse(m.kind, m.modifiers, m.column, m.row, area, modes)
        } else {
            None
        };
        if let Some(bytes) = forwarded {
            if let Some(session) = &self.session {
                let _ = session.writer.send(Bytes::from(bytes));
            }
            return Flow::Continue;
        }

        if let Some(wheel) = Wheel::from_kind(m.kind) {
            self.scroll(false, wheel, SCROLL_STEP);
            return Flow::Redraw;
        }

        // The owner's grid starts at the screen origin, but the subtraction is what
        // keeps this honest if that ever stops being true.
        let cell = (
            m.column.saturating_sub(area.x),
            m.row.saturating_sub(area.y),
        );
        self.link_click(false, m.kind, cell);
        Flow::Continue
    }

    fn handle_event(&mut self, event: Event) -> Flow {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Paste(text) => {
                self.forward_paste(&text);
                Flow::Continue
            }
            Event::FocusGained => {
                let target = self.float_focused();
                self.send_focus(target, true);
                Flow::Continue
            }
            Event::FocusLost => {
                let target = self.float_focused();
                self.send_focus(target, false);
                Flow::Continue
            }
            Event::Resize(cols, rows) => {
                self.size = Size { cols, rows };
                self.rect = Rect::new(0, 0, cols, rows);
                self.parser.write().unwrap().set_size(nz(rows), nz(cols));

                // Tell the background shell it has a new size too.
                if let Some(session) = &self.session {
                    let _ = session.resize.send((rows, cols));
                }

                // Keep the float on screen and take the child's grid with it.
                self.lease.tenant.clamp_to((cols, rows));
                self.lease.sync_size();
                self.renderer.invalidate();

                Flow::Redraw
            }
        }
    }

    /// Blocking crossterm reads run on their own thread so the event loop can
    /// `select!` over input, PTY output and a timer instead of sleeping.
    fn spawn_input_pump(stop: Arc<AtomicBool>) -> UnboundedReceiver<Event> {
        let (tx, rx) = unbounded_channel::<Event>();
        tokio::task::spawn_blocking(move || {
            while !stop.load(Ordering::Acquire) {
                match event::poll(Duration::from_millis(INPUT_POLL_INTERVAL_MS)) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            eprintln!("input read error: {err}");
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(err) => {
                        eprintln!("input poll error: {err}");
                        break;
                    }
                }
            }
        });
        rx
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut input_rx = Self::spawn_input_pump(self.stop.clone());
        let signal = self.signal.clone();

        let mut redraw = true;
        // Frame pacing: a child that floods output raises `Signal` far more often
        // than a terminal can usefully repaint. Frames are coalesced to at most
        // one per `FRAME_MIN_INTERVAL`; input is never delayed by this, because
        // the select below still wakes for it immediately.
        let mut next_frame = Instant::now();

        loop {
            // 1. Housekeeping first, so a child flooding output cannot starve
            //    exit detection (all checks are non-blocking).
            if self.lease.poll_exit() {
                redraw = true;
            }

            if let Some(session) = self.session.as_mut() {
                match session.exited.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
            }

            // The float's shell is gone (the user typed `exit`, or it crashed):
            // give them a fresh prompt instead of a dead window. A failed exec
            // is backed off rather than retried every tick, and it must never
            // take the owner shell down with it — losing the user's session
            // because a floating shell could not start is the worst outcome here.
            if self.lease.tenant.is_dead {
                let now = Instant::now();
                let due = self.tenant_retry_after.is_none_or(|at| now >= at);

                if due {
                    let was_visible = self.lease.tenant_visible;
                    self.lease = Lease::new(self.caps.clone(), self.host_tx.clone());

                    match self.init_tenant() {
                        Ok(()) => {
                            self.lease.tenant_visible = was_visible;
                            self.tenant_scroll = 0;
                            self.link_press = None;
                            self.tenant_retry_after = None;
                            self.renderer.invalidate();
                            // The old child's pictures belong to a process that
                            // no longer exists; the new one starts from an empty
                            // image layer.
                            self.renderer.forget_images();
                        }
                        Err(_) => {
                            self.tenant_retry_after = Some(now + TENANT_RESPAWN_BACKOFF);
                        }
                    }
                    redraw = true;
                }
            }

            // The host's mouse mode, cursor shape and clipboard all follow the
            // focused child; doing it here means one place handles toggles,
            // resizes and child restarts.
            self.sync_host_mouse();
            self.sync_cursor_style();
            self.flush_host_effects();

            // 2. Paint only when something actually changed: the grid moved on
            //    (reader signal) or the pane layout/visibility did.
            if signal.take() {
                redraw = true;
            }

            let now = Instant::now();
            if redraw && now >= next_frame {
                self.draw()?;
                redraw = false;
                next_frame = now + FRAME_MIN_INTERVAL;
            }

            // How long the loop may sleep: until the next allowed frame if one
            // is pending, otherwise the coarse housekeeping tick.
            let sleep_for = if redraw {
                next_frame
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(IDLE_TICK_MS))
            } else {
                Duration::from_millis(IDLE_TICK_MS)
            };

            // 3. Sleep until there is work, never on a fixed frame timer.
            //    No `biased;` on purpose: input must not be able to starve
            //    repaints (a paste flood) nor repaints starve input (a child
            //    flooding output); both arms are cheap and resumable.
            tokio::select! {
                maybe_event = input_rx.recv() => {
                    let Some(first) = maybe_event else {
                        // input pump is gone: nothing can drive the UI anymore
                        break;
                    };

                    match self.handle_event(first) {
                        Flow::Quit => break,
                        Flow::Redraw => redraw = true,
                        Flow::Continue => {}
                    }

                    // Drain whatever else is already queued, so a paste or a
                    // mouse drag does not cost one frame per event.
                    let mut budget = INPUT_DRAIN_BUDGET;
                    while budget > 0 {
                        match input_rx.try_recv() {
                            Ok(event) => {
                                budget -= 1;
                                match self.handle_event(event) {
                                    Flow::Quit => return Ok(()),
                                    Flow::Redraw => redraw = true,
                                    Flow::Continue => {}
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }

                _ = signal.changed() => {
                    // Consume the flag here: the paint at the top of the next
                    // iteration already sees every byte parsed before this
                    // point, so leaving it set would cost a second, identical
                    // frame.
                    signal.take();
                    redraw = true;
                }

                _ = tokio::time::sleep(sleep_for) => {}
            }
        }

        Ok(())
    }
}

/// Hand a clicked hyperlink to the system's opener.
///
/// uncl is a terminal multiplexer without a browser: the thing that owns "open a
/// URL" on the machine is `xdg-open` / `open` / `start`, and that is what a
/// terminal emulator calls internally too. The URL is filtered by
/// [`allowed_url`] first — a child's output is untrusted input, and the scheme is
/// what decides whether a click is allowed to launch anything.
///
/// The child is reaped on a thread of its own: a click should not leave a zombie
/// behind, and uncl should not wait for a browser to start.
fn open_url(bytes: &[u8]) {
    let Some(url) = crate::app::term::links::allowed_url(bytes) else {
        return;
    };

    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        // `start` treats its first argument as the window title, hence the "".
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };

    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => eprintln!("uncl: could not open {url}: {err}"),
    }
}
