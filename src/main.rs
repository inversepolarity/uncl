mod app;
mod constants;
mod geometry;

use anyhow::Result;
use std::time::Duration;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => run(),
        Some("--doctor") => {
            doctor();
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("uncl {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h") => {
            help();
            Ok(())
        }
        Some(other) => {
            eprintln!("uncl: unknown argument `{other}`\n");
            help();
            std::process::exit(2);
        }
    }
}

/// Refuse to run on a console that would mangle every frame.
///
/// uncl writes raw ANSI to stdout (S2 owns the emitter). A legacy Windows console
/// needs `ENABLE_VIRTUAL_TERMINAL_PROCESSING` before it will honour any of it, and
/// crossterm's `ansi_support` module is the thing that turns it on and reports
/// whether the console can do it at all. Failing here with a sentence is strictly
/// better than painting an alternate screen full of `ESC[?25l`.
///
/// Only compiled on Windows: everywhere else the answer is a terminal that speaks
/// ANSI or is not worth supporting.
#[cfg(windows)]
fn ensure_ansi() -> Result<()> {
    if !crossterm::ansi_support::supports_ansi() {
        return Err(anyhow::anyhow!(
            "uncl needs a console with ANSI/VT support (Windows 10 1511+, or Windows Terminal).\n\
             ConPTY on this system cannot render uncl's frames."
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn ensure_ansi() -> Result<()> {
    Ok(())
}

fn run() -> Result<()> {
    ensure_ansi()?;

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

/// What uncl tells its children about the terminal, and why.
fn doctor() {
    // Measure the tty first, so the report shows the cell size children would
    // actually be given rather than the fallback.
    app::term::caps::probe_cell_px();
    let caps = app::term::caps::Capabilities::from_env();
    println!("uncl {} — terminal capabilities\n", env!("CARGO_PKG_VERSION"));
    println!("{}", caps.summary());
    println!();
    println!(
        "children inherit: TERM={} COLORTERM={} TERM_PROGRAM=uncl",
        caps.child_term(),
        if caps.truecolor { "truecolor" } else { "<unset>" }
    );
    println!();
    println!("notes:");
    println!("  * host TERM is not passed through: a sixel/kitty TERM would make children");
    println!("    emit bytes this layer may not transport; UNCL_GRAPHICS says what it does");
    println!("  * sixel is advertised only when the DCS tap is enabled (UNCL_SIXEL=1 + a");
    println!("    host known to render it); see docs/rendering-report.md");
    println!("  * kitty keyboard protocol is never advertised");
}

fn help() {
    println!(
        "uncl {}\n\
         a terminal monoplexer: one floating terminal window over your shell\n\n\
         usage: uncl [--doctor|--version|--help]\n\n\
         keys:\n  \
         Home               toggle the floating terminal\n  \
         Alt+arrows         move it\n  \
         Alt+Shift+arrows   resize it\n  \
         Alt+drag           move it with the mouse\n  \
         border drag        resize it with the mouse\n\n\
         everything else is delivered to whichever terminal has focus.",
        env!("CARGO_PKG_VERSION")
    );
}
