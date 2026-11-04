pub mod clipboard;
pub mod images;
pub mod input;
pub mod lease;
pub mod pty;
pub mod render;
pub mod signal;
pub mod state;
pub mod term;
pub mod ui;
use anyhow::Result;

use ui::owner::Container;

pub async fn run(fresh: bool) -> Result<()> {
    let mut uncl = Container::new(fresh);
    uncl.initialize_pty().await
}
