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
    sync::{Arc, RwLock},
};

use crossterm::{
    cursor,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    style::ResetColor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
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
use crate::constants::*;

use super::tenant::Overlay;

pub struct Lease {
    pub tenant: Overlay,
    pub tenant_parser: Arc<RwLock<vt100::Parser>>,
    pub tenant_visible: bool,
    pub tenant_tx: Sender<Bytes>,
    pub tenant_rx: Option<Receiver<Bytes>>,
    pub tenant_status_tx: Sender<bool>,
    pub tenant_status_rx: Receiver<bool>,
}

pub struct Container {
    pub rect: Rect,
    pub size: Size,
    pub parser: Arc<RwLock<vt100::Parser>>,
    pub tx: Sender<Bytes>,
    pub rx: Option<Receiver<Bytes>>,
    pub status_tx: Sender<bool>,
    pub status_rx: Option<Receiver<bool>>,
    pub lease: Lease,
}

impl Container {
    pub fn new() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT));

        let rect = Rect::new(0, 0, cols, rows);

        // FIX: we want to scroll back to start of the owner
        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 0)));
        let tparser = Arc::new(RwLock::new(vt100::Parser::new(
            DEFAULT_HEIGHT,
            DEFAULT_WIDTH,
            0,
        )));

        // Create channels for PTY status
        let (tx, rx) = channel::<Bytes>(32);
        let (pty_status_tx, pty_status_rx) = channel::<bool>(1);

        let (ttx, trx) = channel::<Bytes>(32);
        let (tpty_status_tx, tpty_status_rx) = channel::<bool>(1);

        let lease = Lease {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: tparser,
            tenant_tx: ttx,
            tenant_rx: Some(trx),
            tenant_status_tx: tpty_status_tx,
            tenant_status_rx: tpty_status_rx,
        };

        let container = Self {
            rect,
            parser,
            size: Size { cols, rows },
            tx,
            rx: Some(rx),
            status_tx: pty_status_tx,
            status_rx: Some(pty_status_rx),
            lease,
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

        //Get pty master/slave
        let master = pair.master;
        let slave = pair.slave;

        //Prepare the shell command
        let mut cmd = CommandBuilder::new_default_prog();
        let cwd = std::env::current_dir().unwrap();
        cmd.cwd(cwd);

        // Clone the status sender for the child process monitoring
        let child_status_tx = self.status_tx.clone();

        //Spawn the shell in pty and monitor for exit
        // TODO: why blocking?
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

            // TODO: why blocking?
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

        enable_raw_mode()?;

        //TODO: enable cursor::SetCursorStyle
        execute!(
            stdout,
            EnableMouseCapture,
            EnterAlternateScreen,
            cursor::EnableBlinking,
            cursor::SetCursorStyle::BlinkingBlock
        )?;

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
        disable_raw_mode()?;
        execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        // Create the terminal block with borders

        let block = Block::default().borders(Borders::NONE);
        let pseudo_term_owner = PseudoTerminal::new(screen).block(block.clone());
        let inner = block.inner(self.rect);
        f.render_widget(pseudo_term_owner, inner);
        f.render_widget(block.clone(), inner);

        if self.lease.tenant_visible && self.tenant_running() {
            self.lease
                .tenant
                .render(f, self.lease.tenant_parser.read().unwrap().screen());
        }
    }

    pub async fn run<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        parser: Arc<RwLock<vt100::Parser>>,
        pty_status_rx: &mut tokio::sync::mpsc::Receiver<bool>,
    ) -> Result<()> {
        terminal.clear()?;

        loop {
            // Draw the terminal UI

            terminal.draw(|f| self.render(f, parser.read().unwrap().screen()))?;

            let kb_sender: Sender<Bytes>;

            if !self.lease.tenant_visible {
                kb_sender = self.tx.clone();
            } else {
                kb_sender = self.lease.tenant_tx.clone();
            }

            // Poll for terminal events with a short timeout
            if event::poll(std::time::Duration::from_millis(50))? {
                let (term_width, term_height) = crossterm::terminal::size()?;

                match event::read()? {
                    Event::Key(key_event) => {
                        if handle_keyboard_input(
                            &mut self.lease,
                            &kb_sender,
                            key_event,
                            (term_width, term_height),
                        )
                        .await
                        {
                            // exit
                            break;
                        }
                    }
                    Event::Mouse(m) => {
                        handle_mouse(&mut self.lease.tenant, m, (term_width, term_height))
                    }
                    Event::FocusGained => {}
                    Event::FocusLost => {}
                    Event::Paste(_) => {}
                    Event::Resize(cols, rows) => {
                        parser.write().unwrap().set_size(rows, cols);
                    }
                };
            }

            // Check if the PTY process has ended (non-blocking)
            if let Ok(true) = pty_status_rx.try_recv() {
                break;
            }

            // Small sleep to prevent CPU spinning
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Ok(())
    }
}
