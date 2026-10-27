use crate::app::images::Store;
use crate::app::pty::{ChildParser, PtySession};
use crate::app::term::callbacks::{ChildCallbacks, ChildState, ReplySink};
use crate::app::ui::tenant::Overlay;
use crate::constants::*;

use bytes::Bytes;

use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::UnboundedSender;

/// A floating shell: its geometry, its parser, and the session of its child.
pub struct Lease {
    pub tenant: Overlay,
    pub tenant_parser: Arc<RwLock<ChildParser>>,
    pub tenant_sink: ReplySink,
    /// Graphics the float's child has drawn (S3). Empty for a shell that never
    /// emits an image, which is the common case.
    pub tenant_images: Arc<Mutex<Store>>,
    /// What the float's child told us about itself: hyperlinks, focus reporting.
    pub tenant_state: Arc<ChildState>,
    pub tenant_visible: bool,
    /// Kept whole (not just its channels) so that dropping the lease kills the
    /// child and unblocks its reader thread.
    session: Option<PtySession>,
}

impl Lease {
    pub fn new(
        caps: Arc<crate::app::term::caps::Capabilities>,
        host: UnboundedSender<Vec<u8>>,
    ) -> Self {
        let (parser, sink, state) = new_parser(caps, host);
        Self {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: parser,
            tenant_sink: sink,
            tenant_images: Arc::new(Mutex::new(Store::new())),
            tenant_state: state,
            session: None,
        }
    }

    /// Wire up a freshly spawned tenant process.
    pub fn attach(&mut self, session: PtySession) {
        self.session = Some(session);
    }

    /// The tenant child's input modes, read from its parser.
    pub fn modes(&self) -> crate::app::input::encode::ChildModes {
        let parser = self.tenant_parser.read().unwrap();
        crate::app::input::encode::ChildModes::from_screen(parser.screen())
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

    /// Resize the grid and ask the child to resize too.
    ///
    /// Callers pass the *content* size (inside the border): the pty, the parser
    /// and the painted area must all agree, or a full-screen app draws for a
    /// size we never render.
    pub fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.tenant_parser
            .write()
            .unwrap()
            .set_size(nz(rows), nz(cols));

        if let Some(session) = &self.session {
            if session.resize.send((rows, cols)).is_err() {
                eprintln!("failed to send resize to tenant");
            }
        }
    }

    /// Re-apply the overlay's current geometry to the child.
    pub fn sync_size(&mut self) {
        let (rows, cols) = self.tenant.content_size();
        let current = {
            let parser = self.tenant_parser.read().unwrap();
            let (screen_rows, screen_cols) = parser.screen().size();
            (screen_rows.get(), screen_cols.get())
        };
        if current != (rows, cols) {
            self.resize_screen(rows, cols);
        }
    }
}

/// Build a child parser with the callbacks that answer its queries, and the
/// scrollback capacity from the environment (S3.6).
///
/// `host` carries the effects that belong to the real terminal (clipboard
/// copies); everything else is answered back into the child's own pty.
pub fn new_parser(
    caps: Arc<crate::app::term::caps::Capabilities>,
    host: UnboundedSender<Vec<u8>>,
) -> (Arc<RwLock<ChildParser>>, ReplySink, Arc<ChildState>) {
    let (callbacks, sink, state) = ChildCallbacks::new(caps, host);
    // Scrollback is inherited from the environment uncl was launched in; see
    // `constants::scrollback_lines`. Both panes get the same capacity.
    let parser = ChildParser::new_with_callbacks(
        nz(DEFAULT_HEIGHT),
        nz(DEFAULT_WIDTH),
        scrollback_lines(),
        callbacks,
    );
    (Arc::new(RwLock::new(parser)), sink, state)
}
