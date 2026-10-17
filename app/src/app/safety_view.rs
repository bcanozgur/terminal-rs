//! App side of the order-entry safety rails: the one gate every order goes through (pre-trade
//! checks, kill switch), the audit log, the `KILL` command and toolbar chip, the Settings ›
//! Trading limits, and the `AUDIT` / `HOL` pane. The logic lives in [`crate::safety`] and
//! [`crate::mktcal`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{Local, Utc};
use egui::{Color32, RichText, Sense, Ui};
use tr_broker::trading::{Amount, OrderRequest};
use tr_broker::{Order, demo};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::mktcal::{self, DayKind, Exchange};
use crate::safety::{self, Assessment, AuditRecord, Facts, KillChange, Limits, Recent, SafetyFile, Verdict};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::worker::{Request, TradeRequest, data_dir};

/// Audit rows drawn at once.
const MAX_ROWS: usize = 500;
/// A limits edit is written to the audit log once the user stops changing it.
const LIMITS_SETTLE: Duration = Duration::from_millis(1500);

/// Why the gate refused an order.
#[derive(Debug, Clone, PartialEq)]
pub enum GateErr {
    /// A check blocked it (or the kill switch is on).
    Blocked(String),
    /// Only warnings: the user must confirm explicitly.
    NeedsConfirm(String),
}

impl GateErr {
    pub fn text(&self) -> &str {
        match self {
            GateErr::Blocked(s) | GateErr::NeedsConfirm(s) => s,
        }
    }
}

/// What the audit log needs to know about a live order that is on its way.
#[derive(Debug, Clone)]
pub struct LiveCtx {
    pub ticket: u64,
    pub checks: Vec<String>,
    pub confirmed: bool,
    pub value: Option<f64>,
}

pub struct SafetyState {
    pub file: SafetyFile,
    pub ledger: safety::Ledger,
    dir: PathBuf,
    /// The settings file was damaged: shown in Settings.
    pub note: Option<String>,
    /// Bumped on every audit write; panes re-read when it changes.
    pub audit_gen: u64,
    pub audit_error: Option<String>,
    limits_before: Option<Limits>,
    limits_changed_at: Option<Instant>,
    pub live_ctx: Option<LiveCtx>,
}

impl SafetyState {
    pub fn load() -> Self {
        let dir = data_dir();
        let (file, note) = safety::load_settings(&dir);
        let (records, _) = safety::read_all(&dir);
        let ledger = safety::ledger_from(&records, Utc::now());
        Self {
            file,
            ledger,
            dir,
            note,
            audit_gen: 0,
            audit_error: None,
            limits_before: None,
            limits_changed_at: None,
            live_ctx: None,
        }
    }

    fn save(&mut self) {
        if let Err(e) = safety::save_settings(&self.dir, &self.file) {
            self.audit_error = Some(format!("Could not save safety settings: {e}"));
        }
    }

    fn append(&mut self, rec: &AuditRecord) {
        match safety::append(&self.dir, rec) {
            Ok(()) => self.audit_error = None,
            Err(e) => self.audit_error = Some(format!("Could not write the audit log: {e}")),
        }
        self.audit_gen += 1;
    }
}

pub fn size_key(req: &OrderRequest) -> String {
    match req.amount {
        Amount::Units(u) => {
            let s = format!("{u:.6}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        Amount::Notional(n) => format!("${n:.2}"),
    }
}

/// An OCC option symbol (`AAPL  260116C00150000`) rather than a stock ticker.
fn looks_like_option(symbol: &str) -> bool {
    let s: String = symbol.chars().filter(|c| !c.is_whitespace()).collect();
    s.len() >= 15
        && s.chars().rev().take(8).all(|c| c.is_ascii_digit())
        && s.chars().rev().nth(8).is_some_and(|c| c == 'C' || c == 'P')
}

impl TerminalApp {
    pub(super) fn kill_on(&self) -> bool {
        self.safety.file.kill.on
    }

    /// All pre-trade findings for `req`, without side effects.
    pub(super) fn pretrade(&self, req: &OrderRequest, quote: Option<f64>) -> Assessment {
        let now = Utc::now();
        let price = req.reference_price(quote);
        let (units, notional) = match req.amount {
            Amount::Units(u) => (Some(u), price.map(|p| u * p)),
            Amount::Notional(n) => (quote.map(|q| n / q), Some(n)),
        };
        let account = self.trade_accounts().into_iter().find(|a| a.id == req.account_id);
        let held = self.held_units(&req.account_id, &req.symbol).unwrap_or(0.0);
        let facts = Facts {
            symbol: req.symbol.clone(),
            side: req.side,
            order_type: req.order_type,
            units,
            notional,
            size_key: size_key(req),
            limit: req.limit.filter(|_| req.order_type.needs_limit()),
            last: quote,
            held_units: held,
            position_value: quote.map(|q| held * q),
            portfolio_value: account.as_ref().and_then(|a| a.total),
            buying_power: account.as_ref().and_then(|a| a.buying_power.or(a.cash)),
            allow_short: req.account_id != demo::ACCOUNT_ID,
            option: looks_like_option(&req.symbol),
        };
        let market = mktcal::status(mktcal::exchange_for_symbol(&req.symbol), now);
        safety::assess(
            &self.safety.file.limits,
            &self.safety.file.kill,
            &self.safety.ledger,
            &facts,
            Some(&market),
            now,
        )
    }

    /// The choke point: every order, demo or live, from the ticket or a Close button, must pass
    /// here right before it is sent. Blocks are audited; unconfirmed warnings are not (nothing
    /// was attempted yet).
    pub(super) fn submit_gate(
        &mut self,
        req: &OrderRequest,
        quote: Option<f64>,
        confirmed: bool,
    ) -> Result<Assessment, GateErr> {
        let a = self.pretrade(req, quote);
        match a.verdict() {
            Verdict::Block => {
                let reason = a.block_text();
                self.audit_order(req, a.summary(), false, notional_of(req, quote), "blocked", reason.clone());
                Err(GateErr::Blocked(reason))
            }
            Verdict::Warn if !confirmed => {
                Err(GateErr::NeedsConfirm(a.warnings().map(|f| f.reason.as_str()).collect::<Vec<_>>().join(" ")))
            }
            _ => Ok(a),
        }
    }

    /// Record an order attempt and what became of it.
    pub(super) fn audit_order(
        &mut self,
        req: &OrderRequest,
        checks: Vec<String>,
        confirmed: bool,
        value: Option<f64>,
        outcome: &str,
        detail: String,
    ) {
        let now = Utc::now();
        let rec = AuditRecord {
            account: req.account_id.clone(),
            symbol: req.symbol.clone(),
            side: req.side.api().into(),
            qty: size_key(req),
            order_type: req.order_type.label().into(),
            price: req
                .limit
                .filter(|_| req.order_type.needs_limit())
                .or(req.stop.filter(|_| req.order_type.needs_stop())),
            value,
            checks,
            confirmed,
            ..AuditRecord::event(now, "order", outcome, detail)
        };
        if rec.went_through() {
            self.safety.ledger.push(Recent {
                at: now,
                symbol: rec.symbol.clone(),
                side: req.side,
                size_key: rec.qty.clone(),
                notional: value,
            });
        }
        self.safety.append(&rec);
    }

    /// Record a demo order's result.
    pub(super) fn audit_demo_result(
        &mut self,
        req: &OrderRequest,
        a: &Assessment,
        confirmed: bool,
        quote: Option<f64>,
        result: &Result<Order, String>,
    ) {
        let (outcome, detail) = match result {
            Ok(o) => ("placed", format!("{} {}", o.status.to_ascii_lowercase(), demo::order_line(o))),
            Err(e) => ("rejected", e.clone()),
        };
        self.audit_order(
            req,
            a.summary(),
            confirmed && a.warnings().next().is_some(),
            notional_of(req, quote),
            outcome,
            detail,
        );
    }

    /// Remember how a live order was checked until the broker answers.
    pub(super) fn set_live_ctx(&mut self, ticket: u64, req: &OrderRequest, a: &Assessment, quote: Option<f64>) {
        self.safety.live_ctx = Some(LiveCtx {
            ticket,
            checks: a.summary(),
            confirmed: a.warnings().next().is_some(),
            value: notional_of(req, quote),
        });
    }

    /// Record the broker's answer to a live (or sample-broker) order.
    pub(super) fn audit_live_result(
        &mut self,
        ticket: u64,
        req: &OrderRequest,
        result: &Result<Option<Box<Order>>, tr_broker::trading::PlaceError>,
    ) {
        use tr_broker::trading::PlaceError;
        let ctx = self.safety.live_ctx.take().filter(|c| c.ticket == ticket);
        let (checks, confirmed, value) = ctx.map_or((Vec::new(), false, None), |c| (c.checks, c.confirmed, c.value));
        let (outcome, detail) = match result {
            Ok(Some(o)) => ("placed", format!("{} #{}", o.status.to_ascii_lowercase(), o.id)),
            Ok(None) => ("placed", "sent".to_string()),
            Err(PlaceError::NotSent(e)) => ("not_sent", e.clone()),
            Err(PlaceError::Rejected(e)) => ("rejected", e.clone()),
            Err(PlaceError::Unknown(e)) => ("unknown", format!("{e} (may have reached the broker)")),
        };
        self.audit_order(req, checks, confirmed, value, outcome, detail);
    }

    // ---- kill switch ------------------------------------------------------------------

    /// `KILL`, `KILL CANCEL` (also cancel open orders), `KILL OFF`.
    pub(super) fn kill_command(&mut self, arg: &str) {
        match arg.trim().to_ascii_uppercase().as_str() {
            "OFF" | "RELEASE" | "STOP" => self.set_kill(false, false, "KILL OFF command"),
            "CANCEL" | "ALL" => self.set_kill(true, true, "KILL CANCEL command"),
            "" | "ON" => self.set_kill(true, self.safety.file.kill.cancel_open, "KILL command"),
            _ => self.status = "KILL, KILL CANCEL (also cancel open orders) or KILL OFF".into(),
        }
    }

    pub(super) fn set_kill(&mut self, on: bool, cancel: bool, why: &str) {
        let now = Utc::now();
        let change = if on { self.safety.file.kill.engage(now, cancel) } else { self.safety.file.kill.release() };
        match change {
            KillChange::Unchanged => {
                self.status = format!("Kill switch is already {}", if on { "ON" } else { "OFF" });
            }
            KillChange::Released => {
                self.safety.save();
                self.safety.append(&AuditRecord::event(now, "kill", "kill_off", why));
                self.trade_toast("Kill switch OFF: orders are allowed again", true);
            }
            KillChange::Engaged { cancel_open } => {
                self.safety.save();
                let n = if cancel_open { self.cancel_all_open() } else { 0 };
                let detail = if cancel_open { format!("{why}; cancelled {n} open orders") } else { why.to_string() };
                self.safety.append(&AuditRecord::event(now, "kill", "kill_on", detail));
                let extra = if cancel_open { format!(" {n} open orders cancelled.") } else { String::new() };
                self.trade_toast(
                    format!("KILL SWITCH ON: new orders are blocked.{extra} KILL OFF releases it."),
                    false,
                );
            }
        }
    }

    /// Cancel every open order: the demo account's, and in LIVE mode the brokerage's.
    fn cancel_all_open(&mut self) -> usize {
        let demo_ids: Vec<String> =
            self.demo.book.orders.iter().filter(|o| o.is_open()).map(|o| o.id.clone()).collect();
        let mut n = demo_ids.len();
        for id in demo_ids {
            self.demo_cancel(&id);
        }
        if self.demo_mode() {
            return n;
        }
        let open: Vec<(String, String)> = self
            .broker
            .portfolio
            .as_ref()
            .map(|pf| {
                pf.orders
                    .iter()
                    .filter(|o| o.is_open() && !o.id.is_empty())
                    .map(|o| (o.account_id.clone(), o.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (account, order) in open {
            if self.broker.portfolio.as_ref().is_some_and(|p| p.simulated) {
                self.trading.sim_trades = true;
                let result = self
                    .broker
                    .portfolio
                    .as_deref_mut()
                    .map(|pf| tr_broker::trading::simulate_cancel(pf, &account, &order));
                if let Some(result) = result {
                    self.on_trade(crate::worker::TradeResponse::Cancelled { account, order, result });
                    n += 1;
                }
            } else if self.prefs.trading.enabled && self.broker.mode() == super::broker::BrokerMode::Live {
                self.worker.send(Request::Trade(TradeRequest::Cancel {
                    creds: self.broker.creds.clone(),
                    account,
                    order,
                }));
                n += 1;
            }
        }
        n
    }

    /// The toolbar toggle. Returns the next free x.
    pub(super) fn kill_toolbar(&mut self, ui: &mut Ui, p: &egui::Painter, x: f32, cy: f32) -> f32 {
        let on = self.kill_on();
        let (label, fill, fg) =
            if on { ("KILL ON", theme::down(), Color32::WHITE) } else { ("KILL", theme::control(), muted()) };
        let galley = p.layout_no_wrap(label.into(), mono(11.0), fg);
        let r = egui::Rect::from_min_size(egui::pos2(x, cy - 10.0), egui::vec2(galley.size().x + 18.0, 20.0));
        let resp = ui.interact(r, egui::Id::new("kill-switch-chip"), Sense::click());
        let fill = if resp.hovered() { fill.gamma_multiply(1.15) } else { fill };
        p.rect_filled(r, 10.0, fill);
        p.galley(r.center() - galley.size() / 2.0, galley, fg);
        let tip = if on {
            "Kill switch is ON: every new order is blocked (cancelling is still allowed). Click, or type KILL OFF, to release."
        } else {
            "Kill switch: click, or type KILL, to block every new order. KILL CANCEL also cancels open orders."
        };
        if resp.on_hover_text(tip).clicked() {
            if on {
                self.set_kill(false, false, "toolbar");
            } else {
                self.set_kill(true, self.safety.file.kill.cancel_open, "toolbar");
            }
        }
        r.right() + 8.0
    }

    // ---- periodic -----------------------------------------------------------------------

    pub(super) fn safety_periodic(&mut self) {
        if let (Some(at), Some(before)) = (self.safety.limits_changed_at, self.safety.limits_before.clone())
            && at.elapsed() >= LIMITS_SETTLE
        {
            self.safety.limits_changed_at = None;
            self.safety.limits_before = None;
            let after = self.safety.file.limits.clone();
            if after != before {
                let detail = format!("{} -> {}", limits_text(&before), limits_text(&after));
                self.safety.append(&AuditRecord::event(Utc::now(), "limits", "limits_changed", detail));
            }
        }
    }

    // ---- Settings ----------------------------------------------------------------------

    /// Settings › Trading › Pre-trade checks and kill switch.
    pub(super) fn safety_settings_ui(&mut self, ui: &mut Ui) {
        ui.add_space(dp(10.0));
        ui.label(RichText::new("Pre-trade checks").font(ui_font(13.5)).color(text()));
        ui.separator();
        let note = |ui: &mut Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };
        if let Some(n) = &self.safety.note {
            ui.label(RichText::new(n).font(ui_font(12.0)).color(theme::down()));
        }
        note(
            ui,
            "Every order (demo and live) is checked before it is sent. Blocks refuse the order; warnings ask for a click on \
             'Place anyway'. Untick a check to switch it off. Changes are saved at once and written to the audit log.",
        );
        let before = self.safety.file.limits.clone();
        let l = &mut self.safety.file.limits;
        opt_f64(
            ui,
            "Max order value",
            &mut l.max_order_notional,
            100_000.0,
            1.0..=1e9,
            "$",
            "Block above this estimated value.",
        );
        opt_f64(
            ui,
            "Max shares per order",
            &mut l.max_order_qty,
            10_000.0,
            1.0..=1e9,
            "",
            "Block above this many shares or contracts.",
        );
        opt_f64(
            ui,
            "Max position size",
            &mut l.max_position_pct,
            40.0,
            1.0..=100.0,
            "%",
            "Warn when a buy takes one stock above this share of the portfolio.",
        );
        let mut daily = l.max_daily_orders.map(f64::from);
        opt_f64(
            ui,
            "Max orders per day",
            &mut daily,
            100.0,
            1.0..=10_000.0,
            "",
            "Block new orders after this many in the last 24 hours.",
        );
        l.max_daily_orders = daily.map(|d| d.round() as u32);
        opt_f64(
            ui,
            "Price collar",
            &mut l.price_collar_pct,
            10.0,
            0.5..=100.0,
            "%",
            "Block a limit price further than this from the last price (typo guard).",
        );
        opt_f64(
            ui,
            "Fat-finger warning",
            &mut l.fat_finger_mult,
            5.0,
            1.5..=1000.0,
            "x",
            "Warn when an order is this many times your recent average (needs 3 earlier orders).",
        );
        if self.safety.file.limits != before {
            if self.safety.limits_before.is_none() {
                self.safety.limits_before = Some(before);
            }
            self.safety.limits_changed_at = Some(Instant::now());
            self.safety.save();
        }
        ui.horizontal(|ui| {
            if ui.small_button("Reset to defaults").clicked() && self.safety.file.limits != Limits::default() {
                let before = std::mem::take(&mut self.safety.file.limits);
                self.safety.limits_before.get_or_insert(before);
                self.safety.limits_changed_at = Some(Instant::now());
                self.safety.save();
            }
        });
        note(
            ui,
            "Always on: duplicate guard (same order within 5 s), buying power, no short sales in the demo account, exchange-closed warning.",
        );

        ui.add_space(dp(6.0));
        ui.label(RichText::new("Kill switch").font(ui_font(13.0)).color(text()));
        let mut cancel = self.safety.file.kill.cancel_open;
        if ui.checkbox(&mut cancel, "Also cancel all open orders when the toolbar switch is turned on").changed() {
            self.safety.file.kill.cancel_open = cancel;
            self.safety.save();
        }
        ui.horizontal(|ui| {
            let on = self.kill_on();
            if on {
                ui.label(RichText::new("ON: new orders are blocked").font(ui_font(12.0)).color(theme::down()));
                if ui.button("Release (KILL OFF)").clicked() {
                    self.set_kill(false, false, "Settings");
                }
            } else if ui.button("Turn on (KILL)").clicked() {
                self.set_kill(true, cancel, "Settings");
            }
        });
        note(ui, "Commands: KILL · KILL CANCEL · KILL OFF · AUDIT · HOL. The state is kept across restarts.");
        if let Some(e) = &self.safety.audit_error {
            ui.label(RichText::new(e).font(ui_font(11.5)).color(theme::down()));
        }
    }
}

pub(super) fn notional_of(req: &OrderRequest, quote: Option<f64>) -> Option<f64> {
    match req.amount {
        Amount::Notional(n) => Some(n),
        Amount::Units(u) => req.reference_price(quote).map(|p| u * p),
    }
}

fn limits_text(l: &Limits) -> String {
    let f = |x: Option<f64>| x.map_or("off".to_string(), |v| format!("{v}"));
    format!(
        "value {} qty {} position% {} daily {} collar% {} fat-finger {}",
        f(l.max_order_notional),
        f(l.max_order_qty),
        f(l.max_position_pct),
        l.max_daily_orders.map_or("off".into(), |v| v.to_string()),
        f(l.price_collar_pct),
        f(l.fat_finger_mult)
    )
}

/// A checkbox plus a number: unticked is `None` (the check is off).
fn opt_f64(
    ui: &mut Ui,
    label: &str,
    v: &mut Option<f64>,
    default: f64,
    range: std::ops::RangeInclusive<f64>,
    unit: &str,
    tip: &str,
) {
    ui.horizontal(|ui| {
        let mut on = v.is_some();
        if ui.checkbox(&mut on, label).on_hover_text(tip).changed() {
            *v = on.then_some(default);
        }
        if let Some(x) = v {
            let mut dv =
                egui::DragValue::new(x).speed(((range.end() - range.start()) / 400.0).clamp(0.5, 500.0)).range(range);
            if unit == "$" {
                dv = dv.prefix("$");
            } else if !unit.is_empty() {
                dv = dv.suffix(unit);
            }
            ui.add(dv);
        }
    });
}

// ---- the AUDIT / HOL pane -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditTab {
    Log,
    Holidays,
}

pub struct AuditPane {
    pub tab: AuditTab,
    symbol: String,
    outcome: String,
    records: Vec<AuditRecord>,
    unreadable: usize,
    seen: Option<u64>,
    export_msg: Option<(String, bool)>,
}

const OUTCOMES: [&str; 8] = ["", "placed", "rejected", "blocked", "unknown", "not_sent", "kill_on", "kill_off"];

impl AuditPane {
    pub fn new(arg: &str) -> Self {
        let tab = match arg.split_whitespace().next().map(str::to_ascii_lowercase).as_deref() {
            Some("hol" | "holidays" | "holiday" | "calendar") => AuditTab::Holidays,
            _ => AuditTab::Log,
        };
        Self {
            tab,
            symbol: String::new(),
            outcome: String::new(),
            records: Vec::new(),
            unreadable: 0,
            seen: None,
            export_msg: None,
        }
    }

    pub fn title(&self) -> String {
        match self.tab {
            AuditTab::Log => "Audit Log".into(),
            AuditTab::Holidays => "Market Holidays".into(),
        }
    }

    pub fn code(&self) -> &'static str {
        "AUDIT"
    }

    pub fn settings(&self) -> &'static str {
        match self.tab {
            AuditTab::Log => "log",
            AuditTab::Holidays => "hol",
        }
    }
}

impl TerminalApp {
    /// `AUDIT` / `HOL`: focus (or open) the pane on that tab.
    pub(super) fn audit_command(&mut self, arg: &str) {
        let want = AuditPane::new(arg).tab;
        let id = self
            .idx(self.focus)
            .filter(|&i| matches!(self.panes[i].kind, PaneKind::Audit(_)))
            .map(|i| self.panes[i].id)
            .or_else(|| self.panes.iter().find(|p| matches!(p.kind, PaneKind::Audit(_))).map(|p| p.id));
        match id {
            Some(id) => {
                if let Some(PaneKind::Audit(a)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    a.tab = want;
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Audit(Box::new(AuditPane::new(arg)))),
        }
    }

    pub(super) fn audit_pane(&mut self, ui: &mut Ui, i: usize) -> Vec<Action> {
        let dir = self.safety.dir.clone();
        let audit_gen = self.safety.audit_gen;
        let kill = self.safety.file.kill.clone();
        let write_error = self.safety.audit_error.clone();
        let PaneKind::Audit(p) = &mut self.panes[i].kind else { return Vec::new() };
        if p.seen != Some(audit_gen) {
            let (recs, bad) = safety::read_all(&dir);
            p.records = recs;
            p.unreadable = bad;
            p.seen = Some(audit_gen);
        }
        if kill.on {
            egui::Frame::new().fill(theme::down()).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "KILL SWITCH ON since {}: new orders are blocked. Type KILL OFF to release.",
                        kill.since
                            .map_or(String::new(), |t| t.with_timezone(&Local).format("%b %-d %H:%M").to_string())
                    ))
                    .font(ui_font(12.0))
                    .color(Color32::WHITE),
                );
            });
        }
        ui.horizontal_wrapped(|ui| {
            for (t, label) in [(AuditTab::Log, "Audit log"), (AuditTab::Holidays, "Holidays")] {
                if ui.selectable_label(p.tab == t, RichText::new(label).font(ui_font(12.5))).clicked() {
                    p.tab = t;
                }
            }
        });
        ui.add_space(dp(4.0));
        match p.tab {
            AuditTab::Log => log_tab(ui, p, &dir, write_error.as_deref()),
            AuditTab::Holidays => holidays_tab(ui),
        }
        Vec::new()
    }
}

fn log_tab(ui: &mut Ui, p: &mut AuditPane, dir: &std::path::Path, write_error: Option<&str>) {
    if let Some(e) = write_error {
        ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut p.symbol)
                .hint_text(RichText::new("Filter symbol…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(120.0)),
        );
        let shown = if p.outcome.is_empty() { "All outcomes".to_string() } else { p.outcome.clone() };
        egui::ComboBox::from_id_salt("audit-outcome").selected_text(shown).show_ui(ui, |ui| {
            for o in OUTCOMES {
                ui.selectable_value(&mut p.outcome, o.to_string(), if o.is_empty() { "All outcomes" } else { o });
            }
        });
        let filtered = p.records.iter().filter(|r| safety::matches(r, &p.symbol, &p.outcome)).count();
        if ui.button("Export CSV").on_hover_text("Write the rows shown to a CSV file in the data folder").clicked() {
            let rows: Vec<&AuditRecord> =
                p.records.iter().rev().filter(|r| safety::matches(r, &p.symbol, &p.outcome)).collect();
            let name = format!("audit-export-{}.csv", Local::now().format("%Y%m%d-%H%M%S"));
            let path = safety::audit_dir(dir).join(name);
            let result = std::fs::create_dir_all(safety::audit_dir(dir))
                .and_then(|()| std::fs::write(&path, safety::to_csv(&rows)));
            p.export_msg = Some(match result {
                Ok(()) => (format!("Exported {} rows to {}", rows.len(), path.display()), false),
                Err(e) => (format!("Export failed: {e}"), true),
            });
        }
        ui.label(
            RichText::new(format!("{filtered} of {} records", p.records.len())).font(ui_font(11.5)).color(muted()),
        );
    });
    if let Some((m, err)) = &p.export_msg {
        ui.add(
            egui::Label::new(RichText::new(m).font(ui_font(11.5)).color(if *err {
                theme::down()
            } else {
                theme::up()
            }))
            .selectable(true),
        );
    }
    if p.unreadable > 0 {
        ui.label(
            RichText::new(format!("{} unreadable audit lines were skipped.", p.unreadable))
                .font(ui_font(11.5))
                .color(amber()),
        );
    }
    ui.add_space(dp(4.0));
    if p.records.is_empty() {
        ui.label(
            RichText::new("No orders or safety changes recorded yet. Every order attempt is logged here.")
                .font(ui_font(12.0))
                .color(muted()),
        );
        return;
    }
    egui::ScrollArea::both().id_salt("audit-list").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("audit-grid").num_columns(11).striped(true).spacing([dp(12.0), dp(4.0)]).show(ui, |ui| {
            for h in [
                "Time",
                "Outcome",
                "Account",
                "Symbol",
                "Side",
                "Qty",
                "Type",
                "Price",
                "Value",
                "Confirmed",
                "Checks / detail",
            ] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            for r in p.records.iter().rev().filter(|r| safety::matches(r, &p.symbol, &p.outcome)).take(MAX_ROWS) {
                let color = match r.outcome.as_str() {
                    "placed" => theme::up(),
                    "blocked" | "rejected" | "not_sent" | "kill_on" => theme::down(),
                    "unknown" => amber(),
                    _ => text(),
                };
                let cell = |ui: &mut Ui, s: &str, c: Color32| {
                    ui.label(RichText::new(s).font(mono(12.0)).color(c));
                };
                cell(ui, &r.at.with_timezone(&Local).format("%b %d %H:%M:%S").to_string(), muted());
                cell(ui, &r.outcome, color);
                cell(ui, &r.account, muted());
                cell(ui, &r.symbol, amber());
                cell(ui, &r.side, text());
                cell(ui, &r.qty, text());
                cell(ui, &r.order_type, muted());
                cell(ui, &r.price.map_or(String::new(), |x| format!("{x:.2}")), text());
                cell(ui, &r.value.map_or(String::new(), |x| format!("{x:.2}")), text());
                cell(ui, if r.confirmed { "yes" } else { "" }, amber());
                let mut note = r.checks.join(" | ");
                if !r.detail.is_empty() {
                    if !note.is_empty() {
                        note.push_str(" — ");
                    }
                    note.push_str(&r.detail);
                }
                ui.add(egui::Label::new(RichText::new(note).font(ui_font(11.5)).color(muted())).truncate());
                ui.end_row();
            }
        });
    });
}

fn holidays_tab(ui: &mut Ui) {
    let now = Utc::now();
    let today = Local::now().date_naive();
    for ex in [Exchange::Nyse, Exchange::Bist] {
        let s = mktcal::status(ex, now);
        let color = if s.is_regular() { theme::up() } else { amber() };
        ui.label(RichText::new(&s.note).font(ui_font(12.5)).color(color));
    }
    let next_year = today.format("%Y").to_string().parse::<i32>().unwrap_or(2026) + 1;
    if !mktcal::religious_known(next_year) {
        ui.label(
            RichText::new(format!(
                "Borsa Istanbul's religious holidays (Ramazan and Kurban Bayramı) for {next_year} are not listed yet, \
                 so closures for them are missing. Check borsaistanbul.com."
            ))
            .font(ui_font(11.5))
            .color(amber()),
        );
    }
    ui.label(
        RichText::new("Computed locally: NYSE by its published rules; Borsa Istanbul from its fixed holidays and listed bayram dates. Extra bridge days or one-off closures are not included.")
            .font(ui_font(11.0))
            .color(muted()),
    );
    ui.add_space(dp(4.0));
    egui::ScrollArea::vertical().id_salt("hol-list").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("hol-grid").num_columns(4).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
            for h in ["Date", "Exchange", "Holiday", "Trading"] {
                ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
            }
            ui.end_row();
            let mut rows: Vec<(chrono::NaiveDate, Exchange, mktcal::Holiday)> = Vec::new();
            for ex in [Exchange::Nyse, Exchange::Bist] {
                rows.extend(mktcal::upcoming(ex, today).into_iter().map(|h| (h.date, ex, h)));
            }
            rows.sort_by_key(|r| (r.0, r.1 == Exchange::Bist));
            for (date, ex, h) in rows {
                let (trading, c) = match h.kind {
                    DayKind::Closed => ("Closed".to_string(), theme::down()),
                    DayKind::Early(t) => (format!("Closes {}", t.format("%H:%M")), amber()),
                };
                ui.label(RichText::new(date.format("%a %b %-d %Y").to_string()).font(mono(12.0)).color(text()));
                ui.label(RichText::new(ex.label()).font(ui_font(12.0)).color(muted()));
                ui.label(RichText::new(&h.name).font(ui_font(12.0)).color(text()));
                ui.label(RichText::new(trading).font(mono(12.0)).color(c));
                ui.end_row();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tr_broker::trading::{OrderType, Side};

    #[test]
    fn option_symbols_are_recognised() {
        assert!(looks_like_option("AAPL260116C00150000"));
        assert!(looks_like_option("AAPL  260116P00150000"));
        assert!(!looks_like_option("AAPL"));
        assert!(!looks_like_option("BRK.B"));
    }

    #[test]
    fn pane_tabs() {
        assert_eq!(AuditPane::new("").tab, AuditTab::Log);
        assert_eq!(AuditPane::new("HOL").tab, AuditTab::Holidays);
        assert_eq!(AuditPane::new("hol").settings(), "hol");
        assert_eq!(AuditPane::new("log").settings(), "log");
    }

    #[test]
    fn sizes_read_naturally() {
        let mut r = OrderRequest {
            account_id: "a".into(),
            symbol: "AAPL".into(),
            side: Side::Buy,
            order_type: OrderType::Market,
            time_in_force: tr_broker::trading::TimeInForce::Day,
            amount: Amount::Units(10.0),
            limit: None,
            stop: None,
        };
        assert_eq!(size_key(&r), "10");
        r.amount = Amount::Units(0.5);
        assert_eq!(size_key(&r), "0.5");
        r.amount = Amount::Notional(500.0);
        assert_eq!(size_key(&r), "$500.00");
        assert_eq!(notional_of(&r, None), Some(500.0));
        r.amount = Amount::Units(2.0);
        assert_eq!(notional_of(&r, Some(10.0)), Some(20.0));
        assert_eq!(notional_of(&r, None), None);
    }

    #[test]
    fn gate_errors_carry_text() {
        assert_eq!(GateErr::Blocked("x".into()).text(), "x");
        assert_eq!(GateErr::NeedsConfirm("y".into()).text(), "y");
    }
}
