use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;

mod input;
mod ui;

use input::keyboard::handle_keyboard_input;
use input::mouse::handle_mouse;

use ui::overlay::Overlay;

pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut overlay = Overlay::new();

    loop {
        let (term_width, term_height) = crossterm::terminal::size()?;

        terminal.draw(|f| overlay.render(f))?;

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
