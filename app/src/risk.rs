//! Worker side of the portfolio risk pane (`RISK`): loads daily history for the holdings and
//! the benchmark, from the shared bar cache when it is fresh and from the provider otherwise.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tr_core::{Bar, Timeframe};
use tr_data::{BarCache, MarketData, Provider};

use crate::worker::PaneId;

/// Daily bars fetched per symbol: a little over two years.
pub const BARS: usize = 560;
/// A cached series is used as is when it has this many bars...
const MIN_CACHED: usize = 300;
/// ...and its last bar is at most this old (covers weekends and holidays).
const FRESH_SECS: i64 = 4 * 86_400;
/// History requests in flight at once.
const PARALLEL: usize = 4;

pub enum RiskReq {
    /// Daily history for each symbol; `force` skips the cache freshness check.
    Load { pane: PaneId, symbols: Vec<String>, force: bool },
}

pub enum RiskResp {
    Bars { pane: PaneId, symbol: String, bars: Result<Arc<Vec<Bar>>, String> },
}

fn cached(cache: &Mutex<Option<BarCache>>, symbol: &str) -> Vec<Bar> {
    cache
        .lock()
        .ok()
        .and_then(|c| c.as_ref().and_then(|c| c.load(symbol, Timeframe::D1, BARS).ok()))
        .unwrap_or_default()
}

async fn load_one(
    provider: &dyn MarketData,
    cache: &Mutex<Option<BarCache>>,
    symbol: &str,
    force: bool,
) -> Result<Arc<Vec<Bar>>, String> {
    let have = cached(cache, symbol);
    let now = chrono::Utc::now().timestamp();
    if !force && have.len() >= MIN_CACHED && have.last().is_some_and(|b| now - b.ts <= FRESH_SECS) {
        return Ok(Arc::new(have));
    }
    match provider.history(symbol, Timeframe::D1, None, BARS).await {
        Ok(bars) if !bars.is_empty() => {
            if let Ok(mut c) = cache.lock()
                && let Some(c) = c.as_mut()
            {
                let _ = c.store(symbol, Timeframe::D1, &bars);
            }
            Ok(Arc::new(bars))
        }
        // Offline or rate-limited: stale history still beats none.
        _ if !have.is_empty() => Ok(Arc::new(have)),
        Ok(_) => Err("no price history".into()),
        Err(e) => Err(format!("{e:#}")),
    }
}

pub async fn handle(
    req: RiskReq,
    provider: &Provider,
    cache: &Mutex<Option<BarCache>>,
    send: &(dyn Fn(RiskResp) + Sync),
) {
    let RiskReq::Load { pane, symbols, force } = req;
    futures::stream::iter(symbols)
        .for_each_concurrent(PARALLEL, |symbol| async move {
            let bars = load_one(provider.as_ref(), cache, &symbol, force).await;
            send(RiskResp::Bars { pane, symbol, bars });
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Yahoo history through the same path the pane uses, twice: network, then the cache.
    /// `cargo test -p terminal-rs risk::tests::live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network"]
    async fn live_history_and_cache() {
        let provider: Provider = tr_data::from_env();
        let cache = Mutex::new(Some(BarCache::open_in_memory().unwrap()));
        for k in ["SPY US Equity", "QQQ US Equity", "XU100 Index", "AAPL US Equity"] {
            let t = std::time::Instant::now();
            let bars = load_one(provider.as_ref(), &cache, k, false).await.unwrap();
            let first = t.elapsed();
            let again = load_one(provider.as_ref(), &cache, k, false).await.unwrap();
            println!("{k}: {} bars, network {first:?}, cached {:?}", bars.len(), t.elapsed() - first);
            assert!(bars.len() > 400 && again.len() == bars.len());
        }
    }
}
