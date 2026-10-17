//! Watchlist extra columns: which are shown per list, how the list is sorted, and the
//! statistics behind them. Everything is computed from daily history (RSI, ATR%, 52 week
//! position, volume) plus Yahoo's quote statistics (market cap, 3M average volume); a value
//! the sources cannot supply stays `None` and the table shows "n/a".

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tr_core::indicators::rsi;
use tr_core::indicators_ext::atr;
use tr_core::{Bar, Timeframe};
use tr_data::MarketData;
use tr_data::qstats::QuoteStats;

/// A column the user can add to a watchlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Col {
    RelVol,
    Rsi,
    AtrPct,
    MarketCap,
    Range52,
    Volume,
}

impl Col {
    pub const ALL: [Col; 6] = [Col::RelVol, Col::Rsi, Col::AtrPct, Col::MarketCap, Col::Range52, Col::Volume];

    pub fn label(self) -> &'static str {
        match self {
            Col::RelVol => "RelVol",
            Col::Rsi => "RSI",
            Col::AtrPct => "ATR%",
            Col::MarketCap => "MCap",
            Col::Range52 => "52W",
            Col::Volume => "Volume",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Col::RelVol => "Relative volume",
            Col::Rsi => "RSI (14)",
            Col::AtrPct => "ATR (14) % of price",
            Col::MarketCap => "Market cap",
            Col::Range52 => "52-week range position",
            Col::Volume => "Day volume",
        }
    }

    pub fn tip(self) -> &'static str {
        match self {
            Col::RelVol => {
                "Day volume over the 3-month average daily volume (Yahoo; else the mean of the last 63 sessions)"
            }
            Col::Rsi => "14-day RSI, Wilder smoothing, daily bars",
            Col::AtrPct => "14-day average true range as a percent of the last close",
            Col::MarketCap => "Market capitalisation (Yahoo)",
            Col::Range52 => "Where the last close sits between the 52-week low (0%) and high (100%)",
            Col::Volume => "Shares traded in the latest session",
        }
    }
}

/// What the table is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Security,
    Last,
    Change,
    Pct,
    Col(Col),
}

/// One list's columns and ordering.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListCols {
    #[serde(default)]
    pub cols: Vec<Col>,
    /// Sort key and whether it is descending; `None` keeps the list's own order.
    #[serde(default)]
    pub sort: Option<(SortKey, bool)>,
}

impl ListCols {
    pub fn toggle(&mut self, c: Col) {
        match self.cols.iter().position(|x| *x == c) {
            Some(i) => {
                self.cols.remove(i);
                if self.sort.is_some_and(|(k, _)| k == SortKey::Col(c)) {
                    self.sort = None;
                }
            }
            None => {
                self.cols.push(c);
                self.cols.sort();
            }
        }
    }

    /// Header click: sort by `key`, then reverse, then back to the list's own order. Numbers
    /// start descending (largest first), names ascending.
    pub fn click(&mut self, key: SortKey) {
        let first = (key, key != SortKey::Security);
        self.sort = match self.sort {
            Some((k, desc)) if k == key => (desc == first.1).then_some((key, !desc)),
            _ => Some(first),
        };
    }
}

/// Statistics for one security.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub rsi: Option<f64>,
    pub atr_pct: Option<f64>,
    /// 0..=1 position in the 52-week range.
    pub range52: Option<f64>,
    pub volume: Option<f64>,
    pub rel_vol: Option<f64>,
    pub market_cap: Option<f64>,
}

impl Stats {
    pub fn get(&self, c: Col) -> Option<f64> {
        match c {
            Col::RelVol => self.rel_vol,
            Col::Rsi => self.rsi,
            Col::AtrPct => self.atr_pct,
            Col::MarketCap => self.market_cap,
            Col::Range52 => self.range52,
            Col::Volume => self.volume,
        }
    }
}

const YEAR_SECS: i64 = 365 * 86_400;
/// Sessions in three months, for the fallback average volume.
const SESSIONS_3M: usize = 63;

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// Statistics from daily bars (oldest first), refined by Yahoo's quote statistics.
pub fn compute(bars: &[Bar], q: Option<&QuoteStats>) -> Stats {
    let Some(last) = bars.last() else { return Stats::default() };
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let mut s = Stats {
        rsi: rsi(&closes, 14).last().copied().and_then(finite),
        atr_pct: atr(bars, 14)
            .last()
            .copied()
            .and_then(finite)
            .filter(|_| last.close > 0.0)
            .map(|a| a / last.close * 100.0),
        ..Stats::default()
    };

    // 52-week position: the Yahoo range when it has one, else the year of bars we hold (only
    // if they really span most of a year, so a new listing shows n/a rather than a short range).
    let year: Vec<&Bar> = bars.iter().filter(|b| b.ts > last.ts - YEAR_SECS).collect();
    let spans_year = year.first().is_some_and(|f| last.ts - f.ts >= 300 * 86_400);
    let own = spans_year.then(|| {
        (year.iter().map(|b| b.low).fold(f64::MAX, f64::min), year.iter().map(|b| b.high).fold(f64::MIN, f64::max))
    });
    let range = q.and_then(|q| q.low_52w.zip(q.high_52w)).or(own);
    s.range52 = range.filter(|(lo, hi)| hi > lo).map(|(lo, hi)| ((last.close - lo) / (hi - lo)).clamp(0.0, 1.0));

    s.volume = q.and_then(|q| q.volume).or(Some(last.volume).filter(|v| *v > 0.0));
    s.market_cap = q.and_then(|q| q.market_cap);
    let avg = q.and_then(|q| q.avg_volume_3m).or_else(|| {
        let prior: Vec<f64> = bars[..bars.len() - 1].iter().rev().take(SESSIONS_3M).map(|b| b.volume).collect();
        (prior.len() >= 20 && prior.iter().all(|v| *v > 0.0)).then(|| prior.iter().sum::<f64>() / prior.len() as f64)
    });
    s.rel_vol = s.volume.zip(avg).filter(|(_, a)| *a > 0.0).map(|(v, a)| v / a);
    s
}

/// Compact number: `3.10T`, `415.2B`, `12.3M`, `850K`, `42`.
pub fn compact(v: f64) -> String {
    match v.abs() {
        x if x >= 1e12 => format!("{:.2}T", v / 1e12),
        x if x >= 1e9 => format!("{:.1}B", v / 1e9),
        x if x >= 1e6 => format!("{:.1}M", v / 1e6),
        x if x >= 1e3 => format!("{:.0}K", v / 1e3),
        _ => format!("{v:.0}"),
    }
}

/// Cell text for a column; `None` renders as n/a.
pub fn format(c: Col, v: f64) -> String {
    match c {
        Col::RelVol => format!("{v:.2}x"),
        Col::Rsi => format!("{v:.1}"),
        Col::AtrPct => format!("{v:.2}%"),
        Col::MarketCap | Col::Volume => compact(v),
        Col::Range52 => format!("{:.0}%", v * 100.0),
    }
}

// ---- app state ---------------------------------------------------------------------------

/// How long statistics stay fresh, and how long a request may be outstanding.
const FRESH: Duration = Duration::from_secs(15 * 60);
const PENDING: Duration = Duration::from_secs(90);
/// Securities per request; the worker runs [`CONCURRENCY`] histories at a time.
const BATCH: usize = 12;
const CONCURRENCY: usize = 3;

/// Settings key of the main watchlist; sidebar lists use their name.
pub const MAIN: &str = "";

#[derive(Default)]
pub struct State {
    /// Columns per list (persisted in `settings.json`).
    pub lists: BTreeMap<String, ListCols>,
    stats: HashMap<String, (Stats, Instant)>,
    asked: HashMap<String, Instant>,
}

impl State {
    pub fn list(&self, name: &str) -> ListCols {
        self.lists.get(name).cloned().unwrap_or_default()
    }

    pub fn edit(&mut self, name: &str, f: impl FnOnce(&mut ListCols)) {
        let l = self.lists.entry(name.to_string()).or_default();
        f(l);
        if *l == ListCols::default() {
            self.lists.remove(name);
        }
    }

    pub fn get(&self, key: &str) -> Option<&Stats> {
        self.stats.get(key).map(|(s, _)| s)
    }

    pub fn pending(&self, key: &str) -> bool {
        self.asked.get(key).is_some_and(|t| t.elapsed() < PENDING)
    }

    /// The next batch of securities among `keys` that need (re)fetching.
    pub fn due(&mut self, keys: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for k in keys {
            let fresh = self.stats.get(k).is_some_and(|(_, t)| t.elapsed() < FRESH);
            if !fresh && !self.pending(k) && !out.contains(k) {
                out.push(k.clone());
                if out.len() == BATCH {
                    break;
                }
            }
        }
        for k in &out {
            self.asked.insert(k.clone(), Instant::now());
        }
        out
    }

    pub fn store(&mut self, rows: Vec<(String, Stats)>) {
        for (k, s) in rows {
            self.asked.remove(&k);
            self.stats.insert(k, (s, Instant::now()));
        }
    }
}

// ---- worker ------------------------------------------------------------------------------

pub enum WlReq {
    Fetch(Vec<String>),
}

pub enum WlResp {
    Stats(Vec<(String, Stats)>),
}

/// Daily bars requested per security: over a year of sessions for the 52-week range.
const HISTORY: usize = 400;

pub async fn handle(req: WlReq, provider: &Arc<dyn MarketData>, send: &(dyn Fn(WlResp) + Sync)) {
    use futures::StreamExt;
    let WlReq::Fetch(keys) = req;
    // One statistics request for the batch; a failure only costs market cap and the like.
    let quote = match provider.quote_stats(&keys).await {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!("watchlist quote statistics: {e:#}");
            Vec::new()
        }
    };
    let quote = &quote;
    let mut rows = futures::stream::iter(keys.into_iter().map(|k| async move {
        let bars = provider.real_history(&k, Timeframe::D1, HISTORY).await;
        let stats = match bars {
            Ok(b) if !b.is_empty() => compute(&b, quote.iter().find(|q| q.symbol == k)),
            Ok(_) => Stats::default(),
            Err(e) => {
                tracing::debug!("watchlist history {k}: {e:#}");
                // Without history the quote statistics still give market cap and volume.
                match quote.iter().find(|q| q.symbol == k) {
                    Some(q) => Stats { market_cap: q.market_cap, volume: q.volume, ..Stats::default() },
                    None => Stats::default(),
                }
            }
        };
        (k, stats)
    }))
    .buffer_unordered(CONCURRENCY);
    while let Some(row) = rows.next().await {
        send(WlResp::Stats(vec![row]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts_days: i64, h: f64, l: f64, c: f64, v: f64) -> Bar {
        Bar::new(ts_days * 86_400, c, h, l, c, v)
    }

    /// 330 daily bars rising 1 per day from 100 with a 2-wide range; volume 1000 except the
    /// last day's 3000.
    fn rising() -> Vec<Bar> {
        (0..330)
            .map(|i| {
                bar(i, 101.0 + i as f64, 99.0 + i as f64, 100.0 + i as f64, if i == 329 { 3000.0 } else { 1000.0 })
            })
            .collect()
    }

    #[test]
    fn stats_from_history() {
        let s = compute(&rising(), None);
        assert_eq!(s.rsi, Some(100.0)); // only gains
        // Every true range is 2 (the high-low range; the gaps to the previous close are 2 and 0).
        let atr_pct = s.atr_pct.unwrap();
        assert!((atr_pct - 2.0 / 429.0 * 100.0).abs() < 1e-6, "{atr_pct}");
        // 52 weeks: low 99 (first bar), high 430; close 429 -> (429 - 99) / (430 - 99).
        assert!((s.range52.unwrap() - 330.0 / 331.0).abs() < 1e-9);
        assert_eq!(s.volume, Some(3000.0));
        assert_eq!(s.rel_vol, Some(3.0)); // 3000 over the mean 1000 of the prior 63 sessions
        assert_eq!(s.market_cap, None);
    }

    #[test]
    fn quote_statistics_win_when_present() {
        let q = QuoteStats {
            symbol: "X".into(),
            market_cap: Some(2.5e9),
            volume: Some(4000.0),
            avg_volume_3m: Some(2000.0),
            high_52w: Some(500.0),
            low_52w: Some(300.0),
        };
        let s = compute(&rising(), Some(&q));
        assert_eq!(s.rel_vol, Some(2.0));
        assert_eq!(s.market_cap, Some(2.5e9));
        assert!((s.range52.unwrap() - 0.645).abs() < 1e-9); // (429 - 300) / 200
    }

    #[test]
    fn short_history_is_na_not_wrong() {
        let bars: Vec<Bar> = (0..40).map(|i| bar(i, 11.0, 9.0, 10.0, 0.0)).collect();
        let s = compute(&bars, None);
        assert_eq!(s.range52, None, "40 days do not make a 52-week range");
        assert_eq!(s.volume, None, "no volume reported");
        assert_eq!(s.rel_vol, None);
        assert_eq!(compute(&[], None), Stats::default());
    }

    #[test]
    fn rel_volume_needs_a_real_average() {
        let mut bars = rising();
        bars.truncate(15); // fewer than 20 prior sessions
        assert_eq!(compute(&bars, None).rel_vol, None);
    }

    #[test]
    fn toggling_and_sorting_columns() {
        let mut l = ListCols::default();
        l.toggle(Col::Volume);
        l.toggle(Col::Rsi);
        assert_eq!(l.cols, vec![Col::Rsi, Col::Volume], "kept in column order");
        l.click(SortKey::Col(Col::Rsi));
        assert_eq!(l.sort, Some((SortKey::Col(Col::Rsi), true)));
        l.click(SortKey::Col(Col::Rsi));
        assert_eq!(l.sort, Some((SortKey::Col(Col::Rsi), false)));
        l.click(SortKey::Col(Col::Rsi));
        assert_eq!(l.sort, None);
        l.click(SortKey::Security);
        assert_eq!(l.sort, Some((SortKey::Security, false)));
        l.click(SortKey::Col(Col::Volume));
        l.toggle(Col::Volume);
        assert_eq!(l.sort, None, "hiding the sorted column clears the sort");
    }

    #[test]
    fn formats() {
        assert_eq!(compact(3.1e12), "3.10T");
        assert_eq!(compact(415_200_000_000.0), "415.2B");
        assert_eq!(compact(850_000.0), "850K");
        assert_eq!(format(Col::Range52, 0.784), "78%");
        assert_eq!(format(Col::RelVol, 1.5), "1.50x");
    }

    #[test]
    fn fetch_scheduling_is_polite() {
        let mut st = State::default();
        let keys: Vec<String> = (0..30).map(|i| format!("S{i}")).collect();
        let first = st.due(&keys);
        assert_eq!(first.len(), BATCH);
        let second = st.due(&keys);
        assert!(second.iter().all(|k| !first.contains(k)), "in-flight keys are not asked twice");
        st.store(first.iter().map(|k| (k.clone(), Stats::default())).collect());
        assert!(st.get(&first[0]).is_some());
        assert!(!st.due(&first).contains(&first[0]), "fresh stats are not refetched");
    }
}
