//! Worker side of the SMART pane: requests, responses and the background loader that fills the
//! on-disk 13F cache (`<data dir>/smartmoney/`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tr_data::smartmoney::{self as sm, Holder, Insiders, Manager, ShortData};

use crate::worker::PaneId;

pub enum SmartReq {
    /// Every curated manager already in the disk cache.
    Cached {
        pane: PaneId,
    },
    Insiders {
        pane: PaneId,
        ticker: String,
        user_agent: String,
    },
    /// One manager (cache first); `force` re-checks the SEC.
    Manager {
        pane: PaneId,
        cik: u64,
        force: bool,
        user_agent: String,
    },
    /// The whole curated list, one manager at a time.
    Index {
        pane: PaneId,
        force: bool,
        user_agent: String,
    },
    /// Which cached managers hold a ticker.
    Holders {
        pane: PaneId,
        ticker: String,
        user_agent: String,
    },
    Short {
        pane: PaneId,
        ticker: String,
    },
}

/// A manager with the tickers of its largest positions.
pub struct ManagerView {
    pub manager: Manager,
    /// CUSIP → ticker, where the issuer name could be matched.
    pub tickers: HashMap<String, String>,
}

pub enum SmartResp {
    Cached {
        pane: PaneId,
        managers: Vec<Arc<Manager>>,
    },
    Insiders {
        pane: PaneId,
        ticker: String,
        data: Result<Arc<Insiders>, String>,
    },
    Manager {
        pane: PaneId,
        cik: u64,
        data: Result<Arc<ManagerView>, String>,
    },
    /// `done` of `total` managers of an index run have been attempted.
    Progress {
        pane: PaneId,
        done: usize,
        total: usize,
    },
    Holders {
        pane: PaneId,
        ticker: String,
        data: Result<Vec<Holder>, String>,
    },
    Short {
        pane: PaneId,
        ticker: String,
        data: Arc<ShortData>,
    },
}

/// Positions with a ticker lookup in the manager view (largest of the latest filing, plus the
/// biggest sold-out ones).
const LOOKUP: usize = 60;

/// Pause between managers of an index run that needed the network.
const GAP: Duration = Duration::from_millis(600);

fn ua(setting: &str) -> String {
    tr_data::edgar::user_agent(setting).unwrap_or_default()
}

async fn view(manager: Manager, user_agent: &str) -> ManagerView {
    let tickers = match sm::title_index(&crate::worker::data_dir(), user_agent).await {
        Ok(idx) => {
            let mut p: Vec<sm::Position> =
                manager.latest.iter().flat_map(|f| f.positions.iter().take(LOOKUP).cloned()).collect();
            p.extend(manager.sold_out(LOOKUP).into_iter().map(|c| c.position));
            idx.resolve(&p)
        }
        Err(_) => HashMap::new(),
    };
    ManagerView { manager, tickers }
}

async fn load_manager(cik: u64, force: bool, user_agent: &str) -> Result<Arc<ManagerView>, String> {
    let m = sm::load_manager(&crate::worker::data_dir(), cik, user_agent, force).await.map_err(|e| format!("{e:#}"))?;
    Ok(Arc::new(view(m, user_agent).await))
}

pub async fn handle(req: SmartReq, send: &(dyn Fn(SmartResp) + Sync)) {
    match req {
        SmartReq::Cached { pane } => {
            let dir = crate::worker::data_dir();
            let managers =
                tokio::task::spawn_blocking(move || sm::all_cached(&dir).into_iter().map(Arc::new).collect())
                    .await
                    .unwrap_or_default();
            send(SmartResp::Cached { pane, managers });
        }
        SmartReq::Insiders { pane, ticker, user_agent } => {
            let data = sm::load_insiders(&crate::worker::data_dir(), &ticker, &ua(&user_agent))
                .await
                .map(Arc::new)
                .map_err(|e| format!("{e:#}"));
            send(SmartResp::Insiders { pane, ticker, data });
        }
        SmartReq::Manager { pane, cik, force, user_agent } => {
            let data = load_manager(cik, force, &ua(&user_agent)).await;
            send(SmartResp::Manager { pane, cik, data });
        }
        SmartReq::Index { pane, force, user_agent } => {
            let user_agent = ua(&user_agent);
            let total = sm::MANAGERS.len();
            for (i, (_, cik, _)) in sm::MANAGERS.iter().enumerate() {
                let dir = crate::worker::data_dir();
                let fresh =
                    !force && sm::read_manager(&dir, *cik).is_some_and(|m| !m.stale(chrono::Utc::now().timestamp()));
                let data = load_manager(*cik, force, &user_agent).await;
                send(SmartResp::Manager { pane, cik: *cik, data });
                send(SmartResp::Progress { pane, done: i + 1, total });
                if !fresh {
                    tokio::time::sleep(GAP).await;
                }
            }
        }
        SmartReq::Holders { pane, ticker, user_agent } => {
            let user_agent = ua(&user_agent);
            let dir = crate::worker::data_dir();
            let data = async {
                let idx = sm::title_index(&dir, &user_agent).await.map_err(|e| format!("{e:#}"))?;
                let t = ticker.clone();
                tokio::task::spawn_blocking(move || {
                    let managers = sm::all_cached(&dir);
                    sm::holders_of(&managers, &t, &idx)
                })
                .await
                .map_err(|e| e.to_string())
            }
            .await;
            send(SmartResp::Holders { pane, ticker, data });
        }
        SmartReq::Short { pane, ticker } => {
            let data = Arc::new(sm::load_short(&crate::worker::data_dir(), &ticker).await);
            send(SmartResp::Short { pane, ticker, data });
        }
    }
}
