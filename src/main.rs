mod app;
mod constants;

use anyhow::Result;
use std::time::Duration;

fn main() -> Result<()> {
    // A hand-built runtime (instead of `#[tokio::main]`) so we can bound how
    // long shutdown waits for the blocking pty reader and input pump threads.
    // They end when their children die, but if one is wedged we would rather
    // exit than leave the user's terminal in raw mode.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(app::run());

    // The terminal has already been restored by the guard inside `app::run`.
    runtime.shutdown_timeout(Duration::from_millis(250));

    result
}
