//! KAP disclosures (`KAP`, `THYAO KAP`): the public disclosure feed of Borsa Istanbul companies,
//! newest first; filter to your watchlists and positions, to a category or to one company, read a
//! disclosure's text inside the pane. Data layer: [`tr_data::kap`].

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, Utc};
use egui::{Color32, CursorIcon, Key, RichText, Sense, Ui};
use tr_data::kap::{Category, Detail, Disclosure};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::kap::{KapReq, KapResp};
use crate::layouts::PaneSettings;
use crate::theme::{self, amber, cyan, dp, mono, muted, text, ui_font};
use crate::worker::Request;

/// Marks a toast as a KAP disclosure (drawn with a "KAP" title).
pub const TOAST_PREFIX: &str = "\u{1}kap:";
/// How often a visible pane re-fetches.
const REFRESH: Duration = Duration::from_secs(120);
/// Disclosures kept in the feed (about two days of the filtered feed).
const KEEP: usize = 1500;
/// Days fetched when the pane first loads (three cover a weekend), then one.
const FIRST_DAYS: u32 = 3;
/// Disclosures drawn in the toast text before "+N more".
const TOAST_MAX: usize = 3;
const ROW_H: f32 = 22.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    /// Companies on the watchlist, the sidebar lists and in open positions.
    Mine,
}

pub struct KapPane {
    scope: Scope,
    category: Option<Category>,
    /// One company's ticker; the pane then lists only its disclosures.
    company: Option<String>,
    /// Ticker box while it is being edited.
    company_edit: Option<String>,

    feed: Vec<Disclosure>,
    feed_error: Option<String>,
    feed_at: Option<i64>,
    feed_loading: bool,
    feed_req: Option<Instant>,

    firm: Vec<Disclosure>,
    firm_for: Option<String>,
    firm_error: Option<String>,
    firm_at: Option<i64>,
    firm_loading: bool,
    firm_req: Option<Instant>,

    /// The disclosure open for reading.
    open: Option<u64>,
    details: HashMap<u64, Result<Detail, String>>,
    detail_loading: HashSet<u64>,
}

impl KapPane {
    pub fn new(scope: Scope, category: Option<Category>, company: Option<&str>) -> Self {
        Self {
            scope,
            category,
            company: company.map(clean_ticker).filter(|c| !c.is_empty()),
            company_edit: None,
            feed: Vec::new(),
            feed_error: None,
            feed_at: None,
            feed_loading: false,
            feed_req: None,
            firm: Vec::new(),
            firm_for: None,
            firm_error: None,
            firm_at: None,
            firm_loading: false,
            firm_req: None,
            open: None,
            details: HashMap::new(),
            detail_loading: HashSet::new(),
        }
    }

    /// From a saved layout: `scope` is `all` or `mine`, `category` a [`Category::key`] or empty.
    pub fn from_settings(scope: &str, category: &str, company: &str) -> Self {
        let scope = if scope == "mine" { Scope::Mine } else { Scope::All };
        Self::new(scope, Category::from_word(category), Some(company))
    }

    pub fn settings(&self) -> PaneSettings {
        PaneSettings::Kap {
            scope: if self.scope == Scope::Mine { "mine" } else { "all" }.into(),
            category: self.category.map(|c| c.key().to_string()).unwrap_or_default(),
            company: self.company.clone().unwrap_or_default(),
        }
    }

    pub fn title(&self) -> String {
        match &self.company {
            Some(c) => format!("{c} KAP"),
            None => "KAP".into(),
        }
    }

    pub fn code(&self) -> &'static str {
        "KAP"
    }

    /// The followed company's ticker.
    pub fn symbol(&self) -> Option<&str> {
        self.company.as_deref()
    }

    /// Show one company's disclosures (link groups, the ticker box, `THYAO KAP`).
    pub fn set_company(&mut self, ticker: &str) {
        let t = clean_ticker(ticker);
        if t.is_empty() || self.company.as_deref() == Some(t.as_str()) {
            return;
        }
        self.company = Some(t);
        self.open = None;
    }

    fn clear_company(&mut self) {
        self.company = None;
        self.open = None;
    }

    /// Merge fresh rows into the feed, newest first. Returns the rows that were not there yet
    /// (empty on the first load).
    fn merge_feed(&mut self, rows: Vec<Disclosure>) -> Vec<Disclosure> {
        let first = self.feed_at.is_none();
        let known: HashSet<u64> = self.feed.iter().map(|d| d.index).collect();
        let fresh: Vec<Disclosure> = rows.iter().filter(|d| !known.contains(&d.index)).cloned().collect();
        self.feed.extend(fresh.iter().cloned());
        self.feed.sort_by_key(|d| std::cmp::Reverse(d.index));
        self.feed.truncate(KEEP);
        self.feed_at = Some(Utc::now().timestamp());
        if first { Vec::new() } else { fresh }
    }

    fn rows(&self) -> &[Disclosure] {
        if self.company.is_some() { &self.firm } else { &self.feed }
    }
}

fn clean_ticker(s: &str) -> String {
    s.trim().to_ascii_uppercase().trim_end_matches(".IS").to_string()
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `KAP`, `KAP MINE`, `KAP DIVIDEND`, `KAP THYAO`, `THYAO KAP`: focus (or open) the pane
    /// with those filters. Words may be combined (`KAP MINE BUYBACK`); `KAP ALL` clears them.
    pub(super) fn kap_command(&mut self, arg: &str, symbol: Option<String>) {
        let mut scope = None;
        let mut category = None;
        let mut company = symbol.map(|s| tr_data::security(&s).ticker);
        let mut clear = false;
        for w in arg.split_whitespace() {
            match w.to_ascii_uppercase().as_str() {
                "MINE" | "MY" | "WL" | "WATCHLIST" | "PORT" | "PORTFOLIO" => scope = Some(Scope::Mine),
                "ALL" | "LATEST" => {
                    scope = Some(Scope::All);
                    clear = true;
                }
                _ => match Category::from_word(w) {
                    Some(c) => category = Some(c),
                    None => company = Some(clean_ticker(w)),
                },
            }
        }
        let is_kap = |k: &PaneKind| matches!(k, PaneKind::Kap(_));
        let target = self
            .idx(self.focus)
            .filter(|&i| is_kap(&self.panes[i].kind))
            .map(|i| self.panes[i].id)
            .or_else(|| self.panes.iter().find(|p| is_kap(&p.kind)).map(|p| p.id));
        match target {
            Some(id) => {
                if let Some(PaneKind::Kap(k)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    if clear {
                        k.clear_company();
                    }
                    if let Some(s) = scope {
                        k.scope = s;
                    }
                    if let Some(c) = category {
                        k.category = Some(c);
                    } else if clear {
                        k.category = None;
                    }
                    if let Some(c) = &company {
                        k.set_company(c);
                    }
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Kap(Box::new(KapPane::new(
                scope.unwrap_or(Scope::All),
                category,
                company.as_deref(),
            )))),
        }
        self.status = match &company {
            Some(c) => format!("{c}: KAP disclosures"),
            None => "KAP disclosures".into(),
        };
    }

    /// Tickers on the watchlist, the sidebar lists and in positions.
    fn kap_mine(&self) -> HashSet<String> {
        self.mine_tickers()
    }

    /// Fetch on first show and every two minutes while a pane is visible, and whatever the
    /// open disclosure needs.
    pub(super) fn kap_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Kap(k) = &mut self.panes[i].kind else { continue };
            let due = |t: Option<Instant>| t.is_none_or(|t| t.elapsed() >= REFRESH);
            match k.company.clone() {
                None => {
                    if !k.feed_loading && due(k.feed_req) {
                        k.feed_loading = true;
                        k.feed_req = Some(Instant::now());
                        let back = if k.feed_at.is_none() { FIRST_DAYS } else { 1 };
                        self.worker.send(Request::Kap(KapReq::Latest { pane: id, back }));
                    }
                }
                Some(code) => {
                    if k.firm_for.as_deref() != Some(code.as_str()) {
                        k.firm_for = Some(code.clone());
                        k.firm.clear();
                        k.firm_error = None;
                        k.firm_at = None;
                        k.firm_req = None;
                    }
                    if !k.firm_loading && due(k.firm_req) {
                        k.firm_loading = true;
                        k.firm_req = Some(Instant::now());
                        self.worker.send(Request::Kap(KapReq::Company { pane: id, code }));
                    }
                }
            }
            if let Some(idx) = k.open {
                let readable = k.rows().iter().chain(&k.feed).any(|d| d.index == idx && d.class != "FR");
                if readable && !k.details.contains_key(&idx) && k.detail_loading.insert(idx) {
                    self.worker.send(Request::Kap(KapReq::Detail { pane: id, index: idx }));
                }
            }
        }
    }

    pub(super) fn on_kap(&mut self, resp: KapResp) {
        let mine = self.kap_mine();
        let pane = match &resp {
            KapResp::Latest { pane, .. } | KapResp::Company { pane, .. } | KapResp::Detail { pane, .. } => *pane,
        };
        let Some(PaneKind::Kap(k)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        let mut toast = Vec::new();
        match resp {
            KapResp::Latest { rows, .. } => {
                k.feed_loading = false;
                match rows {
                    Ok(rows) => {
                        k.feed_error = None;
                        toast = k.merge_feed(rows);
                    }
                    Err(e) => k.feed_error = Some(e),
                }
            }
            KapResp::Company { code, rows, .. } => {
                if k.company.as_deref() == Some(code.as_str()) {
                    k.firm_loading = false;
                    match rows {
                        Ok(rows) => {
                            k.firm_error = None;
                            k.firm = rows;
                            k.firm_at = Some(Utc::now().timestamp());
                        }
                        Err(e) => k.firm_error = Some(e),
                    }
                }
            }
            KapResp::Detail { index, detail, .. } => {
                k.detail_loading.remove(&index);
                k.details.insert(index, detail);
            }
        }
        if let Some(t) = kap_toast(&toast, &mine) {
            self.toasts.push((t, Instant::now()));
        }
    }
}

/// Disclosures already announced in a toast, whichever pane found them.
fn toasted() -> &'static Mutex<HashSet<u64>> {
    static S: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The toast text for new disclosures of `mine` companies (not announced before), if any.
fn kap_toast(fresh: &[Disclosure], mine: &HashSet<String>) -> Option<String> {
    let mut seen = toasted().lock().ok()?;
    let new: Vec<&Disclosure> = fresh
        .iter()
        .filter(|d| d.mentions_any(mine) && seen.insert(d.index))
        .collect();
    toast_text(&new)
}

fn toast_text(new: &[&Disclosure]) -> Option<String> {
    let first = new.first()?;
    let subject = if first.summary.is_empty() { &first.title } else { &first.summary };
    let more = new.len().saturating_sub(1).min(TOAST_MAX * 100);
    let tail = if more > 0 { format!(" (+{more} more)") } else { String::new() };
    let line = format!("{}: {}{tail}", first.code_label(), trunc(subject, 44));
    Some(format!("{TOAST_PREFIX}{line}"))
}

// ---- drawing ------------------------------------------------------------------------------

fn istanbul() -> FixedOffset {
    FixedOffset::east_opt(3 * 3600).expect("UTC+3")
}

/// `14:32` today, `21 Oct 14:32` otherwise (Istanbul time, as on kap.org.tr).
fn when(ts: i64, now: i64) -> String {
    let Some(t) = DateTime::from_timestamp(ts, 0).map(|t| t.with_timezone(&istanbul())) else { return String::new() };
    let today = DateTime::from_timestamp(now, 0).map(|t| t.with_timezone(&istanbul()).date_naive());
    if today == Some(t.date_naive()) { t.format("%H:%M").to_string() } else { t.format("%d %b %H:%M").to_string() }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

fn category_color(c: Category) -> Color32 {
    match c {
        Category::Material => amber(),
        Category::Financial => cyan(),
        Category::Dividend | Category::Buyback => theme::up(),
        Category::Capital => theme::down(),
        Category::Insider => text(),
        Category::Meeting | Category::Debt | Category::Other => muted(),
    }
}

fn matches_query(d: &Disclosure, q: &str) -> bool {
    let q = tr_data::kap::fold(q.trim());
    q.is_empty()
        || [d.code_label().as_str(), &d.company, &d.title, &d.summary]
            .iter()
            .any(|s| tr_data::kap::fold(s).contains(&q))
}

pub fn kap_pane(app: &mut TerminalApp, ui: &mut Ui, i: usize, q: &str) -> Vec<Action> {
    let mine = app.kap_mine();
    let PaneKind::Kap(k) = &mut app.panes[i].kind else { return Vec::new() };
    let mut actions = Vec::new();
    if let Some(idx) = k.open {
        reading(ui, k, idx);
        return actions;
    }
    header(ui, k);
    status_line(ui, k);
    let now = Utc::now().timestamp();
    let rows: Vec<&Disclosure> = k
        .rows()
        .iter()
        .filter(|d| k.category.is_none_or(|c| d.category == c))
        .filter(|d| k.company.is_some() || k.scope == Scope::All || d.mentions_any(&mine))
        .filter(|d| matches_query(d, q))
        .collect();
    let loading = if k.company.is_some() { k.firm_at.is_none() } else { k.feed_at.is_none() };
    if rows.is_empty() {
        let msg = if loading {
            "loading…"
        } else if k.company.is_some() {
            "No disclosures in the last 60 days match."
        } else if k.scope == Scope::Mine && mine.is_empty() {
            "Your watchlists and positions are empty."
        } else {
            "No disclosures match."
        };
        ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
        return actions;
    }
    ui.add_space(dp(2.0));
    let mut open = None;
    let mut company = None;
    let row_h = dp(ROW_H);
    egui::ScrollArea::vertical().id_salt("kap-list").auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            for d in &rows[range] {
                row(ui, d, now, &mine, row_h, &mut open, &mut company, &mut actions);
            }
        },
    );
    if let Some(idx) = open {
        k.open = Some(idx);
    }
    if let Some(c) = company {
        k.set_company(&c);
    }
    actions
}

fn header(ui: &mut Ui, k: &mut KapPane) {
    ui.horizontal_wrapped(|ui| {
        company_box(ui, k);
        ui.separator();
        if k.company.is_none() {
            let sel = Some(if k.scope == Scope::All { 0 } else { 1 });
            if let Some(i) = crate::widgets::segmented(ui, "kap-scope", &["All", "Watchlists + positions"], sel, 22.0) {
                k.scope = if i == 0 { Scope::All } else { Scope::Mine };
            }
        }
        egui::ComboBox::from_id_salt("kap-category")
            .selected_text(RichText::new(k.category.map_or("All categories", Category::label)).font(ui_font(12.5)))
            .show_ui(ui, |ui| {
                if ui.selectable_label(k.category.is_none(), "All categories").clicked() {
                    k.category = None;
                }
                for c in Category::ALL {
                    if ui.selectable_label(k.category == Some(c), c.label()).clicked() {
                        k.category = Some(c);
                    }
                }
            });
        if ui.small_button("Refresh").on_hover_text("Fetch the disclosures now").clicked() {
            k.feed_req = None;
            k.firm_req = None;
        }
        if k.feed_loading || k.firm_loading {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
    });
}

/// Ticker box: type a BIST ticker and press Enter to list that company only; empty shows all.
fn company_box(ui: &mut Ui, k: &mut KapPane) {
    let mut buf = k.company_edit.take().unwrap_or_else(|| k.company.clone().unwrap_or_default());
    let r = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .font(mono(13.0))
            .text_color(amber())
            .hint_text(RichText::new("All companies").font(ui_font(12.0)).color(muted()))
            .desired_width(dp(104.0)),
    );
    let r = r.on_hover_text("A BIST ticker (THYAO) and Enter: that company only. Empty: everyone.");
    if r.lost_focus() {
        if ui.input(|i| i.key_pressed(Key::Enter)) {
            if buf.trim().is_empty() {
                k.clear_company();
            } else {
                k.set_company(&buf);
            }
        }
    } else if r.has_focus() {
        k.company_edit = Some(buf);
    }
    if k.company.is_some() && ui.small_button("×").on_hover_text("Back to all companies").clicked() {
        k.clear_company();
        k.company_edit = None;
    }
}

fn status_line(ui: &mut Ui, k: &KapPane) {
    let (at, err) = if k.company.is_some() { (k.firm_at, &k.firm_error) } else { (k.feed_at, &k.feed_error) };
    let mut s = String::from("KAP");
    match &k.company {
        Some(c) => s.push_str(&format!(" · {c}, last 60 days")),
        None => s.push_str(" · Borsa Istanbul companies, last days"),
    }
    if let Some(t) = at.and_then(|t| DateTime::from_timestamp(t, 0)) {
        s.push_str(&format!(" · updated {}", t.with_timezone(&chrono::Local).format("%H:%M:%S")));
    }
    s.push_str(" · refreshes every 2 min · times are Istanbul");
    ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
    if let Some(e) = err {
        ui.label(RichText::new(format!("Could not refresh: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
}

/// A cell `w` wide with its content truncated to fit.
fn cell(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui) -> Option<egui::Response>) -> Option<egui::Response> {
    ui.allocate_ui_with_layout(egui::vec2(w, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        ui.set_max_width(w);
        add(ui)
    })
    .inner
}

fn clickable(ui: &mut Ui, s: String, color: Color32, font: egui::FontId) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(s).font(font).color(color)).truncate().sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

#[allow(clippy::too_many_arguments)]
fn row(
    ui: &mut Ui,
    d: &Disclosure,
    now: i64,
    mine: &HashSet<String>,
    h: f32,
    open: &mut Option<u64>,
    company: &mut Option<String>,
    actions: &mut Vec<Action>,
) {
    let w = ui.available_width();
    let (tw, cw, nw, yw, lw) = (dp(88.0), dp(92.0), (w * 0.17).max(dp(90.0)), dp(104.0), dp(26.0));
    let sw = (w - tw - cw - nw - yw - lw - dp(5.0) * ui.spacing().item_spacing.x).max(dp(80.0));
    let subject = if d.summary.is_empty() { d.title.clone() } else { d.summary.clone() };
    let is_mine = d.mentions_any(mine);
    let ticker = d.code_label();
    ui.horizontal(|ui| {
        cell(ui, tw, h, |ui| {
            Some(ui.add(egui::Label::new(RichText::new(when(d.time, now)).font(mono(12.0)).color(muted())).truncate()))
        });
        let t = cell(ui, cw, h, |ui| {
            let color = if is_mine { amber() } else { text() };
            Some(clickable(ui, ticker.clone(), color, mono(12.5)).on_hover_text(format!("{}: only this company", d.code())))
        });
        if let Some(t) = t {
            if t.clicked() && !d.code().is_empty() {
                *company = Some(d.code().to_string());
            }
            t.context_menu(|ui| ticker_menu(ui, d, actions));
        }
        cell(ui, nw, h, |ui| {
            Some(ui.add(egui::Label::new(RichText::new(&d.company).font(ui_font(12.0)).color(muted())).truncate()))
        });
        cell(ui, yw, h, |ui| {
            Some(ui.add(
                egui::Label::new(RichText::new(d.category.label()).font(ui_font(11.5)).color(category_color(d.category)))
                    .truncate(),
            ))
        });
        let s = cell(ui, sw, h, |ui| {
            let mut label = subject.clone();
            if d.amended {
                label = format!("[düzeltme] {label}");
            }
            if d.late {
                label = format!("[geç] {label}");
            }
            let tip = if d.title == subject { d.company.clone() } else { format!("{}\n{}", d.title, d.company) };
            Some(clickable(ui, label, text(), ui_font(12.5)).on_hover_text(format!("{tip}\nClick to read here")))
        });
        if let Some(s) = s {
            if s.clicked() {
                *open = Some(d.index);
            }
            s.context_menu(|ui| {
                if ui.button("Read here").clicked() {
                    *open = Some(d.index);
                    ui.close();
                }
                if ui.button("Open on KAP").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(d.link()));
                    ui.close();
                }
            });
        }
        cell(ui, lw, h, |ui| {
            let r = clickable(ui, "↗".into(), cyan(), ui_font(13.0)).on_hover_text("Open on kap.org.tr");
            if r.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(d.link()));
            }
            Some(r)
        });
    });
}

fn ticker_menu(ui: &mut Ui, d: &Disclosure, actions: &mut Vec<Action>) {
    let code = d.code().to_string();
    if code.is_empty() {
        return;
    }
    if ui.button(format!("Disclosures of {code}")).clicked() {
        actions.push(Action::Command(format!("{code} KAP")));
        ui.close();
    }
    if ui.button("Open Chart").clicked() {
        actions.push(Action::OpenChart(code.clone()));
        ui.close();
    }
    if let Some(c) = crate::widgets::research_menu(ui, &code) {
        actions.push(Action::Command(format!("{code} {c}")));
    }
    if ui.button("Add to Watchlist").clicked() {
        actions.push(Action::AddToWatchlist(code.clone()));
        ui.close();
    }
}

fn reading(ui: &mut Ui, k: &mut KapPane, idx: u64) {
    let d = k.rows().iter().chain(&k.feed).find(|d| d.index == idx).cloned();
    ui.horizontal(|ui| {
        if ui.button("← Back").clicked() {
            k.open = None;
        }
        if ui.button("Open on KAP ↗").on_hover_text(tr_data::kap::link(idx)).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(tr_data::kap::link(idx)));
        }
        ui.label(RichText::new(format!("#{idx}")).font(mono(12.0)).color(muted()));
    });
    ui.add_space(dp(4.0));
    if let Some(d) = &d {
        ui.label(RichText::new(&d.company).font(ui_font(14.0)).color(text()).strong());
        ui.label(
            RichText::new(format!(
                "{} · {} · {}",
                d.code_label(),
                d.category.label(),
                when_full(d.time)
            ))
            .font(mono(12.0))
            .color(amber()),
        );
        ui.label(RichText::new(&d.title).font(ui_font(12.5)).color(muted()));
        if !d.summary.is_empty() && d.summary != d.title {
            ui.label(RichText::new(&d.summary).font(ui_font(13.0)).color(text()));
        }
    }
    ui.separator();
    if d.as_ref().is_some_and(|d| d.class == "FR") {
        ui.label(
            RichText::new("Financial reports are large tables and PDFs: open the disclosure on KAP.")
                .font(ui_font(12.0))
                .color(muted()),
        );
        return;
    }
    match k.details.get(&idx) {
        None => {
            ui.label(RichText::new("loading…").font(ui_font(12.0)).color(muted()));
        }
        Some(Err(e)) => {
            ui.label(RichText::new(format!("Could not read it here: {e}")).font(ui_font(12.0)).color(theme::down()));
            if ui.small_button("Retry").clicked() {
                k.details.remove(&idx);
            }
        }
        Some(Ok(det)) => {
            egui::ScrollArea::vertical().id_salt("kap-read").auto_shrink([false, false]).show(ui, |ui| {
                if det.text.is_empty() {
                    ui.label(RichText::new("The form has no text.").font(ui_font(12.0)).color(muted()));
                } else {
                    ui.add(egui::Label::new(RichText::new(&det.text).font(ui_font(12.5)).color(text())).wrap());
                }
                if !det.attachments.is_empty() {
                    ui.add_space(dp(8.0));
                    ui.label(RichText::new("Attachments (open on KAP to download)").font(ui_font(11.5)).color(muted()));
                    for a in &det.attachments {
                        ui.label(RichText::new(format!("• {a}")).font(ui_font(12.0)).color(text()));
                    }
                }
            });
        }
    }
}

fn when_full(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&istanbul()).format("%d.%m.%Y %H:%M:%S").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disc(index: u64, codes: &[&str], related: &[&str], title: &str, summary: &str) -> Disclosure {
        Disclosure {
            index,
            time: 1_792_616_617,
            codes: codes.iter().map(|s| s.to_string()).collect(),
            related: related.iter().map(|s| s.to_string()).collect(),
            company: "ACME A.Ş.".into(),
            member_oid: String::new(),
            class: "ODA".into(),
            title: title.into(),
            summary: summary.into(),
            category: Category::classify("ODA", title, summary),
            attachments: 0,
            late: false,
            amended: false,
        }
    }

    fn set(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn first_load_does_not_toast_later_new_rows_do() {
        let mut p = KapPane::new(Scope::All, None, None);
        let a = disc(10, &["THYAO"], &[], "Özel Durum Açıklaması (Genel)", "Trafik");
        assert!(p.merge_feed(vec![a.clone()]).is_empty());
        let b = disc(11, &["GARAN"], &[], "Özel Durum Açıklaması (Genel)", "Kâr payı");
        let fresh = p.merge_feed(vec![b.clone(), a.clone()]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].index, 11);
        assert_eq!(p.feed.iter().map(|d| d.index).collect::<Vec<_>>(), [11, 10]);
        assert!(p.merge_feed(vec![a, b]).is_empty());
    }

    #[test]
    fn toast_only_for_my_companies_and_only_once() {
        let a = disc(900_001, &["THYAO"], &[], "Özel Durum Açıklaması (Genel)", "Eylül 2026 Trafik Sonuçları");
        let b = disc(900_002, &["MEYSU"], &[], "Özel Durum Açıklaması (Genel)", "Tazminat davası hk.");
        let mine = set(&["THYAO"]);
        let t = kap_toast(&[a.clone(), b], &mine).unwrap();
        assert_eq!(t, format!("{TOAST_PREFIX}THYAO: Eylül 2026 Trafik Sonuçları"));
        assert!(kap_toast(&[a], &mine).is_none(), "already announced");
        assert!(kap_toast(&[], &mine).is_none());
    }

    #[test]
    fn toast_counts_the_rest() {
        let a = disc(900_011, &["THYAO"], &[], "T", "bir");
        let b = disc(900_012, &["AKBNK"], &[], "T", "iki");
        let t = kap_toast(&[a, b], &set(&["THYAO", "AKBNK"])).unwrap();
        assert!(t.ends_with("(+1 more)"), "{t}");
    }

    #[test]
    fn related_tickers_count_as_mine() {
        let d = disc(5, &["TRATE"], &["TARFN"], "Kredi Derecelendirmesi", "Not güncelleme");
        assert!(d.mentions_any(&set(&["TARFN"])));
        assert!(!d.mentions_any(&set(&["THYAO"])));
    }

    #[test]
    fn query_matches_turkish_text_without_accents() {
        let d = disc(1, &["THYAO"], &[], "Özel Durum Açıklaması (Genel)", "Pay Geri Alımı");
        assert!(matches_query(&d, "geri alimi"));
        assert!(matches_query(&d, "thy"));
        assert!(matches_query(&d, "acme"));
        assert!(!matches_query(&d, "temettü"));
        assert!(matches_query(&d, ""));
    }

    #[test]
    fn pane_settings_round_trip() {
        let p = KapPane::from_settings("mine", "dividend", "thyao");
        assert_eq!(p.scope, Scope::Mine);
        assert_eq!(p.category, Some(Category::Dividend));
        assert_eq!(p.symbol(), Some("THYAO"));
        assert_eq!(p.title(), "THYAO KAP");
        let PaneSettings::Kap { scope, category, company } = p.settings() else { panic!() };
        assert_eq!((scope.as_str(), category.as_str(), company.as_str()), ("mine", "dividend", "THYAO"));
        let all = KapPane::from_settings("", "", "");
        assert_eq!((all.scope, all.category, all.symbol()), (Scope::All, None, None));
        assert_eq!(all.title(), "KAP");
    }

    #[test]
    fn ticker_cleaning() {
        assert_eq!(clean_ticker(" thyao.is "), "THYAO");
        assert_eq!(clean_ticker(""), "");
    }

    #[test]
    fn times() {
        // 22.10.2026 00:03:37 Istanbul.
        let ts = 1_792_616_617;
        assert_eq!(when(ts, ts), "00:03");
        assert_eq!(when(ts, ts + 86_400), "22 Oct 00:03");
        assert_eq!(when_full(ts), "22.10.2026 00:03:37");
    }

    #[test]
    fn truncation_is_by_character() {
        assert_eq!(trunc("Özel Durum", 5), "Özel…");
        assert_eq!(trunc("kısa", 10), "kısa");
    }
}
