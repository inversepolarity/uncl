pub mod images;
pub mod input;
pub mod lease;
pub mod pty;
pub mod render;
pub mod signal;
pub mod term;
pub mod ui;
use anyhow::Result;

use ui::owner::Container;

pub async fn run() -> Result<()> {
    let mut uncl = Container::new();
    uncl.initialize_pty().await
}
