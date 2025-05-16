use crate::app::ui::tenant::Overlay;
use crate::constants::*;
use bytes::Bytes;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub struct Lease {
    pub tenant: Overlay,
    pub tenant_parser: Arc<RwLock<vt100::Parser>>,
    pub tenant_visible: bool,
    pub tenant_tx: Sender<Bytes>,
    pub tenant_rx: Option<Receiver<Bytes>>,
    pub tenant_status_tx: Sender<bool>,
    pub tenant_status_rx: Receiver<bool>,
}

impl Lease {
    pub fn new() -> Self {
        let (ttx, trx) = channel::<Bytes>(32);
        let (tpty_status_tx, tpty_status_rx) = channel::<bool>(1);

        let tparser = Arc::new(RwLock::new(vt100::Parser::new(
            DEFAULT_HEIGHT,
            DEFAULT_WIDTH,
            0,
        )));

        let lease = Lease {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: tparser,
            tenant_tx: ttx,
            tenant_rx: Some(trx),
            tenant_status_tx: tpty_status_tx,
            tenant_status_rx: tpty_status_rx,
        };

        lease
    }

    pub fn expired(&mut self) -> bool {
        if self.tenant.is_dead {
            self.tenant_visible = false;
            true
        } else {
            false
        }
    }

    pub fn renew(&mut self) -> Self {
        let (ttx, trx) = channel::<Bytes>(32);
        let (tpty_status_tx, tpty_status_rx) = channel::<bool>(1);

        let tparser = Arc::new(RwLock::new(vt100::Parser::new(
            DEFAULT_HEIGHT,
            DEFAULT_WIDTH,
            0,
        )));

        Lease {
            tenant_visible: false,
            tenant: Overlay::new(),
            tenant_parser: tparser,
            tenant_tx: ttx,
            tenant_rx: Some(trx),
            tenant_status_tx: tpty_status_tx,
            tenant_status_rx: tpty_status_rx,
        }
    }
}
