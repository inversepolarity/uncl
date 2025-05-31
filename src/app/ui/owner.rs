use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::Rect,
    widgets::{Block, Borders},
};

use bytes::Bytes;

use std::{
    io::{self, BufWriter, Read, Write},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crossterm::{
    cursor::MoveTo,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, MouseButton, MouseEventKind, poll, read,
    },
    execute, queue,
    style::ResetColor,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use tokio::{
    sync::mpsc::{Receiver, Sender, channel},
    task::{self},
};

use tui_term::widget::PseudoTerminal;
use vt100::Screen;

pub struct Size {
    cols: u16,
    rows: u16,
}

use crate::app::input::keyboard::handle_keyboard_input;
use crate::app::input::mouse::handle_mouse;
use crate::app::lease::Lease;
use crate::constants::*;

use super::tenant::Overlay;

pub struct Container {
    pub rect: Rect,
    pub size: Size,
    pub parser: Arc<RwLock<vt100::Parser>>,
    pub tx: Sender<Bytes>,
    pub rx: Option<Receiver<Bytes>>,
    pub status_tx: Sender<bool>,
    pub status_rx: Option<Receiver<bool>>,
    pub lease: Lease,
    pub mouse_mode_enabled: Arc<AtomicBool>,
}

impl Container {
    pub fn new() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT));

        let rect = Rect::new(0, 0, cols, rows);

        // FIX: we want to scroll back to start of the owner
        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 0)));

        // Create channels for PTY status
        let (tx, rx) = channel::<Bytes>(32);
        let (pty_status_tx, pty_status_rx) = channel::<bool>(1);

        let lease = Lease::new();
        let mouse_mode_enabled = Arc::new(AtomicBool::new(false));
        let container = Self {
            rect,
            parser,
            size: Size { cols, rows },
            tx,
            rx: Some(rx),
            status_tx: pty_status_tx,
            status_rx: Some(pty_status_rx),
            lease,
            mouse_mode_enabled,
        };

        container
    }

    pub async fn init_tenant(&mut self) -> Result<(), anyhow::Error> {
        let lease = &mut self.lease;
        let tenant_ptr: *mut Overlay = &mut lease.tenant;
        unsafe {
            (*tenant_ptr).initialize_pty(lease).await.unwrap();
        }

        Ok(())
    }

    pub fn tenant_running(&mut self) -> bool {
        !self.lease.tenant_status_rx.is_closed()
    }

    pub async fn initialize_pty(&mut self) -> Result<(), anyhow::Error> {
        self.init_tenant().await?;

        let pty_system = native_pty_system();
        //Create pty pair
        let pair = match pty_system.openpty(PtySize {
            rows: self.size.rows,
            cols: self.size.cols,
            pixel_height: 0,
            pixel_width: 0,
        }) {
            Ok(pair) => pair,
            Err(e) => return Err(e.into()),
        };

        enable_raw_mode()?;
        //Get pty master/slave
        let master = pair.master;
        let slave = pair.slave;

        //Prepare the shell command
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(shell);
        let cwd = std::env::current_dir().unwrap();
        cmd.args(&["-y", "-i", "--login"]);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");

        // Clone the status sender for the child process monitoring
        let child_status_tx = self.status_tx.clone();

        //Spawn the shell in pty and monitor for exit
        task::spawn_blocking(move || {
            let mut child = match slave.spawn_command(cmd) {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("Failed to spawn command: {}", e);
                    // Signal that the PTY process failed to start
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        let _ = child_status_tx.send(true).await;
                    });
                    return;
                }
            };

            // Wait for the child process to exit
            let _exit_status = child.wait().unwrap();
            drop(slave);

            // Signal that the PTY process has exited
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let _ = child_status_tx.send(true).await;
            });
        });

        let mut writer = BufWriter::new(master.take_writer().unwrap());
        let mut reader = master.try_clone_reader().unwrap();

        // Clone status sender for the reader task
        let reader_status_tx = self.status_tx.clone();

        {
            let parser = self.parser.clone();
            let mouse_tracker = self.mouse_mode_enabled.clone();

            task::spawn_blocking(move || {
                let mut buf = [0u8; 8192];
                // TODO: magic number?
                let mut processed_buf = Vec::new();
                loop {
                    // Handle read errors or EOF
                    let size = match reader.read(&mut buf) {
                        Ok(0) => {
                            // EOF detected - terminal process ended
                            let rt = tokio::runtime::Handle::current();
                            rt.block_on(async {
                                let _ = reader_status_tx.send(true).await;
                            });
                            break;
                        }
                        Ok(size) => size,
                        Err(e) => {
                            eprintln!("Read error: {}", e);
                            // Signal error
                            let rt = tokio::runtime::Handle::current();
                            rt.block_on(async {
                                let _ = reader_status_tx.send(true).await;
                            });
                            break;
                        }
                    };

                    if size > 0 {
                        processed_buf.extend_from_slice(&buf[..size]);

                        let data_str = String::from_utf8_lossy(&processed_buf);

                        // Check for mouse mode ENABLE sequences (more comprehensive)
                        if data_str.contains("\x1b[?1000h") ||  // VT200 mouse tracking
                        data_str.contains("\x1b[?1002h") ||  // VT200 button event mouse tracking
                        data_str.contains("\x1b[?1003h") ||  // VT200 any event mouse tracking  
                        data_str.contains("\x1b[?1006h") ||  // SGR mouse mode
                        data_str.contains("\x1b[?1015h") ||  // URXVT mouse mode
                        data_str.contains("\x1b[?9h") ||     // X10 mouse tracking
                        data_str.contains("\x1b[?1005h") ||  // UTF-8 mouse mode
                        data_str.contains("\x1b[?1004h")
                        {
                            // Focus events (often used with mouse)
                            mouse_tracker.store(true, Ordering::Relaxed);
                        }

                        // Check for mouse mode DISABLE sequences
                        if data_str.contains("\x1b[?1000l")
                            || data_str.contains("\x1b[?1002l")
                            || data_str.contains("\x1b[?1003l")
                            || data_str.contains("\x1b[?1006l")
                            || data_str.contains("\x1b[?1015l")
                            || data_str.contains("\x1b[?9l")
                            || data_str.contains("\x1b[?1005l")
                            || data_str.contains("\x1b[?1004l")
                        {
                            mouse_tracker.store(false, Ordering::Relaxed);
                        }

                        // Additional check: Look for DECSET sequences that might indicate mouse capability
                        if data_str.contains("\x1b[?47h") ||    // Alternate screen buffer (often used with mouse apps)
                        data_str.contains("\x1b[?1047h") ||   // Alternate screen buffer
                        data_str.contains("\x1b[?1049h")
                        {
                            // Alternate screen buffer + cursor save
                            // Many mouse-capable apps use alternate screen, so enable mouse preemptively
                            // but only if we're in a terminal that likely supports it
                            if std::env::var("TERM").unwrap_or_default().contains("xterm")
                                || std::env::var("TERM").unwrap_or_default().contains("screen")
                            {
                                mouse_tracker.store(true, Ordering::Relaxed);
                            }
                        }

                        // Check for alternate screen disable (often means mouse apps are exiting)
                        if data_str.contains("\x1b[?47l")
                            || data_str.contains("\x1b[?1047l")
                            || data_str.contains("\x1b[?1049l")
                        {
                            mouse_tracker.store(false, Ordering::Relaxed);
                        }
                        let mut parser = parser.write().unwrap();
                        parser.process(&processed_buf);
                        // Clear the processed portion of the buffer
                        processed_buf.clear();
                    }
                }
            });
        }

        // Set up terminal
        let mut stdout = io::stdout();
        execute!(stdout, ResetColor)?;
        execute!(stdout, EnableMouseCapture, EnterAlternateScreen,)?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut rx = self.rx.take().expect("rx already taken");
        let mut status_rx = self.status_rx.take().expect("status rx already taken");

        // Handle writing to PTY with error detection
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if let Err(e) = writer.write_all(&bytes) {
                    eprintln!("Write error: {}", e);
                    break;
                }
                if let Err(e) = writer.flush() {
                    eprintln!("Flush error: {}", e);
                    break;
                }
            }
            // Clean up resources
            drop(writer);
            drop(master);
        });

        // Run the terminal UI with PTY status monitoring
        self.run(&mut terminal, self.parser.clone(), &mut status_rx)
            .await?;

        // Restore terminal state
        //if self.tenant_running() {
        // TODO: kill tenant processes here
        //}

        disable_raw_mode()?;
        execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
        Ok(())
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        let block = Block::default().borders(Borders::NONE);
        let pseudo_term_owner = PseudoTerminal::new(screen).block(block.clone()).cursor(
            tui_term::widget::Cursor::default()
                .visibility(!self.lease.tenant_visible)
                .style(
                    ratatui::style::Style::default()
                        .add_modifier(ratatui::style::Modifier::RAPID_BLINK),
                ),
        );

        let inner = block.inner(self.rect);
        f.render_widget(pseudo_term_owner, inner);
        f.render_widget(block.clone(), inner);
        if self.lease.tenant_visible && self.tenant_running() {
            self.lease
                .tenant
                .render(f, self.lease.tenant_parser.read().unwrap().screen());
        }
    }

    pub async fn run<B: Backend + std::io::Write>(
        &mut self,
        terminal: &mut Terminal<B>,
        parser: Arc<RwLock<vt100::Parser>>,
        pty_status_rx: &mut tokio::sync::mpsc::Receiver<bool>,
    ) -> Result<()> {
        let mut stdout = io::stdout();
        queue!(stdout, ResetColor, Clear(ClearType::All), MoveTo(0, 0))?;
        stdout.flush()?;
        terminal.clear()?;
        terminal.flush()?;

        loop {
            let mut sender: Sender<Bytes> = self.tx.clone();

            if self.lease.tenant_visible {
                if self.tenant_running() {
                    sender = self.lease.tenant_tx.clone();
                } else {
                    // Important: If tenant is visible but not running, reset state
                    self.lease.tenant_visible = false;
                }
            }

            // Poll for terminal events with a short timeout
            if poll(std::time::Duration::from_millis(0))? {
                let (term_width, term_height) = crossterm::terminal::size()?;

                match read()? {
                    Event::Key(key_event) => {
                        if handle_keyboard_input(
                            &mut self.lease,
                            &sender,
                            key_event,
                            (term_width, term_height),
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Event::Mouse(m) => {
                        if !self.lease.tenant_visible {
                            // Only send mouse events if application has enabled mouse mode
                            if self.mouse_mode_enabled.load(Ordering::Relaxed) {
                                let (button_code, action) = match m.kind {
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
                                    _ => (-1, ' '),
                                };

                                if button_code >= 0 {
                                    let mouse_sequence = format!(
                                        "\x1b[<{};{};{}{}",
                                        button_code,
                                        m.column + 1,
                                        m.row + 1,
                                        action
                                    );

                                    let bytes = Bytes::from(mouse_sequence.into_bytes());
                                    if let Err(e) = sender.try_send(bytes) {
                                        eprintln!("Failed to send mouse event: {}", e);
                                    }
                                }
                            }
                            // If mouse mode not enabled, ignore mouse events completely
                        } else {
                            handle_mouse(&mut self.lease, m, (term_width, term_height)).await;
                        }
                    }
                    Event::FocusGained => {}
                    Event::FocusLost => {}
                    Event::Paste(_) => {}
                    Event::Resize(cols, rows) => {
                        //TODO: fix
                        parser.write().unwrap().set_size(rows, cols);
                        if self.lease.tenant_visible {
                            self.lease
                                .resize_screen(
                                    self.lease.tenant.rect.height,
                                    self.lease.tenant.rect.width,
                                )
                                .await;
                        }
                    }
                };
            }

            // Check if the PTY process has ended (non-blocking)
            if let Ok(true) = pty_status_rx.try_recv() {
                break;
            }

            if let Ok(true) = self.lease.tenant_status_rx.try_recv() {
                self.lease.tenant_visible = false;
                self.lease.tenant.is_dead = true;
            }

            if self.lease.expired() {
                self.lease.tenant.cleanup(terminal)?;
                self.lease = self.lease.renew();
                self.init_tenant().await?;
                enable_raw_mode()?;
                execute!(stdout, EnableMouseCapture)?;
            }

            // Small sleep to prevent CPU spinning
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            terminal.draw(|f| self.render(f, parser.read().unwrap().screen()))?;
        }

        Ok(())
    }
}
