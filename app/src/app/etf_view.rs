//! US ETF contents (`ETF`): a list of popular funds, one fund's top holdings as a donut chart
//! and list with its sector weights, reverse search (which ETFs hold a stock) and the overlap
//! of two funds. Data layer: [`tr_data::etf`].

use std::collections::{HashMap, HashSet};
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use egui::{Align2, Color32, CursorIcon, Key, Mesh, RichText, Sense, Shape, Ui, vec2};
use tr_data::etf::{self, Fund, Held, Overlap, Source};

use super::fa_view::fmt_money;
use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::etf::{EtfReq, EtfResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::worker::Request;

/// Holdings shown as slices and rows of the detail view; the rest is one "Other" slice.
const TOP: usize = 10;
/// Shared holdings listed in the overlap view.
const OVERLAP_ROWS: usize = 60;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Detail(String),
    Reverse(String),
    Overlap(String, String),
}

/// A computed reverse search or overlap, kept until the funds or the question change.
enum Calc {
    Reverse { query: String, stamp: (usize, i64), hits: Vec<Held> },
    Overlap { a: String, b: String, stamp: (usize, i64), result: Box<Overlap> },
}

pub struct EtfPane {
    pub view: View,
    funds: HashMap<String, Arc<Fund>>,
    loading: HashSet<String>,
    errors: HashMap<String, String>,
    /// Loads to send on the next tick: ticker and whether to force a re-check.
    queue: Vec<(String, bool)>,
    need_cached: bool,
    /// An index run was asked for (`true` = force).
    index_req: Option<bool>,
    /// Progress of the running index run.
    index: Option<(usize, usize)>,
    input: String,
    rev_input: String,
    ov_a: String,
    ov_b: String,
    calc: Option<Calc>,
}

/// A ticker as typed: first word, upper case (`spy`, `SPY US Equity` → `SPY`).
fn clean(s: &str) -> String {
    s.split_whitespace().next().unwrap_or_default().to_ascii_uppercase()
}

fn date(d: chrono::NaiveDate) -> String {
    d.format("%b %-d, %Y").to_string()
}

impl EtfPane {
    pub fn new(mode: &str, a: &str, b: &str) -> Self {
        let (a, b) = (clean(a), clean(b));
        let view = match mode {
            "detail" if !a.is_empty() => View::Detail(a),
            "reverse" if !a.is_empty() => View::Reverse(a),
            "overlap" if !a.is_empty() && !b.is_empty() => View::Overlap(a, b),
            _ => View::List,
        };
        let mut p = Self {
            view: View::List,
            funds: HashMap::new(),
            loading: HashSet::new(),
            errors: HashMap::new(),
            queue: Vec::new(),
            need_cached: true,
            index_req: None,
            index: None,
            input: String::new(),
            rev_input: String::new(),
            ov_a: String::new(),
            ov_b: String::new(),
            calc: None,
        };
        p.open(view);
        p
    }

    /// `(mode, a, b)` for saved layouts.
    pub fn settings(&self) -> (&'static str, String, String) {
        match &self.view {
            View::List => ("list", String::new(), String::new()),
            View::Detail(t) => ("detail", t.clone(), String::new()),
            View::Reverse(t) => ("reverse", t.clone(), String::new()),
            View::Overlap(a, b) => ("overlap", a.clone(), b.clone()),
        }
    }

    pub fn title(&self) -> String {
        match &self.view {
            View::List => "ETFs".into(),
            View::Detail(t) => format!("{t} ETF"),
            View::Reverse(t) => format!("ETFs holding {t}"),
            View::Overlap(a, b) => format!("{a} vs {b}"),
        }
    }

    pub fn code(&self) -> &'static str {
        "ETF"
    }

    fn want(&mut self, ticker: &str, force: bool) {
        let t = clean(ticker);
        if t.is_empty() || self.loading.contains(&t) {
            return;
        }
        self.errors.remove(&t);
        self.loading.insert(t.clone());
        self.queue.push((t, force));
    }

    /// Switch view, fetching what it needs.
    pub fn open(&mut self, view: View) {
        match &view {
            View::List => {}
            View::Detail(t) => {
                self.input = t.clone();
                self.want(&t.clone(), false);
            }
            View::Reverse(t) => self.rev_input = t.clone(),
            View::Overlap(a, b) => {
                (self.ov_a, self.ov_b) = (a.clone(), b.clone());
                self.want(&a.clone(), false);
                self.want(&b.clone(), false);
            }
        }
        self.view = view;
        self.calc = None;
    }

    fn stamp(&self) -> (usize, i64) {
        (self.funds.len(), self.funds.values().map(|f| f.updated).sum())
    }

    /// Fund counts for the "N ETFs indexed" line: (total, with full holdings).
    fn indexed(&self) -> (usize, usize) {
        (self.funds.len(), self.funds.values().filter(|f| !f.partial()).count())
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    fn etf_target(&self) -> Option<crate::worker::PaneId> {
        let is_etf =
            |id: crate::worker::PaneId| self.idx(id).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::Etf(_)));
        if is_etf(self.focus) {
            return Some(self.focus);
        }
        self.panes.iter().find(|p| matches!(p.kind, PaneKind::Etf(_))).map(|p| p.id)
    }

    fn with_etf(&mut self, view: View) {
        match self.etf_target() {
            Some(id) => {
                if let Some(PaneKind::Etf(e)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    e.open(view);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => {
                let mut e = EtfPane::new("list", "", "");
                e.open(view);
                self.open_panel(PaneKind::Etf(Box::new(e)));
            }
        }
    }

    /// `ETF`, `ETF SPY`, `ETF SPY QQQ` (overlap), `ETF HELD AAPL` (reverse search).
    pub(super) fn etf_command(&mut self, arg: &str) {
        let words: Vec<String> = arg.split_whitespace().map(str::to_ascii_uppercase).collect();
        let view = match words.as_slice() {
            [] => View::List,
            [t] => View::Detail(t.clone()),
            [k, t] if matches!(k.as_str(), "HELD" | "IN" | "WHO" | "REV" | "HOLDERS") => View::Reverse(t.clone()),
            [a, b, ..] => View::Overlap(a.clone(), b.clone()),
        };
        self.with_etf(view);
    }

    /// Load the disk cache once and send queued fetches and index runs.
    pub(super) fn etf_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Etf(e) = &mut self.panes[i].kind else { continue };
            let ua = self.prefs.panels.sec_user_agent.clone();
            if std::mem::take(&mut e.need_cached) {
                self.worker.send(Request::Etf(EtfReq::Cached { pane: id }));
            }
            for (ticker, force) in std::mem::take(&mut e.queue) {
                self.worker.send(Request::Etf(EtfReq::Load { pane: id, ticker, force, user_agent: ua.clone() }));
            }
            if let Some(force) = e.index_req.take() {
                let tickers: Vec<String> = etf::CURATED.iter().map(|(t, ..)| t.to_string()).collect();
                e.loading.extend(tickers.iter().cloned());
                e.index = Some((0, tickers.len()));
                self.worker.send(Request::Etf(EtfReq::Index { pane: id, tickers, force, user_agent: ua }));
            }
        }
    }

    pub(super) fn on_etf(&mut self, resp: EtfResp) {
        let pane = match &resp {
            EtfResp::Cached { pane, .. } | EtfResp::Fund { pane, .. } | EtfResp::Progress { pane, .. } => *pane,
        };
        let Some(PaneKind::Etf(e)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        match resp {
            EtfResp::Cached { funds, .. } => {
                for f in funds {
                    if e.funds.get(&f.ticker).is_none_or(|old| old.updated < f.updated) {
                        e.funds.insert(f.ticker.clone(), f);
                    }
                }
            }
            EtfResp::Fund { ticker, fund, .. } => {
                e.loading.remove(&ticker);
                match fund {
                    Ok(f) => {
                        e.errors.remove(&ticker);
                        e.funds.insert(ticker, f);
                    }
                    Err(m) => {
                        e.errors.insert(ticker, m);
                    }
                }
            }
            EtfResp::Progress { done, total, .. } => e.index = (done < total).then_some((done, total)),
        }
    }
}

// ---- drawing ------------------------------------------------------------------------------

/// Colour of slice `i`; the sixth palette colour onwards are lightened.
fn slice_color(i: usize) -> Color32 {
    let base = theme::series(i);
    if i >= 6 { base.lerp_to_gamma(Color32::WHITE, 0.4) } else { base }
}

fn pct(w: f64) -> String {
    if w >= 10.0 { format!("{w:.1}%") } else { format!("{w:.2}%") }
}

fn link(ui: &mut Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(label).font(mono(12.5)).color(color)).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn dim(ui: &mut Ui, s: impl Into<String>) {
    ui.label(RichText::new(s.into()).font(ui_font(11.5)).color(muted()));
}

pub fn etf(ui: &mut Ui, p: &mut EtfPane) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut next: Option<View> = None;
    header(ui, p, &mut next);
    ui.add_space(dp(4.0));
    match p.view.clone() {
        View::List => list(ui, p, &mut next),
        View::Detail(t) => detail(ui, p, &t, &mut next, &mut actions),
        View::Reverse(t) => reverse(ui, p, &t, &mut next, &mut actions),
        View::Overlap(a, b) => overlap(ui, p, &a, &b, &mut next),
    }
    if let Some(v) = next {
        p.open(v);
    }
    actions
}

fn header(ui: &mut Ui, p: &mut EtfPane, next: &mut Option<View>) {
    ui.horizontal_wrapped(|ui| {
        let tab =
            |ui: &mut Ui, on: bool, label: &str| ui.selectable_label(on, RichText::new(label).font(ui_font(12.5)));
        if tab(ui, p.view == View::List, "Funds").clicked() {
            *next = Some(View::List);
        }
        if tab(ui, matches!(p.view, View::Reverse(_)), "Held by (reverse search)").clicked() {
            *next = Some(View::Reverse(clean(&p.rev_input)));
        }
        if tab(ui, matches!(p.view, View::Overlap(..)), "Overlap").clicked() {
            let (a, b) = (clean(&p.ov_a), clean(&p.ov_b));
            *next = Some(View::Overlap(
                if a.is_empty() { "VOO".into() } else { a },
                if b.is_empty() { "QQQ".into() } else { b },
            ));
        }
        ui.separator();
        let r = ui.add(
            egui::TextEdit::singleline(&mut p.input)
                .hint_text(RichText::new("Open ETF ticker…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(130.0)),
        );
        let go = ui.small_button("Open").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
        if go && !clean(&p.input).is_empty() {
            *next = Some(View::Detail(clean(&p.input)));
        }
    });
    ui.horizontal_wrapped(|ui| {
        let (n, full) = p.indexed();
        dim(
            ui,
            format!(
                "{n} ETF{} indexed on disk ({full} with full holdings, {} top-10 only). Reverse search and overlap use these.",
                if n == 1 { "" } else { "s" },
                n - full
            ),
        );
        match p.index {
            Some((done, total)) => {
                ui.add(egui::Spinner::new().size(dp(12.0)));
                dim(ui, format!("indexing {done}/{total}…"));
            }
            None => {
                let r = ui.small_button("Index curated list").on_hover_text(format!(
                    "Download holdings of the {} popular ETFs below, one at a time (SEC and Yahoo rate limits). \
                     Funds already fresh on disk are skipped.",
                    etf::CURATED.len()
                ));
                if r.clicked() {
                    p.index_req = Some(false);
                }
            }
        }
    });
}

fn status(p: &EtfPane, ticker: &str) -> (String, Color32) {
    if let Some(f) = p.funds.get(ticker) {
        let s = match (f.source, f.as_of) {
            (Source::Nport, Some(d)) => format!("N-PORT as of {} · {} holdings", date(d), f.positions),
            (Source::Nport, None) => format!("N-PORT · {} holdings", f.positions),
            (Source::Yahoo, _) => "Yahoo · top 10 only".into(),
        };
        return (s, text());
    }
    if p.loading.contains(ticker) {
        return ("loading…".into(), muted());
    }
    match p.errors.get(ticker) {
        Some(_) => ("no data".into(), theme::down()),
        None => ("not indexed".into(), muted()),
    }
}

fn list(ui: &mut Ui, p: &EtfPane, next: &mut Option<View>) {
    egui::ScrollArea::vertical().id_salt("etf-list").auto_shrink([false, false]).show(ui, |ui| {
        let extra: Vec<&Arc<Fund>> = {
            let mut v: Vec<_> =
                p.funds.values().filter(|f| !etf::CURATED.iter().any(|(t, ..)| *t == f.ticker)).collect();
            v.sort_by(|a, b| a.ticker.cmp(&b.ticker));
            v
        };
        let rows: Vec<(String, String, String)> = etf::CURATED
            .iter()
            .map(|(t, n, c)| (t.to_string(), n.to_string(), c.to_string()))
            .chain(extra.iter().map(|f| (f.ticker.clone(), f.name.clone(), "Other".to_string())))
            .collect();
        egui::Grid::new("etf-list-grid").num_columns(4).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            for h in ["ETF", "Name", "Category", "Holdings"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for (t, name, cat) in rows {
                if link(ui, &t, amber()).on_hover_text(format!("Open {t} holdings")).clicked() {
                    *next = Some(View::Detail(t.clone()));
                }
                ui.label(RichText::new(name).font(ui_font(12.5)).color(text()));
                ui.label(RichText::new(cat).font(ui_font(11.5)).color(muted()));
                let (s, c) = status(p, &t);
                let r = ui.label(RichText::new(s).font(ui_font(11.5)).color(c));
                if let Some(e) = p.errors.get(&t) {
                    r.on_hover_text(e);
                }
                ui.end_row();
            }
        });
        ui.add_space(dp(8.0));
        dim(
            ui,
            "Sources: SEC N-PORT-P filings (full portfolios, filed quarterly with a 60-day lag, so always a few months old) \
             and Yahoo Finance (top 10 and sector weights; the only source for funds that file no N-PORT, such as \
             SPY, DIA and GLD).",
        );
    });
}

fn back(ui: &mut Ui, next: &mut Option<View>) {
    if ui.small_button("◀ Back").on_hover_text("Back to the fund list").clicked() {
        *next = Some(View::List);
    }
}

fn detail(ui: &mut Ui, p: &mut EtfPane, t: &str, next: &mut Option<View>, actions: &mut Vec<Action>) {
    let fund = p.funds.get(t).cloned();
    ui.horizontal(|ui| {
        back(ui, next);
        if ui.small_button("Refresh").on_hover_text("Check the SEC for a newer filing").clicked() {
            p.want(t, true);
        }
        if ui.small_button("Chart").on_hover_text(format!("Open the {t} price chart")).clicked() {
            actions.push(Action::Command(t.to_string()));
        }
        if p.loading.contains(t) {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "loading…");
        }
    });
    let Some(f) = fund else {
        match p.errors.get(t) {
            Some(e) => {
                ui.add_space(dp(8.0));
                ui.label(RichText::new(format!("{t}: {e}")).font(ui_font(12.5)).color(theme::down()));
            }
            None if !p.loading.contains(t) => dim(ui, "Nothing loaded."),
            None => {}
        }
        return;
    };
    egui::ScrollArea::vertical().id_salt(("etf-detail", t)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(dp(4.0));
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&f.ticker).font(mono(17.0)).color(amber()));
            ui.label(RichText::new(&f.name).font(ui_font(16.0)).color(text()));
        });
        meta(ui, &f);
        ui.add_space(dp(8.0));
        let (top, other) = f.top_and_other(TOP);
        let wide = ui.available_width() > dp(640.0);
        let mut hover = None;
        let charts = |ui: &mut Ui, hover: &mut Option<usize>| {
            *hover = donut(ui, &f, top, other, dp(210.0));
            ui.add_space(dp(16.0));
            ui.vertical(|ui| sectors(ui, &f));
        };
        if wide {
            ui.horizontal_top(|ui| charts(ui, &mut hover));
        } else {
            charts(ui, &mut hover);
        }
        ui.add_space(dp(10.0));
        ui.label(RichText::new(format!("Top {} holdings", top.len())).font(ui_font(13.0)).color(text()));
        let suffix = if f.partial() { " (Yahoo lists only these)" } else { "" };
        dim(ui, format!("% of net assets{suffix}. Right-click a row for the chart or the ETFs that hold it."));
        holdings_table(ui, &f, top, other, hover, next, actions);
        ui.add_space(dp(12.0));
    });
}

fn meta(ui: &mut Ui, f: &Fund) {
    let line = match f.source {
        Source::Nport => {
            let mut s = String::from("Source: SEC N-PORT-P");
            if let Some(d) = f.as_of {
                s += &format!(" · holdings as of {}", date(d));
            }
            if let Some(d) = f.filed {
                s += &format!(" (filed {})", date(d));
            }
            s += &format!(" · {} positions", f.positions);
            if let Some(n) = f.net_assets {
                s += &format!(" · net assets {}", fmt_money(n));
            }
            s
        }
        Source::Yahoo => "Source: Yahoo Finance · top 10 holdings only, no report date. This fund files no SEC N-PORT \
                          (e.g. a unit investment trust or commodity trust)."
            .into(),
    };
    ui.add(egui::Label::new(RichText::new(line).font(ui_font(11.5)).color(muted())).wrap());
    if f.source == Source::Nport {
        dim(
            ui,
            format!(
                "Tickers resolved by issuer name for {} of {} holdings (best effort).",
                f.resolved(),
                f.holdings.len()
            ),
        );
    }
}

/// Donut of the top holdings plus "Other". Returns the slice under the pointer.
fn donut(ui: &mut Ui, f: &Fund, top: &[etf::Holding], other: f64, size: f32) -> Option<usize> {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let slices: Vec<(String, f64, Color32)> = top
        .iter()
        .enumerate()
        .map(|(i, h)| (h.ticker.clone().unwrap_or_else(|| h.name.clone()), h.weight, slice_color(i)))
        .chain((other > 0.05).then(|| ("Other".to_string(), other, theme::control_hover())))
        .collect();
    let total: f64 = slices.iter().map(|s| s.1).sum();
    if total <= 0.0 {
        return None;
    }
    let center = rect.center();
    let outer = size / 2.0 - 2.0;
    let inner = outer * 0.58;
    let mut mesh = Mesh::default();
    let mut start = -PI / 2.0;
    let mut bounds = Vec::new();
    for (_, w, color) in &slices {
        let sweep = (*w / total) as f32 * TAU;
        let steps = ((sweep / TAU * 120.0).ceil() as usize).max(1);
        let base = mesh.vertices.len() as u32;
        for k in 0..=steps {
            let a = start + sweep * k as f32 / steps as f32;
            let (s, c) = a.sin_cos();
            mesh.colored_vertex(center + vec2(c, s) * outer, *color);
            mesh.colored_vertex(center + vec2(c, s) * inner, *color);
        }
        for k in 0..steps as u32 {
            let i = base + 2 * k;
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i + 1, i + 3, i + 2);
        }
        bounds.push((start, start + sweep));
        start += sweep;
    }
    let painter = ui.painter_at(rect);
    painter.add(Shape::mesh(mesh));
    // Separators between slices.
    for (a, _) in &bounds {
        let (s, c) = a.sin_cos();
        painter.line_segment(
            [center + vec2(c, s) * inner, center + vec2(c, s) * outer],
            egui::Stroke::new(1.5, theme::window()),
        );
    }
    let shown: f64 = top.iter().map(|h| h.weight).sum();
    painter.text(center - vec2(0.0, dp(8.0)), Align2::CENTER_CENTER, &f.ticker, mono(15.0), amber());
    painter.text(
        center + vec2(0.0, dp(9.0)),
        Align2::CENTER_CENTER,
        format!("top {}: {}", top.len(), pct(shown)),
        ui_font(11.0),
        muted(),
    );
    let hover = resp.hover_pos().and_then(|pos| {
        let d = pos - center;
        let r = d.length();
        if r < inner || r > outer {
            return None;
        }
        let mut a = d.y.atan2(d.x);
        if a < -PI / 2.0 {
            a += TAU;
        }
        bounds.iter().position(|(s, e)| a >= *s && a < *e)
    });
    if let Some(i) = hover {
        let (name, w, _) = &slices[i];
        resp.on_hover_text(format!("{name}: {}", pct(*w)));
    }
    hover
}

fn sectors(ui: &mut Ui, f: &Fund) {
    ui.label(RichText::new("Sectors").font(ui_font(13.0)).color(text()));
    if f.sectors.is_empty() {
        let why = match f.source {
            Source::Nport => "N-PORT carries no sector data and Yahoo lists none for this fund.",
            Source::Yahoo => "Yahoo lists none for this fund.",
        };
        ui.add(
            egui::Label::new(RichText::new(format!("Not available. {why}")).font(ui_font(11.5)).color(muted())).wrap(),
        );
        return;
    }
    dim(ui, "Share of the equity portfolio · Yahoo Finance");
    let max = f.sectors.iter().map(|s| s.weight).fold(0.0, f64::max).max(1.0);
    let bar_w = dp(150.0);
    for (i, s) in f.sectors.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.add_sized(
                [dp(150.0), dp(16.0)],
                egui::Label::new(RichText::new(&s.label).font(ui_font(11.5)).color(text())).truncate(),
            );
            let (r, _) = ui.allocate_exact_size(vec2(bar_w, dp(10.0)), Sense::hover());
            let w = (s.weight / max) as f32 * r.width();
            ui.painter().rect_filled(egui::Rect::from_min_size(r.min, vec2(w, r.height())), 2.0, slice_color(i));
            ui.label(RichText::new(pct(s.weight)).font(mono(11.5)).color(muted()));
        });
    }
}

fn holdings_table(
    ui: &mut Ui,
    f: &Fund,
    top: &[etf::Holding],
    other: f64,
    hover: Option<usize>,
    next: &mut Option<View>,
    actions: &mut Vec<Action>,
) {
    egui::Grid::new(("etf-top", &f.ticker)).num_columns(6).striped(true).spacing([dp(12.0), dp(4.0)]).show(ui, |ui| {
        for h in ["", "#", "Holding", "Ticker", "Weight", "Value"] {
            ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
        }
        ui.end_row();
        for (i, h) in top.iter().enumerate() {
            let on = hover == Some(i);
            let (sw, _) = ui.allocate_exact_size(vec2(dp(10.0), dp(10.0)), Sense::hover());
            ui.painter().rect_filled(sw, 2.0, slice_color(i));
            ui.label(RichText::new((i + 1).to_string()).font(mono(11.5)).color(muted()));
            let name = ui.label(RichText::new(&h.name).font(ui_font(12.5)).color(if on { amber() } else { text() }));
            // (open the chart?, ticker) picked from a context menu.
            let mut picked: Option<(bool, String)> = None;
            let mut menu = |ui: &mut Ui| {
                if let Some(t) = &h.ticker {
                    if ui.button(format!("Chart {t}")).clicked() {
                        picked = Some((true, t.clone()));
                        ui.close();
                    }
                    if ui.button(format!("Which ETFs hold {t}")).clicked() {
                        picked = Some((false, t.clone()));
                        ui.close();
                    }
                } else {
                    ui.label(RichText::new("No ticker resolved for this issuer").color(muted()));
                }
            };
            name.context_menu(&mut menu);
            match &h.ticker {
                Some(t) => {
                    let r = link(ui, t, amber()).on_hover_text("Open the chart");
                    if r.clicked() {
                        actions.push(Action::Command(t.clone()));
                    }
                    r.context_menu(&mut menu);
                }
                None => {
                    ui.label(RichText::new("–").font(mono(12.5)).color(muted())).on_hover_text("Ticker unknown");
                }
            }
            match picked {
                Some((true, t)) => actions.push(Action::Command(t)),
                Some((false, t)) => *next = Some(View::Reverse(t)),
                None => {}
            }
            ui.label(RichText::new(pct(h.weight)).font(mono(12.5)).color(text()));
            ui.label(RichText::new(h.value.map_or_else(|| "–".into(), fmt_money)).font(mono(12.0)).color(muted()));
            ui.end_row();
        }
        if other > 0.05 {
            let (sw, _) = ui.allocate_exact_size(vec2(dp(10.0), dp(10.0)), Sense::hover());
            ui.painter().rect_filled(sw, 2.0, theme::control_hover());
            ui.label("");
            let rest = f.positions.saturating_sub(top.len());
            let label = if f.partial() { "Other holdings".to_string() } else { format!("Other ({rest} positions)") };
            ui.label(RichText::new(label).font(ui_font(12.5)).color(muted()));
            ui.label("");
            ui.label(RichText::new(pct(other)).font(mono(12.5)).color(muted()));
            let value = f.net_assets.map(|n| n * other / 100.0).filter(|_| !f.partial());
            ui.label(RichText::new(value.map_or_else(|| "–".into(), fmt_money)).font(mono(12.0)).color(muted()));
            ui.end_row();
        }
    });
}

fn reverse(ui: &mut Ui, p: &mut EtfPane, t: &str, next: &mut Option<View>, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Which ETFs hold").font(ui_font(12.5)).color(text()));
        let r = ui.add(
            egui::TextEdit::singleline(&mut p.rev_input)
                .hint_text(RichText::new("stock ticker, e.g. NVDA").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(150.0)),
        );
        let go = ui.small_button("Search").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
        if go {
            *next = Some(View::Reverse(clean(&p.rev_input)));
        }
    });
    if t.is_empty() {
        dim(ui, "Enter a stock ticker to see which indexed ETFs hold it, largest weight first.");
        return;
    }
    let stamp = p.stamp();
    if !matches!(&p.calc, Some(Calc::Reverse { query, stamp: s, .. }) if query == t && *s == stamp) {
        let funds: Vec<Arc<Fund>> = p.funds.values().cloned().collect();
        p.calc = Some(Calc::Reverse { query: t.to_string(), stamp, hits: etf::reverse_search(&funds, t) });
    }
    let Some(Calc::Reverse { hits, .. }) = &p.calc else { return };
    let (n, full) = p.indexed();
    ui.add_space(dp(4.0));
    ui.label(
        RichText::new(format!("{t}: held by {} of {n} indexed ETFs", hits.len())).font(ui_font(13.0)).color(text()),
    );
    dim(
        ui,
        format!(
            "{full} indexed with full holdings; {} top-10-only funds list the {t} weight only when it is in their top 10. \
             Index the curated list for broader coverage.",
            n - full
        ),
    );
    if hits.is_empty() {
        return;
    }
    egui::ScrollArea::vertical().id_salt("etf-reverse").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("etf-reverse-grid").num_columns(4).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            for h in ["ETF", "Name", "Weight", "Value"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for h in hits {
                if link(ui, &h.fund, amber()).on_hover_text("Open this fund").clicked() {
                    *next = Some(View::Detail(h.fund.clone()));
                }
                ui.label(RichText::new(&h.name).font(ui_font(12.5)).color(text()));
                let note = if h.partial { " (top 10)" } else { "" };
                ui.label(RichText::new(format!("{}{note}", pct(h.weight))).font(mono(12.5)).color(text()));
                ui.label(RichText::new(h.value.map_or_else(|| "–".into(), fmt_money)).font(mono(12.0)).color(muted()));
                ui.end_row();
            }
        });
        if ui.small_button(format!("Chart {t}")).clicked() {
            actions.push(Action::Command(t.to_string()));
        }
    });
}

fn overlap(ui: &mut Ui, p: &mut EtfPane, a: &str, b: &str, next: &mut Option<View>) {
    ui.horizontal_wrapped(|ui| {
        let mut enter = false;
        for (buf, hint) in [(&mut p.ov_a, "ETF A"), (&mut p.ov_b, "ETF B")] {
            let r = ui.add(
                egui::TextEdit::singleline(buf)
                    .hint_text(RichText::new(hint).font(ui_font(12.0)).color(muted()))
                    .font(mono(12.5))
                    .desired_width(dp(90.0)),
            );
            enter |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        }
        if ui.small_button("Compare").clicked() || enter {
            *next = Some(View::Overlap(clean(&p.ov_a), clean(&p.ov_b)));
        }
    });
    let (Some(fa), Some(fb)) = (p.funds.get(a).cloned(), p.funds.get(b).cloned()) else {
        for t in [a, b] {
            if !p.funds.contains_key(t) {
                match p.errors.get(t) {
                    Some(e) => ui.label(RichText::new(format!("{t}: {e}")).font(ui_font(12.0)).color(theme::down())),
                    None => ui.label(RichText::new(format!("Loading {t}…")).font(ui_font(12.0)).color(muted())),
                };
            }
        }
        return;
    };
    let stamp = p.stamp();
    if !matches!(&p.calc, Some(Calc::Overlap { a: x, b: y, stamp: s, .. }) if x == a && y == b && *s == stamp) {
        p.calc = Some(Calc::Overlap { a: a.into(), b: b.into(), stamp, result: Box::new(etf::overlap(&fa, &fb)) });
    }
    let Some(Calc::Overlap { result: o, .. }) = &p.calc else { return };
    ui.add_space(dp(6.0));
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{a} / {b} overlap")).font(ui_font(13.0)).color(text()));
        ui.label(RichText::new(pct(o.percent)).font(mono(18.0)).color(amber()))
            .on_hover_text("Sum over shared holdings of the smaller of the two weights");
    });
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width().min(dp(420.0)), dp(8.0)), Sense::hover());
    ui.painter().rect_filled(r, 3.0, theme::control());
    let w = (o.percent / 100.0).clamp(0.0, 1.0) as f32 * r.width();
    ui.painter().rect_filled(egui::Rect::from_min_size(r.min, vec2(w, r.height())), 3.0, amber());
    dim(
        ui,
        format!(
            "{} shared holdings · {a}: {} positions, {b}: {} positions",
            o.shared.len(),
            fa.positions,
            fb.positions
        ),
    );
    if o.partial {
        ui.label(
            RichText::new("One fund lists only its top 10 holdings (Yahoo): the overlap is a lower bound.")
                .font(ui_font(11.5))
                .color(amber()),
        );
    }
    let asof = |f: &Fund| f.as_of.map_or_else(|| "no report date".to_string(), |d| format!("as of {}", date(d)));
    dim(ui, format!("{a} {} · {b} {}", asof(&fa), asof(&fb)));
    egui::ScrollArea::vertical().id_salt("etf-overlap").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("etf-overlap-grid").num_columns(5).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            for h in ["Holding", "Ticker", a, b, "Common"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for s in o.shared.iter().take(OVERLAP_ROWS) {
                ui.label(RichText::new(&s.name).font(ui_font(12.5)).color(text()));
                ui.label(RichText::new(s.ticker.as_deref().unwrap_or("–")).font(mono(12.5)).color(amber()));
                ui.label(RichText::new(pct(s.a)).font(mono(12.5)).color(text()));
                ui.label(RichText::new(pct(s.b)).font(mono(12.5)).color(text()));
                ui.label(RichText::new(pct(s.common())).font(mono(12.5)).color(muted()));
                ui.end_row();
            }
        });
        if o.shared.len() > OVERLAP_ROWS {
            dim(ui, format!("… and {} smaller shared holdings", o.shared.len() - OVERLAP_ROWS));
        }
    });
}
