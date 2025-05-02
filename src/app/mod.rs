use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use ratatui::{Terminal, prelude::CrosstermBackend};

mod input;
mod ui;

use input::keyboard::handle_keyboard_input;
use input::mouse::handle_mouse;

use ui::overlay::Overlay;

use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};

use std::sync::mpsc;
use std::{io, thread};

use crate::constants::*;

pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut overlay = Overlay::new();
    let (tx, _): (mpsc::Sender<()>, mpsc::Receiver<()>) = mpsc::channel();
    let manager = GlobalHotKeyManager::new().expect("Failed to create hotkey manager");

    manager
        .register(TOGGLE_HOTKEY)
        .expect("Failed to register hotkey");

    thread::spawn(move || {
        for event in GlobalHotKeyEvent::receiver() {
            if event.id == TOGGLE_HOTKEY.id {
                let _ = tx.send(());
            }
        }
    });

    loop {
        let (term_width, term_height) = crossterm::terminal::size()?;

        {
            terminal.draw(|f| overlay.render(f))?;
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key_event) => {
                    if handle_keyboard_input(&mut overlay, key_event, (term_width, term_height)) {
                        break;
                    }
                }

                Event::Mouse(m) => handle_mouse(&mut overlay, m, (term_width, term_height)),

                Event::FocusGained => {
                    // Handle focus gained event if needed
                    // You can leave it empty if you don't need to do anything
                }

                Event::FocusLost => {
                    // Handle focus lost event if needed
                    // You can leave it empty if you don't need to do anything
                }

                Event::Paste(_) => {
                    // Handle paste event if needed
                    // You can leave it empty if you don't need to do anything
                }

                _ => {} // Add this as a fallback for any other events
            }
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}
