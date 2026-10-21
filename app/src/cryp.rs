//! Worker side of the CRYP pane: one request refreshes every section.

use std::sync::Arc;

use tr_data::cryptomkt::{self, Overview};

use crate::worker::PaneId;

pub enum CrypReq {
    Load { pane: PaneId },
}

pub enum CrypResp {
    Overview { pane: PaneId, ov: Result<Arc<Overview>, String> },
}

pub async fn handle(req: CrypReq, send: &(dyn Fn(CrypResp) + Sync)) {
    match req {
        CrypReq::Load { pane } => {
            let ov = cryptomkt::fetch().await.map(Arc::new).map_err(|e| format!("{e:#}"));
            send(CrypResp::Overview { pane, ov });
        }
    }
}
