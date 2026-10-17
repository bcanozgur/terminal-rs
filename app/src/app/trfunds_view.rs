//! Turkish mutual funds (`TRF`): the KAP fund list with search, one fund's card, asset and stock
//! allocation as donut charts and lists, monthly buys and sells, and reverse search (which
//! funds hold a BIST stock). Data layer: [`tr_data::trfunds`].

use std::collections::{HashMap, HashSet};
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use egui::{Align2, Color32, CursorIcon, Key, Mesh, RichText, Sense, Shape, Ui, vec2};
use tr_data::trfunds::{self, FundData, FundInfo, Held, Move};

use super::fa_view::fmt_money_in;
use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::trfunds::{TrfReq, TrfResp};
use crate::worker::Request;

/// Rows of the stock and asset lists and slices of the donuts; the rest is one "Other" slice.
const TOP: usize = 10;
/// Rows of the buy/sell tables.
const CHANGE_ROWS: usize = 25;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Detail(String),
    Reverse(String),
}

pub struct TrfPane {
    pub view: View,
    list: Option<Arc<Vec<FundInfo>>>,
    /// Upper-case `code name manager type` of each list row, for the search box.
    hay: Vec<String>,
    list_loading: bool,
    list_error: Option<String>,
    funds: HashMap<String, Arc<FundData>>,
    loading: HashSet<String>,
    errors: HashMap<String, String>,
    queue: Vec<(String, bool)>,
    need_cached: bool,
    need_list: bool,
    index_req: Option<bool>,
    index: Option<(usize, usize)>,
    search: String,
    indexed_only: bool,
    input: String,
    rev_input: String,
    calc: Option<(String, (usize, i64), Vec<Held>)>,
}

fn clean(s: &str) -> String {
    s.split_whitespace().next().unwrap_or_default().to_ascii_uppercase()
}

impl TrfPane {
    pub fn new(mode: &str, a: &str) -> Self {
        let a = clean(a);
        let view = match mode {
            "detail" if !a.is_empty() => View::Detail(a),
            "reverse" if !a.is_empty() => View::Reverse(trfunds::clean_ticker(&a)),
            _ => View::List,
        };
        let mut p = Self {
            view: View::List,
            list: None,
            hay: Vec::new(),
            list_loading: false,
            list_error: None,
            funds: HashMap::new(),
            loading: HashSet::new(),
            errors: HashMap::new(),
            queue: Vec::new(),
            need_cached: true,
            need_list: true,
            index_req: None,
            index: None,
            search: String::new(),
            indexed_only: false,
            input: String::new(),
            rev_input: String::new(),
            calc: None,
        };
        p.open(view);
        p
    }

    /// `(mode, a)` for saved layouts.
    pub fn settings(&self) -> (&'static str, String) {
        match &self.view {
            View::List => ("list", String::new()),
            View::Detail(c) => ("detail", c.clone()),
            View::Reverse(t) => ("reverse", t.clone()),
        }
    }

    pub fn title(&self) -> String {
        match &self.view {
            View::List => "TR Funds".into(),
            View::Detail(c) => format!("{c} Fund"),
            View::Reverse(t) => format!("Funds holding {t}"),
        }
    }

    pub fn code(&self) -> &'static str {
        "TRF"
    }

    fn want(&mut self, code: &str, force: bool) {
        let c = clean(code);
        if c.is_empty() || self.loading.contains(&c) {
            return;
        }
        self.errors.remove(&c);
        self.loading.insert(c.clone());
        self.queue.push((c, force));
    }

    pub fn open(&mut self, view: View) {
        match &view {
            View::List => {}
            View::Detail(c) => {
                self.input = c.clone();
                self.want(&c.clone(), false);
            }
            View::Reverse(t) => self.rev_input = t.clone(),
        }
        self.view = view;
        self.calc = None;
        // Other panes may have indexed funds since the cache was last read.
        self.need_cached = true;
    }

    fn set_list(&mut self, list: Arc<Vec<FundInfo>>) {
        self.hay = list
            .iter()
            .map(|f| trfunds::tr_upper(&format!("{} {} {} {}", f.code, f.name, f.manager, f.kind)))
            .collect();
        self.list = Some(list);
    }

    fn stamp(&self) -> (usize, i64) {
        (self.funds.len(), self.funds.values().map(|f| f.updated).sum())
    }

    /// Funds on disk with stock positions: what reverse search covers.
    fn searchable(&self) -> usize {
        self.funds.values().filter(|f| f.latest().is_some_and(|s| !s.report.positions.is_empty())).count()
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    fn trf_target(&self) -> Option<crate::worker::PaneId> {
        let is_trf = |id: crate::worker::PaneId| {
            self.idx(id).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::TrFunds(_)))
        };
        if is_trf(self.focus) {
            return Some(self.focus);
        }
        self.panes.iter().find(|p| matches!(p.kind, PaneKind::TrFunds(_))).map(|p| p.id)
    }

    fn with_trf(&mut self, view: View) {
        match self.trf_target() {
            Some(id) => {
                if let Some(PaneKind::TrFunds(e)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    e.open(view);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => {
                let mut e = TrfPane::new("list", "");
                e.open(view);
                self.open_panel(PaneKind::TrFunds(Box::new(e)));
            }
        }
    }

    /// `TRF`, `TRF AFT` (fund detail), `TRF HELD THYAO` (reverse search).
    pub(super) fn trf_command(&mut self, arg: &str) {
        let words: Vec<String> = arg.split_whitespace().map(str::to_ascii_uppercase).collect();
        let view = match words.as_slice() {
            [] => View::List,
            [k, t, ..] if matches!(k.as_str(), "HELD" | "IN" | "WHO" | "REV" | "HOLDERS") => {
                View::Reverse(trfunds::clean_ticker(t))
            }
            [c, ..] => View::Detail(c.clone()),
        };
        self.with_trf(view);
    }

    /// `THYAO TRFS`: the funds holding a stock.
    pub(super) fn trf_held(&mut self, ticker: &str) {
        self.trf_command(&format!("HELD {ticker}"));
    }

    pub(super) fn trf_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::TrFunds(e) = &mut self.panes[i].kind else { continue };
            if std::mem::take(&mut e.need_cached) {
                self.worker.send(Request::TrFunds(TrfReq::Cached { pane: id }));
            }
            if std::mem::take(&mut e.need_list) && !e.list_loading {
                e.list_loading = true;
                self.worker.send(Request::TrFunds(TrfReq::List { pane: id }));
            }
            for (code, force) in std::mem::take(&mut e.queue) {
                self.worker.send(Request::TrFunds(TrfReq::Load { pane: id, code, force }));
            }
            if let Some(force) = e.index_req.take() {
                let codes: Vec<String> = trfunds::CURATED.iter().map(|c| c.to_string()).collect();
                e.loading.extend(codes.iter().cloned());
                e.index = Some((0, codes.len()));
                self.worker.send(Request::TrFunds(TrfReq::Index { pane: id, codes, force }));
            }
        }
    }

    pub(super) fn on_trfunds(&mut self, resp: TrfResp) {
        let pane = match &resp {
            TrfResp::Cached { pane, .. }
            | TrfResp::List { pane, .. }
            | TrfResp::Fund { pane, .. }
            | TrfResp::Progress { pane, .. } => *pane,
        };
        let Some(PaneKind::TrFunds(e)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        match resp {
            TrfResp::Cached { list, funds, .. } => {
                if let Some(l) = list.filter(|_| e.list.is_none()) {
                    e.set_list(l);
                }
                for f in funds {
                    if e.funds.get(&f.info.code).is_none_or(|old| old.updated < f.updated) {
                        e.funds.insert(f.info.code.clone(), f);
                    }
                }
            }
            TrfResp::List { list, .. } => {
                e.list_loading = false;
                match list {
                    Ok(l) => {
                        e.list_error = None;
                        e.set_list(l);
                    }
                    Err(m) => e.list_error = Some(m),
                }
            }
            TrfResp::Fund { code, fund, .. } => {
                e.loading.remove(&code);
                match fund {
                    Ok(f) => {
                        e.errors.remove(&code);
                        e.funds.insert(code, f);
                    }
                    Err(m) => {
                        e.errors.insert(code, m);
                    }
                }
            }
            TrfResp::Progress { done, total, .. } => {
                e.index = (done < total).then_some((done, total));
                e.need_cached |= done >= total;
            }
        }
    }
}

// ---- drawing ------------------------------------------------------------------------------

fn slice_color(i: usize) -> Color32 {
    let base = theme::series(i);
    if i >= 6 { base.lerp_to_gamma(Color32::WHITE, 0.4) } else { base }
}

fn pct(w: f64) -> String {
    if w.abs() >= 10.0 { format!("{w:.1}%") } else { format!("{w:.2}%") }
}

fn signed(w: f64) -> String {
    format!("{w:+.2}")
}

fn tl(v: f64) -> String {
    fmt_money_in(v, "TRY ")
}

fn link(ui: &mut Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(label).font(mono(12.5)).color(color)).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn dim(ui: &mut Ui, s: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(s.into()).font(ui_font(11.5)).color(muted())).wrap());
}

pub fn trfunds(ui: &mut Ui, p: &mut TrfPane) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut next: Option<View> = None;
    header(ui, p, &mut next);
    ui.add_space(dp(4.0));
    match p.view.clone() {
        View::List => list(ui, p, &mut next),
        View::Detail(c) => detail(ui, p, &c, &mut next, &mut actions),
        View::Reverse(t) => reverse(ui, p, &t, &mut next, &mut actions),
    }
    if let Some(v) = next {
        p.open(v);
    }
    actions
}

fn header(ui: &mut Ui, p: &mut TrfPane, next: &mut Option<View>) {
    ui.horizontal_wrapped(|ui| {
        let tab =
            |ui: &mut Ui, on: bool, label: &str| ui.selectable_label(on, RichText::new(label).font(ui_font(12.5)));
        if tab(ui, p.view == View::List, "Funds").clicked() {
            *next = Some(View::List);
        }
        if tab(ui, matches!(p.view, View::Reverse(_)), "Held by (reverse search)").clicked() {
            *next = Some(View::Reverse(trfunds::clean_ticker(&p.rev_input)));
        }
        ui.separator();
        let r = ui.add(
            egui::TextEdit::singleline(&mut p.input)
                .hint_text(RichText::new("Open fund code…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(130.0)),
        );
        let go = ui.small_button("Open").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
        if go && !clean(&p.input).is_empty() {
            *next = Some(View::Detail(clean(&p.input)));
        }
    });
    ui.horizontal_wrapped(|ui| {
        dim(
            ui,
            format!(
                "{} fund{} on disk ({} with stock positions). Reverse search covers these.",
                p.funds.len(),
                if p.funds.len() == 1 { "" } else { "s" },
                p.searchable()
            ),
        );
        match p.index {
            Some((done, total)) => {
                ui.add(egui::Spinner::new().size(dp(12.0)));
                dim(ui, format!("indexing {done}/{total}…"));
            }
            None => {
                let r = ui.small_button("Index curated list").on_hover_text(format!(
                    "Download the two newest portfolio reports of {} popular funds from KAP, one fund at a time with a \
                     pause between funds. Funds already fresh on disk are skipped.",
                    trfunds::CURATED.len()
                ));
                if r.clicked() {
                    p.index_req = Some(false);
                }
            }
        }
    });
}

fn status(p: &TrfPane, code: &str) -> (String, Color32) {
    if let Some(f) = p.funds.get(code) {
        return match f.latest() {
            Some(s) if !s.report.positions.is_empty() => {
                (format!("{} · {} stocks", s.label(), s.report.positions.len()), text())
            }
            Some(s) => (format!("{} · no stock rows", s.label()), muted()),
            None => ("no report".into(), muted()),
        };
    }
    if p.loading.contains(code) {
        return ("loading…".into(), muted());
    }
    match p.errors.get(code) {
        Some(_) => ("no data".into(), theme::down()),
        None => (String::new(), muted()),
    }
}

fn list(ui: &mut Ui, p: &mut TrfPane, next: &mut Option<View>) {
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut p.search)
                .hint_text(RichText::new("Search code, name, manager or type…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(280.0)),
        );
        ui.checkbox(&mut p.indexed_only, RichText::new("Indexed only").font(ui_font(12.0)));
        if p.list_loading {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "loading the fund list from KAP…");
        }
    });
    if let Some(e) = &p.list_error {
        ui.label(RichText::new(format!("Fund list: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    let Some(all) = p.list.clone() else {
        if !p.list_loading && p.list_error.is_none() {
            dim(ui, "Nothing loaded.");
        }
        return;
    };
    let q = trfunds::tr_upper(p.search.trim());
    let rows: Vec<usize> = (0..all.len())
        .filter(|&i| q.is_empty() || p.hay[i].contains(&q))
        .filter(|&i| !p.indexed_only || p.funds.contains_key(&all[i].code))
        .collect();
    dim(ui, format!("{} of {} funds. Click a code to open the fund.", rows.len(), all.len()));
    let h = dp(18.0);
    egui::ScrollArea::vertical().id_salt("trf-list").auto_shrink([false, false]).show_rows(
        ui,
        h,
        rows.len(),
        |ui, range| {
            for &i in &rows[range] {
                let f = &all[i];
                ui.horizontal(|ui| {
                    ui.set_height(h);
                    ui.allocate_ui(vec2(dp(56.0), h), |ui| {
                        if link(ui, &f.code, amber()).on_hover_text("Open this fund").clicked() {
                            *next = Some(View::Detail(f.code.clone()));
                        }
                    });
                    ui.allocate_ui(vec2(dp(420.0), h), |ui| {
                        ui.add(egui::Label::new(RichText::new(&f.name).font(ui_font(12.0)).color(text())).truncate());
                    });
                    ui.allocate_ui(vec2(dp(150.0), h), |ui| {
                        ui.add(egui::Label::new(RichText::new(&f.kind).font(ui_font(11.5)).color(muted())).truncate());
                    });
                    let (s, c) = status(p, &f.code);
                    let r = ui.label(RichText::new(s).font(ui_font(11.5)).color(c));
                    if let Some(e) = p.errors.get(&f.code) {
                        r.on_hover_text(e);
                    }
                });
            }
        },
    );
}

fn back(ui: &mut Ui, next: &mut Option<View>) {
    if ui.small_button("◀ Back").on_hover_text("Back to the fund list").clicked() {
        *next = Some(View::List);
    }
}

fn detail(ui: &mut Ui, p: &mut TrfPane, code: &str, next: &mut Option<View>, actions: &mut Vec<Action>) {
    let fund = p.funds.get(code).cloned();
    ui.horizontal(|ui| {
        back(ui, next);
        if ui.small_button("Refresh").on_hover_text("Check KAP for a newer report").clicked() {
            p.want(code, true);
        }
        if p.loading.contains(code) {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "loading from KAP…");
        }
    });
    let Some(f) = fund else {
        match p.errors.get(code) {
            Some(e) => {
                ui.add_space(dp(8.0));
                ui.label(RichText::new(format!("{code}: {e}")).font(ui_font(12.5)).color(theme::down()));
            }
            None if !p.loading.contains(code) => dim(ui, "Nothing loaded."),
            None => {}
        }
        return;
    };
    egui::ScrollArea::vertical().id_salt(("trf-detail", code)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(dp(4.0));
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&f.info.code).font(mono(17.0)).color(amber()));
            ui.label(RichText::new(&f.info.name).font(ui_font(15.0)).color(text()));
        });
        card(ui, &f);
        if let Some(n) = &f.note {
            ui.add_space(dp(4.0));
            ui.label(RichText::new(n).font(ui_font(11.5)).color(amber()));
        }
        let Some(snap) = f.latest() else { return };
        ui.add_space(dp(10.0));
        ui.label(
            RichText::new(format!("Portfolio report: {} (KAP, published {})", snap.label(), snap.published))
                .font(ui_font(12.5))
                .color(text()),
        );
        let report = &snap.report;
        let wide = ui.available_width() > dp(700.0);
        let mut body = |ui: &mut Ui| {
            ui.vertical(|ui| mix_block(ui, &f));
            ui.add_space(dp(18.0));
            ui.vertical(|ui| stock_block(ui, &f, actions));
        };
        ui.add_space(dp(6.0));
        if wide {
            ui.horizontal_top(|ui| body(ui));
        } else {
            body(ui);
        }
        ui.add_space(dp(12.0));
        changes_block(ui, &f, actions);
        if report.positions.is_empty() {
            dim(ui, "No stock positions could be read from this report.");
        }
        ui.add_space(dp(12.0));
    });
}

fn field(ui: &mut Ui, label: &str, value: Option<String>, why: &str) {
    ui.label(RichText::new(label).font(ui_font(11.5)).color(muted()));
    match value {
        Some(v) => ui.label(RichText::new(v).font(ui_font(12.5)).color(text())),
        None => ui.label(RichText::new("n/a").font(ui_font(12.5)).color(muted())).on_hover_text(why),
    };
}

fn card(ui: &mut Ui, f: &FundData) {
    let pr = &f.profile;
    let snap = f.latest().map(|s| &s.report);
    let fee = |v: Option<f64>| v.map(|v| format!("{v:.2}%"));
    let not_in_form = "Not stated in KAP's general information form.";
    egui::Grid::new(("trf-card", &f.info.code)).num_columns(4).spacing([dp(18.0), dp(4.0)]).show(ui, |ui| {
        field(ui, "Founder", Some(f.info.manager.clone()).filter(|s| !s.is_empty()), not_in_form);
        field(ui, "Type", Some(f.info.kind.clone()).filter(|s| s != "–"), not_in_form);
        ui.end_row();
        field(ui, "Fund manager", pr.fund_manager.clone(), not_in_form);
        field(ui, "Inception", pr.inception.map(|d| d.format("%b %-d, %Y").to_string()), not_in_form);
        ui.end_row();
        field(ui, "Management fee", fee(pr.mgmt_fee).map(|s| format!("{s} / year")), not_in_form);
        field(
            ui,
            "Entry / exit fee",
            pr.entry_fee.zip(pr.exit_fee).map(|(a, b)| format!("{a:.2}% / {b:.2}%")),
            not_in_form,
        );
        ui.end_row();
        field(ui, "Performance fee", fee(pr.perf_fee), not_in_form);
        field(ui, "ISIN", pr.isin.clone(), not_in_form);
        ui.end_row();
        field(ui, "Fund size", snap.and_then(|r| r.net_assets).map(tl), "No portfolio report could be read.");
        field(
            ui,
            "Units outstanding",
            snap.and_then(|r| r.units).map(|u| format!("{u:.0}")),
            "Not stated in the report.",
        );
        ui.end_row();
        field(
            ui,
            "Unit price",
            snap.and_then(|r| r.unit_price).map(|v| format!("{v:.6}")),
            "Not stated in the report.",
        );
        field(
            ui,
            "Price change",
            snap.and_then(|r| {
                Some(format!("{}% month · {}% YTD · {}% 12m", signed(r.monthly?), signed(r.ytd?), signed(r.yearly?)))
            }),
            "Not stated in the report.",
        );
        ui.end_row();
        field(
            ui,
            "Settlement (valör)",
            pr.valor.clone(),
            "KAP's structured fund data does not carry the settlement day; it is only free text in the prospectus.",
        );
        ui.end_row();
    });
}

/// A donut of `(label, weight, colour)` slices; returns the slice under the pointer.
fn donut(ui: &mut Ui, slices: &[(String, f64, Color32)], size: f32, center: (&str, String)) -> Option<usize> {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let total: f64 = slices.iter().map(|s| s.1.max(0.0)).sum();
    if total <= 0.0 {
        return None;
    }
    let c = rect.center();
    let outer = size / 2.0 - 2.0;
    let inner = outer * 0.58;
    let mut mesh = Mesh::default();
    let mut start = -PI / 2.0;
    let mut bounds = Vec::new();
    for (_, w, color) in slices {
        let sweep = (w.max(0.0) / total) as f32 * TAU;
        let steps = ((sweep / TAU * 120.0).ceil() as usize).max(1);
        let base = mesh.vertices.len() as u32;
        for k in 0..=steps {
            let a = start + sweep * k as f32 / steps as f32;
            let (s, co) = a.sin_cos();
            mesh.colored_vertex(c + vec2(co, s) * outer, *color);
            mesh.colored_vertex(c + vec2(co, s) * inner, *color);
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
    for (a, _) in &bounds {
        let (s, co) = a.sin_cos();
        painter
            .line_segment([c + vec2(co, s) * inner, c + vec2(co, s) * outer], egui::Stroke::new(1.5, theme::window()));
    }
    painter.text(c - vec2(0.0, dp(8.0)), Align2::CENTER_CENTER, center.0, mono(14.0), amber());
    painter.text(c + vec2(0.0, dp(9.0)), Align2::CENTER_CENTER, center.1, ui_font(11.0), muted());
    let hover = resp.hover_pos().and_then(|pos| {
        let d = pos - c;
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

fn legend_row(ui: &mut Ui, color: Color32, label: &str, weight: f64, on: bool) {
    ui.horizontal(|ui| {
        let (sw, _) = ui.allocate_exact_size(vec2(dp(10.0), dp(10.0)), Sense::hover());
        ui.painter().rect_filled(sw, 2.0, color);
        ui.add_sized(
            [dp(170.0), dp(16.0)],
            egui::Label::new(RichText::new(label).font(ui_font(12.0)).color(if on { amber() } else { text() }))
                .truncate(),
        );
        ui.label(RichText::new(pct(weight)).font(mono(12.0)).color(muted()));
    });
}

fn mix_block(ui: &mut Ui, f: &FundData) {
    ui.label(RichText::new("Asset allocation").font(ui_font(13.0)).color(text()));
    let Some(r) = f.latest().map(|s| &s.report) else { return };
    if r.mix.is_empty() {
        dim(ui, "Not available: the report lists no portfolio mix.");
        return;
    }
    dim(ui, "Average share of the month's portfolio (the report's own mix, not the month-end split).");
    let top: Vec<&trfunds::Alloc> = r.mix.iter().take(TOP).collect();
    let slices: Vec<(String, f64, Color32)> =
        top.iter().enumerate().map(|(i, a)| (a.label.clone(), a.weight, slice_color(i))).collect();
    let total: f64 = r.mix.iter().map(|a| a.weight).sum();
    let hover = donut(ui, &slices, dp(190.0), ("Mix", pct(total)));
    ui.add_space(dp(6.0));
    for (i, (label, w, c)) in slices.iter().enumerate() {
        legend_row(ui, *c, label, *w, hover == Some(i));
    }
}

fn stock_block(ui: &mut Ui, f: &FundData, actions: &mut Vec<Action>) {
    ui.label(RichText::new("Stock allocation").font(ui_font(13.0)).color(text()));
    let Some(r) = f.latest().map(|s| &s.report) else { return };
    let (top, other) = r.top_and_other(TOP);
    if top.is_empty() {
        dim(ui, "Not available: no stock positions could be read from the report.");
        return;
    }
    dim(
        ui,
        format!(
            "Top {} of {} positions, % of fund value; positions under {}% are ignored.",
            top.len(),
            r.significant().count(),
            trfunds::MIN_WEIGHT
        ),
    );
    let mut slices: Vec<(String, f64, Color32)> =
        top.iter().enumerate().map(|(i, p)| (p.ticker.clone(), p.weight, slice_color(i))).collect();
    if other > 0.05 {
        slices.push(("Other".into(), other, theme::control_hover()));
    }
    let shown: f64 = top.iter().map(|p| p.weight).sum();
    let hover = donut(ui, &slices, dp(190.0), ("Stocks", format!("top {}: {}", top.len(), pct(shown))));
    ui.add_space(dp(6.0));
    for (i, (label, w, c)) in slices.iter().enumerate() {
        ui.horizontal(|ui| {
            let (sw, _) = ui.allocate_exact_size(vec2(dp(10.0), dp(10.0)), Sense::hover());
            ui.painter().rect_filled(sw, 2.0, *c);
            if label != "Other" {
                let r =
                    link(ui, label, if hover == Some(i) { text() } else { amber() }).on_hover_text("Open the chart");
                if r.clicked() {
                    actions.push(Action::Command(format!("{label}.IS")));
                }
            } else {
                ui.label(RichText::new("Other").font(mono(12.5)).color(muted()));
            }
            ui.label(RichText::new(pct(*w)).font(mono(12.0)).color(muted()));
        });
    }
}

fn changes_block(ui: &mut Ui, f: &FundData, actions: &mut Vec<Action>) {
    ui.label(RichText::new("Monthly buys and sells").font(ui_font(13.0)).color(text()));
    let (Some(cur), Some(prev)) = (f.latest(), f.previous()) else {
        dim(ui, "Needs two monthly reports; KAP returned only one for this fund.");
        return;
    };
    dim(
        ui,
        format!(
            "{} → {}: by share count when both reports state it (price moves alone are not trades), else by weight. \
             Positions under {}% on both sides are ignored.",
            prev.label(),
            cur.label(),
            trfunds::MIN_WEIGHT
        ),
    );
    let changes = f.changes();
    if changes.is_empty() {
        dim(ui, "No significant changes.");
        return;
    }
    egui::Grid::new(("trf-changes", &f.info.code)).num_columns(6).striped(true).spacing([dp(14.0), dp(4.0)]).show(
        ui,
        |ui| {
            for h in ["Stock", "Move", "Weight before", "Weight after", "Change", "Shares"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for c in changes.iter().take(CHANGE_ROWS) {
                if link(ui, &c.ticker, amber()).on_hover_text("Open the chart").clicked() {
                    actions.push(Action::Command(format!("{}.IS", c.ticker)));
                }
                let color = match c.kind {
                    Move::New | Move::Added => theme::up(),
                    Move::Reduced | Move::Exited => theme::down(),
                };
                ui.label(RichText::new(c.kind.label()).font(ui_font(12.0)).color(color));
                ui.label(RichText::new(pct(c.before)).font(mono(12.0)).color(muted()));
                ui.label(RichText::new(pct(c.after)).font(mono(12.0)).color(text()));
                ui.label(RichText::new(format!("{} pp", signed(c.delta_weight()))).font(mono(12.0)).color(color));
                let shares = match c.delta_shares_pct() {
                    Some(d) => format!("{d:+.1}%"),
                    None => match c.kind {
                        Move::New => "new".into(),
                        Move::Exited => "sold out".into(),
                        _ => "–".into(),
                    },
                };
                ui.label(RichText::new(shares).font(mono(12.0)).color(muted()));
                ui.end_row();
            }
        },
    );
    if changes.len() > CHANGE_ROWS {
        dim(ui, format!("… and {} smaller changes", changes.len() - CHANGE_ROWS));
    }
}

fn reverse(ui: &mut Ui, p: &mut TrfPane, t: &str, next: &mut Option<View>, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Which funds hold").font(ui_font(12.5)).color(text()));
        let r = ui.add(
            egui::TextEdit::singleline(&mut p.rev_input)
                .hint_text(RichText::new("BIST ticker, e.g. THYAO").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(150.0)),
        );
        let go = ui.small_button("Search").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
        if go {
            *next = Some(View::Reverse(trfunds::clean_ticker(&p.rev_input)));
        }
    });
    if t.is_empty() {
        dim(ui, "Enter a BIST ticker to see which indexed funds hold it, largest weight first.");
        return;
    }
    let stamp = p.stamp();
    if !matches!(&p.calc, Some((q, s, _)) if q == t && *s == stamp) {
        let funds: Vec<Arc<FundData>> = p.funds.values().cloned().collect();
        p.calc = Some((t.to_string(), stamp, trfunds::reverse_search(&funds, t)));
    }
    let Some((_, _, hits)) = &p.calc else { return };
    ui.add_space(dp(4.0));
    ui.label(
        RichText::new(format!("{t}: held by {} of {} indexed funds", hits.len(), p.searchable()))
            .font(ui_font(13.0))
            .color(text()),
    );
    dim(
        ui,
        "Latest monthly report of each fund on disk; funds whose report layout is not recognised are not covered. \
         Use 'Index curated list' for broader coverage.",
    );
    if hits.is_empty() {
        return;
    }
    egui::ScrollArea::vertical().id_salt("trf-reverse").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("trf-reverse-grid").num_columns(5).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            for h in ["Fund", "Name", "Weight", "Previous", "Report"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for h in hits {
                if link(ui, &h.code, amber()).on_hover_text("Open this fund").clicked() {
                    *next = Some(View::Detail(h.code.clone()));
                }
                ui.add(egui::Label::new(RichText::new(&h.name).font(ui_font(12.0)).color(text())).truncate());
                ui.label(RichText::new(pct(h.weight)).font(mono(12.5)).color(text()));
                ui.label(RichText::new(h.previous.map_or_else(|| "–".into(), pct)).font(mono(12.0)).color(muted()));
                ui.label(RichText::new(&h.period).font(ui_font(11.5)).color(muted()));
                ui.end_row();
            }
        });
        if ui.small_button(format!("Chart {t}")).clicked() {
            actions.push(Action::Command(format!("{t}.IS")));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_view_rereads_the_disk_cache() {
        let mut p = TrfPane::new("list", "");
        p.need_cached = false;
        p.open(View::Reverse("THYAO".into()));
        assert!(p.need_cached);
        p.need_cached = false;
        p.open(View::Detail("TI2".into()));
        assert!(p.need_cached);
        assert_eq!(p.settings(), ("detail", "TI2".into()));
    }

    #[test]
    fn saved_layout_roundtrip() {
        assert_eq!(TrfPane::new("reverse", "thyao.is").view, View::Reverse("THYAO".into()));
        assert_eq!(TrfPane::new("detail", "").view, View::List);
    }
}
