//! Background worker. All network, SQLite, indicator and LOD-pyramid work runs on a
//! tokio runtime; the UI thread only sends [`Request`]s and drains [`Response`]s, so it
//! never blocks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use chrono::Datelike;
use tokio::sync::Mutex;
use tr_chart::gpu::{LineKind, LineRef};
use tr_chart::scene::{Overlay, SeriesGpu, line_ref};
use tr_congress::db::{Alert, PoliticianSummary};
use tr_congress::performance::{self, Performance, TradeReturn};
use tr_congress::refresh::{self, HttpSource, NewTradeAlert, RefreshConfig};
use tr_congress::{Db, Politician, Trade};
use tr_core::{Bar, Security, Series, Timeframe, indicators};
use tr_data::{BarCache, Provider, Quote, SearchHit, YieldCurves};

use crate::alerts::{self, AlertBook, Ext, Market, RuleOp};
use crate::command::IndicatorSpec;
use crate::theme;

pub type PaneId = u64;

/// Minutes between Congress disclosure checks (`TR_CONGRESS_REFRESH_MIN`; 0 disables).
pub const CONGRESS_REFRESH_MIN: u64 = 30;

/// Seconds between price-alert quote checks (`TR_ALERT_POLL_SECS`; 0 disables polling, live
/// ticks still count).
pub const ALERT_POLL_SECS: u64 = 15;

/// Daily closes behind indicator alerts are refetched this often.
const ALERT_CLOSES_TTL: Duration = Duration::from_secs(600);

pub enum Request {
    /// Load the brokerage portfolio through SnapTrade (`None` = the simulated sample).
    Portfolio(Option<tr_broker::Credentials>),
    /// Register a SnapTrade user under the partner keys.
    BrokerRegister {
        creds: tr_broker::Credentials,
        user_id: String,
    },
    /// Generate a read-only Connection Portal link (optionally to reconnect a connection).
    BrokerPortal {
        creds: tr_broker::Credentials,
        dark: bool,
        reconnect: Option<String>,
    },
    /// Remove a brokerage connection at the user's explicit request.
    BrokerDisconnect {
        creds: tr_broker::Credentials,
        id: String,
    },
    /// Option chain for one expiration (`None` = nearest).
    Options {
        pane: PaneId,
        symbol: String,
        expiration: Option<i64>,
    },
    /// Headlines for a market key (`None` = general market news).
    News {
        pane: PaneId,
        symbol: Option<String>,
    },
    OrderBook {
        pane: PaneId,
        symbol: String,
        depth: usize,
    },
    History {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        limit: usize,
        /// Display currency (`None` = as quoted).
        currency: Option<String>,
    },
    /// Prepend older history to `existing`.
    MoreHistory {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        existing: Arc<Vec<Bar>>,
        limit: usize,
        currency: Option<String>,
    },
    Indicators {
        pane: PaneId,
        generation: u64,
        bars: Arc<Vec<Bar>>,
        tf: Timeframe,
        specs: Vec<IndicatorSpec>,
        /// Colours are baked into overlays, so the UI passes its current palette.
        palette: theme::Palette,
    },
    Compare {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        base: Arc<Vec<Bar>>,
    },
    /// Plug-in studies ([`crate::studies`]) on a chart's bars.
    Studies {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        bars: Arc<Vec<Bar>>,
        configs: Vec<crate::studies::StudyConfig>,
    },
    Markers {
        pane: PaneId,
        symbol: String,
    },
    Quotes(Vec<String>),
    Latest {
        limit: usize,
    },
    TickerTrades {
        pane: PaneId,
        symbol: String,
    },
    Profile {
        pane: PaneId,
        query: String,
    },
    SearchPoliticians(String),
    Track {
        query: String,
        on: bool,
    },
    Alerts,
    /// Marks Congress and price alerts read.
    MarkAlertsSeen,
    /// Change the price/indicator alert rules ([`crate::alerts`]).
    AlertRules(RuleOp),
    RefreshCongress,
    /// A pane closed or changed: stop its live feed and drop its series.
    Forget(PaneId),
    YieldCurves,
    Search(String),
    /// Order entry (see `app::trading`); only sent once trading is enabled.
    Trade(TradeRequest),
    /// SEC EDGAR profile and statements for `DES` / `FA` (`user_agent` from Settings).
    Edgar {
        pane: PaneId,
        symbol: String,
        user_agent: String,
    },
    /// Earnings calendar and history (see `earnings_worker`).
    Earnings(crate::earnings_worker::EarnReq),
    /// Economic calendar week or release history (`ECO`, `ECOF`).
    Eco {
        pane: PaneId,
        req: crate::eco::EcoRequest,
    },
}

/// A trading call through SnapTrade. Previews never place anything; placing takes a preview
/// the user confirmed.
pub enum TradeRequest {
    Preview {
        creds: tr_broker::Credentials,
        req: tr_broker::trading::OrderRequest,
        ticket: u64,
    },
    Place {
        creds: tr_broker::Credentials,
        preview: Box<tr_broker::trading::OrderPreview>,
        ticket: u64,
    },
    Cancel {
        creds: tr_broker::Credentials,
        account: String,
        order: String,
    },
    /// Connection Portal link requesting trading access.
    Portal {
        creds: tr_broker::Credentials,
        dark: bool,
    },
    /// Which brokerage connections can trade (brokerage support and connection access): one
    /// `/authorizations` request, however many accounts there are.
    Capabilities {
        creds: tr_broker::Credentials,
    },
}

pub enum TradeResponse {
    Preview {
        ticket: u64,
        result: Result<Box<tr_broker::trading::OrderPreview>, String>,
    },
    Placed {
        ticket: u64,
        preview: Box<tr_broker::trading::OrderPreview>,
        result: Result<Option<Box<tr_broker::Order>>, tr_broker::trading::PlaceError>,
    },
    Cancelled {
        account: String,
        order: String,
        result: Result<(), String>,
    },
    Portal(Result<String, String>),
    /// Capabilities by connection (SnapTrade authorization) id.
    Capabilities(Result<std::collections::HashMap<String, tr_broker::trading::Capability>, CapsError>),
}

/// Why trading access could not be read. A rate limit is reported apart from real errors so
/// the app can back off quietly instead of showing an error.
#[derive(Debug, Clone, PartialEq)]
pub struct CapsError {
    pub message: String,
    pub throttled: bool,
    pub retry_after: Option<Duration>,
}

pub struct BuiltIndicator {
    pub spec: IndicatorSpec,
    pub overlays: Vec<(String, Overlay)>,
}

pub struct Profile {
    pub politician: Politician,
    pub trades: Vec<Trade>,
    pub perf: Performance,
    pub returns: Vec<TradeReturn>,
    pub tracked: bool,
}

pub enum Response {
    Portfolio(Result<Box<tr_broker::Portfolio>, String>),
    /// `(userId, userSecret)` for a newly registered SnapTrade user.
    BrokerRegistered(Result<(String, String), String>),
    BrokerPortal(Result<String, String>),
    /// The connection id removed, or why not.
    BrokerDisconnected(Result<String, String>),
    Options {
        pane: PaneId,
        symbol: String,
        chain: Result<Box<tr_data::OptionChain>, String>,
    },
    News {
        pane: PaneId,
        symbol: Option<String>,
        items: Result<Vec<tr_data::NewsItem>, String>,
    },
    OrderBook {
        pane: PaneId,
        symbol: String,
        book: Result<tr_data::OrderBook, String>,
    },
    History {
        pane: PaneId,
        generation: u64,
        symbol: String,
        tf: Timeframe,
        bars: Arc<Vec<Bar>>,
        gpu: SeriesGpu,
        /// Number of bars prepended (paging), 0 otherwise.
        prepended: usize,
        from_cache: bool,
        exhausted: bool,
        /// A live-feed update of the latest bar.
        live: bool,
    },
    HistoryError {
        pane: PaneId,
        generation: u64,
        error: String,
    },
    Indicators {
        pane: PaneId,
        generation: u64,
        built: Vec<BuiltIndicator>,
    },
    Studies {
        pane: PaneId,
        generation: u64,
        built: Vec<crate::studies::BuiltStudy>,
    },
    Compare {
        pane: PaneId,
        generation: u64,
        symbol: String,
        line: LineRef,
    },
    Markers {
        pane: PaneId,
        symbol: String,
        trades: Vec<Trade>,
    },
    Quotes(Vec<Quote>),
    Latest(Vec<Trade>),
    TickerTrades {
        pane: PaneId,
        symbol: String,
        trades: Vec<Trade>,
    },
    Profile {
        pane: PaneId,
        profile: Option<Box<Profile>>,
        query: String,
    },
    Politicians(Vec<PoliticianSummary>),
    Alerts(Vec<Alert>),
    NewAlerts(Vec<NewTradeAlert>),
    /// The price-alert rule book after a change.
    AlertBook(Box<AlertBook>),
    /// Price/indicator rules that just fired.
    PriceAlerts(Vec<alerts::Fired>),
    Status(String),
    CongressRefreshed(String),
    Curves(Box<YieldCurves>),
    SearchHits {
        query: String,
        hits: Vec<SearchHit>,
    },
    Trade(Box<TradeResponse>),
    Edgar {
        pane: PaneId,
        symbol: String,
        company: Result<Arc<tr_data::edgar::Company>, String>,
    },
    Earnings(crate::earnings_worker::EarnResp),
    Eco {
        pane: PaneId,
        resp: crate::eco::EcoResponse,
    },
}

pub struct Worker {
    tx: tokio::sync::mpsc::UnboundedSender<Request>,
    pub rx: mpsc::Receiver<Response>,
    _rt: tokio::runtime::Runtime,
}

/// Bars per pane tagged with the load generation they belong to.
type SeriesMap = HashMap<PaneId, (u64, Arc<Vec<Bar>>)>;

struct Shared {
    provider: Provider,
    /// Authoritative bars per chart pane (history, paging and live ticks all merge here),
    /// tagged with the pane's load generation.
    series: std::sync::Mutex<SeriesMap>,
    cache: std::sync::Mutex<Option<BarCache>>,
    db: Mutex<Db>,
    alerts: std::sync::Mutex<PriceAlerts>,
    out: mpsc::Sender<Response>,
    ctx: egui::Context,
}

/// Price-alert rules and the market data they are evaluated against.
#[derive(Default)]
struct PriceAlerts {
    book: AlertBook,
    /// Where the book is saved (`None` in tests).
    path: Option<PathBuf>,
    markets: HashMap<String, Market>,
    closes_at: HashMap<String, std::time::Instant>,
}

impl PriceAlerts {
    fn save(&self) {
        if let Some(path) = &self.path
            && let Err(e) = self.book.save(path)
        {
            tracing::warn!("saving alert rules: {e}");
        }
    }
}

impl Shared {
    fn send(&self, r: Response) {
        let _ = self.out.send(r);
        self.ctx.request_repaint();
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TR_DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/terminal-rs")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("terminal-rs")
    }
}

impl Worker {
    pub fn spawn(ctx: egui::Context, provider: Provider) -> anyhow::Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let db = Db::open(dir.join("congress.sqlite3")).or_else(|e| {
            tracing::warn!("congress db unavailable ({e:#}); using in-memory store");
            Db::open_in_memory()
        })?;
        let cache =
            BarCache::open(dir.join("bars.sqlite3")).map_err(|e| tracing::warn!("bar cache unavailable: {e:#}")).ok();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("tr-worker")
            .build()?;
        let (tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (out, rx) = mpsc::channel();
        let alerts_path = dir.join("alerts.json");
        let book = AlertBook::load(&alerts_path);
        let _ = out.send(Response::AlertBook(Box::new(book.clone())));
        let shared = Arc::new(Shared {
            provider,
            series: Default::default(),
            cache: std::sync::Mutex::new(cache),
            db: Mutex::new(db),
            alerts: std::sync::Mutex::new(PriceAlerts { book, path: Some(alerts_path), ..Default::default() }),
            out,
            ctx,
        });

        // Request dispatcher: every request runs as its own task.
        let s = shared.clone();
        rt.spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let s = s.clone();
                tokio::spawn(async move { handle(&s, req).await });
            }
        });

        // Scheduled congressional disclosure refresh.
        let s = shared.clone();
        rt.spawn(async move {
            let minutes: u64 = std::env::var("TR_CONGRESS_REFRESH_MIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(CONGRESS_REFRESH_MIN);
            if minutes == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut tick = tokio::time::interval(Duration::from_secs(minutes * 60));
            loop {
                tick.tick().await;
                congress_refresh(&s).await;
            }
        });

        // Price alerts: quote the securities with rules on a timer (live ticks and the
        // app's own quote requests are checked as they arrive too).
        let s = shared.clone();
        rt.spawn(async move {
            let secs: u64 =
                std::env::var("TR_ALERT_POLL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(ALERT_POLL_SECS);
            if secs == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut tick = tokio::time::interval(Duration::from_secs(secs));
            loop {
                tick.tick().await;
                poll_alerts(&s).await;
            }
        });
        Ok(Self { tx, rx, _rt: rt })
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

async fn congress_refresh(s: &Shared) {
    s.send(Response::Status("Refreshing congressional disclosures (Senate eFD + House Clerk)…".into()));
    let source = match HttpSource::new() {
        Ok(src) => src,
        Err(e) => return s.send(Response::Status(format!("congress refresh failed: {e:#}"))),
    };
    let cfg = RefreshConfig::incremental(&*s.db.lock().await);
    match refresh::refresh(&source, &s.db, &cfg).await {
        Ok(report) => {
            if !report.alerts.is_empty() {
                s.send(Response::NewAlerts(report.alerts.clone()));
            }
            s.send(Response::CongressRefreshed(report.summary()));
        }
        Err(e) => s.send(Response::Status(format!("congress refresh failed: {e:#}"))),
    }
}

/// Update what is known about `symbol`, evaluate its alert rules and publish what fired.
fn alert_update(s: &Shared, symbol: &str, update: impl FnOnce(&mut Market)) {
    let now = chrono::Utc::now().timestamp();
    let (fired, book) = {
        let mut a = s.alerts.lock().expect("alerts lock");
        if !a.book.watches(symbol) {
            return;
        }
        let a = &mut *a;
        let m = a.markets.entry(symbol.to_string()).or_default();
        update(m);
        let (fired, changed) = a.book.evaluate(symbol, m, now);
        if changed {
            a.save();
        }
        (fired, changed.then(|| a.book.clone()))
    };
    if let Some(book) = book {
        if book.notify {
            for f in &fired {
                let msg = f.message.clone();
                tokio::task::spawn_blocking(move || tr_macos::notify("terminal-rs alert", &msg));
            }
        }
        if !fired.is_empty() {
            s.send(Response::PriceAlerts(fired));
        }
        s.send(Response::AlertBook(Box::new(book)));
    }
}

fn alert_quotes(s: &Shared, quotes: &[Quote]) {
    for q in quotes {
        alert_update(s, &q.symbol, |m| {
            m.last = Some(q.last);
            m.prev_close = (q.prev_close > 0.0).then_some(q.prev_close);
        });
    }
}

/// Quote every security with an enabled rule, refreshing the daily closes and extended-hours
/// prices the rules need. Real sources only: alerts never fire on sample fallback data.
async fn poll_alerts(s: &Shared) {
    let watched = s.alerts.lock().expect("alerts lock").book.watched();
    if watched.is_empty() {
        return;
    }
    let symbols: Vec<String> = watched.iter().map(|(k, ..)| k.clone()).collect();
    match s.provider.real_quotes(&symbols).await {
        Ok(q) => alert_quotes(s, &q),
        Err(e) => tracing::debug!("alert quotes: {e:#}"),
    }
    for (symbol, closes, ext) in watched {
        let stale = || {
            s.alerts.lock().expect("alerts lock").closes_at.get(&symbol).is_none_or(|t| t.elapsed() > ALERT_CLOSES_TTL)
        };
        if closes && stale() {
            match s.provider.real_history(&symbol, Timeframe::D1, 400).await {
                Ok(bars) if !bars.is_empty() => {
                    s.alerts.lock().expect("alerts lock").closes_at.insert(symbol.clone(), std::time::Instant::now());
                    alert_update(s, &symbol, |m| m.closes = bars.iter().map(|b| b.close).collect());
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("alert history {symbol}: {e:#}"),
            }
        }
        if ext {
            let state = match s.provider.extended(&symbol).await {
                Ok(Some(q)) => q.move_pct().map_or(Ext::Regular, Ext::Move),
                Ok(None) => Ext::Unknown,
                Err(e) => {
                    tracing::debug!("alert extended hours {symbol}: {e:#}");
                    continue;
                }
            };
            alert_update(s, &symbol, |m| m.ext = state);
        }
    }
}

fn session_key(tf: Timeframe) -> impl Fn(i64) -> i64 {
    move |ts| {
        let d = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default();
        match tf {
            t if t.is_intraday() => ts.div_euclid(86_400),
            Timeframe::D1 => (d.year() * 12 + d.month() as i32) as i64,
            _ => d.year() as i64,
        }
    }
}

fn overlay(values: &[f64], color: egui::Color32, kind: LineKind) -> Overlay {
    Overlay { line: line_ref(values), color: theme::rgba(color), kind, scale: 1.0 }
}

pub fn build_indicators(
    bars: &[Bar],
    tf: Timeframe,
    specs: &[IndicatorSpec],
    palette: &theme::Palette,
) -> Vec<BuiltIndicator> {
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let c = palette.series[i % palette.series.len()];
            let overlays = match *spec {
                IndicatorSpec::Sma(n) => vec![(spec.label(), overlay(&indicators::sma(&closes, n), c, LineKind::Line))],
                IndicatorSpec::Ema(n) => vec![(spec.label(), overlay(&indicators::ema(&closes, n), c, LineKind::Line))],
                IndicatorSpec::Vwap => {
                    vec![(spec.label(), overlay(&indicators::vwap_by(bars, session_key(tf)), c, LineKind::Line))]
                }
                IndicatorSpec::Bollinger(n, k) => {
                    let b = indicators::bollinger(&closes, n, k);
                    let dim = c.gamma_multiply(0.7);
                    vec![
                        ("BB upper".into(), overlay(&b.upper, dim, LineKind::Line)),
                        ("BB basis".into(), overlay(&b.middle, c, LineKind::Line)),
                        ("BB lower".into(), overlay(&b.lower, dim, LineKind::Line)),
                    ]
                }
                IndicatorSpec::Rsi(n) => {
                    vec![(spec.label(), overlay(&indicators::rsi(&closes, n), palette.series[1], LineKind::Line))]
                }
                IndicatorSpec::Macd(f, sl, sg) => {
                    let m = indicators::macd(&closes, f, sl, sg);
                    vec![
                        ("Hist".into(), overlay(&m.histogram, egui::Color32::WHITE, LineKind::Hist)),
                        ("MACD".into(), overlay(&m.macd, palette.cyan, LineKind::Line)),
                        ("Signal".into(), overlay(&m.signal, palette.amber, LineKind::Line)),
                    ]
                }
            };
            BuiltIndicator { spec: *spec, overlays }
        })
        .collect()
}

/// Align `other`'s closes to `base` timestamps (last close at or before each bar). The
/// chart rebases the result to the first visible bar every frame.
pub fn align_compare(base: &[Bar], other: &[Bar]) -> Vec<f64> {
    let mut out = vec![f64::NAN; base.len()];
    let mut j = 0;
    for (i, b) in base.iter().enumerate() {
        while j + 1 < other.len() && other[j + 1].ts <= b.ts {
            j += 1;
        }
        if let Some(o) = other.get(j).filter(|o| o.ts <= b.ts) {
            out[i] = o.close;
        }
    }
    out
}

fn default_limit(tf: Timeframe) -> usize {
    match tf {
        Timeframe::M1 => 5_000,
        Timeframe::M5 | Timeframe::M15 | Timeframe::M30 => 5_000,
        Timeframe::H1 | Timeframe::H4 => 4_000,
        Timeframe::D1 => 6_000,
        Timeframe::W1 | Timeframe::MN1 => 2_000,
    }
}

/// Bars requested on first load; `TR_HISTORY_LIMIT` overrides (e.g. 1000000 with sample data).
pub fn initial_limit(tf: Timeframe) -> usize {
    std::env::var("TR_HISTORY_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| default_limit(tf))
}

async fn handle(s: &Arc<Shared>, req: Request) {
    match req {
        Request::History { pane, generation, symbol, tf, limit, currency } => {
            let convert_to = currency.filter(|c| *c != tr_data::native_currency(&symbol));
            if !claim_series(s, pane, generation) {
                return; // a newer load for this pane is already running
            }
            // Paint from the disk cache immediately, then refresh from the network.
            let cached = s.cache.lock().ok().and_then(|c| c.as_ref().and_then(|c| c.load(&symbol, tf, limit).ok()));
            if let Some(bars) = cached.filter(|b| !b.is_empty() && convert_to.is_none()) {
                let bars = Arc::new(bars);
                let gpu = build_gpu(bars.clone()).await;
                s.send(Response::History {
                    pane,
                    generation,
                    symbol: symbol.clone(),
                    tf,
                    bars,
                    gpu,
                    prepended: 0,
                    from_cache: true,
                    exhausted: false,
                    live: false,
                });
            }
            match s.provider.history(&symbol, tf, None, limit).await {
                Ok(bars) if !bars.is_empty() => {
                    if let Ok(mut c) = s.cache.lock()
                        && let Some(c) = c.as_mut()
                    {
                        let _ = c.store(&symbol, tf, &bars);
                    }
                    let exhausted = bars.len() < limit / 2;
                    let bars = match &convert_to {
                        Some(to) => {
                            let from = tr_data::native_currency(&symbol);
                            match tr_data::convert_bars(&*s.provider, bars, from, to, tf).await {
                                Ok(b) => b,
                                Err(e) => {
                                    let error = format!("{from}->{to} conversion: {e:#}");
                                    return s.send(Response::HistoryError { pane, generation, error });
                                }
                            }
                        }
                        None => bars,
                    };
                    let bars = Arc::new(bars);
                    if !store_series(s, pane, generation, bars.clone()) {
                        return; // superseded while loading
                    }
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol: symbol.clone(),
                        tf,
                        bars,
                        gpu,
                        prepended: 0,
                        from_cache: false,
                        exhausted,
                        live: false,
                    });
                    // Live ticks are quoted natively, so converted charts stay on history.
                    if convert_to.is_none() {
                        start_live(s.clone(), pane, generation, symbol, tf);
                    }
                }
                Ok(_) => s.send(Response::HistoryError { pane, generation, error: format!("no data for {symbol}") }),
                Err(e) => s.send(Response::HistoryError { pane, generation, error: format!("{e:#}") }),
            }
        }
        Request::MoreHistory { pane, generation, symbol, tf, existing, limit, currency } => {
            let convert_to = currency.filter(|c| *c != tr_data::native_currency(&symbol));
            // Page from the worker's copy (it may hold newer live bars than the UI's).
            let base = current_series(s, pane, generation).filter(|b| !b.is_empty()).unwrap_or(existing);
            let Some(first) = base.first().copied() else { return };
            match s.provider.history(&symbol, tf, Some(first.ts), limit).await {
                Ok(older) => {
                    let older: Vec<Bar> = older.into_iter().filter(|b| b.ts < first.ts).collect();
                    let older = match &convert_to {
                        Some(to) => {
                            let from = tr_data::native_currency(&symbol);
                            match tr_data::convert_bars(&*s.provider, older, from, to, tf).await {
                                Ok(b) => b,
                                Err(e) => {
                                    let error = format!("{from}->{to} conversion: {e:#}");
                                    return s.send(Response::HistoryError { pane, generation, error });
                                }
                            }
                        }
                        None => older,
                    };
                    let prepended = older.len();
                    let bars = {
                        let mut map = s.series.lock().expect("series lock");
                        let Some((g, cur)) = map.get_mut(&pane).filter(|(g, _)| *g == generation) else { return };
                        let _ = g;
                        let mut merged = older;
                        merged.extend_from_slice(cur);
                        *cur = Arc::new(merged);
                        cur.clone()
                    };
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol,
                        tf,
                        bars,
                        gpu,
                        prepended,
                        from_cache: false,
                        exhausted: prepended == 0,
                        live: false,
                    });
                }
                Err(e) => s.send(Response::HistoryError { pane, generation, error: format!("{e:#}") }),
            }
        }
        Request::Indicators { pane, generation, bars, tf, specs, palette } => {
            let built = tokio::task::spawn_blocking(move || build_indicators(&bars, tf, &specs, &palette))
                .await
                .unwrap_or_default();
            s.send(Response::Indicators { pane, generation, built });
        }
        Request::Studies { pane, generation, symbol, tf, bars, configs } => {
            let built = tokio::task::spawn_blocking(move || crate::studies::build(&symbol, tf, &bars, &configs))
                .await
                .unwrap_or_default();
            s.send(Response::Studies { pane, generation, built });
        }
        Request::Compare { pane, generation, symbol, tf, base } => {
            let limit = base.len().max(100);
            match s.provider.history(&symbol, tf, None, limit).await {
                Ok(other) => {
                    let line = tokio::task::spawn_blocking(move || line_ref(&align_compare(&base, &other))).await;
                    if let Ok(line) = line {
                        s.send(Response::Compare { pane, generation, symbol, line });
                    }
                }
                Err(e) => s.send(Response::Status(format!("compare {symbol}: {e:#}"))),
            }
        }
        Request::Markers { pane, symbol } => {
            let sec = tr_data::security(&symbol);
            let trades = if sec.has_congress_trades() {
                s.db.lock().await.trades_for_ticker(&sec.ticker, 2_000).unwrap_or_default()
            } else {
                Vec::new()
            };
            s.send(Response::Markers { pane, symbol, trades });
        }
        Request::Quotes(symbols) => {
            // Real quotes feed the alert rules; sample stand-ins (offline) only fill the panels.
            let quotes = match s.provider.real_quotes(&symbols).await {
                Ok(q) if !q.is_empty() => {
                    alert_quotes(s, &q);
                    Ok(q)
                }
                _ => s.provider.quotes(&symbols).await,
            };
            match quotes {
                Ok(q) => s.send(Response::Quotes(q)),
                Err(e) => s.send(Response::Status(format!("quotes: {e:#}"))),
            }
        }
        Request::Latest { limit } => {
            let t = s.db.lock().await.latest_trades(limit).unwrap_or_default();
            s.send(Response::Latest(t));
        }
        Request::TickerTrades { pane, symbol } => {
            let ticker = Security::parse(&symbol).map(|x| x.ticker).unwrap_or_else(|| symbol.clone());
            let trades = s.db.lock().await.trades_for_ticker(&ticker, 1_000).unwrap_or_default();
            s.send(Response::TickerTrades { pane, symbol, trades });
        }
        Request::Profile { pane, query } => {
            let profile = profile(s, &query).await;
            s.send(Response::Profile { pane, profile: profile.map(Box::new), query });
        }
        Request::SearchPoliticians(q) => {
            let r = s.db.lock().await.search_politicians(&q, 500).unwrap_or_default();
            s.send(Response::Politicians(r));
        }
        Request::Track { query, on } => {
            let db = s.db.lock().await;
            let hit = resolve_politician(&db, &query);
            match hit {
                Some(p) => {
                    let _ = db.set_tracked(p.id, on);
                    let verb = if on { "Tracking" } else { "Stopped tracking" };
                    s.send(Response::Status(format!("{verb} {} — alerts fire when they file a new PTR", p.name)));
                }
                None => s.send(Response::Status(format!("No politician matches '{query}' yet (run REFRESH)"))),
            }
            let r = db.search_politicians("", 500).unwrap_or_default();
            s.send(Response::Politicians(r));
        }
        Request::Alerts => {
            let a = s.db.lock().await.alerts(false, 200).unwrap_or_default();
            s.send(Response::Alerts(a));
        }
        Request::MarkAlertsSeen => {
            {
                let db = s.db.lock().await;
                let _ = db.mark_alerts_seen();
                s.send(Response::Alerts(db.alerts(false, 200).unwrap_or_default()));
            }
            alert_rules(s, RuleOp::MarkSeen);
        }
        Request::AlertRules(op) => {
            // A new or re-enabled rule is checked against fresh quotes straight away.
            let check = matches!(op, RuleOp::Add(_) | RuleOp::Enable(_, true));
            alert_rules(s, op);
            if check {
                poll_alerts(s).await;
            }
        }
        Request::RefreshCongress => congress_refresh(s).await,
        Request::Forget(pane) => {
            s.series.lock().expect("series lock").remove(&pane);
        }
        Request::YieldCurves => match s.provider.yield_curves().await {
            Ok(c) => s.send(Response::Curves(Box::new(c))),
            Err(e) => s.send(Response::Status(format!("yield curve: {e:#}"))),
        },
        Request::Portfolio(creds) => {
            let result = match creds {
                None => Ok(tr_broker::sample::portfolio()),
                Some(c) => match tr_broker::SnapTrade::new(c) {
                    Ok(client) => client.portfolio().await,
                    Err(e) => Err(e),
                },
            };
            s.send(Response::Portfolio(result.map(Box::new).map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerRegister { creds, user_id } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.register_user(&user_id).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerRegistered(result.map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerPortal { creds, dark, reconnect } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.connection_portal_url(dark, reconnect.as_deref()).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerPortal(result.map_err(|e| format!("{e:#}"))));
        }
        Request::BrokerDisconnect { creds, id } => {
            let result = match tr_broker::SnapTrade::new(creds) {
                Ok(client) => client.delete_connection(&id).await,
                Err(e) => Err(e),
            };
            s.send(Response::BrokerDisconnected(result.map(|()| id).map_err(|e| format!("{e:#}"))));
        }
        Request::Options { pane, symbol, expiration } => {
            let chain = s.provider.options(&symbol, expiration).await.map(Box::new).map_err(|e| format!("{e:#}"));
            s.send(Response::Options { pane, symbol, chain });
        }
        Request::News { pane, symbol } => {
            let items = s.provider.news(symbol.as_deref()).await.map_err(|e| format!("{e:#}"));
            s.send(Response::News { pane, symbol, items });
        }
        Request::OrderBook { pane, symbol, depth } => {
            let book = s.provider.order_book(&symbol, depth).await.map_err(|e| format!("{e:#}"));
            s.send(Response::OrderBook { pane, symbol, book });
        }
        Request::Search(query) => {
            let hits = s.provider.search(&query).await.unwrap_or_default();
            s.send(Response::SearchHits { query, hits });
        }
        Request::Trade(t) => s.send(Response::Trade(Box::new(trade(t).await))),
        Request::Edgar { pane, symbol, user_agent } => {
            let ua = tr_data::edgar::user_agent(&user_agent).unwrap_or_default();
            let dir = tr_data::edgar::cache_dir(&data_dir());
            let company = tr_data::edgar::company(&dir, &symbol, &ua).await.map(Arc::new).map_err(|e| format!("{e:#}"));
            s.send(Response::Edgar { pane, symbol, company });
        }
        Request::Earnings(r) => {
            crate::earnings_worker::handle(r, &s.provider, &|r| s.send(Response::Earnings(r))).await
        }
        Request::Eco { pane, req } => s.send(Response::Eco { pane, resp: crate::eco::fetch(req).await }),
    }
}

fn alert_rules(s: &Shared, op: RuleOp) {
    let book = {
        let mut a = s.alerts.lock().expect("alerts lock");
        if !a.book.apply(op, chrono::Utc::now().timestamp()) {
            return;
        }
        a.save();
        a.book.clone()
    };
    s.send(Response::AlertBook(Box::new(book)));
}

async fn trade(t: TradeRequest) -> TradeResponse {
    let err = |e: anyhow::Error| format!("{e:#}");
    let client = |creds| tr_broker::SnapTrade::new(creds).map_err(err);
    match t {
        TradeRequest::Preview { creds, req, ticket } => {
            let result = match client(creds) {
                Ok(c) => c.order_impact(&req).await.map(Box::new).map_err(err),
                Err(e) => Err(e),
            };
            TradeResponse::Preview { ticket, result }
        }
        TradeRequest::Place { creds, preview, ticket } => {
            let result = match client(creds) {
                Ok(c) => c.place_previewed(&preview).await.map(|o| o.map(Box::new)),
                Err(e) => Err(tr_broker::trading::PlaceError::NotSent(e)),
            };
            TradeResponse::Placed { ticket, preview, result }
        }
        TradeRequest::Cancel { creds, account, order } => {
            let result = match client(creds) {
                Ok(c) => c.cancel_order(&account, &order).await.map_err(err),
                Err(e) => Err(e),
            };
            TradeResponse::Cancelled { account, order, result }
        }
        TradeRequest::Portal { creds, dark } => TradeResponse::Portal(match client(creds) {
            Ok(c) => c.trading_portal_url(dark).await.map_err(err),
            Err(e) => Err(e),
        }),
        TradeRequest::Capabilities { creds } => TradeResponse::Capabilities(match client(creds) {
            Ok(c) => c.connection_capabilities().await.map_err(|e| {
                let throttle = tr_broker::snaptrade::throttle(&e);
                CapsError { message: err(e), throttled: throttle.is_some(), retry_after: throttle.flatten() }
            }),
            Err(message) => Err(CapsError { message, throttled: false, retry_after: None }),
        }),
    }
}

/// Register a load for `generation`; generations only move forward, since request
/// tasks run concurrently and may start out of order.
fn claim_series(s: &Shared, pane: PaneId, generation: u64) -> bool {
    let mut map = s.series.lock().expect("series lock");
    match map.get(&pane) {
        Some((g, _)) if *g > generation => false,
        Some((g, _)) if *g == generation => true,
        _ => {
            map.insert(pane, (generation, Arc::new(Vec::new())));
            true
        }
    }
}

/// Store freshly loaded bars for a pane if `generation` is still the newest.
fn store_series(s: &Shared, pane: PaneId, generation: u64, bars: Arc<Vec<Bar>>) -> bool {
    let mut map = s.series.lock().expect("series lock");
    match map.get(&pane) {
        Some((g, _)) if *g > generation => false,
        _ => {
            map.insert(pane, (generation, bars));
            true
        }
    }
}

fn current_series(s: &Shared, pane: PaneId, generation: u64) -> Option<Arc<Vec<Bar>>> {
    s.series.lock().expect("series lock").get(&pane).filter(|(g, _)| *g == generation).map(|(_, b)| b.clone())
}

/// Merge the provider's live feed (if any) into the pane's series, publishing at most
/// four updates a second. Stops when the pane reloads, closes or the feed ends.
fn start_live(s: Arc<Shared>, pane: PaneId, generation: u64, symbol: String, tf: Timeframe) {
    let Some(mut feed) = s.provider.live(&symbol, tf) else { return };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut dirty = false;
        loop {
            tokio::select! {
                bar = feed.recv() => {
                    let Some(bar) = bar else { break };
                    {
                        let mut map = s.series.lock().expect("series lock");
                        match map.get_mut(&pane) {
                            Some((g, bars)) if *g == generation => dirty |= Series::apply_live(Arc::make_mut(bars), bar),
                            _ => break,
                        }
                    }
                    // Every tick is the latest price for this security's alert rules.
                    alert_update(&s, &symbol, |m| m.last = Some(bar.close));
                }
                _ = tick.tick() => {
                    if !dirty {
                        if current_series(&s, pane, generation).is_none() {
                            break;
                        }
                        continue;
                    }
                    dirty = false;
                    let Some(bars) = current_series(&s, pane, generation) else { break };
                    let gpu = build_gpu(bars.clone()).await;
                    s.send(Response::History {
                        pane,
                        generation,
                        symbol: symbol.clone(),
                        tf,
                        bars,
                        gpu,
                        prepended: 0,
                        from_cache: false,
                        exhausted: false,
                        live: true,
                    });
                }
            }
        }
        tracing::debug!("live feed for pane {pane} ({symbol}) stopped");
    });
}

async fn build_gpu(bars: Arc<Vec<Bar>>) -> SeriesGpu {
    tokio::task::spawn_blocking(move || SeriesGpu::build(&bars)).await.expect("pyramid build task")
}

fn resolve_politician(db: &Db, query: &str) -> Option<Politician> {
    if let Ok(id) = query.trim().parse::<i64>() {
        return db.politician(id).ok().flatten();
    }
    // Prefer an exact last-name hit, then substring matches by activity.
    let all = db.search_politicians(query.trim(), 50).ok()?;
    let q = query.trim().to_ascii_lowercase();
    all.iter()
        .find(|p| p.politician.name.to_ascii_lowercase().split_whitespace().last() == Some(q.as_str()))
        .or(all.first())
        .map(|p| p.politician.clone())
}

async fn profile(s: &Shared, query: &str) -> Option<Profile> {
    let (politician, trades, tracked) = {
        let db = s.db.lock().await;
        let p = resolve_politician(&db, query)?;
        let trades = db.trades_for_politician(p.id).ok()?;
        let tracked = db.is_tracked(p.id).unwrap_or(false);
        (p, trades, tracked)
    };
    // Daily closes for each traded ticker (bounded) to estimate performance.
    let mut tickers: Vec<String> = trades.iter().filter_map(|t| t.ticker.clone()).collect();
    tickers.sort();
    tickers.dedup();
    tickers.truncate(40);
    let oldest = trades.iter().map(|t| t.tx_date).min();
    let days = oldest.map(|d| (chrono::Utc::now().date_naive() - d).num_days().max(30) as usize + 10).unwrap_or(400);
    let mut prices: std::collections::HashMap<String, Vec<Bar>> = std::collections::HashMap::new();
    let provider = s.provider.clone();
    let fetches = tickers.into_iter().map(|t| {
        let provider = provider.clone();
        async move {
            let r = provider.history(&format!("{t} US Equity"), Timeframe::D1, None, days.min(4000)).await;
            (t, r)
        }
    });
    for (t, r) in futures_join(fetches).await {
        if let Ok(b) = r {
            prices.insert(t, b);
        }
    }
    let (perf, returns) = performance::estimate(
        &trades,
        |tk, date| {
            let bars = prices.get(tk)?;
            let ts = date.and_hms_opt(23, 59, 59)?.and_utc().timestamp();
            let i = bars.partition_point(|b| b.ts <= ts).checked_sub(1)?;
            Some(bars[i].close)
        },
        |tk| prices.get(tk).and_then(|b| b.last()).map(|b| b.close),
    );
    Some(Profile { politician, trades, perf, returns, tracked })
}

/// Run futures concurrently, at most 6 in flight (keeps data providers happy).
async fn futures_join<F, T>(futs: impl Iterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::StreamExt;
    futures::stream::iter(futs).buffered(6).collect().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_alignment_carries_last_close() {
        let base: Vec<Bar> = (0..5).map(|i| Bar::new(i * 10, 100.0, 100.0, 100.0, 100.0 + i as f64, 1.0)).collect();
        let other: Vec<Bar> = vec![Bar::new(5, 0.0, 0.0, 0.0, 50.0, 1.0), Bar::new(25, 0.0, 0.0, 0.0, 55.0, 1.0)];
        let a = align_compare(&base, &other);
        assert!(a[0].is_nan());
        assert_eq!(&a[1..], &[50.0, 50.0, 55.0, 55.0]);
    }

    #[test]
    fn indicators_build_expected_overlays() {
        let bars = tr_core::sample::generate("X", Timeframe::D1, 300, 1_790_000_000);
        let built = build_indicators(
            &bars,
            Timeframe::D1,
            &[IndicatorSpec::Bollinger(20, 2.0), IndicatorSpec::Macd(12, 26, 9), IndicatorSpec::Vwap],
            &crate::themes::terminal_light().palette(theme::DEFAULT_ACCENT),
        );
        assert_eq!(built[0].overlays.len(), 3);
        assert_eq!(built[1].overlays[0].1.kind, LineKind::Hist);
        assert_eq!(built[2].overlays.len(), 1);
    }

    #[test]
    fn series_generations_only_move_forward() {
        let (out, _rx) = mpsc::channel();
        let s = Shared {
            provider: std::sync::Arc::new(tr_data::sample::SampleProvider::at(0, 10)),
            series: Default::default(),
            cache: std::sync::Mutex::new(None),
            db: Mutex::new(Db::open_in_memory().unwrap()),
            alerts: Default::default(),
            out,
            ctx: egui::Context::default(),
        };
        // The newer load's task starts first; the older one must not take over.
        assert!(claim_series(&s, 1, 2));
        assert!(!claim_series(&s, 1, 1));
        assert!(!store_series(&s, 1, 1, Arc::new(vec![])));
        assert!(store_series(&s, 1, 2, Arc::new(vec![Bar::new(0, 1.0, 1.0, 1.0, 1.0, 1.0)])));
        assert_eq!(current_series(&s, 1, 2).unwrap().len(), 1);
        assert!(current_series(&s, 1, 1).is_none());
    }

    #[test]
    fn price_alerts_fire_once_from_ticks() {
        let (out, rx) = mpsc::channel();
        let mut book = AlertBook::default();
        book.notify = false;
        let spec = crate::alerts::parse(&["AAPL", ">", "250"]).unwrap();
        book.add("AAPL US Equity", spec.cond, false, 0);
        let s = Shared {
            provider: std::sync::Arc::new(tr_data::sample::SampleProvider::at(0, 10)),
            series: Default::default(),
            cache: std::sync::Mutex::new(None),
            db: Mutex::new(Db::open_in_memory().unwrap()),
            alerts: std::sync::Mutex::new(PriceAlerts { book, ..Default::default() }),
            out,
            ctx: egui::Context::default(),
        };
        for p in [240.0, 251.0, 252.0, 249.0, 255.0] {
            alert_update(&s, "AAPL US Equity", |m| m.last = Some(p));
        }
        // Quotes for other securities are ignored.
        alert_quotes(
            &s,
            &[Quote {
                symbol: "MSFT US Equity".into(),
                last: 900.0,
                prev_close: 1.0,
                ts: 0,
                spark: vec![],
                extended: None,
            }],
        );
        let fired: Vec<_> = rx
            .try_iter()
            .filter_map(|r| match r {
                Response::PriceAlerts(f) => Some(f),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].message, "AAPL: Last >= 250 (now 251.00)");
        assert!(!s.alerts.lock().unwrap().book.rules[0].enabled, "one-shot switched off");
    }

    #[test]
    fn join_all_runs_everything() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let out = rt.block_on(futures_join((0..20).map(|i| async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            i * 2
        })));
        assert_eq!(out, (0..20).map(|i| i * 2).collect::<Vec<_>>());
    }
}
