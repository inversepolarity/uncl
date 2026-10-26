use anyhow::Result;
use portable_pty::CommandBuilder;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Modifier, Style},
};

use bytes::Bytes;

use std::{
    io::{self, Stdout},
    sync::{Arc, RwLock},
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use vt100::{MouseProtocolMode, Screen};

use crate::app::input::keyboard::handle_keyboard_input;
use crate::app::input::mouse::handle_mouse;
use crate::app::lease::{Lease, new_parser};
use crate::app::pty::{self, PtySession};
use crate::app::signal::Signal;
use crate::constants::*;

use super::widget::{Cursor, PseudoTerminal};

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

pub struct Container {
    pub rect: Rect,
    pub size: Size,
    pub parser: Arc<RwLock<vt100::Parser>>,
    pub session: Option<PtySession>,
    pub lease: Lease,
    pub signal: Arc<Signal>,
}

impl Container {
    pub fn new() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT));

        Self {
            rect: Rect::new(0, 0, cols, rows),
            size: Size { cols, rows },
            parser: new_parser(),
            session: None,
            lease: Lease::new(),
            signal: Signal::new(),
        }
    }

    /// Spawn the floating shell and store its channels on the lease.
    fn init_tenant(&mut self) -> Result<()> {
        let parser = self.lease.tenant_parser.clone();
        let signal = self.signal.clone();
        let session = self.lease.tenant.spawn(parser, signal)?;
        self.lease.attach(session);
        Ok(())
    }

    fn tenant_running(&self) -> bool {
        self.lease.running()
    }

    pub async fn initialize_pty(&mut self) -> Result<(), anyhow::Error> {
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
        cmd.env("TERM", "xterm-256color");

        let session = pty::spawn(
            cmd,
            self.size.rows,
            self.size.cols,
            self.parser.clone(),
            self.signal.clone(),
        )?;
        self.session = Some(session);

        enable_raw_mode()?;

        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnterAlternateScreen
        )?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        let result = self.run(&mut terminal).await;

        // Restore the terminal exactly once, in one batch.
        disable_raw_mode()?;
        execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )?;

        result
    }

    /// Paint the whole screen. Both grids are cloned under their locks first, so
    /// the parser is never blocked for the duration of a frame.
    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let owner: Screen = self.parser.read().unwrap().screen().clone();
        let tenant: Screen = self.lease.tenant_parser.read().unwrap().screen().clone();
        let tenant_visible = self.lease.tenant_visible && self.tenant_running();

        terminal.draw(|f| self.render(f, &owner, &tenant, tenant_visible))?;
        Ok(())
    }

    pub fn render(
        &mut self,
        f: &mut Frame,
        owner: &Screen,
        tenant: &Screen,
        tenant_visible: bool,
    ) {
        let cursor = Cursor::default()
            .visibility(!tenant_visible)
            .style(Style::default().add_modifier(Modifier::RAPID_BLINK));

        f.render_widget(
            PseudoTerminal::new(owner).cursor(cursor),
            self.rect,
        );

        if tenant_visible {
            self.lease.tenant.render(f, tenant);
        }
    }

    /// Send bytes to whichever pane currently owns the keyboard.
    fn active_sender(&self) -> Option<UnboundedSender<Bytes>> {
        if self.lease.tenant_visible && self.tenant_running() {
            self.lease.tenant_tx.clone()
        } else {
            self.session.as_ref().map(|session| session.writer.clone())
        }
    }

    fn active_screen(&self) -> Option<std::sync::RwLockReadGuard<'_, vt100::Parser>> {
        if self.lease.tenant_visible && self.tenant_running() {
            self.lease.tenant_parser.read().ok()
        } else {
            self.parser.read().ok()
        }
    }

    fn forward_paste(&self, text: &str) {
        let Some(sender) = self.active_sender() else {
            return;
        };

        let bracketed = self
            .active_screen()
            .is_some_and(|parser| parser.screen().bracketed_paste());

        let mut payload = Vec::with_capacity(text.len() + 12);
        if bracketed {
            payload.extend_from_slice(b"\x1b[200~");
        }
        payload.extend_from_slice(text.as_bytes());
        if bracketed {
            payload.extend_from_slice(b"\x1b[201~");
        }

        let _ = sender.send(Bytes::from(payload));
    }

    /// Forward a mouse event to the owner shell when it asked for mouse
    /// reporting. Mode-awareness replaces the old hand-rolled byte sniffing;
    /// per-mode encoding (X10/UTF-8) is S1.
    fn forward_mouse(&self, m: MouseEvent) {
        let mouse_mode = self
            .parser
            .read()
            .unwrap()
            .screen()
            .mouse_protocol_mode();
        if matches!(mouse_mode, MouseProtocolMode::None) {
            return;
        }

        let (button_code, final_byte) = match m.kind {
            MouseEventKind::Down(MouseButton::Left) => (0, 'M'),
            MouseEventKind::Up(MouseButton::Left) => (0, 'm'),
            MouseEventKind::Down(MouseButton::Right) => (2, 'M'),
            MouseEventKind::Up(MouseButton::Right) => (2, 'm'),
            MouseEventKind::Down(MouseButton::Middle) => (1, 'M'),
            MouseEventKind::Up(MouseButton::Middle) => (1, 'm'),
            MouseEventKind::Drag(MouseButton::Left) => (32, 'M'),
            MouseEventKind::Drag(MouseButton::Right) => (34, 'M'),
            MouseEventKind::Drag(MouseButton::Middle) => (33, 'M'),
            MouseEventKind::ScrollUp => (64, 'M'),
            MouseEventKind::ScrollDown => (65, 'M'),
            _ => return,
        };

        let Some(sender) = self
            .session
            .as_ref()
            .map(|session| session.writer.clone())
        else {
            return;
        };

        let sequence = format!(
            "\x1b[<{};{};{}{}",
            button_code,
            m.column + 1,
            m.row + 1,
            final_byte
        );
        let _ = sender.send(Bytes::from(sequence.into_bytes()));
    }

    fn handle_event(&mut self, event: Event) -> Flow {
        // Cached, not queried: this used to be an ioctl per input event.
        let term_size = (self.size.cols, self.size.rows);

        match event {
            Event::Key(key) => {
                // Windows reports release/repeat events as well; sending all of
                // them made every keystroke fire twice there.
                if key.kind != KeyEventKind::Press {
                    return Flow::Continue;
                }

                if key.code == KeyCode::Home {
                    self.lease.tenant_visible = !self.lease.tenant_visible;
                    return Flow::Redraw;
                }

                let Some(sender) = self.active_sender() else {
                    return Flow::Continue;
                };

                let quit = handle_keyboard_input(
                    &mut self.lease,
                    &sender,
                    key,
                    term_size,
                );

                if quit { Flow::Quit } else { Flow::Continue }
            }

            Event::Mouse(m) => {
                if self.lease.tenant_visible {
                    handle_mouse(&mut self.lease, m, term_size);
                    Flow::Redraw
                } else {
                    self.forward_mouse(m);
                    Flow::Continue
                }
            }

            Event::Paste(text) => {
                self.forward_paste(&text);
                Flow::Continue
            }

            Event::Resize(cols, rows) => {
                self.size = Size { cols, rows };
                self.rect = Rect::new(0, 0, cols, rows);
                self.parser
                    .write()
                    .unwrap()
                    .set_size(nz(rows), nz(cols));
                // TODO(S1): move the float back inside the new bounds and
                // resize its child; today we only re-anchor the owner grid.
                Flow::Redraw
            }

            _ => Flow::Continue,
        }
    }

    /// Blocking crossterm reads run on their own thread so the event loop can
    /// `select!` over input, PTY output and a frame timer instead of sleeping.
    fn spawn_input_pump() -> UnboundedReceiver<Event> {
        let (tx, rx) = unbounded_channel::<Event>();
        tokio::task::spawn_blocking(move || {
            loop {
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

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let mut input_rx = Self::spawn_input_pump();
        let signal = self.signal.clone();
        let mut ticker = tokio::time::interval(Duration::from_millis(IDLE_TICK_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick is immediate

        let mut redraw = true;

        loop {
            if redraw {
                self.draw(terminal)?;
                redraw = false;
            }

            // Bytes may have landed while we were painting.
            if signal.take() {
                redraw = true;
                continue;
            }

            // No `biased;` on purpose: input must not be able to starve
            // repaints (a paste flood) nor repaints starve input (a child
            // flooding output), and both arms are cheap and resumable.
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
                    // mouse drag does not take one frame per event.
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
                    // Consume the flag here: the paint below already sees every
                    // byte that was parsed before this point, so leaving it set
                    // would cost a second, identical frame.
                    signal.take();
                    redraw = true;
                }

                _ = ticker.tick() => {}
            }

            // Child lifecycle (cheap, non-blocking; covers both panes).
            if self.lease.poll_exit() {
                self.lease.tenant_visible = false;
                redraw = true;
            }

            if let Some(session) = self.session.as_mut() {
                match session.exited.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
            }

            if self.lease.expired() {
                self.lease = self.lease.renew();
                self.init_tenant()?;
                redraw = true;
            }
        }

        Ok(())
    }
}
