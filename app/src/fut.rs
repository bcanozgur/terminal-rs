//! Worker side of the FUT pane: futures curve, VIX term structure and sector heatmap, with a
//! short cache so several panes and quick tab flips do not repeat requests to Yahoo.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tr_data::fut::{self, Curve, Heat, VixTerm};

use crate::worker::PaneId;

/// Shorter than the pane's one-minute refresh, so a refresh always gets new data.
const TTL: Duration = Duration::from_secs(50);

pub enum FutReq {
    Curve { pane: PaneId, root: String, force: bool },
    Vix { pane: PaneId, force: bool },
    Heat { pane: PaneId, world: bool, force: bool },
}

pub enum FutResp {
    Curve { pane: PaneId, res: Result<Arc<Curve>, String> },
    Vix { pane: PaneId, res: Result<Arc<VixTerm>, String> },
    Heat { pane: PaneId, res: Result<Arc<Heat>, String> },
}

#[derive(Clone)]
enum Cached {
    Curve(Arc<Curve>),
    Vix(Arc<VixTerm>),
    Heat(Arc<Heat>),
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, Cached)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Cached)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn cached(key: &str, force: bool) -> Option<Cached> {
    if force {
        return None;
    }
    let c = cache().lock().ok()?;
    c.get(key).filter(|(t, _)| t.elapsed() < TTL).map(|(_, v)| v.clone())
}

fn store(key: String, v: Cached) {
    if let Ok(mut c) = cache().lock() {
        c.insert(key, (Instant::now(), v));
    }
}

pub async fn handle(req: FutReq, send: &(dyn Fn(FutResp) + Sync)) {
    match req {
        FutReq::Curve { pane, root, force } => {
            let key = format!("curve:{root}");
            let res = match (cached(&key, force), fut::root(&root)) {
                (Some(Cached::Curve(c)), _) => Ok(c),
                (_, None) => Err(format!("unknown futures root {root}")),
                (_, Some(r)) => fut::fetch_curve(r).await.map(Arc::new).map_err(|e| format!("{e:#}")),
            };
            if let Ok(c) = &res {
                store(key, Cached::Curve(c.clone()));
            }
            send(FutResp::Curve { pane, res });
        }
        FutReq::Vix { pane, force } => {
            let res = match cached("vix", force) {
                Some(Cached::Vix(v)) => Ok(v),
                _ => fut::fetch_vix().await.map(Arc::new).map_err(|e| format!("{e:#}")),
            };
            if let Ok(v) = &res {
                store("vix".into(), Cached::Vix(v.clone()));
            }
            send(FutResp::Vix { pane, res });
        }
        FutReq::Heat { pane, world, force } => {
            let key = format!("heat:{world}");
            let res = match cached(&key, force) {
                Some(Cached::Heat(h)) => Ok(h),
                _ => fut::fetch_heat(world).await.map(Arc::new).map_err(|e| format!("{e:#}")),
            };
            if let Ok(h) = &res {
                store(key, Cached::Heat(h.clone()));
            }
            send(FutResp::Heat { pane, res });
        }
    }
}
