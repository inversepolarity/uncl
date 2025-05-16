pub mod input;
pub mod lease;
pub mod ui;
use anyhow::Result;

use ui::owner::Container;

pub async fn run() -> Result<()> {
    let mut uncl = Container::new();
    uncl.initialize_pty().await.unwrap();
    Ok(())
}
