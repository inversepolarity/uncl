use anyhow::Result;
use portable_pty::CommandBuilder;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Position, Rect},
};

use bytes::Bytes;

use std::{
    io::{self, Stdout, Write},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use vt100::{CursorStyle, MouseProtocolMode, Screen};

use crate::app::input::encode::{ChildModes, encode_key, encode_mouse};
use crate::app::input::mouse::{self, MouseRoute};
use crate::app::lease::{Lease, new_parser};
use crate::app::pty::{self, ChildParser, PtySession};
use crate::app::signal::Signal;
use crate::app::term::caps::Capabilities;
use crate::app::term::callbacks::ReplySink;
use crate::constants::*;

use super::widget::PseudoTerminal;

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
}

impl Container {
    pub fn new() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT));

        let caps = Arc::new(Capabilities::from_env());
        let (host_tx, host_rx) = unbounded_channel::<Vec<u8>>();
        let (parser, parser_sink) = new_parser(caps.clone(), host_tx.clone());

        Self {
            rect: Rect::new(0, 0, cols, rows),
            size: Size { cols, rows },
            caps: caps.clone(),
            parser,
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
        }
    }

    /// Spawn the floating shell and store its session on the lease.
    fn init_tenant(&mut self) -> Result<()> {
        let parser = self.lease.tenant_parser.clone();
        let sink = self.lease.tenant_sink.clone();
        let signal = self.signal.clone();
        let session = self.lease.tenant.spawn(parser, sink, signal, &self.caps)?;
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
        )?;
        self.session = Some(session);

        // Engage the guard *before* the first state change it has to undo, and
        // declare it before `terminal` so it is dropped after the terminal.
        enable_raw_mode()?;
        let _guard = TerminalGuard::engage();

        // Mouse reporting is *not* enabled here: it is driven per focused
        // child by `sync_host_mouse`, which is what stops a `?1003`-less child
        // from drowning us in motion events (see `HostMouse`).
        execute!(io::stdout(), EnableBracketedPaste, EnterAlternateScreen)?;
        self.sync_host_mouse();

        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;

        let result = self.run(&mut terminal).await;

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

    /// Paint the whole screen. Both grids are cloned under their locks first, so
    /// the parser is never blocked for the duration of a frame.
    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let owner: Screen = self.parser.read().unwrap().screen().clone();
        let tenant: Screen = self.lease.tenant_parser.read().unwrap().screen().clone();
        let tenant_visible = self.lease.tenant_visible && self.tenant_running();

        terminal.draw(|f| {
            f.render_widget(PseudoTerminal::new(&owner), self.rect);

            if tenant_visible {
                let content = self.lease.tenant.content_rect();
                self.lease.tenant.render(f, &tenant);
                place_cursor(f, &tenant, content);
            } else {
                place_cursor(f, &owner, self.rect);
            }
        })?;
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
        let mut desired = HostMouse::for_modes(self.focused_modes());
        if self.float_focused() {
            desired = desired.at_least_press();
        }

        if desired == self.host_mouse {
            return;
        }
        self.host_mouse = desired;

        let mut stdout = io::stdout();
        let _ = stdout.write_all(desired.sequence());
        let _ = stdout.flush();
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

        let code = decscusr(style);
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
            }
            // Children that asked for `?1004` must see the focus move, or every
            // TUI in the float keeps painting its "unfocused" cursor style.
            self.refocus(self.float_focused());
            return Flow::Redraw;
        }

        let term_size = (self.size.cols, self.size.rows);

        // Float geometry: Alt+arrows move, Alt+Shift+arrows resize.
        if self.float_focused() && self.lease.tenant.geometry_key(key, term_size) {
            self.lease.sync_size();
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
                if let Some(bytes) = encode_mouse(m.kind, m.modifiers, col, row, content, modes) {
                    if let Some(sender) = self.lease.writer() {
                        let _ = sender.send(Bytes::from(bytes));
                    }
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
                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    mouse::begin(&mut self.lease.tenant, m);
                } else if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) {
                    if mouse::end(&mut self.lease.tenant) {
                        // A finished resize changes the child's grid size.
                        self.lease.sync_size();
                    }
                } else if mouse::drag(&mut self.lease.tenant, m, term_size) {
                    // Live-resize the child while the pointer moves, so a
                    // full-screen app reflows during the drag.
                    self.lease.sync_size();
                }
                Flow::Redraw
            }

            MouseRoute::HideFloat => {
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.lease.tenant_visible = false;
                    self.lease.tenant.child_grab = false;
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
        if !modes.wants_mouse() {
            return Flow::Continue;
        }

        if let Some(bytes) = encode_mouse(m.kind, m.modifiers, m.column, m.row, area, modes) {
            if let Some(session) = &self.session {
                let _ = session.writer.send(Bytes::from(bytes));
            }
        }

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

    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut input_rx = Self::spawn_input_pump(self.stop.clone());
        let signal = self.signal.clone();
        let mut ticker = tokio::time::interval(Duration::from_millis(IDLE_TICK_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick is immediate

        let mut redraw = true;

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
                            self.tenant_retry_after = None;
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
            let parsed = signal.take();
            if redraw || parsed {
                self.draw(terminal)?;
                redraw = false;
            }

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

                _ = ticker.tick() => {}
            }
        }

        Ok(())
    }
}

/// Put the *real* terminal cursor where the focused child's cursor is.
///
/// This is what restores a blinking cursor: the hardware cursor blinks with the
/// user's own settings and takes the shape their terminal is configured for,
/// which a painted cell can never do. When the child hides its cursor (BTOP
/// does) nothing is placed and ratatui keeps it hidden.
/// `DECSCUSR` parameter for a `vt100` cursor style.
fn decscusr(style: CursorStyle) -> u16 {
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

fn place_cursor(f: &mut Frame, screen: &Screen, area: Rect) {
    if screen.hide_cursor() {
        return;
    }

    let (row, col) = screen.cursor_position();
    if row >= area.height || col >= area.width {
        return;
    }

    let x = area.x.saturating_add(col);
    let y = area.y.saturating_add(row);
    let bounds = f.area();
    if x >= bounds.right() || y >= bounds.bottom() {
        return;
    }

    f.set_cursor_position(Position::new(x, y));
}
