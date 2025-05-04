mod app;
mod constants;
use anyhow::Result;
use tokio;

#[tokio::main]
async fn main() -> Result<()> {
    app::run().await.unwrap();
    Ok(())
}
