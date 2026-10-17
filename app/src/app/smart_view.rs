//! Smart money (`SMART`): insider transactions (SEC Form 4), institutional holdings (SEC 13F of
//! a curated list of managers) and FINRA short interest for the active symbol.
//! Data layer: [`tr_data::smartmoney`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{Color32, CursorIcon, Key, RichText, Sense, Ui};
use tr_data::smartmoney::{self as sm, Holder, Insiders, Manager, Move, ShortData, TxKind};

use super::fa_view::{fmt_compact, fmt_money};
use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::layouts::PaneSettings;
use crate::smart::{ManagerView, SmartReq, SmartResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{PaneId, Request};

/// Positions listed per manager.
const TOP: usize = 25;
/// Sold-out positions listed per manager.
const SOLD: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Insiders,
    Institutions,
    Short,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Insiders, Tab::Institutions, Tab::Short];

    fn label(self) -> &'static str {
        match self {
            Tab::Insiders => "Insiders (Form 4)",
            Tab::Institutions => "Institutions (13F)",
            Tab::Short => "Short interest",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Tab::Insiders => "insiders",
            Tab::Institutions => "institutions",
            Tab::Short => "short",
        }
    }

    fn from_key(s: &str) -> Self {
        match s {
            "institutions" => Tab::Institutions,
            "short" => Tab::Short,
            _ => Tab::Insiders,
        }
    }

    /// A tab named on the command line (`SMART SHORT`, `SMART 13F`).
    pub fn from_word(w: &str) -> Option<Self> {
        match w.to_ascii_uppercase().as_str() {
            "INS" | "INSIDERS" | "INSIDER" | "FORM4" | "F4" => Some(Tab::Insiders),
            "13F" | "INST" | "INSTITUTIONS" | "HOLDERS" | "FUNDS" => Some(Tab::Institutions),
            "SHORT" | "SI" | "SHORTS" => Some(Tab::Short),
            _ => None,
        }
    }
}

/// The institutions tab: the manager list, one manager, or who holds the active symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inst {
    List,
    Manager(u64),
    Holders,
}

pub struct SmartPane {
    /// The security the pane follows (market key).
    pub symbol: String,
    pub tab: Tab,
    inst: Inst,
    /// Ticker box in the header while it is being edited.
    symbol_edit: Option<String>,
    /// Insiders tab: buys and sells only (hide awards, exercises, tax withholding, gifts).
    open_market: bool,

    insiders: Option<Arc<Insiders>>,
    ins_error: Option<String>,
    /// Ticker the insider data was last requested for.
    ins_for: Option<String>,

    managers: HashMap<u64, Arc<Manager>>,
    views: HashMap<u64, Arc<ManagerView>>,
    mgr_loading: HashSet<u64>,
    mgr_errors: HashMap<u64, String>,
    mgr_queue: Vec<(u64, bool)>,
    need_cached: bool,
    /// An index run was asked for (`true` = force).
    index_req: Option<bool>,
    index: Option<(usize, usize)>,

    holders: Option<Result<Vec<Holder>, String>>,
    holders_for: Option<String>,

    short: Option<Arc<ShortData>>,
    short_for: Option<String>,
}

impl SmartPane {
    pub fn new(symbol: &str, tab: Tab) -> Self {
        Self {
            symbol: symbol.into(),
            tab,
            inst: Inst::List,
            symbol_edit: None,
            open_market: true,
            insiders: None,
            ins_error: None,
            ins_for: None,
            managers: HashMap::new(),
            views: HashMap::new(),
            mgr_loading: HashSet::new(),
            mgr_errors: HashMap::new(),
            mgr_queue: Vec::new(),
            need_cached: true,
            index_req: None,
            index: None,
            holders: None,
            holders_for: None,
            short: None,
            short_for: None,
        }
    }

    pub fn from_settings(symbol: &str, tab: &str) -> Self {
        Self::new(symbol, Tab::from_key(tab))
    }

    pub fn settings(&self) -> PaneSettings {
        PaneSettings::Smart { symbol: self.symbol.clone(), tab: self.tab.key().into() }
    }

    pub fn title(&self) -> String {
        format!("{} Smart money", tr_data::security(&self.symbol).ticker)
    }

    pub fn code(&self) -> &'static str {
        "SMART"
    }

    /// Follow a new security (link groups, the ticker box), keeping the tab.
    pub fn set_symbol(&mut self, symbol: &str) {
        if self.symbol == symbol {
            return;
        }
        self.symbol = symbol.into();
        self.insiders = None;
        self.ins_error = None;
        self.ins_for = None;
        self.holders = None;
        self.holders_for = None;
        self.short = None;
        self.short_for = None;
        if self.inst == Inst::Holders {
            self.inst = Inst::List;
        }
    }

    /// The EDGAR ticker, when the security is a US-listed stock or ETF.
    fn ticker(&self) -> Option<String> {
        tr_data::edgar::edgar_ticker(&self.symbol)
    }

    fn want_manager(&mut self, cik: u64, force: bool) {
        if self.mgr_loading.insert(cik) {
            self.mgr_errors.remove(&cik);
            self.mgr_queue.push((cik, force));
        }
    }
}

fn cik_name(cik: u64) -> &'static str {
    sm::MANAGERS.iter().find(|(_, c, _)| *c == cik).map_or("Manager", |(n, ..)| n)
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `SMART`, `SMART SHORT`, `SMART 13F AAPL`, `AAPL SMART`: open (or reuse) the pane.
    pub(super) fn smart_command(&mut self, arg: &str, symbol: Option<String>) {
        let mut tab = None;
        let mut sym = symbol;
        for w in arg.split_whitespace() {
            match Tab::from_word(w) {
                Some(t) => tab = Some(t),
                None => sym = Some(tr_data::security(w).key()),
            }
        }
        let sym = sym.or_else(|| self.focused_symbol()).unwrap_or_else(|| "AAPL US Equity".into());
        let is_smart = |id: PaneId| self.idx(id).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::Smart(_)));
        let target = if is_smart(self.focus) {
            Some(self.focus)
        } else {
            self.panes.iter().find(|p| matches!(p.kind, PaneKind::Smart(_))).map(|p| p.id)
        };
        match target {
            Some(id) => {
                if let Some(PaneKind::Smart(s)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    s.set_symbol(&sym);
                    if let Some(t) = tab {
                        s.tab = t;
                    }
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Smart(Box::new(SmartPane::new(&sym, tab.unwrap_or(Tab::Insiders))))),
        }
        self.status = format!("{}: smart money", tr_data::security(&sym).ticker);
    }

    /// Send the fetches the visible SMART panes are waiting for.
    pub(super) fn smart_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Smart(s) = &mut self.panes[i].kind else { continue };
            let ua = self.prefs.panels.sec_user_agent.clone();
            let ticker = s.ticker();
            if std::mem::take(&mut s.need_cached) {
                self.worker.send(Request::Smart(SmartReq::Cached { pane: id }));
            }
            for (cik, force) in std::mem::take(&mut s.mgr_queue) {
                self.worker.send(Request::Smart(SmartReq::Manager { pane: id, cik, force, user_agent: ua.clone() }));
            }
            if let Some(force) = s.index_req.take() {
                s.mgr_loading.extend(sm::MANAGERS.iter().map(|(_, c, _)| *c));
                s.index = Some((0, sm::MANAGERS.len()));
                self.worker.send(Request::Smart(SmartReq::Index { pane: id, force, user_agent: ua.clone() }));
            }
            let Some(t) = ticker else { continue };
            match s.tab {
                Tab::Insiders if s.ins_for.as_deref() != Some(&t) => {
                    s.ins_for = Some(t.clone());
                    s.ins_error = None;
                    self.worker.send(Request::Smart(SmartReq::Insiders { pane: id, ticker: t, user_agent: ua }));
                }
                Tab::Institutions if s.inst == Inst::Holders && s.holders_for.as_deref() != Some(&t) => {
                    s.holders_for = Some(t.clone());
                    self.worker.send(Request::Smart(SmartReq::Holders { pane: id, ticker: t, user_agent: ua }));
                }
                Tab::Short if s.short_for.as_deref() != Some(&t) => {
                    s.short_for = Some(t.clone());
                    self.worker.send(Request::Smart(SmartReq::Short { pane: id, ticker: t }));
                }
                _ => {}
            }
        }
    }

    pub(super) fn on_smart(&mut self, resp: SmartResp) {
        let pane = match &resp {
            SmartResp::Cached { pane, .. }
            | SmartResp::Insiders { pane, .. }
            | SmartResp::Manager { pane, .. }
            | SmartResp::Progress { pane, .. }
            | SmartResp::Holders { pane, .. }
            | SmartResp::Short { pane, .. } => *pane,
        };
        let Some(PaneKind::Smart(s)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        let current = s.ticker();
        match resp {
            SmartResp::Cached { managers, .. } => {
                for m in managers {
                    if s.managers.get(&m.cik).is_none_or(|old| old.updated < m.updated) {
                        s.managers.insert(m.cik, m);
                    }
                }
            }
            SmartResp::Insiders { ticker, data, .. } => {
                if current.as_deref() == Some(ticker.as_str()) {
                    match data {
                        Ok(d) => {
                            s.insiders = Some(d);
                            s.ins_error = None;
                        }
                        Err(e) => s.ins_error = Some(e),
                    }
                }
            }
            SmartResp::Manager { cik, data, .. } => {
                s.mgr_loading.remove(&cik);
                match data {
                    Ok(v) => {
                        s.mgr_errors.remove(&cik);
                        s.managers.insert(cik, Arc::new(v.manager.clone()));
                        s.views.insert(cik, v);
                    }
                    Err(e) => {
                        s.mgr_errors.insert(cik, e);
                    }
                }
            }
            SmartResp::Progress { done, total, .. } => {
                s.index = (done < total).then_some((done, total));
                if done >= total {
                    // The reverse lookup reads the cache: ask again.
                    s.holders_for = None;
                }
            }
            SmartResp::Holders { ticker, data, .. } => {
                if current.as_deref() == Some(ticker.as_str()) {
                    s.holders = Some(data);
                }
            }
            SmartResp::Short { ticker, data, .. } => {
                if current.as_deref() == Some(ticker.as_str()) {
                    s.short = Some(data);
                }
            }
        }
    }
}

// ---- formatting ---------------------------------------------------------------------------

/// `1,234,567`.
fn int(v: f64) -> String {
    let n = v.round() as i64;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn signed_int(v: f64) -> String {
    if v > 0.0 { format!("+{}", int(v)) } else { int(v) }
}

fn pct(v: f64) -> String {
    format!("{v:.1}%")
}

fn signed_pct(v: f64) -> String {
    format!("{v:+.1}%")
}

fn day(d: chrono::NaiveDate) -> String {
    d.format("%b %-d, %Y").to_string()
}

fn short_day(d: chrono::NaiveDate) -> String {
    d.format("%b %-d").to_string()
}

fn dim(ui: &mut Ui, s: impl Into<String>) {
    ui.label(RichText::new(s.into()).font(ui_font(11.5)).color(muted()));
}

fn head(ui: &mut Ui, cols: &[&str]) {
    for h in cols {
        ui.label(RichText::new(*h).font(ui_font(11.0)).color(muted()));
    }
    ui.end_row();
}

fn cell(ui: &mut Ui, s: impl Into<String>, color: Color32) {
    ui.label(RichText::new(s.into()).font(mono(12.0)).color(color));
}

fn link(ui: &mut Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(label).font(mono(12.5)).color(color)).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn error(ui: &mut Ui, s: &str) {
    ui.add_space(dp(6.0));
    ui.label(RichText::new(s).font(ui_font(12.5)).color(theme::down()));
}

fn move_color(m: Move) -> Color32 {
    match m {
        Move::New | Move::Added => theme::up(),
        Move::Reduced | Move::Sold => theme::down(),
        Move::Unchanged => muted(),
    }
}

fn move_text(m: Move, pct: Option<f64>) -> String {
    match (m, pct) {
        (Move::Added | Move::Reduced, Some(p)) => format!("{} {}", m.label(), signed_pct(p)),
        _ => m.label().to_string(),
    }
}

// ---- drawing ------------------------------------------------------------------------------

pub fn smart(ui: &mut Ui, p: &mut SmartPane) -> Vec<Action> {
    let mut actions = Vec::new();
    header(ui, p);
    ui.add_space(dp(4.0));
    if p.ticker().is_none() {
        let name = tr_data::security(&p.symbol).key();
        error(
            ui,
            &format!(
                "{name}: SEC EDGAR and FINRA cover US-listed stocks and ETFs only. Type a US ticker in the box above."
            ),
        );
        // The curated-manager list needs no symbol.
        if p.tab == Tab::Institutions {
            institutions(ui, p, &mut actions);
        }
        return actions;
    }
    match p.tab {
        Tab::Insiders => insiders(ui, p),
        Tab::Institutions => institutions(ui, p, &mut actions),
        Tab::Short => short(ui, p),
    }
    actions
}

fn header(ui: &mut Ui, p: &mut SmartPane) {
    ui.horizontal_wrapped(|ui| {
        symbol_box(ui, p);
        ui.separator();
        let sel = Tab::ALL.iter().position(|t| *t == p.tab);
        let labels: Vec<&str> = Tab::ALL.iter().map(|t| t.label()).collect();
        if let Some(i) = widgets::segmented(ui, "smart-tab", &labels, sel, 22.0) {
            p.tab = Tab::ALL[i];
        }
    });
}

/// Editable ticker: type a symbol and press Enter to load it; Escape keeps the old one.
fn symbol_box(ui: &mut Ui, p: &mut SmartPane) {
    let ticker = tr_data::security(&p.symbol).ticker;
    let mut buf = p.symbol_edit.take().unwrap_or(ticker);
    let r = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .font(mono(13.0))
            .text_color(amber())
            .hint_text("AAPL")
            .desired_width(dp(72.0)),
    );
    let r = r.on_hover_text("Type a US ticker and press Enter");
    if r.lost_focus() {
        let typed = buf.trim().to_ascii_uppercase();
        if ui.input(|i| i.key_pressed(Key::Enter)) && !typed.is_empty() {
            let key = tr_data::security(&typed).key();
            p.set_symbol(&key);
        }
    } else if r.has_focus() {
        p.symbol_edit = Some(buf);
    }
}

// ---- insiders -----------------------------------------------------------------------------

fn insiders(ui: &mut Ui, p: &mut SmartPane) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut p.open_market, RichText::new("Buys and sells only").font(ui_font(12.0)))
            .on_hover_text("Hide awards, option exercises, tax withholding and gifts");
        if ui.small_button("Refresh").clicked() {
            p.ins_for = None;
            p.insiders = None;
        }
        if p.ins_for.is_some() && p.insiders.is_none() && p.ins_error.is_none() {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "reading Form 4 filings…");
        }
    });
    if let Some(e) = &p.ins_error {
        error(ui, e);
        return;
    }
    let Some(d) = p.insiders.clone() else { return };
    let today = chrono::Local::now().date_naive();
    let s = sm::summarize(&d.txs, today, 90);
    egui::ScrollArea::vertical().id_salt(("smart-ins", &p.symbol)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(dp(2.0));
        ui.label(
            RichText::new(format!("{} · insider activity, last 90 days (open market)", d.issuer))
                .font(ui_font(13.0))
                .color(text()),
        );
        ui.horizontal_wrapped(|ui| {
            let side = |ui: &mut Ui, label: &str, s: sm::Side, color: Color32| {
                ui.label(
                    RichText::new(format!(
                        "{label}: {} trade{} by {} insider{} · {} sh · {}",
                        s.count,
                        if s.count == 1 { "" } else { "s" },
                        s.insiders,
                        if s.insiders == 1 { "" } else { "s" },
                        fmt_compact(s.shares),
                        fmt_money(s.value),
                    ))
                    .font(mono(12.0))
                    .color(color),
                );
            };
            side(ui, "Buys", s.buys, theme::up());
            side(ui, "Sells", s.sells, theme::down());
            let net = s.net_value();
            let color = if net > 0.0 { theme::up() } else if net < 0.0 { theme::down() } else { muted() };
            let sign = if net >= 0.0 { "+" } else { "-" };
            ui.label(
                RichText::new(format!("Net: {sign}{} ({} sh)", fmt_money(net.abs()), signed_int(s.net_shares())))
                    .font(mono(12.5))
                    .color(color),
            );
        });
        dim(
            ui,
            format!(
                "From {} of {} Form 4 filings in the last {} days. Open market = transaction codes P (purchase) and S \
                 (sale); sales under a 10b5-1 plan are marked ‡.",
                d.read,
                d.filings,
                sm::INSIDER_DAYS
            ),
        );
        ui.add_space(dp(6.0));
        let rows: Vec<&sm::InsiderTx> =
            d.txs.iter().filter(|t| !p.open_market || matches!(t.kind(), TxKind::Buy | TxKind::Sell)).collect();
        if rows.is_empty() {
            dim(
                ui,
                if d.txs.is_empty() {
                    "No Form 4 transactions in the look-back window."
                } else {
                    "No open-market buys or sells in the look-back window. Untick the box to see awards and exercises."
                },
            );
            return;
        }
        egui::Grid::new(("smart-ins-grid", &p.symbol)).num_columns(8).striped(true).spacing([dp(14.0), dp(4.0)]).show(
            ui,
            |ui| {
                head(ui, &["Date", "Insider", "Title", "Type", "Shares", "Price", "Value", "Owned after"]);
                for t in rows.iter().take(150) {
                    let color = match t.kind() {
                        TxKind::Buy => theme::up(),
                        TxKind::Sell => theme::down(),
                        _ => muted(),
                    };
                    cell(ui, short_day(t.date), muted());
                    ui.add(egui::Label::new(RichText::new(&t.insider).font(ui_font(12.5)).color(text())).truncate());
                    ui.add(egui::Label::new(RichText::new(&t.title).font(ui_font(11.5)).color(muted())).truncate());
                    let mark = if t.plan && t.kind() == TxKind::Sell { " ‡" } else { "" };
                    ui.label(RichText::new(format!("{} ({}){mark}", t.kind().label(), t.code)).font(mono(12.0)).color(color))
                        .on_hover_text(format!(
                            "Form 4 filed {}{}{}",
                            day(t.filed),
                            if t.direct { "" } else { " · held indirectly" },
                            if t.plan { " · Rule 10b5-1 plan" } else { "" }
                        ));
                    cell(ui, int(t.shares), text());
                    cell(ui, t.price.map_or_else(|| "–".into(), |v| format!("${v:.2}")), text());
                    cell(ui, t.value().map_or_else(|| "–".into(), fmt_money), color);
                    cell(ui, t.owned_after.map_or_else(|| "–".into(), int), muted());
                    ui.end_row();
                }
            },
        );
        if rows.len() > 150 {
            dim(ui, format!("{} older rows not shown.", rows.len() - 150));
        }
        ui.add_space(dp(8.0));
        dim(
            ui,
            "Source: SEC EDGAR Form 4 (non-derivative transactions; derivative grants are not listed). Filed within two \
             business days of the trade. A market-wide feed of large insider purchases is not included.",
        );
    });
}

// ---- institutions -------------------------------------------------------------------------

fn institutions(ui: &mut Ui, p: &mut SmartPane, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        let ticker = p.ticker();
        let holders_label = match &ticker {
            Some(t) => format!("Who holds {t}"),
            None => "Who holds".to_string(),
        };
        let sel = match p.inst {
            Inst::List | Inst::Manager(_) => 0,
            Inst::Holders => 1,
        };
        if let Some(i) = widgets::segmented(ui, "smart-inst", &["Managers", &holders_label], Some(sel), 20.0) {
            p.inst = if i == 0 { Inst::List } else { Inst::Holders };
        }
        ui.separator();
        match p.index {
            Some((done, total)) => {
                ui.add(egui::Spinner::new().size(dp(12.0)));
                dim(ui, format!("indexing {done}/{total}…"));
            }
            None => {
                let n = p.managers.values().filter(|m| m.latest.is_some()).count();
                let r = ui.small_button("Index curated managers").on_hover_text(format!(
                    "Download the latest two 13F filings of the {} managers, one at a time (SEC rate limit). \
                     Managers already fresh on disk are skipped.",
                    sm::MANAGERS.len()
                ));
                if r.clicked() {
                    p.index_req = Some(false);
                }
                dim(ui, format!("{n} of {} indexed", sm::MANAGERS.len()));
            }
        }
    });
    ui.add_space(dp(2.0));
    match p.inst {
        Inst::List => manager_list(ui, p),
        Inst::Manager(cik) => manager_detail(ui, p, cik, actions),
        Inst::Holders => holders(ui, p),
    }
}

fn manager_list(ui: &mut Ui, p: &mut SmartPane) {
    let mut open: Option<u64> = None;
    egui::ScrollArea::vertical().id_salt("smart-mgr-list").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("smart-mgr-grid").num_columns(5).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            head(ui, &["Manager", "Style", "13F period", "Reported equities", "Positions"]);
            for (name, cik, style) in sm::MANAGERS {
                if link(ui, name, amber()).on_hover_text("Top positions and changes").clicked() {
                    open = Some(*cik);
                }
                ui.label(RichText::new(*style).font(ui_font(11.5)).color(muted()));
                match p.managers.get(cik).and_then(|m| m.latest.as_ref()) {
                    Some(f) => {
                        cell(ui, day(f.period), text());
                        cell(ui, fmt_money(f.total), text());
                        cell(ui, f.positions.len().to_string(), muted());
                    }
                    None => {
                        let (s, c) = if p.mgr_loading.contains(cik) {
                            ("loading…".to_string(), muted())
                        } else if let Some(e) = p.mgr_errors.get(cik) {
                            (format!("error: {}", e.chars().take(60).collect::<String>()), theme::down())
                        } else {
                            ("not indexed".to_string(), muted())
                        };
                        ui.label(RichText::new(s).font(ui_font(11.5)).color(c));
                        ui.label("");
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space(dp(8.0));
        dim(
            ui,
            "Source: SEC 13F-HR filings. Managers report long US-listed equity positions 45 days after each quarter end, \
             so holdings are 6 weeks to 4½ months old; short positions and non-US holdings are not shown.",
        );
    });
    if let Some(cik) = open {
        p.inst = Inst::Manager(cik);
        // Always ask: a cached manager answers at once and brings the ticker lookup.
        if !p.views.contains_key(&cik) {
            p.want_manager(cik, false);
        }
    }
}

fn manager_detail(ui: &mut Ui, p: &mut SmartPane, cik: u64, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        if ui.small_button("◀ Back").on_hover_text("Back to the manager list").clicked() {
            p.inst = Inst::List;
        }
        if ui.small_button("Refresh").on_hover_text("Check the SEC for a newer 13F").clicked() {
            p.want_manager(cik, true);
        }
        if p.mgr_loading.contains(&cik) {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "loading…");
        }
    });
    let Some(v) = p.views.get(&cik).cloned() else {
        match p.mgr_errors.get(&cik) {
            Some(e) => error(ui, &format!("{}: {e}", cik_name(cik))),
            None if !p.mgr_loading.contains(&cik) => p.want_manager(cik, false),
            None => {}
        }
        return;
    };
    let m = &v.manager;
    let Some(latest) = &m.latest else {
        dim(ui, "No 13F filing found.");
        return;
    };
    egui::ScrollArea::vertical().id_salt(("smart-mgr", cik)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(dp(4.0));
        ui.label(RichText::new(&m.name).font(ui_font(16.0)).color(text()));
        dim(
            ui,
            format!(
                "13F-HR for {} (filed {}) · {} equity positions · {} reported{}",
                day(latest.period),
                day(latest.filed),
                latest.positions.len(),
                fmt_money(latest.total),
                match &m.prev {
                    Some(prev) => format!(" · changes vs {}", day(prev.period)),
                    None => " · no previous filing to compare".into(),
                }
            ),
        );
        ui.add_space(dp(6.0));
        ui.label(RichText::new(format!("Top {TOP} positions")).font(ui_font(13.0)).color(text()));
        let changes = m.top(TOP);
        position_grid(ui, "smart-top", &changes, latest.total, &v.tickers, actions);
        let sold = m.sold_out(SOLD);
        if !sold.is_empty() {
            ui.add_space(dp(10.0));
            ui.label(RichText::new("Sold out since the previous quarter").font(ui_font(13.0)).color(text()));
            position_grid(ui, "smart-sold", &sold, 0.0, &v.tickers, actions);
        }
        ui.add_space(dp(8.0));
        dim(
            ui,
            "Share counts are summed over all lines of the filing for an issuer. Tickers are matched from issuer names \
             (blank when the name is ambiguous or unknown).",
        );
    });
}

fn position_grid(
    ui: &mut Ui,
    id: &str,
    rows: &[sm::Change],
    total: f64,
    tickers: &HashMap<String, String>,
    actions: &mut Vec<Action>,
) {
    egui::Grid::new(id).num_columns(8).striped(true).spacing([dp(12.0), dp(4.0)]).show(ui, |ui| {
        head(ui, &["#", "Ticker", "Issuer", "Shares", "Value", "% portfolio", "Change", "Shares Δ"]);
        for (i, c) in rows.iter().enumerate() {
            cell(ui, (i + 1).to_string(), muted());
            match tickers.get(&c.position.cusip) {
                Some(t) => {
                    if link(ui, t, amber()).on_hover_text("Open the chart").clicked() {
                        actions.push(Action::Command(t.clone()));
                    }
                }
                None => {
                    ui.label(RichText::new("–").font(mono(12.5)).color(muted())).on_hover_text("Ticker unknown");
                }
            }
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{} {}", c.position.name, c.position.class))
                        .font(ui_font(12.0))
                        .color(text()),
                )
                .truncate(),
            );
            cell(ui, int(c.position.shares), text());
            cell(ui, fmt_money(c.position.value), text());
            cell(ui, if total > 0.0 { pct(c.position.value / total * 100.0) } else { "–".into() }, muted());
            cell(ui, move_text(c.mv, c.pct), move_color(c.mv));
            let delta = if c.mv == Move::Sold { -c.position.shares } else { c.position.shares - c.prev_shares };
            cell(
                ui,
                if c.mv == Move::Unchanged && c.pct.is_none() { "–".into() } else { signed_int(delta) },
                move_color(c.mv),
            );
            ui.end_row();
        }
    });
}

fn holders(ui: &mut Ui, p: &mut SmartPane) {
    let Some(t) = p.ticker() else { return };
    let indexed = p.managers.values().filter(|m| m.latest.is_some()).count();
    match &p.holders {
        None => {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
        Some(Err(e)) => error(ui, e),
        Some(Ok(h)) => {
            ui.label(
                RichText::new(format!(
                    "{} of {} indexed curated managers report holding {t}",
                    h.len(),
                    indexed.max(h.len())
                ))
                .font(ui_font(13.0))
                .color(text()),
            );
            if indexed < sm::MANAGERS.len() {
                dim(
                    ui,
                    "Only managers already indexed on disk are searched: use 'Index curated managers' for all of them.",
                );
            }
            ui.add_space(dp(4.0));
            if h.is_empty() {
                dim(ui, format!("No indexed manager holds {t} (or its name could not be matched)."));
            } else {
                egui::ScrollArea::vertical().id_salt(("smart-holders", &t)).auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new(("smart-holders-grid", &t))
                        .num_columns(7)
                        .striped(true)
                        .spacing([dp(14.0), dp(4.0)])
                        .show(ui, |ui| {
                            head(ui, &["Manager", "13F period", "Shares", "Value", "% portfolio", "Rank", "Change vs previous quarter"]);
                            for x in h {
                                ui.label(RichText::new(&x.manager).font(ui_font(12.5)).color(text()));
                                cell(ui, day(x.period), muted());
                                cell(ui, int(x.shares), text());
                                cell(ui, fmt_money(x.value), text());
                                cell(ui, pct(x.weight), text());
                                cell(ui, format!("#{}", x.rank), muted());
                                cell(ui, move_text(x.mv, x.pct), move_color(x.mv));
                                ui.end_row();
                            }
                        });
                    ui.add_space(dp(8.0));
                    dim(
                        ui,
                        "13F filings carry no tickers: holdings are matched to the ticker by issuer name, so an unusual \
                         spelling can be missed. Reported positions are as of each manager's latest quarter end.",
                    );
                });
            }
        }
    }
}

// ---- short interest -----------------------------------------------------------------------

fn short(ui: &mut Ui, p: &mut SmartPane) {
    ui.horizontal(|ui| {
        if ui.small_button("Refresh").clicked() {
            p.short_for = None;
            p.short = None;
        }
        if p.short_for.is_some() && p.short.is_none() {
            ui.add(egui::Spinner::new().size(dp(12.0)));
            dim(ui, "downloading FINRA files…");
        }
    });
    let Some(d) = p.short.clone() else { return };
    egui::ScrollArea::vertical().id_salt(("smart-short", &p.symbol)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(dp(4.0));
        ui.label(RichText::new(format!("{} · short interest (FINRA, twice a month)", d.ticker)).font(ui_font(13.0)).color(text()));
        match (&d.interest.first(), &d.interest_error) {
            (Some(l), _) => {
                ui.horizontal_wrapped(|ui| {
                    let color = if l.change() > 0.0 { theme::down() } else { theme::up() };
                    ui.label(RichText::new(format!("{} sh short", int(l.short))).font(mono(14.0)).color(text()));
                    ui.label(RichText::new(format!("{:.2} days to cover", l.days_to_cover)).font(mono(13.0)).color(amber()));
                    ui.label(
                        RichText::new(format!(
                            "{} vs prior period{}",
                            signed_int(l.change()),
                            l.change_pct().map_or(String::new(), |c| format!(" ({})", signed_pct(c)))
                        ))
                        .font(mono(13.0))
                        .color(color),
                    );
                });
                dim(ui, format!("As of settlement date {}.", day(l.settle)));
            }
            (None, Some(e)) => {
                ui.label(RichText::new(format!("n/a: {e}")).font(ui_font(12.5)).color(theme::down()));
            }
            (None, None) => {}
        }
        if !d.interest.is_empty() {
            ui.add_space(dp(4.0));
            egui::Grid::new(("smart-si", &d.ticker)).num_columns(6).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
                head(ui, &["Settlement", "Short interest", "Change", "Change %", "Avg daily volume", "Days to cover"]);
                for r in &d.interest {
                    // More shorts is bearish pressure: red.
                    let color = if r.change() > 0.0 { theme::down() } else if r.change() < 0.0 { theme::up() } else { muted() };
                    cell(ui, day(r.settle), muted());
                    cell(ui, int(r.short), text());
                    cell(ui, signed_int(r.change()), color);
                    cell(ui, r.change_pct().map_or_else(|| "–".into(), signed_pct), color);
                    cell(ui, int(r.avg_volume), muted());
                    cell(ui, format!("{:.2}", r.days_to_cover), text());
                    ui.end_row();
                }
            });
        }
        dim(
            ui,
            "Source: FINRA consolidated short interest files (cdn.finra.org). Published about a week after each \
             settlement date (the 15th and the last business day of the month).",
        );

        ui.add_space(dp(14.0));
        ui.label(
            RichText::new(format!("{} · daily short SALE VOLUME (not short interest)", d.ticker))
                .font(ui_font(13.0))
                .color(text()),
        );
        dim(
            ui,
            "Reg SHO daily files: shares sold short during the day on FINRA-reported venues (trade reporting facilities \
             and dark pools, not the whole market). A high ratio mostly reflects market-making and hedging, not a \
             growing short position.",
        );
        match (d.volume.is_empty(), &d.volume_error) {
            (true, Some(e)) => {
                ui.label(RichText::new(format!("n/a: {e}")).font(ui_font(12.5)).color(theme::down()));
            }
            (true, None) => {}
            (false, _) => {
                let avg = d.volume.iter().filter_map(sm::ShortVolume::ratio).sum::<f64>()
                    / d.volume.iter().filter(|v| v.ratio().is_some()).count().max(1) as f64;
                dim(ui, format!("Average short volume ratio over {} days: {}", d.volume.len(), pct(avg)));
                egui::Grid::new(("smart-sv", &d.ticker)).num_columns(5).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
                    head(ui, &["Date", "Short volume", "Short exempt", "Total volume", "Short volume ratio"]);
                    for v in &d.volume {
                        cell(ui, day(v.date), muted());
                        cell(ui, int(v.short), text());
                        cell(ui, int(v.exempt), muted());
                        cell(ui, int(v.total), muted());
                        cell(ui, v.ratio().map_or_else(|| "–".into(), pct), amber());
                        ui.end_row();
                    }
                });
            }
        }
        ui.add_space(dp(8.0));
    });
}
