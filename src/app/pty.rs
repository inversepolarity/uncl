//! Shared PTY plumbing for the owner and tenant shells.
//!
//! One `spawn()` sets up the four moving parts both panes need, and hands back
//! only the handles the UI cares about:
//!
//! * a blocking reader thread that feeds the `vt100` parser and raises the
//!   frame signal (no byte-sniffing: terminal modes are read from the parser),
//! * a coalescing writer task, so a keystroke or a paste becomes a single
//!   `write` syscall instead of one per byte,
//! * a resize task that owns the master side,
//! * an exit notification for the supervisor,
//! * a `ChildKiller` so the session can be torn down synchronously on quit.
//!
//! The killer matters for a clean exit: dropping the session SIGHUPs the child,
//! which closes the pty, which makes the blocked reader return EOF. Without it
//! the reader thread never finishes and the process hangs on runtime shutdown
//! with the terminal still in raw mode.

use std::{
    io::{Read, Write},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::signal::Signal;
use crate::app::term::callbacks::{ChildCallbacks, ReplySink};
use crate::constants::PTY_WRITE_BATCH;

/// A child's grid, with the callbacks that answer its terminal queries.
pub type ChildParser = vt100::Parser<ChildCallbacks>;

/// Everything the UI needs to talk to a spawned child.
pub struct PtySession {
    /// Bytes destined for the child's stdin. Unbounded: sending must never
    /// block the event loop (that was the old 32-slot channel's failure mode).
    pub writer: UnboundedSender<Bytes>,
    /// Requested `(rows, cols)` changes.
    pub resize: UnboundedSender<(u16, u16)>,
    /// Fires once, when the reader sees EOF.
    pub exited: UnboundedReceiver<()>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl PtySession {
    /// Whether the child is still accepting input.
    pub fn writer_open(&self) -> bool {
        !self.writer.is_closed()
    }

    /// Terminate the child. Idempotent.
    pub fn shutdown(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Also runs when a pane is replaced (float restart) or when the whole
        // app unwinds, so children are never left orphaned on a dead pty.
        let _ = self.killer.kill();
    }
}

/// Spawn `cmd` on a fresh PTY of the given size.
///
/// `parser` receives everything the child writes; `signal` is raised after each
/// batch so the event loop can repaint.
pub fn spawn(
    cmd: CommandBuilder,
    rows: u16,
    cols: u16,
    parser: Arc<RwLock<ChildParser>>,
    sink: ReplySink,
    signal: Arc<Signal>,
) -> Result<PtySession> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pty")?;

    let master = pair.master;
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("failed to spawn command in pty")?;
    drop(pair.slave);

    let killer = child.clone_killer();

    let mut reader = master.try_clone_reader().context("pty reader")?;
    let mut writer = master.take_writer().context("pty writer")?;

    let (writer_tx, mut writer_rx) = unbounded_channel::<Bytes>();

    // Terminal queries are answered through the same writer the user's input
    // uses, so replies are ordered with keystrokes and coalesced by the batching
    // task below.
    sink.install(&writer_tx);
    let (resize_tx, mut resize_rx) = unbounded_channel::<(u16, u16)>();
    let (exited_tx, exited_rx) = unbounded_channel::<()>();

    // Reader: parse, then signal. The write lock is held only while the parser
    // runs; the renderer never holds it across a frame (it clones the screen).
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut parser = parser.write().unwrap();
                        parser.process(&buf[..n]);
                    }
                    signal.raise();
                }
                Err(err) => {
                    eprintln!("pty read error: {err}");
                    break;
                }
            }
        }
        let _ = exited_tx.send(());
    });

    // Writer: drain the queue and flush once, so N queued key events cost one
    // syscall pair instead of N.
    tokio::spawn(async move {
        let mut batch: Vec<u8> = Vec::with_capacity(8192);
        while let Some(first) = writer_rx.recv().await {
            batch.clear();
            batch.extend_from_slice(&first);
            while batch.len() < PTY_WRITE_BATCH {
                match writer_rx.try_recv() {
                    Ok(more) => batch.extend_from_slice(&more),
                    Err(_) => break,
                }
            }
            if let Err(err) = writer.write_all(&batch) {
                eprintln!("pty write error: {err}");
                break;
            }
            if let Err(err) = writer.flush() {
                eprintln!("pty flush error: {err}");
                break;
            }
        }
    });

    // Resize: the master handle lives here so `resize` cannot race the reader.
    // When the session is dropped this task ends and the master is closed,
    // which is what delivers the final SIGHUP to the child's process group.
    tokio::spawn(async move {
        while let Some((rows, cols)) = resize_rx.recv().await {
            if let Err(err) = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                eprintln!("pty resize error: {err}");
                break;
            }
        }
    });

    // Reap the child. Exit detection itself comes from reader EOF above.
    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
    });

    Ok(PtySession {
        writer: writer_tx,
        resize: resize_tx,
        exited: exited_rx,
        killer,
    })
}
