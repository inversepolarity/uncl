use anyhow::Result;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    prelude::CrosstermBackend,
    style::{Color, Style},
    widgets::{Block, Borders, block::Position},
};

use std::io::{BufWriter, Read, Write};

use crossterm::{
    cursor::MoveTo,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute, queue,
    style::ResetColor,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};

use tokio::task::{self};

use tui_term::widget::PseudoTerminal;
use vt100::Screen;

use crate::app::lease::Lease;

use crate::constants::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, DEFAULT_X, DEFAULT_Y, MIN_HEIGHT, MIN_WIDTH, ResizeDirection,
};

pub struct Size {
    cols: u16,
    rows: u16,
}

pub struct Overlay {
    pub rect: Rect,
    pub dragging: bool,
    pub drag_offset: (u16, u16),
    pub resizing: bool,
    pub resize_direction: Option<ResizeDirection>,
    pub size: Size,
    pub is_dead: bool,
}

impl Overlay {
    pub fn new() -> Self {
        let overlay = Self {
            rect: Rect::new(DEFAULT_X, DEFAULT_Y, DEFAULT_WIDTH, DEFAULT_HEIGHT),
            dragging: false,
            drag_offset: (0, 0),
            resizing: false,
            resize_direction: None,
            size: Size {
                cols: DEFAULT_WIDTH,
                rows: DEFAULT_HEIGHT,
            },
            is_dead: true,
        };

        overlay
    }

    pub async fn initialize_pty(&mut self, lease: &mut Lease) -> Result<(), anyhow::Error> {
        let pty_system = native_pty_system();
        //Create pty pair
        let pair = match pty_system.openpty(PtySize {
            rows: self.size.rows - 4,
            cols: self.size.cols - 4,
            pixel_height: 0,
            pixel_width: 0,
        }) {
            Ok(pair) => pair,
            Err(e) => return Err(e.into()),
        };

        //Get pty master/slave
        let master = pair.master;
        let slave = pair.slave;

        // Create a channel for resize operations
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(10);

        // Set the resize sender in the lease
        lease.set_resize_sender(resize_tx);

        //Prepare the shell command
        let mut cmd = CommandBuilder::new_default_prog();
        let cwd = std::env::current_dir().unwrap();
        cmd.cwd(cwd);

        // Clone the status sender for the child process monitoring
        let child_status_tx = lease.tenant_status_tx.clone();
        let resize_status_tx = lease.tenant_status_tx.clone();

        let mut writer = BufWriter::new(master.take_writer().unwrap());
        let mut reader = master.try_clone_reader().unwrap();

        task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                while let Some((rows, cols)) = resize_rx.recv().await {
                    if let Err(e) = master.resize(PtySize {
                        rows: rows - 4,
                        cols: cols - 4,
                        pixel_height: 0,
                        pixel_width: 0,
                    }) {
                        eprintln!("Failed to resize PTY: {}", e);
                        // Optionally signal error
                        let _ = resize_status_tx.send(true).await;
                        break;
                    }
                }
            });
            drop(master);
        });

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

        // Clone status sender for the reader task
        let reader_status_tx = lease.tenant_status_tx.clone();
        {
            let parser = lease.tenant_parser.clone();
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
        let backend = CrosstermBackend::new(std::io::stdout());
        let mut terminal = Terminal::new(backend)?;

        let mut rx = lease.tenant_rx.take().unwrap();

        let stdout = terminal.backend_mut();
        execute!(stdout, ResetColor)?;

        enable_raw_mode()?;

        execute!(stdout, EnableMouseCapture)?;

        // Critical: Clear terminal buffer before displaying anything
        queue!(stdout, ResetColor, Clear(ClearType::All), MoveTo(0, 0))?;
        std::io::Write::flush(stdout)?;

        terminal.clear()?;

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
        });

        self.is_dead = false;

        // Restore terminal state
        disable_raw_mode()?;
        execute!(std::io::stdout(), DisableMouseCapture)?;
        terminal.show_cursor()?;
        Ok(())
    }

    pub fn cleanup<B: Backend + std::io::Write>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()> {
        // Properly clean up terminal state before returning to owner
        // This is crucial to prevent control characters and input issues
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            DisableMouseCapture,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;

        terminal.clear()?;
        Ok(())
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        let t = format!("s:{}:{}", self.size.rows, self.size.cols);
        let block = Block::default()
            .borders(Borders::ALL)
            .title_position(Position::Bottom)
            .title_alignment(ratatui::layout::Alignment::Right)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Color::Green)
            .title(t)
            .style(Style::default().bg(Color::Reset));
        let pseudo_term = PseudoTerminal::new(screen).block(block.clone()).cursor(
            tui_term::widget::Cursor::default().style(
                ratatui::style::Style::default()
                    .add_modifier(ratatui::style::Modifier::RAPID_BLINK),
            ),
        );

        let inner = block.inner(self.rect);
        f.render_widget(pseudo_term, inner);
        f.render_widget(block.clone(), inner);
    }

    pub fn resize_to(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        bounds: (u16, u16),
    ) {
        //FIX: there are more ways to resize than handled here
        // Ignore any resize attempts that fall below the minimum constraints
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            return;
        }

        // Safeguard: Ensure x and y are within bounds (can't move beyond the bounds of the screen)
        x = x.max(0).min(bounds.0.saturating_sub(1)); // Prevent x from exceeding bounds width
        y = y.max(0).min(bounds.1.saturating_sub(1)); // Prevent y from exceeding bounds height

        // Calculate the max width and height that are available for resizing
        let max_width = bounds.0.saturating_sub(x);
        let max_height = bounds.1.saturating_sub(y);

        // Safeguard against overflow by ensuring we do not resize past bounds or minimum sizes
        width = width.min(max_width).max(MIN_WIDTH);
        height = height.min(max_height).max(MIN_HEIGHT);

        // Ensure the x and y positions are within bounds based on the new size
        // This ensures the new window does not go out of bounds when resizing
        if x > bounds.0.saturating_sub(width) {
            x = bounds.0.saturating_sub(width); // Prevent x from going past bounds
        }
        if y > bounds.1.saturating_sub(height) {
            y = bounds.1.saturating_sub(height); // Prevent y from going past bounds
        }

        // Set the new window dimensions (x, y, width, height)
        self.rect.x = x;
        self.rect.y = y;
        self.rect.width = width;
        self.rect.height = height;
        self.size.cols = width;
        self.size.rows = height;
    }

    pub fn move_to(&mut self, target_x: u16, target_y: u16, bounds: (u16, u16)) {
        let max_x = bounds.0.saturating_sub(self.rect.width);
        let max_y = bounds.1.saturating_sub(self.rect.height);

        self.rect.x = target_x.min(max_x);
        self.rect.y = target_y.min(max_y);
    }
}
