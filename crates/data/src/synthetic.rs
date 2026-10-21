//! Formula (synthetic) securities on top of any provider: `=XU100/USDTRY`, `GRAMALTIN`, ...
//! (see [`tr_core::formula`]). [`Synthetic`] wraps the real provider; market keys that are not
//! formulas pass straight through, formula keys are computed from the legs' own history,
//! quotes and live feeds.
//!
//! Live: every leg's live feed (or, for a leg without one, a 15 s poll of its latest bars) is
//! merged and the newest bar present in all legs is recomputed on each update.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tr_core::formula::{self, Formula, align_key};
use tr_core::{Bar, Timeframe};

use crate::qstats::QuoteStats;
use crate::{ExtendedQuote, LiveFeed, MarketData, NewsItem, OptionChain, OrderBook, Provider, Quote, SearchHit, YieldCurves};

const POLL: Duration = Duration::from_secs(15);

pub struct Synthetic {
    inner: Provider,
}

impl Synthetic {
    pub fn new(inner: Provider) -> Self {
        Self { inner }
    }
}

/// The formula behind `symbol`, `Some(Err)` when it is one but does not parse.
fn formula_of(symbol: &str) -> Option<Result<Formula>> {
    formula::from_symbol(symbol).map(|r| r.map_err(|e| anyhow::anyhow!("bad formula {symbol}: {e}")))
}

impl Synthetic {
    async fn history_of(&self, f: &Formula, tf: Timeframe, end: Option<i64>, limit: usize, real: bool) -> Result<Vec<Bar>> {
        let keys = f.leg_keys();
        // Legs trade on different calendars, so fetch a margin to still end up with `limit` joined bars.
        let want = if keys.len() == 1 { limit } else { limit + limit / 2 + 50 };
        let inner = &self.inner;
        let fetched = futures::future::join_all(keys.iter().map(|k| async move {
            if real { inner.real_history(k, tf, want).await } else { inner.history(k, tf, end, want).await }
        }))
        .await;
        let mut series = Vec::with_capacity(keys.len());
        for (k, r) in keys.iter().zip(fetched) {
            match r {
                Ok(b) if !b.is_empty() => series.push(b),
                // Paging past the oldest bar of a leg: nothing more to show.
                Ok(_) if end.is_some() => return Ok(Vec::new()),
                Ok(_) => bail!("formula leg {k} has no {tf} bars"),
                Err(e) => bail!("formula leg {k}: {e:#}"),
            }
        }
        let mut out = f.combine(tf, &series);
        if out.is_empty() {
            if end.is_some() {
                return Ok(out);
            }
            bail!("formula legs {} share no {tf} bars (different sessions or zero denominators)", keys.join(", "));
        }
        if out.len() > limit {
            out.drain(..out.len() - limit);
        }
        Ok(out)
    }

    async fn quotes_of(&self, symbols: &[String], real: bool) -> Result<Vec<Quote>> {
        let (formulas, plain): (Vec<_>, Vec<_>) = symbols.iter().cloned().partition(|s| formula::from_symbol(s).is_some());
        let mut out = if plain.is_empty() {
            Vec::new()
        } else if real {
            self.inner.real_quotes(&plain).await?
        } else {
            self.inner.quotes(&plain).await?
        };
        for s in formulas {
            let Some(Ok(f)) = formula_of(&s) else { continue };
            let keys = f.leg_keys();
            let legs = if real { self.inner.real_quotes(&keys).await } else { self.inner.quotes(&keys).await };
            let Ok(legs) = legs else { continue };
            let by_key: Vec<Option<&Quote>> = keys.iter().map(|k| legs.iter().find(|q| &q.symbol == k)).collect();
            if let Some(q) = combine_quotes(&s, &f, &by_key) {
                out.push(q);
            }
        }
        Ok(out)
    }
}

/// Quote of a formula from its legs' quotes (`None` if a leg is missing or the result is not finite).
fn combine_quotes(symbol: &str, f: &Formula, legs: &[Option<&Quote>]) -> Option<Quote> {
    let legs: Vec<&Quote> = legs.iter().copied().collect::<Option<_>>()?;
    let last = f.eval(&legs.iter().map(|q| q.last).collect::<Vec<_>>())?;
    let prev_close = f.eval(&legs.iter().map(|q| q.prev_close).collect::<Vec<_>>()).unwrap_or(0.0);
    let n = legs.iter().map(|q| q.spark.len()).min().unwrap_or(0);
    let spark = (0..n)
        .map(|i| {
            let vals: Vec<f64> = legs.iter().map(|q| q.spark[q.spark.len() - n + i]).collect();
            f.eval(&vals)
        })
        .collect::<Option<Vec<f64>>>()
        .unwrap_or_default();
    Some(Quote {
        symbol: symbol.to_string(),
        last,
        prev_close,
        ts: legs.iter().map(|q| q.ts).max()?,
        spark,
        extended: None,
        source: legs[0].source,
        traded: legs.iter().filter_map(|q| q.traded).max(),
        delay_min: legs.iter().filter_map(|q| q.delay_min).max(),
    })
}

#[async_trait]
impl MarketData for Synthetic {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        match formula_of(symbol) {
            Some(f) => self.history_of(&f?, tf, end, limit, false).await,
            None => self.inner.history(symbol, tf, end, limit).await,
        }
    }

    fn quote_source(&self) -> crate::QuoteSource {
        self.inner.quote_source()
    }

    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.quotes_of(symbols, false).await
    }

    /// Merges the legs' live feeds (see the module docs).
    fn live(&self, symbol: &str, tf: Timeframe) -> Option<LiveFeed> {
        let Some(f) = formula_of(symbol) else { return self.inner.live(symbol, tf) };
        let f = f.ok()?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(run_live(self.inner.clone(), f, tf, tx));
        Some(rx)
    }

    async fn real_quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        self.quotes_of(symbols, true).await
    }

    async fn real_history(&self, symbol: &str, tf: Timeframe, limit: usize) -> Result<Vec<Bar>> {
        match formula_of(symbol) {
            Some(f) => self.history_of(&f?, tf, None, limit, true).await,
            None => self.inner.real_history(symbol, tf, limit).await,
        }
    }

    async fn quote_stats(&self, symbols: &[String]) -> Result<Vec<QuoteStats>> {
        let plain: Vec<String> = symbols.iter().filter(|s| formula::from_symbol(s).is_none()).cloned().collect();
        if plain.is_empty() { Ok(Vec::new()) } else { self.inner.quote_stats(&plain).await }
    }

    async fn extended(&self, symbol: &str) -> Result<Option<ExtendedQuote>> {
        if formula::from_symbol(symbol).is_some() { Ok(None) } else { self.inner.extended(symbol).await }
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.inner.search(query).await
    }

    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        match symbol {
            Some(s) if formula::from_symbol(s).is_some() => Ok(Vec::new()),
            _ => self.inner.news(symbol).await,
        }
    }

    async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<OptionChain> {
        if formula::from_symbol(symbol).is_some() {
            bail!("no options on a formula security ({symbol})");
        }
        self.inner.options(symbol, expiration).await
    }

    async fn order_book(&self, symbol: &str, depth: usize) -> Result<OrderBook> {
        if formula::from_symbol(symbol).is_some() {
            bail!("no order book for a formula security ({symbol})");
        }
        self.inner.order_book(symbol, depth).await
    }

    async fn yield_curves(&self) -> Result<YieldCurves> {
        self.inner.yield_curves().await
    }
}

/// Feed the merged live bars of a formula into `tx` until the receiver is dropped.
async fn run_live(inner: Provider, f: Formula, tf: Timeframe, tx: mpsc::Sender<Bar>) {
    let keys = f.leg_keys();
    let (utx, mut urx) = mpsc::channel::<(usize, Bar)>(256);
    for (i, key) in keys.into_iter().enumerate() {
        let (inner, utx) = (inner.clone(), utx.clone());
        tokio::spawn(async move {
            // Seed with the latest bars so every leg has the current period from the start.
            for b in inner.history(&key, tf, None, 3).await.unwrap_or_default() {
                if utx.send((i, b)).await.is_err() {
                    return;
                }
            }
            match inner.live(&key, tf) {
                Some(mut feed) => {
                    while let Some(b) = feed.recv().await {
                        if utx.send((i, b)).await.is_err() {
                            return;
                        }
                    }
                }
                None => loop {
                    tokio::time::sleep(POLL).await;
                    for b in inner.history(&key, tf, None, 2).await.unwrap_or_default() {
                        if utx.send((i, b)).await.is_err() {
                            return;
                        }
                    }
                },
            }
        });
    }
    drop(utx);
    let mut legs: Vec<BTreeMap<i64, Bar>> = vec![BTreeMap::new(); f.legs().len()];
    loop {
        let (i, bar) = tokio::select! {
            _ = tx.closed() => return,
            u = urx.recv() => match u { Some(u) => u, None => return },
        };
        let m = &mut legs[i];
        m.insert(align_key(tf, bar.ts), bar);
        while m.len() > 8 {
            m.pop_first();
        }
        // The newest period every leg has a bar for.
        let Some(key) = legs[0].keys().rev().find(|k| legs.iter().all(|m| m.contains_key(k))).copied() else {
            continue;
        };
        let row: Vec<Bar> = legs.iter().map(|m| m[&key]).collect();
        if let Some(b) = f.combine_bars(&row)
            && tx.send(b).await.is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QuoteSource;
    use std::sync::Arc;

    /// Canned bars per market key; quotes derive from the last two daily bars.
    struct Mock(Vec<(&'static str, Vec<Bar>)>);

    #[async_trait]
    impl MarketData for Mock {
        fn name(&self) -> &str {
            "mock"
        }

        async fn history(&self, symbol: &str, _tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
            let Some((_, b)) = self.0.iter().find(|(k, _)| *k == symbol) else { bail!("no data for {symbol}") };
            Ok(crate::clip(b.clone(), end, limit))
        }

        fn quote_source(&self) -> QuoteSource {
            QuoteSource::Sample
        }
    }

    fn bars(closes: &[(i64, f64)]) -> Vec<Bar> {
        closes.iter().map(|&(d, c)| Bar::new(d * 86_400 + 7 * 3600, c, c, c, c, 1.0)).collect()
    }

    fn synth() -> Synthetic {
        let m = Mock(vec![
            ("XU100 Index", bars(&[(1, 10_000.0), (2, 11_000.0), (3, 12_000.0)])),
            ("USDTRY Curncy", bars(&[(2, 40.0), (3, 0.0), (4, 41.0)])),
            ("GC1 Comdty", bars(&[(1, 3000.0), (2, 3100.0), (3, 3110.0)])),
        ]);
        Synthetic::new(Arc::new(m))
    }

    #[tokio::test]
    async fn history_joins_legs_and_reports_missing_ones() {
        let s = synth();
        let b = s.history("=XU100/USDTRY", Timeframe::D1, None, 100).await.unwrap();
        // Day 2 only: day 3 divides by zero, days 1 and 4 miss a leg.
        assert_eq!(b.len(), 1);
        assert!((b[0].close - 275.0).abs() < 1e-9 && b[0].volume == 0.0);
        let g = s.history("GRAMALTIN", Timeframe::D1, None, 100).await.unwrap();
        assert!((g[0].close - 3100.0 * 40.0 / 31.1035).abs() < 1e-9);
        let e = s.history("=XU100/BTCUSD", Timeframe::D1, None, 10).await.unwrap_err().to_string();
        assert!(e.contains("BTCUSD Curncy"), "{e}");
        let e = s.history("=XU100/", Timeframe::D1, None, 10).await.unwrap_err().to_string();
        assert!(e.contains("bad formula"), "{e}");
        // Plain keys pass through.
        assert_eq!(s.history("GC1 Comdty", Timeframe::D1, None, 2).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn quotes_derive_from_legs() {
        let s = synth();
        let q = s.quotes(&["=XU100/USDTRY".into(), "GC1 Comdty".into()]).await.unwrap();
        assert_eq!(q.len(), 2);
        let g = q.iter().find(|q| q.symbol == "GC1 Comdty").unwrap();
        assert_eq!(g.last, 3110.0);
        // USDTRY's latest close is 41 (day 4), XU100's 12000; prev 0 -> division skipped.
        let x = q.iter().find(|q| q.symbol == "=XU100/USDTRY").unwrap();
        assert!((x.last - 12_000.0 / 41.0).abs() < 1e-9, "{}", x.last);
    }
}
