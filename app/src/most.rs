//! Worker side of the MOST pane: one request fetches all three Yahoo screener lists.

use std::sync::Arc;

use tr_data::most::{self, Board};

use crate::worker::PaneId;

pub enum MostReq {
    Load { pane: PaneId },
}

pub enum MostResp {
    Board { pane: PaneId, board: Result<Arc<Board>, String> },
}

pub async fn handle(req: MostReq, send: &(dyn Fn(MostResp) + Sync)) {
    match req {
        MostReq::Load { pane } => {
            let board = most::fetch().await.map(Arc::new).map_err(|e| format!("{e:#}"));
            send(MostResp::Board { pane, board });
        }
    }
}
