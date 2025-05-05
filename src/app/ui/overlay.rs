use anyhow::Result;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    prelude::CrosstermBackend,
    style::{Color, Style, Stylize},
    widgets::{Block, Borders, Clear, block::Position},
};

use bytes::Bytes;

use std::{
    io::{self, BufWriter, Read, Write},
    sync::{Arc, RwLock},
};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    style::ResetColor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use tokio::{
    sync::mpsc::{Sender, channel},
    task::{self},
};

use tui_term::widget::PseudoTerminal;
use vt100::Screen;

pub struct Size {
    cols: u16,
    rows: u16,
}

pub struct Overlay {
    pub visible: bool,
    pub rect: Rect,
    pub dragging: bool,
    pub drag_offset: (u16, u16),
    pub resizing: bool,
    pub resize_direction: Option<ResizeDirection>,
    pub size: Size,
}

use crate::app::input::keyboard::handle_keyboard_input;
use crate::app::input::mouse::handle_mouse;
use crate::constants::{self, *};

impl Overlay {
    pub fn new() -> Self {
        let overlay = Self {
            visible: true,
            rect: Rect::new(DEFAULT_X, DEFAULT_Y, DEFAULT_WIDTH, DEFAULT_HEIGHT),
            dragging: false,
            drag_offset: (0, 0),
            resizing: false,
            resize_direction: None,
            size: Size { cols: 0, rows: 0 },
        };

        overlay
    }

    pub async fn initialize_pty(&mut self) -> Result<(), anyhow::Error> {
        let pty_system = native_pty_system();

        //Get Terminal Size
        let term_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => {
                self.size = Size { cols, rows };
                (cols, rows)
            }
            Err(e) => {
                eprintln!("Failed to get terminal size: {}", e);
                (constants::DEFAULT_WIDTH, constants::DEFAULT_HEIGHT)
            }
        };

        //Create pty pair
        let pair = match pty_system.openpty(PtySize {
            rows: term_size.1,
            cols: term_size.0,
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

        // Create channels for PTY status
        let (tx, mut rx) = channel::<Bytes>(32);
        let (pty_status_tx, mut pty_status_rx) = channel::<bool>(1);

        // Clone the status sender for the child process monitoring
        let child_status_tx = pty_status_tx.clone();

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

        let parser = Arc::new(RwLock::new(vt100::Parser::new(
            self.size.rows,
            self.size.cols,
            0,
        )));

        // Clone status sender for the reader task
        let reader_status_tx = pty_status_tx.clone();

        {
            let parser = parser.clone();
            task::spawn_blocking(move || {
                let mut buf = [0u8; 8192];
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
        let mut stdout = io::stdout();
        execute!(stdout, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

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
        self.run(&mut terminal, parser, tx, &mut pty_status_rx)
            .await?;

        // Restore terminal state
        disable_raw_mode()?;
        execute!(std::io::stdout(), DisableMouseCapture)?;
        terminal.show_cursor()?;
        Ok(())
    }

    pub fn cleanup(&mut self) {
        //Kill the child if it exists
    }

    pub async fn run<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        parser: Arc<RwLock<vt100::Parser>>,
        sender: Sender<Bytes>,
        pty_status_rx: &mut tokio::sync::mpsc::Receiver<bool>,
    ) -> Result<()> {
        loop {
            // Draw the terminal UI
            terminal.draw(|f| self.render(f, parser.read().unwrap().screen()))?;

            // Poll for terminal events with a short timeout
            if event::poll(std::time::Duration::from_millis(50))? {
                let (term_width, term_height) = crossterm::terminal::size()?;

                match event::read()? {
                    Event::Key(key_event) => {
                        if handle_keyboard_input(
                            self,
                            &sender,
                            key_event,
                            (term_width, term_height),
                        )
                        .await
                        {
                            // User pressed 'q' - exit
                            break;
                        }
                    }
                    Event::Mouse(m) => handle_mouse(self, m, (term_width, term_height)),
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
                // PTY process has ended, exit the loop
                break;
            }

            // Small sleep to prevent CPU spinning
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        Ok(())
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        // Create the terminal block with borders
        let block = Block::default()
            .borders(Borders::ALL)
            .title_position(Position::Bottom)
            .title_alignment(ratatui::layout::Alignment::Right)
            .title("uncl 0.1a")
            .style(Style::default().bg(Color::Black));

        let pseudo_term = PseudoTerminal::new(screen).block(block.clone());

        f.render_widget(pseudo_term, self.rect);

        f.render_widget(block.clone(), self.rect);
    }

    pub fn resize_to(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        bounds: (u16, u16),
    ) {
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
    }

    pub fn move_to(&mut self, target_x: u16, target_y: u16, bounds: (u16, u16)) {
        let max_x = bounds.0.saturating_sub(self.rect.width);
        let max_y = bounds.1.saturating_sub(self.rect.height);

        self.rect.x = target_x.min(max_x);
        self.rect.y = target_y.min(max_y);
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.cleanup();
    }
}
