//! Worker side of the Turkish funds pane (`TRF`): requests, responses and the background loader
//! that fills the on-disk fund cache (`<data dir>/trfunds/`).

use std::sync::Arc;
use std::time::Duration;

use tr_data::trfunds::{self, FundData, FundInfo};

use crate::worker::PaneId;

pub enum TrfReq {
    /// The fund list and every fund already in the disk cache.
    Cached { pane: PaneId },
    /// Download the fund list from KAP when the disk copy is old.
    List { pane: PaneId },
    /// One fund; `force` re-checks KAP even when the cached copy is fresh.
    Load { pane: PaneId, code: String, force: bool },
    /// A batch (the curated list), one fund at a time with a pause between funds.
    Index { pane: PaneId, codes: Vec<String>, force: bool },
}

pub enum TrfResp {
    Cached {
        pane: PaneId,
        list: Option<Arc<Vec<FundInfo>>>,
        funds: Vec<Arc<FundData>>,
    },
    List {
        pane: PaneId,
        list: Result<Arc<Vec<FundInfo>>, String>,
    },
    Fund {
        pane: PaneId,
        code: String,
        fund: Result<Arc<FundData>, String>,
    },
    /// `done` of `total` funds of an index run have been attempted.
    Progress {
        pane: PaneId,
        done: usize,
        total: usize,
    },
}

/// Pause between funds of an index run that needed the network.
const GAP: Duration = Duration::from_millis(1500);

pub async fn handle(req: TrfReq, send: &(dyn Fn(TrfResp) + Sync)) {
    let dir = trfunds::cache_dir(&crate::worker::data_dir());
    match req {
        TrfReq::Cached { pane } => {
            let d = dir.clone();
            let (list, funds) = tokio::task::spawn_blocking(move || {
                (trfunds::read_fund_list(&d).map(Arc::new), trfunds::all_cached(&d).into_iter().map(Arc::new).collect())
            })
            .await
            .unwrap_or_default();
            send(TrfResp::Cached { pane, list, funds });
        }
        TrfReq::List { pane } => {
            let list = trfunds::fund_list(&dir).await.map(Arc::new).map_err(|e| format!("{e:#}"));
            send(TrfResp::List { pane, list });
        }
        TrfReq::Load { pane, code, force } => {
            let fund = trfunds::load(&dir, &code, force).await.map(Arc::new).map_err(|e| format!("{e:#}"));
            send(TrfResp::Fund { pane, code, fund });
        }
        TrfReq::Index { pane, codes, force } => {
            let total = codes.len();
            for (i, code) in codes.into_iter().enumerate() {
                let now = chrono::Utc::now().timestamp();
                let fresh = !force && trfunds::read_cached(&dir, &code).is_some_and(|f| !f.stale(now));
                let fund = trfunds::load(&dir, &code, force).await.map(Arc::new).map_err(|e| format!("{e:#}"));
                send(TrfResp::Fund { pane, code, fund });
                send(TrfResp::Progress { pane, done: i + 1, total });
                if !fresh {
                    tokio::time::sleep(GAP).await;
                }
            }
        }
    }
}
