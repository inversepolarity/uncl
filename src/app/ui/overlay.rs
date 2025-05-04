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

use bytes::Bytes;
use crossterm::event::{self, Event};

use std::{
    io::{BufWriter, Read, Write},
    sync::{Arc, RwLock},
};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    style::ResetColor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use tokio::{
    sync::mpsc::{Sender, channel},
    task::{self},
};

use std::io;
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

        //Spawn the shell in pty
        task::spawn_blocking(move || {
            let mut child = slave.spawn_command(cmd).unwrap();
            let _child_exit_status = child.wait().unwrap();
            drop(slave);
        });

        let mut writer = BufWriter::new(master.take_writer().unwrap());
        let mut reader = master.try_clone_reader().unwrap();

        let parser = Arc::new(RwLock::new(vt100::Parser::new(
            self.size.rows,
            self.size.cols,
            0,
        )));

        {
            let parser = parser.clone();
            task::spawn_blocking(move || {
                let mut buf = [0u8; 8192];
                let mut processed_buf = Vec::new();
                loop {
                    let size = reader.read(&mut buf).unwrap();
                    if size == 0 {
                        break;
                    }
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

        let mut stdout = io::stdout();
        execute!(stdout, ResetColor)?;
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let (tx, mut rx) = channel::<Bytes>(32);
        // Drop writer on purpose
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                writer.write_all(&bytes).unwrap();
                writer.flush().unwrap();
            }
            drop(master);
        });

        self.run(&mut terminal, parser, tx).await?;

        // restore terminal
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
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
    ) -> Result<()> {
        loop {
            let (term_width, term_height) = crossterm::terminal::size()?;

            terminal.draw(|f| self.render(f, parser.read().unwrap().screen()))?;

            if event::poll(std::time::Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key_event) => {
                        if handle_keyboard_input(
                            self,
                            &sender,
                            key_event,
                            (term_width, term_height),
                        )
                        .await
                        {}
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
        }
    }

    pub fn render(&mut self, f: &mut Frame, screen: &Screen) {
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White).bg(Color::Reset))
            .title_position(Position::Bottom)
            .title_alignment(ratatui::layout::Alignment::Right)
            .title("uncl 0.1a");

        // Calculate the inner area inside the block (excluding borders)
        // let inner_area = block.inner(self.rect);
        let pseudo_term = PseudoTerminal::new(screen).block(block);
        f.render_widget(pseudo_term, self.rect);
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
