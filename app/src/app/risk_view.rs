//! Portfolio risk (`RISK`, `CORR`): volatility, VaR/CVaR, beta and friends against a
//! benchmark, drawdown curves, per-position risk contribution and a correlation heatmap.
//! Computed locally from daily history of the held positions; the maths is [`tr_core::risk`].

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::NaiveDate;
use egui::{Align2, Color32, CursorIcon, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use tr_broker::{InstrumentKind, Portfolio};
use tr_core::risk::{self, Contribution, DrawdownStats, VsBenchmark};
use tr_core::{Bar, Security};
use tr_data::Quote;

use super::broker::BrokerState;
use super::portfolio_view::money;
use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::risk::{RiskReq, RiskResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::Request;

/// Annual risk-free rate assumed for Sharpe, Sortino and alpha (a constant, not a live yield).
pub const RISK_FREE: f64 = 0.04;
/// A holding needs this many daily bars to be included.
const MIN_BARS: usize = 60;
/// Shared trading days needed to compute anything.
const MIN_DAYS: usize = 30;
/// Benchmarks offered as buttons: label and market key.
const BENCHMARKS: [(&str, &str); 3] = [("SPY", "SPY US Equity"), ("QQQ", "QQQ US Equity"), ("XU100", "XU100 Index")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Contribution,
    Correlation,
}

impl Tab {
    fn word(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Contribution => "contrib",
            Tab::Correlation => "corr",
        }
    }

    fn from_word(w: &str) -> Self {
        match w {
            "contrib" => Tab::Contribution,
            "corr" => Tab::Correlation,
            _ => Tab::Overview,
        }
    }
}

/// Market key of a benchmark name (`SPY`, `XU100`, any other ticker).
pub fn bench_key(name: &str) -> String {
    let n = name.trim().to_ascii_uppercase();
    BENCHMARKS
        .iter()
        .find(|(l, _)| *l == n)
        .map(|(_, k)| k.to_string())
        .or_else(|| Security::parse(&n).map(|s| s.key()))
        .unwrap_or_else(|| format!("{n} US Equity"))
}

// ---- holdings --------------------------------------------------------------------------

/// A position (merged across accounts) as the risk maths sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Holding {
    pub label: String,
    /// Terminal market key used to fetch history; `None` when the instrument has none.
    pub key: Option<String>,
    pub kind: InstrumentKind,
    pub mv: Option<f64>,
    pub cash: bool,
}

impl Holding {
    /// Why the holding cannot be analysed, or `None` when it can.
    fn skip_reason(&self) -> Option<String> {
        if self.cash {
            return Some("cash equivalent".into());
        }
        if self.kind == InstrumentKind::Option {
            return Some("option: no price history (its underlying's risk is not modelled)".into());
        }
        if self.key.is_none() {
            return Some("no market-data symbol for this instrument".into());
        }
        match self.mv {
            Some(v) if v.abs() > 1e-9 => None,
            _ => Some("no price, so no weight".into()),
        }
    }
}

/// Current holdings at live prices, merged by market key.
pub fn holdings(pf: &Portfolio, b: &BrokerState, quotes: &HashMap<String, Quote>) -> Vec<Holding> {
    let mut out: Vec<Holding> = Vec::new();
    for p in &pf.positions {
        let mv = p.market_value_at(b.live_price(quotes, p));
        // Borsa Istanbul listings (`THYAO.IS`) have no US-style key but Yahoo serves them by symbol.
        let sym = p.symbol.trim().to_ascii_uppercase();
        let bist =
            (matches!(p.kind, InstrumentKind::Stock | InstrumentKind::Etf) && sym.ends_with(".IS") && sym.len() > 3)
                .then_some(sym);
        let key = p.market_key().or(bist);
        if let Some(h) = key.as_ref().and_then(|k| out.iter_mut().find(|h| h.key.as_ref() == Some(k))) {
            h.mv = match (h.mv, mv) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
            continue;
        }
        out.push(Holding { label: p.short_label(), key, kind: p.kind, mv, cash: p.cash_equivalent });
    }
    out
}

// ---- analysis --------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub key: String,
    pub mv: f64,
    pub weight: f64,
    pub contrib: Contribution,
}

#[derive(Debug, Clone)]
pub struct Stats {
    /// Trading days of returns used.
    pub n: usize,
    pub vol: f64,
    pub var95: Option<f64>,
    pub var99: Option<f64>,
    pub cvar95: Option<f64>,
    pub pvar95: Option<f64>,
    pub pvar99: Option<f64>,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub total_return: f64,
    pub bench_return: Option<f64>,
    pub vs: Option<VsBenchmark>,
    pub dd: Option<DrawdownStats>,
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub rows: Vec<Row>,
    pub skipped: Vec<(String, String)>,
    pub bench: String,
    /// Why the benchmark figures are n/a.
    pub bench_note: Option<String>,
    /// Day numbers of the equity points (one more than the return count).
    pub days: Vec<i64>,
    /// Portfolio value rebased to 100, the benchmark likewise, and the drawdown (≤ 0).
    pub equity: Vec<f64>,
    pub bench_equity: Option<Vec<f64>>,
    pub dd: Vec<f64>,
    pub stats: Option<Stats>,
    /// Included market value (gross), the base the currency VaR applies to.
    pub base: f64,
    /// Correlation of the holdings' daily returns, then the benchmark when present.
    pub corr_labels: Vec<String>,
    pub corr_keys: Vec<String>,
    pub corr: Vec<Vec<Option<f64>>>,
    /// Set when nothing could be computed.
    pub problem: Option<String>,
}

pub fn date(day: i64) -> String {
    NaiveDate::from_num_days_from_ce_opt((day + 719_163) as i32).map_or_else(|| "?".into(), |d| d.to_string())
}

/// Compute everything from the holdings and their daily bars. `window` is the number of
/// trading days of returns to use (the most recent).
pub fn analyse(
    hs: &[Holding],
    bars: &HashMap<String, Result<Arc<Vec<Bar>>, String>>,
    bench: &str,
    window: usize,
) -> Analysis {
    let mut a = Analysis {
        rows: Vec::new(),
        skipped: Vec::new(),
        bench: bench.to_string(),
        bench_note: None,
        days: Vec::new(),
        equity: Vec::new(),
        bench_equity: None,
        dd: Vec::new(),
        stats: None,
        base: 0.0,
        corr_labels: Vec::new(),
        corr_keys: Vec::new(),
        corr: Vec::new(),
        problem: None,
    };
    let series_of = |k: &str| -> Result<Vec<(i64, f64)>, String> {
        match bars.get(k) {
            Some(Ok(b)) => Ok(b.iter().map(|b| (b.ts, b.close)).collect()),
            Some(Err(e)) => Err(e.clone()),
            None => Err("not loaded".into()),
        }
    };
    // Eligible holdings with enough history.
    let mut inc: Vec<(&Holding, Vec<(i64, f64)>)> = Vec::new();
    for h in hs {
        if let Some(why) = h.skip_reason() {
            if !h.cash {
                a.skipped.push((h.label.clone(), why));
            }
            continue;
        }
        let key = h.key.as_deref().unwrap_or_default();
        match series_of(key) {
            Ok(s) if s.len() > MIN_BARS => inc.push((h, s)),
            Ok(s) => {
                a.skipped.push((h.label.clone(), format!("only {} daily bars of history (need {MIN_BARS})", s.len())))
            }
            Err(e) => a.skipped.push((h.label.clone(), format!("no history: {e}"))),
        }
    }
    if inc.is_empty() {
        a.problem = Some(if hs.iter().all(|h| h.cash) {
            "No positions to analyse (cash only).".into()
        } else {
            "None of the holdings has usable price history.".into()
        });
        return a;
    }
    let bkey = bench_key(bench);
    let bench_series = match series_of(&bkey) {
        Ok(s) if s.len() > MIN_BARS => Some(s),
        Ok(s) => {
            a.bench_note = Some(format!("{bench} has only {} daily bars", s.len()));
            None
        }
        Err(e) => {
            a.bench_note = Some(format!("{bench} history unavailable: {e}"));
            None
        }
    };
    let mut all: Vec<Vec<(i64, f64)>> = inc.iter().map(|(_, s)| s.clone()).collect();
    let mut with_bench = false;
    if let Some(bs) = bench_series {
        all.push(bs);
        with_bench = true;
        if risk::align_closes(&all).0.len() <= MIN_DAYS {
            let n = risk::align_closes(&all).0.len();
            a.bench_note =
                Some(format!("only {n} trading days in common with {bench} (different calendar or currency?)"));
            all.pop();
            with_bench = false;
        }
    }
    let (days, closes) = risk::align_closes(&all);
    if days.len() <= MIN_DAYS {
        a.problem = Some(format!("The holdings share only {} trading days of history.", days.len()));
        a.rows = Vec::new();
        return a;
    }
    let rets: Vec<Vec<f64>> = closes.iter().map(|c| risk::returns(c)).collect();
    let n_all = rets[0].len();
    let w = window.min(n_all);
    let tail = |r: &Vec<f64>| r[r.len() - w..].to_vec();
    let rets: Vec<Vec<f64>> = rets.iter().map(tail).collect();
    a.days = days[days.len() - w - 1..].to_vec();
    let n_assets = inc.len();
    let bench_r = with_bench.then(|| rets[n_assets].clone());
    // Weights on gross included value; shorts carry negative weight.
    let gross: f64 = inc.iter().map(|(h, _)| h.mv.unwrap_or(0.0).abs()).sum();
    let weights: Vec<f64> = inc.iter().map(|(h, _)| h.mv.unwrap_or(0.0) / gross).collect();
    a.base = gross;
    let port = risk::portfolio_returns(&rets[..n_assets], &weights);
    let curve = risk::equity_curve(&port);
    a.dd = risk::drawdowns(&curve);
    a.equity = curve.iter().map(|v| v * 100.0).collect();
    a.bench_equity = bench_r.as_ref().map(|b| risk::equity_curve(b).iter().map(|v| v * 100.0).collect());
    let contribs = risk::contributions(&rets[..n_assets], &weights, bench_r.as_deref());
    for (((h, _), wgt), c) in inc.iter().zip(&weights).zip(&contribs) {
        a.rows.push(Row {
            label: h.label.clone(),
            key: h.key.clone().unwrap_or_default(),
            mv: h.mv.unwrap_or(0.0),
            weight: *wgt,
            contrib: *c,
        });
    }
    let mut series: Vec<Vec<f64>> = rets[..n_assets].to_vec();
    a.corr_labels = a.rows.iter().map(|r| r.label.clone()).collect();
    a.corr_keys = a.rows.iter().map(|r| r.key.clone()).collect();
    if let Some(b) = &bench_r {
        series.push(b.clone());
        a.corr_labels.push(bench.to_string());
        a.corr_keys.push(bkey);
    }
    a.corr = risk::corr_matrix(&series);
    a.stats = Some(Stats {
        n: port.len(),
        vol: risk::annual_vol(&port),
        var95: risk::var_historical(&port, 0.95),
        var99: risk::var_historical(&port, 0.99),
        cvar95: risk::cvar_historical(&port, 0.95),
        pvar95: risk::var_parametric(&port, 0.95),
        pvar99: risk::var_parametric(&port, 0.99),
        sharpe: risk::sharpe(&port, RISK_FREE),
        sortino: risk::sortino(&port, RISK_FREE),
        total_return: curve.last().map_or(0.0, |v| v - 1.0),
        bench_return: bench_r.as_ref().map(|b| risk::equity_curve(b).last().map_or(0.0, |v| v - 1.0)),
        vs: bench_r.as_ref().and_then(|b| risk::vs_benchmark(&port, b, RISK_FREE)),
        dd: risk::drawdown_stats(&curve),
    });
    a
}

// ---- pane ------------------------------------------------------------------------------

pub struct RiskPane {
    pub tab: Tab,
    pub bench: String,
    /// Trading days of history analysed: 252 (1Y) or 504 (2Y).
    pub window: usize,
    bars: HashMap<String, Result<Arc<Vec<Bar>>, String>>,
    /// Requested and not answered yet.
    pending: HashSet<String>,
    refresh: bool,
    ver: u64,
    cache: Option<(u64, Analysis)>,
}

impl RiskPane {
    pub fn new(tab: &str, bench: &str, window: usize) -> Self {
        let bench = bench.trim().to_ascii_uppercase();
        Self {
            tab: Tab::from_word(tab),
            bench: if bench.is_empty() { "SPY".into() } else { bench },
            window: if window >= 400 { 504 } else { 252 },
            bars: HashMap::new(),
            pending: HashSet::new(),
            refresh: false,
            ver: 0,
            cache: None,
        }
    }

    /// `(tab, benchmark, window)` for saved layouts.
    pub fn settings(&self) -> (&'static str, String, usize) {
        (self.tab.word(), self.bench.clone(), self.window)
    }

    pub fn title(&self) -> String {
        match self.tab {
            Tab::Correlation => "Correlation".into(),
            _ => "Portfolio Risk".into(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self.tab {
            Tab::Correlation => "CORR",
            _ => "RISK",
        }
    }

    /// Apply a `RISK [CORR|CONTRIB|OVERVIEW] [BENCHMARK] [1Y|2Y]` argument.
    pub fn apply_args(&mut self, arg: &str) {
        for w in arg.split_whitespace().map(str::to_ascii_uppercase) {
            match w.as_str() {
                "CORR" | "CORRELATION" | "HEATMAP" => self.tab = Tab::Correlation,
                "CONTRIB" | "CONTRIBUTION" | "TABLE" => self.tab = Tab::Contribution,
                "OVERVIEW" | "SUMMARY" => self.tab = Tab::Overview,
                "1Y" => self.window = 252,
                "2Y" => self.window = 504,
                b if b.len() <= 10 => self.bench = b.to_string(),
                _ => {}
            }
        }
    }

    fn analysis(&mut self, hs: &[Holding]) -> Option<&Analysis> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&self.bench, self.window, self.ver).hash(&mut h);
        for x in hs {
            (&x.key, &x.label, x.cash, x.mv.map(|v| v.round() as i64)).hash(&mut h);
        }
        let key = h.finish();
        if self.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            self.cache = Some((key, analyse(hs, &self.bars, &self.bench, self.window)));
        }
        self.cache.as_ref().map(|(_, a)| a)
    }
}

impl TerminalApp {
    /// `RISK`, `CORR`, `RISK QQQ 2Y`: show the risk pane, reusing an open one.
    pub(super) fn risk_command(&mut self, arg: &str) {
        let is_risk = |p: &super::Pane| matches!(p.kind, PaneKind::Risk(_));
        let id = self.idx(self.focus).filter(|&i| is_risk(&self.panes[i])).map(|i| self.panes[i].id);
        let id = id.or_else(|| self.panes.iter().find(|p| is_risk(p)).map(|p| p.id));
        match id {
            Some(id) => {
                if let Some(PaneKind::Risk(r)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    r.apply_args(arg);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => {
                let mut r = RiskPane::new("overview", "", 252);
                r.apply_args(arg);
                self.open_panel(PaneKind::Risk(Box::new(r)));
            }
        }
    }

    pub(super) fn on_risk(&mut self, resp: RiskResp) {
        let RiskResp::Bars { pane, symbol, bars } = resp;
        if let Some(PaneKind::Risk(r)) = self.pane_mut(pane).map(|p| &mut p.kind) {
            r.pending.remove(&symbol);
            r.bars.insert(symbol, bars);
            r.ver += 1;
        }
    }

    /// Draw the pane for the account in use and send the history requests it asks for.
    pub(super) fn risk_pane(&mut self, ui: &mut Ui, i: usize) -> Vec<Action> {
        let id = self.panes[i].id;
        let demo = (self.prefs.trading.account_mode == crate::prefs::AccountMode::Demo).then_some(&*self.demo.pf);
        let pf = demo.or(self.broker.portfolio.as_deref());
        let hs = pf.map(|pf| holdings(pf, &self.broker, &self.quotes)).unwrap_or_default();
        let ccy = pf.and_then(|p| p.accounts.first()).map_or("USD", |a| a.currency.as_str()).to_string();
        let PaneKind::Risk(r) = &mut self.panes[i].kind else { return Vec::new() };
        let mut actions = Vec::new();
        let ask = risk(ui, r, &hs, pf.is_some(), &ccy, &mut actions);
        if let Some((symbols, force)) = ask {
            self.worker.send(Request::Risk(RiskReq::Load { pane: id, symbols, force }));
        }
        actions
    }
}

// ---- drawing ---------------------------------------------------------------------------

fn pct(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

fn signed_pct(x: f64) -> String {
    format!("{:+.2}%", x * 100.0)
}

fn opt(x: Option<f64>, f: impl Fn(f64) -> String) -> String {
    x.map_or_else(|| "n/a".into(), f)
}

fn dim(ui: &mut Ui, s: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(s.into()).font(ui_font(11.5)).color(muted())).wrap());
}

/// Draws the pane. Returns the symbols to request (and whether to bypass the cache).
pub fn risk(
    ui: &mut Ui,
    p: &mut RiskPane,
    hs: &[Holding],
    have_portfolio: bool,
    ccy: &str,
    actions: &mut Vec<Action>,
) -> Option<(Vec<String>, bool)> {
    let bkey = bench_key(&p.bench);
    let mut need: Vec<String> = hs.iter().filter(|h| h.skip_reason().is_none()).filter_map(|h| h.key.clone()).collect();
    need.push(bkey);
    need.sort();
    need.dedup();
    let force = std::mem::take(&mut p.refresh);
    let ask: Vec<String> =
        need.iter().filter(|k| force || (!p.bars.contains_key(*k) && !p.pending.contains(*k))).cloned().collect();
    p.pending.extend(ask.iter().cloned());

    header(ui, p);
    ui.add_space(dp(4.0));
    if !have_portfolio {
        dim(ui, "Waiting for the portfolio…");
    } else if hs.is_empty() {
        dim(ui, "No positions. Buy something in the demo account (or connect a brokerage) to see its risk.");
    } else {
        let loading = !p.pending.is_empty();
        if loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0));
                dim(ui, format!("Loading daily history… {} left", p.pending.len()));
            });
        }
        let tab = p.tab;
        let bench = p.bench.clone();
        // While history arrives, keep showing the last result instead of a half-loaded one.
        let shown = if loading { p.cache.as_ref().map(|c| &c.1) } else { p.analysis(hs) };
        if let Some(a) = shown {
            {
                if let Some(why) = &a.problem {
                    ui.label(RichText::new(why).font(ui_font(12.5)).color(amber()));
                    skipped_list(ui, a);
                } else {
                    egui::ScrollArea::vertical().id_salt("risk-scroll").auto_shrink([false, false]).show(ui, |ui| {
                        match tab {
                            Tab::Overview => overview(ui, a, ccy),
                            Tab::Contribution => contribution(ui, a, &bench, ccy, actions),
                            Tab::Correlation => correlation(ui, a, actions),
                        }
                    });
                }
            }
        }
    }
    (!ask.is_empty()).then_some((ask, force))
}

fn header(ui: &mut Ui, p: &mut RiskPane) {
    ui.horizontal_wrapped(|ui| {
        let tabs = ["Overview", "Contribution", "Correlation"];
        let sel = Some(match p.tab {
            Tab::Overview => 0,
            Tab::Contribution => 1,
            Tab::Correlation => 2,
        });
        if let Some(i) = widgets::segmented(ui, "risk-tab", &tabs, sel, 20.0) {
            p.tab = [Tab::Overview, Tab::Contribution, Tab::Correlation][i];
        }
        ui.add_space(dp(6.0));
        let mut labels: Vec<&str> = BENCHMARKS.iter().map(|(l, _)| *l).collect();
        let custom = !labels.contains(&p.bench.as_str());
        let mine = p.bench.clone();
        if custom {
            labels.push(&mine);
        }
        let sel = labels.iter().position(|l| *l == p.bench);
        if let Some(i) = widgets::segmented(ui, "risk-bench", &labels, sel, 20.0) {
            p.bench = labels[i].to_string();
        }
        ui.label(RichText::new("benchmark").font(ui_font(11.0)).color(muted()));
        ui.add_space(dp(6.0));
        let sel = Some(usize::from(p.window >= 400));
        if let Some(i) = widgets::segmented(ui, "risk-window", &["1Y", "2Y"], sel, 20.0) {
            p.window = if i == 1 { 504 } else { 252 };
        }
        if ui.small_button("Refresh").on_hover_text("Re-download the daily history").clicked() {
            p.refresh = true;
        }
    });
}

fn skipped_list(ui: &mut Ui, a: &Analysis) {
    if a.skipped.is_empty() {
        return;
    }
    ui.add_space(dp(6.0));
    ui.collapsing(
        RichText::new(format!("{} holding(s) not included", a.skipped.len())).font(ui_font(11.5)).color(amber()),
        |ui| {
            for (l, why) in &a.skipped {
                dim(ui, format!("{l}: {why}"));
            }
        },
    );
}

struct Card {
    label: &'static str,
    value: String,
    sub: String,
    color: Color32,
    tip: &'static str,
}

fn card(label: &'static str, value: String, sub: impl Into<String>, tip: &'static str) -> Card {
    Card { label, value, sub: sub.into(), color: text(), tip }
}

fn draw_cards(ui: &mut Ui, cards: &[Card]) {
    let w = dp(150.0);
    let h = dp(56.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(dp(6.0), dp(6.0));
        for c in cards {
            let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, theme::CARD_RADIUS, theme::card());
            p.text(rect.min + vec2(dp(8.0), dp(6.0)), Align2::LEFT_TOP, c.label, ui_font(10.5), muted());
            p.text(rect.min + vec2(dp(8.0), dp(20.0)), Align2::LEFT_TOP, &c.value, mono(15.0), c.color);
            p.text(rect.min + vec2(dp(8.0), dp(40.0)), Align2::LEFT_TOP, &c.sub, ui_font(10.5), muted());
            resp.on_hover_text(c.tip);
        }
    });
}

fn overview(ui: &mut Ui, a: &Analysis, ccy: &str) {
    let Some(s) = &a.stats else { return };
    let cur = |f: Option<f64>| f.map_or_else(|| "n/a".into(), |f| money(f * a.base, ccy));
    let mut cards = vec![
        card(
            "Volatility (ann.)",
            pct(s.vol),
            format!("{} daily returns", s.n),
            "Standard deviation of daily portfolio returns × √252",
        ),
        card(
            "VaR 95% · 1 day",
            opt(s.var95, pct),
            cur(s.var95),
            "Historical: the loss exceeded on 5% of days in this window, applied to today's weights",
        ),
        card("VaR 99% · 1 day", opt(s.var99, pct), cur(s.var99), "Historical: the loss exceeded on 1% of days"),
        card(
            "CVaR 95% (ES)",
            opt(s.cvar95, pct),
            cur(s.cvar95),
            "Expected shortfall: the average loss on the worst 5% of days",
        ),
        card(
            "Parametric VaR 95%",
            opt(s.pvar95, pct),
            cur(s.pvar95),
            "Normal-distribution VaR from the mean and volatility, for comparison with the historical figure",
        ),
        card("Parametric VaR 99%", opt(s.pvar99, pct), cur(s.pvar99), "Normal-distribution VaR at 99%"),
    ];
    let na = |why: &str| format!("n/a · {why}");
    match &s.vs {
        Some(v) => {
            cards.push(card(
                "Beta",
                format!("{:.2}", v.beta),
                format!("vs {}", a.bench),
                "Sensitivity of portfolio returns to the benchmark",
            ));
            cards.push(card(
                "Alpha (ann.)",
                signed_pct(v.alpha),
                "Jensen, rf 4%",
                "Annualised return not explained by beta, after the risk-free rate",
            ));
            cards.push(card(
                "R² · correlation",
                format!("{:.2} · {:.2}", v.r_squared, v.correlation),
                format!("vs {}", a.bench),
                "Share of portfolio variance explained by the benchmark, and the correlation",
            ));
            cards.push(card(
                "Tracking error",
                pct(v.tracking_error),
                format!("IR {}", opt(v.information_ratio, |x| format!("{x:.2}"))),
                "Annualised volatility of portfolio minus benchmark returns; IR = active return / tracking error",
            ));
        }
        None => {
            let why = a.bench_note.clone().unwrap_or_else(|| "benchmark has no variance".into());
            for l in ["Beta", "Alpha (ann.)", "R² · correlation", "Tracking error"] {
                cards.push(card(
                    l,
                    "n/a".into(),
                    na(&why).chars().take(24).collect::<String>(),
                    "Needs a benchmark with overlapping history",
                ));
            }
        }
    }
    cards.push(card(
        "Sharpe",
        opt(s.sharpe, |x| format!("{x:.2}")),
        format!("rf {:.0}% assumed", RISK_FREE * 100.0),
        "Annualised excess return per unit of volatility; the risk-free rate is a fixed assumption",
    ));
    cards.push(card(
        "Sortino",
        opt(s.sortino, |x| format!("{x:.2}")),
        format!("rf {:.0}% assumed", RISK_FREE * 100.0),
        "Like Sharpe but penalises only downside deviation below the risk-free rate",
    ));
    let mut dd =
        card("Max drawdown", "n/a".into(), "", "Largest peak-to-trough fall of the weighted portfolio in the window");
    let mut cdd = card("Current drawdown", "n/a".into(), "", "Distance from the portfolio's high in the window");
    if let Some(d) = &s.dd {
        dd.value = format!("-{}", pct(d.max));
        dd.color = theme::down();
        dd.sub = format!("{} → {}", date(a.days[d.max_peak]), date(a.days[d.max_trough]));
        cdd.value = if d.current < 1e-9 { "0.00%".into() } else { format!("-{}", pct(d.current)) };
        cdd.color = if d.current < 1e-9 { theme::up() } else { theme::down() };
        cdd.sub = if d.current < 1e-9 { "at a high".into() } else { "below the high".into() };
    }
    cards.push(dd);
    cards.push(cdd);
    cards.push(card(
        "Return",
        signed_pct(s.total_return),
        s.bench_return.map_or_else(|| "no benchmark".into(), |b| format!("{} {}", a.bench, signed_pct(b))),
        "Total return of today's weights over the window (daily rebalanced)",
    ));
    draw_cards(ui, &cards);
    ui.add_space(dp(6.0));
    if let (Some(first), Some(last)) = (a.days.first(), a.days.last()) {
        dim(
            ui,
            format!(
                "{} → {} · {} trading days · {} holdings · weights from current market value ({} gross){}",
                date(*first),
                date(*last),
                s.n,
                a.rows.len(),
                money(a.base, ccy),
                if a.rows.iter().any(|r| r.weight < 0.0) { ", shorts negative" } else { "" },
            ),
        );
    }
    if let Some(n) = &a.bench_note {
        ui.label(RichText::new(format!("Benchmark: {n}")).font(ui_font(11.5)).color(amber()));
    }
    ui.add_space(dp(4.0));
    curve_chart(ui, a);
    ui.add_space(dp(6.0));
    dim(
        ui,
        "Method: today's weights applied to daily returns (history of the same positions, not the account's real P&L); \
         values are not converted between currencies. Local computation from Yahoo/Alpaca daily bars.",
    );
    skipped_list(ui, a);
}

fn line(p: &egui::Painter, pts: Vec<Pos2>, color: Color32, width: f32) {
    p.add(Shape::line(pts, Stroke::new(width, color)));
}

fn curve_chart(ui: &mut Ui, a: &Analysis) {
    let n = a.equity.len();
    if n < 2 {
        return;
    }
    let width = ui.available_width();
    let height = dp(300.0).min(width * 0.7).max(dp(200.0));
    let (resp, p) = ui.allocate_painter(vec2(width, height), Sense::hover());
    let rect = resp.rect;
    let left = rect.left() + dp(44.0);
    let right = rect.right() - dp(6.0);
    let top_plot = Rect::from_min_max(pos2(left, rect.top() + dp(4.0)), pos2(right, rect.top() + height * 0.62));
    let dd_plot = Rect::from_min_max(pos2(left, top_plot.bottom() + dp(10.0)), pos2(right, rect.bottom() - dp(16.0)));
    let x_of = |i: usize| left + (right - left) * i as f32 / (n - 1) as f32;
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for v in a.equity.iter().chain(a.bench_equity.iter().flatten()) {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    let pad = ((hi - lo) * 0.06).max(0.5);
    let (lo, hi) = (lo - pad, hi + pad);
    let y_top = |v: f64| top_plot.bottom() - top_plot.height() * ((v - lo) / (hi - lo)) as f32;
    let min_dd = a.dd.iter().cloned().fold(0.0, f64::min).min(-0.01);
    let y_dd = |v: f64| dd_plot.top() + dd_plot.height() * (v / min_dd) as f32;
    for r in [top_plot, dd_plot] {
        p.rect_filled(r, 3.0, theme::card());
    }
    // Grid and labels.
    for k in 0..=4 {
        let v = lo + (hi - lo) * k as f64 / 4.0;
        let y = y_top(v);
        p.line_segment([pos2(left, y), pos2(right, y)], Stroke::new(1.0, theme::grid()));
        p.text(pos2(left - dp(4.0), y), Align2::RIGHT_CENTER, format!("{v:.0}"), mono(10.0), muted());
    }
    for k in 0..=2 {
        let v = min_dd * k as f64 / 2.0;
        let y = y_dd(v);
        p.line_segment([pos2(left, y), pos2(right, y)], Stroke::new(1.0, theme::grid()));
        p.text(pos2(left - dp(4.0), y), Align2::RIGHT_CENTER, format!("{:.0}%", v * 100.0), mono(10.0), muted());
    }
    for (k, al) in [(0usize, Align2::LEFT_TOP), (n / 2, Align2::CENTER_TOP), (n - 1, Align2::RIGHT_TOP)] {
        p.text(pos2(x_of(k), dd_plot.bottom() + dp(2.0)), al, date(a.days[k]), mono(10.0), muted());
    }
    // The deepest drawdown.
    if let Some(d) = a.stats.as_ref().and_then(|s| s.dd).filter(|d| d.max > 0.0) {
        for r in [top_plot, dd_plot] {
            let band = Rect::from_x_y_ranges(x_of(d.max_peak)..=x_of(d.max_trough), r.y_range());
            p.rect_filled(band, 0.0, theme::amber_wash());
        }
    }
    // Drawdown area.
    let fill = theme::down().gamma_multiply(0.35);
    let y0 = dd_plot.top();
    for i in 0..n - 1 {
        let (x1, x2) = (x_of(i), x_of(i + 1));
        p.add(Shape::convex_polygon(
            vec![pos2(x1, y0), pos2(x1, y_dd(a.dd[i])), pos2(x2, y_dd(a.dd[i + 1])), pos2(x2, y0)],
            fill,
            Stroke::NONE,
        ));
    }
    line(&p, (0..n).map(|i| pos2(x_of(i), y_dd(a.dd[i]))).collect(), theme::down(), 1.2);
    if let Some(b) = &a.bench_equity {
        line(&p, (0..n).map(|i| pos2(x_of(i), y_top(b[i]))).collect(), muted(), 1.2);
    }
    line(&p, (0..n).map(|i| pos2(x_of(i), y_top(a.equity[i]))).collect(), theme::accent(), 1.8);
    p.text(
        top_plot.left_top() + vec2(dp(6.0), dp(4.0)),
        Align2::LEFT_TOP,
        "Portfolio (rebased to 100)",
        ui_font(10.5),
        theme::accent(),
    );
    if a.bench_equity.is_some() {
        p.text(
            top_plot.left_top() + vec2(dp(6.0), dp(18.0)),
            Align2::LEFT_TOP,
            format!("{} (rebased to 100)", a.bench),
            ui_font(10.5),
            muted(),
        );
    }
    p.text(
        dd_plot.left_top() + vec2(dp(6.0), dp(3.0)),
        Align2::LEFT_TOP,
        "Drawdown from running high",
        ui_font(10.5),
        theme::down(),
    );
    // Crosshair.
    if let Some(m) = resp.hover_pos().filter(|m| m.x >= left && m.x <= right) {
        let i = (((m.x - left) / (right - left)) * (n - 1) as f32).round().clamp(0.0, (n - 1) as f32) as usize;
        let x = x_of(i);
        for r in [top_plot, dd_plot] {
            p.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.0, muted()));
        }
        let mut s = format!("{}   Portfolio {:.1}", date(a.days[i]), a.equity[i]);
        if let Some(b) = &a.bench_equity {
            s.push_str(&format!("   {} {:.1}", a.bench, b[i]));
        }
        s.push_str(&format!("   DD {:.2}%", a.dd[i] * 100.0));
        let galley = p.layout_no_wrap(s, mono(11.0), text());
        let at = pos2(
            (x + dp(8.0)).min(right - galley.size().x - dp(8.0)).max(left),
            top_plot.bottom() - galley.size().y - dp(6.0),
        );
        p.rect_filled(
            Rect::from_min_size(at - vec2(4.0, 2.0), galley.size() + vec2(8.0, 4.0)),
            3.0,
            theme::window().gamma_multiply(0.92),
        );
        p.galley(at, galley, text());
    }
}

fn contribution(ui: &mut Ui, a: &Analysis, bench: &str, ccy: &str, actions: &mut Vec<Action>) {
    let Some(s) = &a.stats else { return };
    dim(
        ui,
        format!(
            "Portfolio volatility {} · each row's component is weight × marginal contribution; shares sum to 100%. Click a symbol to chart it.",
            pct(s.vol)
        ),
    );
    ui.add_space(dp(4.0));
    let mut rows: Vec<&Row> = a.rows.iter().collect();
    rows.sort_by(|x, y| y.contrib.share.total_cmp(&x.contrib.share));
    let heads = [
        "Symbol",
        "Value",
        "Weight",
        "Vol (ann.)",
        &format!("Beta vs {bench}"),
        "Marginal",
        "Component",
        "% of risk",
        "",
    ];
    egui::Grid::new("risk-contrib").num_columns(heads.len()).spacing([dp(14.0), dp(5.0)]).striped(true).show(
        ui,
        |ui| {
            for h in heads {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for r in rows {
                let sym = ui.add(
                    egui::Label::new(RichText::new(&r.label).font(mono(12.5)).color(theme::accent()))
                        .sense(Sense::click()),
                );
                if sym.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Open chart").clicked() {
                    actions.push(Action::OpenChart(r.key.clone()));
                }
                let mono_cell = |ui: &mut Ui, s: String| {
                    ui.label(RichText::new(s).font(mono(12.0)));
                };
                mono_cell(ui, money(r.mv, ccy));
                mono_cell(ui, signed_pct(r.weight).replace('+', ""));
                mono_cell(ui, pct(r.contrib.vol));
                mono_cell(ui, opt(r.contrib.beta, |b| format!("{b:.2}")));
                mono_cell(ui, pct(r.contrib.marginal));
                mono_cell(ui, pct(r.contrib.component));
                mono_cell(ui, format!("{:.1}%", r.contrib.share * 100.0));
                let (rect, _) = ui.allocate_exact_size(vec2(dp(90.0), dp(10.0)), Sense::hover());
                let frac = r.contrib.share.abs().min(1.0) as f32;
                let color = if r.contrib.share >= 0.0 { amber() } else { theme::up() };
                ui.painter().rect_filled(rect, 2.0, theme::grid());
                ui.painter().rect_filled(
                    Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height())),
                    2.0,
                    color,
                );
                ui.end_row();
            }
        },
    );
    skipped_list(ui, a);
}

/// Colour of a correlation: warm for positive, cool for negative, flat near zero.
fn corr_color(c: f64) -> Color32 {
    let target = if c >= 0.0 { amber() } else { theme::cyan() };
    widgets::lerp_color(theme::card(), target, (c.abs() * 0.9) as f32)
}

fn correlation(ui: &mut Ui, a: &Analysis, actions: &mut Vec<Action>) {
    let n = a.corr_labels.len();
    if n < 2 {
        dim(ui, "Correlation needs at least two series (a second holding or a benchmark).");
        return;
    }
    dim(
        ui,
        "Correlation of daily returns over the window. Warm = move together, cool = move opposite. Click a cell (or a \
         label) to chart that row's symbol.",
    );
    ui.add_space(dp(4.0));
    let label_w = dp(64.0);
    let head_h = dp(18.0);
    let cell = ((ui.available_width() - label_w - dp(8.0)) / n as f32).clamp(dp(24.0), dp(58.0));
    let (resp, p) = ui.allocate_painter(vec2(label_w + cell * n as f32, head_h + cell * n as f32), Sense::click());
    let origin = resp.rect.min + vec2(label_w, head_h);
    let short = |s: &str, k: usize| s.chars().take(k).collect::<String>();
    let fit = (cell / dp(7.0)) as usize;
    for (k, l) in a.corr_labels.iter().enumerate() {
        let bench = a.stats.is_some() && k == n - 1 && a.bench_equity.is_some();
        let color = if bench { amber() } else { text() };
        p.text(
            pos2(origin.x + cell * (k as f32 + 0.5), origin.y - dp(3.0)),
            Align2::CENTER_BOTTOM,
            short(l, fit.max(3)),
            mono(10.0),
            color,
        );
        p.text(
            pos2(origin.x - dp(4.0), origin.y + cell * (k as f32 + 0.5)),
            Align2::RIGHT_CENTER,
            short(l, 9),
            mono(10.5),
            color,
        );
    }
    for i in 0..n {
        for j in 0..n {
            let r = Rect::from_min_size(origin + vec2(cell * j as f32, cell * i as f32), vec2(cell, cell)).shrink(1.0);
            match a.corr[i][j] {
                Some(c) => {
                    p.rect_filled(r, 2.0, corr_color(c));
                    if cell >= dp(34.0) {
                        p.text(r.center(), Align2::CENTER_CENTER, format!("{c:.2}"), mono(10.0), text());
                    }
                }
                None => {
                    p.rect_filled(r, 2.0, theme::grid());
                    if cell >= dp(34.0) {
                        p.text(r.center(), Align2::CENTER_CENTER, "n/a", mono(10.0), muted());
                    }
                }
            }
        }
    }
    let cell_at = |pos: Pos2| {
        let d = pos - origin;
        (d.x >= 0.0 && d.y >= 0.0)
            .then(|| ((d.y / cell) as usize, (d.x / cell) as usize))
            .filter(|&(i, j)| i < n && j < n)
    };
    if let Some((i, j)) = resp.hover_pos().and_then(cell_at) {
        let c = a.corr[i][j].map_or_else(|| "n/a (no variance)".into(), |c| format!("{c:.3}"));
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), resp.id.with("tip"), egui::PopupAnchor::Pointer)
            .show(|ui| {
                ui.label(format!("{} vs {}: {c}", a.corr_labels[i], a.corr_labels[j]));
            });
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if resp.clicked() {
        if let Some((i, _)) = resp.interact_pointer_pos().and_then(cell_at) {
            actions.push(Action::OpenChart(a.corr_keys[i].clone()));
        } else if let Some(pos) = resp.interact_pointer_pos() {
            // Row labels live left of the grid.
            let y = pos.y - origin.y;
            if pos.x < origin.x && y >= 0.0 && ((y / cell) as usize) < n {
                actions.push(Action::OpenChart(a.corr_keys[(y / cell) as usize].clone()));
            }
        }
    }
    ui.add_space(dp(6.0));
    ui.horizontal(|ui| {
        dim(ui, "-1");
        for k in 0..=10 {
            let c = -1.0 + 0.2 * k as f64;
            let (r, _) = ui.allocate_exact_size(vec2(dp(18.0), dp(10.0)), Sense::hover());
            ui.painter().rect_filled(r, 1.0, corr_color(c));
        }
        dim(ui, "+1");
    });
    skipped_list(ui, a);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars_from(closes: &[f64]) -> Result<Arc<Vec<Bar>>, String> {
        Ok(Arc::new(closes.iter().enumerate().map(|(i, &c)| Bar::new(i as i64 * 86_400, c, c, c, c, 0.0)).collect()))
    }

    /// Prices following `rets` from 100.
    fn path(rets: &[f64]) -> Vec<f64> {
        let mut v = 100.0;
        std::iter::once(v)
            .chain(rets.iter().map(|r| {
                v *= 1.0 + r;
                v
            }))
            .collect()
    }

    fn rets(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 0.04
            })
            .collect()
    }

    fn hold(label: &str, key: Option<&str>, kind: InstrumentKind, mv: Option<f64>) -> Holding {
        Holding { label: label.into(), key: key.map(Into::into), kind, mv, cash: false }
    }

    #[test]
    fn holding_identical_to_the_benchmark_has_beta_one() {
        let r = rets(300, 1);
        let mut bars = HashMap::new();
        bars.insert("SPY US Equity".to_string(), bars_from(&path(&r)));
        let hs = [hold("SPY", Some("SPY US Equity"), InstrumentKind::Etf, Some(1000.0))];
        let a = analyse(&hs, &bars, "SPY", 252);
        let s = a.stats.unwrap();
        let v = s.vs.unwrap();
        assert!((v.beta - 1.0).abs() < 1e-9 && (v.r_squared - 1.0).abs() < 1e-9);
        assert_eq!(s.n, 252);
        assert!(s.var99.unwrap() >= s.var95.unwrap());
        assert!((a.rows[0].contrib.share - 1.0).abs() < 1e-9);
        assert_eq!(a.days.len(), a.equity.len());
        assert!(a.corr[0][1].unwrap() > 0.999);
    }

    #[test]
    fn skips_are_explained_and_weights_follow_market_value() {
        let (r1, r2) = (rets(300, 2), rets(300, 3));
        let mut bars = HashMap::new();
        bars.insert("A US Equity".to_string(), bars_from(&path(&r1)));
        bars.insert("B US Equity".to_string(), bars_from(&path(&r2)));
        bars.insert("SPY US Equity".to_string(), bars_from(&path(&rets(300, 4))));
        bars.insert("NEW US Equity".to_string(), bars_from(&path(&rets(10, 5))));
        bars.insert("BAD US Equity".to_string(), Err("HTTP 404".into()));
        let mut cash = hold("USD", None, InstrumentKind::Cash, Some(50.0));
        cash.cash = true;
        let hs = [
            hold("A", Some("A US Equity"), InstrumentKind::Stock, Some(750.0)),
            hold("B", Some("B US Equity"), InstrumentKind::Stock, Some(250.0)),
            hold("NEW", Some("NEW US Equity"), InstrumentKind::Stock, Some(10.0)),
            hold("BAD", Some("BAD US Equity"), InstrumentKind::Stock, Some(10.0)),
            hold("AAPL 261218C250", Some("AAPL US Equity"), InstrumentKind::Option, Some(10.0)),
            hold("7203.T", None, InstrumentKind::Stock, Some(10.0)),
            hold("NOPX", Some("NOPX US Equity"), InstrumentKind::Stock, None),
            cash,
        ];
        let a = analyse(&hs, &bars, "SPY", 252);
        assert_eq!(a.rows.len(), 2);
        assert!((a.rows[0].weight - 0.75).abs() < 1e-9 && (a.rows[1].weight - 0.25).abs() < 1e-9);
        assert!((a.base - 1000.0).abs() < 1e-9);
        let why = |l: &str| a.skipped.iter().find(|(n, _)| n == l).map(|(_, w)| w.clone()).unwrap();
        assert!(why("NEW").contains("only"));
        assert!(why("BAD").contains("404"));
        assert!(why("AAPL 261218C250").contains("option"));
        assert!(why("7203.T").contains("no market-data"));
        assert!(why("NOPX").contains("no price"));
        assert!(a.skipped.iter().all(|(n, _)| n != "USD"), "cash is silently left out");
        let share: f64 = a.rows.iter().map(|r| r.contrib.share).sum();
        assert!((share - 1.0).abs() < 1e-9);
        assert_eq!(a.corr_labels, vec!["A", "B", "SPY"]);
    }

    #[test]
    fn missing_benchmark_leaves_the_rest_intact() {
        let mut bars = HashMap::new();
        bars.insert("A US Equity".to_string(), bars_from(&path(&rets(200, 6))));
        bars.insert("XU100 Index".to_string(), Err("no data".into()));
        let hs = [hold("A", Some("A US Equity"), InstrumentKind::Stock, Some(10.0))];
        let a = analyse(&hs, &bars, "XU100", 252);
        let s = a.stats.unwrap();
        assert!(s.vs.is_none() && a.bench_note.unwrap().contains("XU100"));
        assert!(s.vol > 0.0 && a.bench_equity.is_none());
        assert_eq!(s.n, 200);
    }

    #[test]
    fn nothing_usable_reports_a_problem() {
        let hs = [hold("X", Some("X US Equity"), InstrumentKind::Stock, Some(10.0))];
        let a = analyse(&hs, &HashMap::new(), "SPY", 252);
        assert!(a.problem.is_some() && a.stats.is_none());
        assert_eq!(a.skipped.len(), 1);
    }

    #[test]
    fn benchmark_keys_and_days() {
        assert_eq!(bench_key("spy"), "SPY US Equity");
        assert_eq!(bench_key("XU100"), "XU100 Index");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_000), "2024-10-04");
    }

    #[test]
    fn arguments_pick_tab_benchmark_and_window() {
        let mut p = RiskPane::new("overview", "", 252);
        p.apply_args("corr qqq 2y");
        assert_eq!((p.tab, p.bench.as_str(), p.window), (Tab::Correlation, "QQQ", 504));
        assert_eq!(p.settings(), ("corr", "QQQ".to_string(), 504));
    }

    #[test]
    fn renders_every_tab_without_panicking() {
        fn run(ctx: &egui::Context, f: impl FnMut(&mut Ui)) {
            ctx.run_ui(Default::default(), f).textures_delta.clear();
        }
        let ctx = egui::Context::default();
        let mut b = BrokerState::load(true);
        b.creds = tr_broker::Credentials::default();
        let pf = tr_broker::sample::portfolio();
        let hs = holdings(&pf, &b, &HashMap::new());
        assert!(hs.iter().any(|h| h.key.is_some()), "sample holdings map to market keys");
        let mut pane = RiskPane::new("overview", "SPY", 252);
        // First frame: nothing loaded, the pane asks for every eligible symbol.
        let mut ask = None;
        let mut actions = Vec::new();
        run(&ctx, |ui| ask = risk(ui, &mut pane, &hs, true, "USD", &mut actions));
        let (symbols, force) = ask.take().expect("history requested");
        assert!(!force && symbols.contains(&"SPY US Equity".to_string()));
        // Asking again while the answers are pending does not repeat the request.
        run(&ctx, |ui| ask = risk(ui, &mut pane, &hs, true, "USD", &mut actions));
        assert!(ask.is_none());
        for (n, k) in symbols.iter().enumerate() {
            let r = if n == 1 { Err("HTTP 404".to_string()) } else { bars_from(&path(&rets(400, n as u64 + 10))) };
            pane.pending.remove(k);
            pane.bars.insert(k.clone(), r);
            pane.ver += 1;
        }
        for tab in [Tab::Overview, Tab::Contribution, Tab::Correlation] {
            pane.tab = tab;
            for _ in 0..2 {
                run(&ctx, |ui| ask = risk(ui, &mut pane, &hs, true, "USD", &mut actions));
            }
            assert!(ask.is_none());
        }
        let a = pane.analysis(&hs).unwrap();
        assert!(a.problem.is_none() && a.stats.is_some(), "{:?}", a.problem);
        // Empty and not-yet-loaded states.
        let mut empty = RiskPane::new("corr", "", 504);
        run(&ctx, |ui| {
            risk(ui, &mut empty, &[], true, "USD", &mut actions);
            risk(ui, &mut empty, &[], false, "USD", &mut actions);
        });
    }

    /// Real history for a small book, printed. `-- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "network"]
    async fn live_analysis() {
        let provider = tr_data::from_env();
        let mut bars = HashMap::new();
        let book = [("AAPL", 5000.0), ("MSFT", 3000.0), ("NVDA", 2000.0), ("SPY", 1000.0), ("THYAO.IS", 500.0)];
        let mut hs = Vec::new();
        for (t, mv) in book {
            let key = if t.ends_with(".IS") { t.to_string() } else { format!("{t} US Equity") };
            let b = provider.history(&key, tr_core::Timeframe::D1, None, 560).await;
            bars.insert(key.clone(), b.map(Arc::new).map_err(|e| e.to_string()));
            hs.push(hold(t, Some(&key), InstrumentKind::Stock, Some(mv)));
        }
        for k in ["SPY US Equity", "XU100 Index"] {
            let b = provider.history(k, tr_core::Timeframe::D1, None, 560).await;
            bars.insert(k.to_string(), b.map(Arc::new).map_err(|e| e.to_string()));
        }
        for bench in ["SPY", "XU100"] {
            let a = analyse(&hs, &bars, bench, 252);
            println!("{bench}: problem {:?} note {:?} skipped {:?}", a.problem, a.bench_note, a.skipped);
            println!("{:#?}", a.stats);
            for r in &a.rows {
                println!(
                    "{} w {:.3} vol {:.3} share {:.3} beta {:?}",
                    r.label, r.weight, r.contrib.vol, r.contrib.share, r.contrib.beta
                );
            }
        }
    }
}
