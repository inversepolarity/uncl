use crate::app::pty::PtySession;
use crate::app::ui::tenant::Overlay;
use crate::constants::*;

use bytes::Bytes;

use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::UnboundedSender;

/// A floating shell: its geometry, its parser, and the session of its child.
///
/// S0 keeps this deliberately thin — it owns the child's lifecycle and the grid
/// it renders from. Input routing and geometry unification are S1.
pub struct Lease {
    pub tenant: Overlay,
    pub tenant_parser: Arc<RwLock<vt100::Parser>>,
    pub tenant_visible: bool,
    /// Kept whole (not just its channels) so that dropping the lease kills the
    /// child and unblocks its reader thread.
    session: Option<PtySession>,
}

impl Lease {
    pub fn new() -> Self {
        Self {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: new_parser(),
            session: None,
        }
    }

    /// Wire up a freshly spawned tenant process.
    pub fn attach(&mut self, session: PtySession) {
        self.session = Some(session);
    }

    pub fn writer(&self) -> Option<UnboundedSender<Bytes>> {
        self.session.as_ref().map(|session| session.writer.clone())
    }

    /// Whether the tenant's child is still alive. Cheap and lock-free, unlike
    /// the old `status_rx.is_closed()`.
    pub fn running(&self) -> bool {
        self.session.as_ref().is_some_and(PtySession::writer_open)
    }

    /// Non-blocking check for child exit; tears the session down when it fires.
    pub fn poll_exit(&mut self) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };

        match session.exited.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                self.tenant.is_dead = true;
                // Drops the session: kills the (already dead) child and lets
                // its threads finish.
                self.session = None;
                true
            }
            Err(TryRecvError::Empty) => false,
        }
    }

    /// Kill the tenant's child and let its threads finish.
    pub fn shutdown(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.shutdown();
        }
        self.session = None;
    }

    pub fn expired(&mut self) -> bool {
        if self.tenant.is_dead {
            self.tenant_visible = false;
            true
        } else {
            false
        }
    }

    /// A lease for a fresh shell. Visibility always starts hidden, as before.
    pub fn renew(&mut self) -> Self {
        Self::new()
    }

    /// Resize the grid and ask the child to resize too.
    pub fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.tenant_parser
            .write()
            .unwrap()
            .set_size(nz(rows), nz(cols));

        // The child is told the size of the area it actually draws into, i.e.
        // inside the border. The parser is told the same number, so a
        // full-screen app can no longer draw for a size we never render.
        if let Some(session) = &self.session {
            if session.resize.send((rows, cols)).is_err() {
                eprintln!("failed to send resize to tenant");
            }
        }
    }
}

pub fn new_parser() -> Arc<RwLock<vt100::Parser>> {
    Arc::new(RwLock::new(vt100::Parser::new(
        nz(DEFAULT_HEIGHT),
        nz(DEFAULT_WIDTH),
        0,
    )))
}
