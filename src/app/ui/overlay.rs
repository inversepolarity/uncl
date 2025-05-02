use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, block::Position},
};

use std::{
    io::Read,
    sync::{Arc, Mutex},
};

use std::thread;

pub struct Overlay {
    pub visible: bool,
    pub terminal_content: Arc<Mutex<String>>,
    pub terminal_buffer: Arc<Mutex<Vec<u8>>>,
    pub pty_master_writer: Option<Result<Box<dyn std::io::Write + Send>, anyhow::Error>>,
    pub child_process: Option<Box<dyn portable_pty::Child + Send>>,
    pub rect: Rect,
    pub dragging: bool,
    pub drag_offset: (u16, u16),
    pub resizing: bool,
    pub resize_direction: Option<ResizeDirection>,
}

use crate::constants::{self, *};

impl Overlay {
    pub fn new() -> Self {
        let mut overlay = Self {
            visible: true,
            terminal_content: Arc::new(Mutex::new(String::new())),
            terminal_buffer: Arc::new(Mutex::new(Vec::new())),
            pty_master_writer: None,
            child_process: None,
            rect: Rect::new(DEFAULT_X, DEFAULT_Y, DEFAULT_WIDTH, DEFAULT_HEIGHT),
            dragging: false,
            drag_offset: (0, 0),
            resizing: false,
            resize_direction: None,
        };

        if let Err(e) = overlay.initialize_pty() {
            eprintln!("Failed to initialize PTY: {}", e);
        }

        overlay
    }

    pub fn initialize_pty(&mut self) -> Result<(), anyhow::Error> {
        let pty_system = native_pty_system();

        //Get Terminal Size
        let term_size = match crossterm::terminal::size() {
            Ok((cols, rows)) => (cols, rows),
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
        let mut cmd = CommandBuilder::new(DEFAULT_SHELL);
        cmd.env("TERM", DEFAULT_TERM_TYPE);

        //Spawn the shell in pty
        let child = match slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => return Err(e.into()),
        };

        //Store the child process for cleanup
        self.child_process = Some(child);

        //Create a writer for sending input to pty
        let writer = master.take_writer();
        self.pty_master_writer = Some(writer);

        let buffer_clone = Arc::clone(&self.terminal_buffer);
        let content_clone = Arc::clone(&self.terminal_content);

        //Thread to read from the pty master and update overlay content
        thread::spawn(move || {
            let mut reader = match master.try_clone_reader() {
                Ok(reader) => reader,
                Err(e) => {
                    eprintln!("Failed to clone PTY reader: {}", e);
                    return;
                }
            };

            let mut buffer = [0u8; 1024];

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break, //EOF
                    Ok(n) => {
                        //Append new data to the buffer
                        if let Ok(mut buf) = buffer_clone.lock() {
                            buf.extend_from_slice(&buffer[..n]);

                            //Limit buffer size to prevent memory issues
                            if buf.len() > MAX_BUFFER_SIZE {
                                let new_start = buf.len() - MAX_BUFFER_SIZE;
                                *buf = buf[new_start..].to_vec();
                            }

                            //Process buffer for display
                            if let Ok(mut terminal_content) = content_clone.lock() {
                                //Convert entire buffer to string for handling utf8
                                let s = String::from_utf8_lossy(&buf).to_string();
                                //Keep only last MAX_BUFFER_LINES lines
                                let lines: Vec<&str> = s.lines().collect();
                                if lines.len() > MAX_BUFFER_LINES {
                                    *terminal_content =
                                        lines[lines.len() - MAX_BUFFER_LINES..].join("\n");
                                } else {
                                    *terminal_content = s.to_string();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reading from PTY: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    pub fn cleanup(&mut self) {
        //Kill the child if it exists
        if let Some(mut child) = self.child_process.take() {
            if let Err(e) = child.kill() {
                eprintln!("Faied to kill child process: {}", e);
            }
        }

        //Clear the writer
        self.pty_master_writer = None;

        //Clear buffers
        if let Ok(mut buf) = self.terminal_buffer.lock() {
            buf.clear();
        }

        if let Ok(mut content) = self.terminal_content.lock() {
            content.clear();
        }
    }

    pub fn render(&self, f: &mut Frame) {
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White).bg(Color::Reset))
            .title_position(Position::Bottom)
            .title_alignment(ratatui::layout::Alignment::Right)
            .title("uncl 0.1a");

        // Calculate the inner area inside the block (excluding borders)
        let inner_area = block.inner(self.rect);

        f.render_widget(block, self.rect);

        // Get terminal content
        if let Ok(content) = self.terminal_content.lock() {
            if !content.is_empty() {
                // Create a paragraph widget to display the terminal content
                let paragraph = ratatui::widgets::Paragraph::new(content.as_str())
                    .style(Style::default().fg(Color::Reset).bg(Color::Reset));

                // Render the paragraph inside the inner area of the block
                f.render_widget(paragraph, inner_area);
            }
        }
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
