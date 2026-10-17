//! Worker side of the ETF pane (`ETF`): requests, responses and the background loader that
//! fills the on-disk fund cache (`<data dir>/etf/`).

use std::sync::Arc;
use std::time::Duration;

use tr_data::etf::{self, Fund};

use crate::worker::PaneId;

pub enum EtfReq {
    /// Every fund already in the disk cache.
    Cached { pane: PaneId },
    /// One fund; `force` re-checks the SEC even when the cached copy is fresh.
    Load { pane: PaneId, ticker: String, force: bool, user_agent: String },
    /// A batch (the curated list), one fund at a time to stay within SEC and Yahoo rate limits.
    Index { pane: PaneId, tickers: Vec<String>, force: bool, user_agent: String },
}

pub enum EtfResp {
    Cached {
        pane: PaneId,
        funds: Vec<Arc<Fund>>,
    },
    Fund {
        pane: PaneId,
        ticker: String,
        fund: Result<Arc<Fund>, String>,
    },
    /// `done` of `total` funds of an index run have been attempted.
    Progress {
        pane: PaneId,
        done: usize,
        total: usize,
    },
}

/// Pause between funds of an index run that needed the network.
const GAP: Duration = Duration::from_millis(600);

async fn load(ticker: &str, force: bool, user_agent: &str) -> Result<Arc<Fund>, String> {
    let ua = tr_data::edgar::user_agent(user_agent).unwrap_or_default();
    let dir = etf::cache_dir(&crate::worker::data_dir());
    etf::load(&dir, ticker, &ua, force).await.map(Arc::new).map_err(|e| format!("{e:#}"))
}

pub async fn handle(req: EtfReq, send: &(dyn Fn(EtfResp) + Sync)) {
    match req {
        EtfReq::Cached { pane } => {
            let dir = etf::cache_dir(&crate::worker::data_dir());
            let funds = tokio::task::spawn_blocking(move || etf::all_cached(&dir).into_iter().map(Arc::new).collect())
                .await
                .unwrap_or_default();
            send(EtfResp::Cached { pane, funds });
        }
        EtfReq::Load { pane, ticker, force, user_agent } => {
            let fund = load(&ticker, force, &user_agent).await;
            send(EtfResp::Fund { pane, ticker, fund });
        }
        EtfReq::Index { pane, tickers, force, user_agent } => {
            let dir = etf::cache_dir(&crate::worker::data_dir());
            let total = tickers.len();
            for (i, ticker) in tickers.into_iter().enumerate() {
                let now = chrono::Utc::now().timestamp();
                let fresh = !force && etf::read_cached(&dir, &ticker).is_some_and(|f| !f.stale(now));
                let fund = load(&ticker, force, &user_agent).await;
                send(EtfResp::Fund { pane, ticker, fund });
                send(EtfResp::Progress { pane, done: i + 1, total });
                if !fresh {
                    tokio::time::sleep(GAP).await;
                }
            }
        }
    }
}
