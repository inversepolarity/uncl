use crate::app::pty::PtySession;
use crate::app::ui::tenant::Overlay;
use crate::constants::*;

use bytes::Bytes;

use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// A floating shell: its geometry, its parser, and the channels to its child.
///
/// S0 keeps this deliberately thin — it owns the child's lifecycle and the grid
/// it renders from. Input routing and geometry unification are S1.
pub struct Lease {
    pub tenant: Overlay,
    pub tenant_parser: Arc<RwLock<vt100::Parser>>,
    pub tenant_visible: bool,
    /// Bytes to the tenant's child; `None` until it has been spawned.
    pub tenant_tx: Option<UnboundedSender<Bytes>>,
    pub tenant_resize_tx: Option<UnboundedSender<(u16, u16)>>,
    pub tenant_exited: Option<UnboundedReceiver<()>>,
}

impl Lease {
    pub fn new() -> Self {
        Self {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: new_parser(),
            tenant_tx: None,
            tenant_resize_tx: None,
            tenant_exited: None,
        }
    }

    /// Wire up a freshly spawned tenant process.
    pub fn attach(&mut self, session: PtySession) {
        self.tenant_tx = Some(session.writer);
        self.tenant_resize_tx = Some(session.resize);
        self.tenant_exited = Some(session.exited);
    }

    /// Whether the tenant's child is still alive (its writer has not been
    /// dropped). Cheap and lock-free, unlike the old `status_rx.is_closed()`.
    pub fn running(&self) -> bool {
        self.tenant_tx.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// Non-blocking check for child exit; marks the overlay dead when it fires.
    pub fn poll_exit(&mut self) -> bool {
        let Some(rx) = self.tenant_exited.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(()) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.tenant.is_dead = true;
                self.tenant_tx = None;
                self.tenant_resize_tx = None;
                true
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
        }
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

        if let Some(resize_tx) = &self.tenant_resize_tx {
            if resize_tx.send((rows, cols)).is_err() {
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
