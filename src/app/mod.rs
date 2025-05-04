pub mod input;
pub mod ui;
use anyhow::Result;

use ui::overlay::Overlay;

pub async fn run() -> Result<()> {
    let mut overlay = Overlay::new();
    overlay.initialize_pty().await.unwrap();

    Ok(())
}
