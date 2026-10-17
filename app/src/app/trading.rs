//! Order entry: the order ticket (`BUY`, `SELL`, `TRADE`, ⇧B / ⇧S, Buy/Sell buttons on charts,
//! watchlist and position rows, the ⌘K palette), the demo account's one-click orders, the
//! live SnapTrade flow with its single confirmation, cancelling and closing, and
//! Settings › Trading.
//!
//! Two accounts modes (Settings › Trading, or the DEMO / LIVE badge in the toolbar):
//!
//! * **Demo** (the default): a local paper-trading account with virtual cash. Orders are
//!   placed with one click and filled against the terminal's quotes; nothing leaves the
//!   machine.
//! * **Live**: the brokerage accounts connected through SnapTrade. The safety rails are:
//!   1. Live orders are off until enabled in Settings › Trading, which takes an explicit
//!      acknowledgement. The ticket still opens, but nothing can be sent.
//!   2. An order can only be placed from a SnapTrade preview of exactly the ticket's current
//!      fields; editing anything makes the preview stale. Previews expire before SnapTrade's
//!      five-minute limit.
//!   3. Placing always goes through one confirmation showing account, symbol, side, quantity
//!      and estimated cost. Escape or a click outside cancels.
//!   4. An optional maximum order value refuses large orders before they are sent.
//!
//! Trading access (can this connection trade?) is read once per brokerage connection from
//! `/authorizations`, cached for an hour (also on disk) with a manual refresh, and backs off
//! quietly when SnapTrade rate-limits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use serde::{Deserialize, Serialize};
use tr_broker::trading::{
    self, Amount, Capability, OrderPreview, OrderRequest, OrderType, PlaceError, Side, TimeInForce,
};
use tr_broker::{Account, InstrumentKind, Order, demo};

mod dialogs;
mod settings_tab;
mod ticket_ui;

use super::TerminalApp;
use super::broker::{BrokerMode, open_external};
use super::portfolio_view::{money, price, qty};
use super::settings_view::SettingsCmd;
use crate::command::TradeArgs;
use crate::prefs::AccountMode;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{CapsError, Request, TradeRequest, TradeResponse, data_dir};

/// Marks a toast as coming from order entry (titled "Trading" rather than a Congress alert),
/// followed by `+` for a success or `!` for a rejection.
pub const TOAST_PREFIX: &str = "\u{1}trading:";

/// One order ticket.
#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    /// Matches worker responses to the ticket that asked.
    pub id: u64,
    pub account: Option<String>,
    /// Broker ticker as typed (`AAPL`, `BRK.B`).
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    /// Size is a dollar amount rather than units.
    pub dollars: bool,
    pub qty: String,
    pub limit: String,
    pub stop: String,
    /// Stop orders and time in force are shown.
    pub advanced: bool,
    /// `SELL ALL`: the quantity follows the whole position of whichever account is chosen,
    /// until the user edits it (`all_qty` is what was filled in automatically).
    pub all: bool,
    pub all_qty: String,
    /// Frames left to put the cursor in the quantity field (just opened; the command line
    /// may still hold focus for a frame after running `BUY …`).
    pub focus_qty: u8,
    pub preview: Option<Box<OrderPreview>>,
    pub previewing: bool,
    /// Live: open the confirmation as soon as the preview arrives.
    pub review: bool,
    pub confirming: bool,
    pub placing: bool,
    pub error: Option<String>,
    /// Result of the last placement.
    pub done: Option<String>,
}

fn parse_num(s: &str) -> Option<f64> {
    s.trim().trim_start_matches('$').replace(',', "").parse::<f64>().ok()
}

fn fmt_input(x: f64) -> String {
    let s = format!("{x:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Ticket {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            account: None,
            symbol: String::new(),
            side: Side::Buy,
            order_type: OrderType::Market,
            tif: TimeInForce::Day,
            dollars: false,
            qty: String::new(),
            limit: String::new(),
            stop: String::new(),
            advanced: false,
            all: false,
            all_qty: String::new(),
            focus_qty: 0,
            preview: None,
            previewing: false,
            review: false,
            confirming: false,
            placing: false,
            error: None,
            done: None,
        }
    }

    /// Fill the ticket from a command or row action.
    pub fn apply(&mut self, args: &TradeArgs) {
        if let Some(b) = args.buy {
            self.side = if b { Side::Buy } else { Side::Sell };
        }
        if let Some(k) = &args.symbol {
            self.symbol = broker_symbol(k);
        }
        if let Some(u) = args.units {
            self.dollars = false;
            self.qty = fmt_input(u);
        }
        if let Some(n) = args.notional {
            self.dollars = true;
            self.qty = fmt_input(n);
            self.order_type = OrderType::Market;
            self.tif = TimeInForce::Day;
        }
        if let Some(l) = args.limit {
            self.limit = fmt_input(l);
        }
        if let Some(s) = args.stop {
            self.stop = fmt_input(s);
        }
        self.order_type = match (args.stop.is_some(), args.limit.is_some()) {
            (true, true) => OrderType::StopLimit,
            (true, false) => OrderType::Stop,
            (false, true) => OrderType::Limit,
            (false, false) => self.order_type,
        };
        if self.order_type.needs_stop() {
            self.advanced = true;
        }
        self.preview = None;
        self.review = false;
        self.error = None;
        self.done = None;
    }

    /// The order the ticket describes, or why it is incomplete.
    pub fn request(&self) -> Result<OrderRequest, String> {
        let account = self.account.clone().ok_or("Choose an account.")?;
        let q = parse_num(&self.qty).ok_or(if self.dollars { "Enter an amount." } else { "Enter a quantity." })?;
        let field = |s: &str, needed: bool, what: &str| -> Result<Option<f64>, String> {
            if !needed {
                return Ok(None);
            }
            parse_num(s).map(Some).ok_or(format!("Enter a {what} price."))
        };
        let req = OrderRequest {
            account_id: account,
            symbol: trading::clean_symbol(&self.symbol).ok_or("Enter a symbol.")?,
            side: self.side,
            order_type: self.order_type,
            time_in_force: self.tif,
            amount: if self.dollars { Amount::Notional(q) } else { Amount::Units(q) },
            limit: field(&self.limit, self.order_type.needs_limit(), "limit")?,
            stop: field(&self.stop, self.order_type.needs_stop(), "stop")?,
        };
        req.validate()?;
        Ok(req)
    }

    /// The preview, if it still describes exactly what the ticket says.
    pub fn current_preview(&self) -> Option<&OrderPreview> {
        let p = self.preview.as_deref()?;
        (self.request().ok().as_ref() == Some(&p.request)).then_some(p)
    }

    /// Terminal market key for quotes and charts.
    pub fn market_key(&self) -> Option<String> {
        tr_broker::model::market_key(InstrumentKind::Stock, &self.symbol, "USD")
    }

    fn busy(&self) -> bool {
        self.previewing || self.placing
    }
}

/// Broker ticker for a terminal market key (`AAPL US Equity` → `AAPL`).
pub fn broker_symbol(key: &str) -> String {
    tr_data::security(key).ticker
}

/// A cancel waiting for confirmation (or in flight).
#[derive(Debug, Clone, PartialEq)]
pub struct CancelPrompt {
    pub order: Order,
    pub busy: bool,
    pub error: Option<String>,
}

/// Trading access by brokerage connection, as cached on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CapsCache {
    pub fetched: Option<chrono::DateTime<chrono::Utc>>,
    pub caps: HashMap<String, Capability>,
}

fn caps_path() -> std::path::PathBuf {
    data_dir().join("trading-access.json")
}

#[derive(Default)]
pub struct TradingState {
    pub ticket: Option<Ticket>,
    next_id: u64,
    pub cancel: Option<CancelPrompt>,
    /// Settings › Trading: the enable acknowledgement dialog is open.
    pub enable_prompt: bool,
    pub ack: bool,
    /// The switch-to-live confirmation is open.
    pub live_prompt: bool,
    /// Settings › Trading feedback: text and whether it is an error.
    pub message: Option<(String, bool)>,
    pub portal_busy: bool,
    last_quote: Option<Instant>,
    /// The last ticket submission, to ignore a double click or a held Enter.
    last_submit: Option<Instant>,
    /// Trading access per connection (live accounts), loaded from disk at start.
    pub caps: Option<CapsCache>,
    caps_loaded: bool,
    caps_inflight: Option<Instant>,
    /// No automatic re-read before this (after a failure or a rate limit).
    caps_retry_at: Option<Instant>,
    caps_failures: u32,
    /// The last read failed for a reason other than a rate limit.
    pub caps_error: Option<String>,
    /// SnapTrade is rate-limiting; reads resume after this.
    pub caps_throttled: Option<Instant>,
    /// Simulated orders were placed or cancelled: keep the sample portfolio across refreshes.
    pub sim_trades: bool,
}

/// How long trading access is trusted before it is read again.
const CAPS_TTL: Duration = Duration::from_secs(60 * 60);
/// Re-reads while the user may be reconnecting with trading access in the browser.
const CAPS_CONNECTING: Duration = Duration::from_secs(30);
/// Back-off after a failed read (doubles per failure, up to `CAPS_MAX_BACKOFF`).
const CAPS_BACKOFF: Duration = Duration::from_secs(60);
const CAPS_MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Add a hint to errors that usually mean the connection is read-only.
fn explain(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("403") || l.contains("permission") || l.contains("not allowed") || l.contains("read-only") {
        format!("{e} — the connection may be read-only: Settings › Trading › Reconnect with Trading Access.")
    } else {
        e.to_string()
    }
}

/// `in 40 s` / `in 3 min` until `t`.
fn until(t: Instant) -> String {
    let s = t.saturating_duration_since(Instant::now()).as_secs();
    if s > 90 { format!("in {} min", s.div_ceil(60)) } else { format!("in {} s", s.max(1)) }
}

impl TerminalApp {
    /// Live orders may be sent.
    fn trading_enabled(&self) -> bool {
        self.prefs.trading.enabled
    }

    pub(super) fn trade_toast(&mut self, msg: impl Into<String>, ok: bool) {
        let msg = msg.into();
        tracing::info!("trade: {msg}");
        self.status = msg.clone();
        self.toasts.push((format!("{TOAST_PREFIX}{}{msg}", if ok { '+' } else { '!' }), Instant::now()));
    }

    /// What `account` can do: everything in the demo account and the simulated portfolio, the
    /// cached capability of its connection for live accounts (`None` while unknown).
    pub(super) fn capability(&self, account: &str) -> Option<Capability> {
        let everything = |brokerage: String| Capability {
            brokerage,
            broker_trading: true,
            connection_trading: true,
            fractional: true,
            disabled: false,
        };
        if account == demo::ACCOUNT_ID {
            return Some(everything("Demo account".into()));
        }
        match self.broker.mode() {
            BrokerMode::Simulated => Some(everything(
                self.broker
                    .portfolio
                    .as_ref()
                    .and_then(|pf| pf.account(account))
                    .map_or("Simulated Broker".into(), |a| a.institution.clone()),
            )),
            BrokerMode::Live => {
                let conn = self.broker.portfolio.as_ref()?.account(account)?.connection_id.clone();
                self.trading.caps.as_ref()?.caps.get(&conn).cloned()
            }
            BrokerMode::Setup => None,
        }
    }

    fn load_caps_cache(&mut self) {
        if std::mem::replace(&mut self.trading.caps_loaded, true) {
            return;
        }
        let cached: Option<CapsCache> = std::fs::read(caps_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        if self.trading.caps.is_none() {
            self.trading.caps = cached;
        }
    }

    /// Re-read trading access when needed (live connections only): when nothing is cached,
    /// when the cache is over an hour old, every [`CAPS_CONNECTING`] while the user may be
    /// reconnecting in the browser, or when an account's connection is not in the cache.
    /// `force` (the Refresh button) skips the age check. Never while a read is in flight or
    /// during a back-off.
    pub(super) fn ensure_capabilities(&mut self, force: bool) {
        if self.broker.mode() != BrokerMode::Live || self.demo_mode() {
            return;
        }
        self.load_caps_cache();
        let t = &self.trading;
        if t.caps_inflight.is_some_and(|s| s.elapsed() < Duration::from_secs(60)) {
            return;
        }
        let backing_off = t.caps_retry_at.is_some_and(|r| r > Instant::now());
        if backing_off && !force {
            return;
        }
        let age =
            t.caps.as_ref().and_then(|c| c.fetched).map(|f| (chrono::Utc::now() - f).to_std().unwrap_or_default());
        let connecting = self.broker.connecting_since.is_some_and(|s| s.elapsed() < Duration::from_secs(300));
        let ttl = if connecting { CAPS_CONNECTING } else { CAPS_TTL };
        let unknown_connection = match (self.broker.portfolio.as_deref(), t.caps.as_ref()) {
            (Some(pf), Some(c)) if !pf.simulated => {
                pf.accounts.iter().any(|a| !a.connection_id.is_empty() && !c.caps.contains_key(&a.connection_id))
            }
            _ => false,
        };
        let stale = age.is_none_or(|a| a > ttl);
        if force || stale || unknown_connection {
            self.trading.caps_inflight = Some(Instant::now());
            self.worker.send(Request::Trade(TradeRequest::Capabilities { creds: self.broker.creds.clone() }));
        }
    }

    fn on_capabilities(&mut self, result: Result<HashMap<String, Capability>, CapsError>) {
        let t = &mut self.trading;
        t.caps_inflight = None;
        match result {
            Ok(caps) => {
                let cache = CapsCache { fetched: Some(chrono::Utc::now()), caps };
                if let Ok(json) = serde_json::to_vec_pretty(&cache)
                    && let Err(e) = crate::layouts::write_atomic(&caps_path(), &json)
                {
                    tracing::warn!("saving trading access: {e}");
                }
                t.caps = Some(cache);
                t.caps_error = None;
                t.caps_throttled = None;
                t.caps_retry_at = None;
                t.caps_failures = 0;
            }
            Err(e) => {
                let backoff = (CAPS_BACKOFF * 2u32.pow(t.caps_failures.min(4))).min(CAPS_MAX_BACKOFF);
                t.caps_failures += 1;
                let wait = e.retry_after.unwrap_or(backoff).max(Duration::from_secs(5));
                let at = Instant::now() + wait;
                t.caps_retry_at = Some(at);
                if e.throttled {
                    // Expected with many accounts: keep the cache and say nothing alarming.
                    tracing::info!("trading access: rate limited, retrying in {wait:?}");
                    t.caps_throttled = Some(at);
                } else {
                    tracing::warn!("trading access: {}", e.message);
                    t.caps_error = Some(e.message);
                }
            }
        }
    }

    /// One line about trading access while it is unknown, for the ticket and settings.
    fn caps_pending_note(&self) -> String {
        match (&self.trading.caps_error, self.trading.caps_throttled) {
            (_, Some(at)) if at > Instant::now() => {
                format!("Checking trading access… SnapTrade is busy, retrying {}.", until(at))
            }
            (Some(e), _) => format!("Could not check trading access ({e}); retrying automatically."),
            _ => "Checking trading access…".into(),
        }
    }

    /// A portfolio load finished. In sample mode with simulated trades, keep the session's
    /// portfolio instead of the fresh sample so a refresh does not undo them.
    pub(super) fn keep_simulated_trades(
        &self,
        result: Result<Box<tr_broker::Portfolio>, String>,
    ) -> Result<Box<tr_broker::Portfolio>, String> {
        match (&result, self.broker.portfolio.as_ref()) {
            (Ok(fresh), Some(cur)) if self.trading.sim_trades && fresh.simulated && cur.simulated => {
                let mut kept = cur.clone();
                kept.fetched = fresh.fetched;
                Ok(kept)
            }
            _ => result,
        }
    }

    /// Accounts orders can go to: the demo account, or the live (or sample) accounts.
    pub(super) fn trade_accounts(&self) -> Vec<Account> {
        if self.demo_mode() {
            return vec![self.demo.book.account()];
        }
        self.broker.portfolio.as_ref().map(|p| p.accounts.clone()).unwrap_or_default()
    }

    /// Units held in `account` (positive for longs).
    pub(super) fn held_units(&self, account: &str, symbol: &str) -> Option<f64> {
        let pf = if account == demo::ACCOUNT_ID { Some(&*self.demo.pf) } else { self.broker.portfolio.as_deref() };
        pf?.positions.iter().find(|p| p.account_id == account && p.symbol.eq_ignore_ascii_case(symbol)).map(|p| p.units)
    }

    /// `BUY`/`SELL`/`TRADE`, ⇧B/⇧S and the Buy/Sell buttons: open (or refill) the ticket.
    pub(super) fn open_ticket(&mut self, args: TradeArgs, account: Option<String>) {
        let t = &mut self.trading;
        let mut ticket = match t.ticket.take() {
            // Keep the account and type choices of an idle ticket.
            Some(old) if !old.busy() => Ticket { id: old.id, ..old },
            _ => {
                t.next_id += 1;
                Ticket::new(t.next_id)
            }
        };
        ticket.confirming = false;
        if args.symbol.is_some() && args.limit.is_none() && args.stop.is_none() && args.notional.is_none() {
            ticket.order_type = OrderType::Market;
            ticket.limit.clear();
            ticket.stop.clear();
            if args.units.is_none() {
                ticket.qty.clear();
            }
        }
        ticket.apply(&args);
        let accounts = self.trade_accounts();
        let known = |id: &str| accounts.iter().any(|a| a.id == id);
        ticket.account = account
            .filter(|a| known(a))
            .or(ticket.account.filter(|a| known(a)))
            .or(self.prefs.trading.default_account.clone().filter(|a| known(a)))
            .or((accounts.len() == 1).then(|| accounts[0].id.clone()));
        // `SELL ALL TSLA`, Close: the whole position (kept in sync with the account choice).
        ticket.all = args.all;
        ticket.all_qty.clear();
        if args.all {
            ticket.side = Side::Sell;
            ticket.dollars = false;
            ticket.qty.clear();
        }
        self.status = match (&args.symbol, self.demo_mode()) {
            (Some(k), true) => format!("Demo order ticket: {} {}", ticket.side.label(), broker_symbol(k)),
            (Some(k), false) => format!("LIVE order ticket: {} {}", ticket.side.label(), broker_symbol(k)),
            (None, demo) => format!("{} order ticket", if demo { "Demo" } else { "LIVE" }),
        };
        ticket.focus_qty = 4;
        self.trading.ticket = Some(ticket);
        self.sync_sell_all();
        self.trading.last_quote = None;
        self.ensure_capabilities(false);
        if !self.demo_mode() && self.broker.portfolio.is_none() && !self.broker.loading {
            self.refresh_portfolio();
        }
    }

    /// Keep a `SELL ALL` ticket's quantity equal to the chosen account's whole position. Any
    /// manual edit of the quantity, or switching to Buy, ends it.
    fn sync_sell_all(&mut self) {
        let Some(t) = self.trading.ticket.as_ref().filter(|t| t.all) else { return };
        // Any quantity other than the one filled in automatically (including one typed before
        // an account was chosen) is the user's.
        if t.side != Side::Sell || t.dollars || t.qty != t.all_qty {
            if let Some(t) = self.trading.ticket.as_mut() {
                t.all = false;
            }
            return;
        }
        let (account, symbol) = (t.account.clone(), t.symbol.trim().to_string());
        let units = account.as_deref().and_then(|a| {
            if a == demo::ACCOUNT_ID { Some(self.demo.book.available(&symbol)) } else { self.held_units(a, &symbol) }
        });
        let Some(t) = self.trading.ticket.as_mut() else { return };
        match (account, units.filter(|u| *u > 0.0)) {
            (None, _) => {}
            (Some(_), Some(u)) => {
                t.qty = fmt_input(u);
                t.all_qty = t.qty.clone();
                if t.error.as_deref().is_some_and(|e| e.starts_with("No ")) {
                    t.error = None;
                }
            }
            (Some(_), None) => {
                t.qty.clear();
                t.all_qty.clear();
                t.error = Some(format!("No {symbol} shares to sell in this account."));
            }
        }
    }

    /// ⇧B / ⇧S: a ticket for the security in focus (or the ticket's last one).
    pub(super) fn quick_trade(&mut self, buy: bool) {
        let symbol = self
            .focused_symbol()
            .filter(|s| matches!(tr_data::security(s).class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf));
        self.open_ticket(TradeArgs { buy: Some(buy), symbol, ..Default::default() }, None);
    }

    /// Close a position: in the demo account a market sell at once; live, a ticket for the
    /// whole position that still goes through the preview and confirmation.
    pub(super) fn close_position(&mut self, account: &str, key: &str) {
        let symbol = broker_symbol(key);
        if account == demo::ACCOUNT_ID {
            return self.demo_close(&symbol);
        }
        self.open_ticket(
            TradeArgs { buy: Some(false), symbol: Some(key.into()), all: true, ..Default::default() },
            Some(account.into()),
        );
    }

    fn ticket_quote(&self, t: &Ticket) -> Option<f64> {
        self.quotes.get(&t.market_key()?).map(|q| q.last).filter(|x| x.is_finite() && *x > 0.0)
    }

    /// Keep the ticket's quote fresh and its expiry countdown ticking.
    pub(super) fn trading_periodic(&mut self, ctx: &egui::Context) {
        self.demo_periodic();
        if self.trading.ticket.is_none() {
            return;
        }
        self.ensure_capabilities(false);
        let Some(t) = &self.trading.ticket else { return };
        if t.preview.is_some() || t.busy() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(k) = t.market_key()
            && self.trading.last_quote.is_none_or(|q| q.elapsed() > Duration::from_secs(10))
        {
            self.worker.send(Request::Quotes(vec![k]));
            self.trading.last_quote = Some(Instant::now());
        }
    }

    /// The configured order-size limit, checked against an estimate.
    fn over_limit(&self, preview: &OrderPreview, quote: Option<f64>) -> Option<String> {
        let max = self.prefs.trading.max_order_value.filter(|m| *m > 0.0)?;
        match preview.estimated_value(quote) {
            Some(v) if v.abs() <= max => None,
            Some(v) => Some(format!(
                "Estimated value {} is over your {} limit (Settings › Trading).",
                money(v.abs(), &preview.currency),
                money(max, &preview.currency)
            )),
            None => Some("Cannot estimate this order's value to check your order limit; use a limit order.".into()),
        }
    }

    /// The ticket's main button: place now (demo) or review and confirm (live).
    fn ticket_submit(&mut self, confirmed: bool) {
        // One order per deliberate action: a held Enter or a double click is not a second order.
        if self.trading.last_submit.is_some_and(|t| t.elapsed() < Duration::from_millis(600)) {
            return;
        }
        self.trading.last_submit = Some(Instant::now());
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let quote = self.ticket_quote(t);
        let req = match t.request() {
            Ok(r) => r,
            Err(e) => return self.ticket_error(e),
        };
        let assessment = match self.submit_gate(&req, quote, confirmed) {
            Ok(a) => a,
            Err(e) => return self.ticket_error(e.text().to_string()),
        };
        if req.account_id == demo::ACCOUNT_ID {
            let result = self.demo_submit(&req, quote);
            self.audit_demo_result(&req, &assessment, confirmed, quote, &result);
            if let Some(t) = self.trading.ticket.as_mut() {
                match result {
                    Ok(o) => {
                        t.error = None;
                        t.done = Some(match o.status.as_str() {
                            "EXECUTED" => demo::fill_message(&o),
                            "CANCELED" => format!("{} cancelled: not fillable now", demo::order_line(&o)),
                            _ => format!("{} working (see ORD)", demo::order_line(&o)),
                        });
                    }
                    Err(e) => {
                        t.done = None;
                        t.error = Some(e);
                    }
                }
            }
            return;
        }
        // Live: a current preview goes straight to the confirmation; otherwise preview first.
        let fresh =
            self.trading.ticket.as_ref().and_then(|t| t.current_preview()).filter(|p| !p.expired(chrono::Utc::now()));
        if let Some(p) = fresh {
            return match self.over_limit(p, quote) {
                Some(e) => self.ticket_error(e),
                None => {
                    let t = self.trading.ticket.as_mut().expect("checked above");
                    t.confirming = true;
                    t.error = None;
                }
            };
        }
        self.ticket_preview(req, quote);
    }

    fn ticket_preview(&mut self, req: OrderRequest, quote: Option<f64>) {
        if !self.trading_enabled() {
            return self.ticket_error("Live orders are off. Enable them in Settings › Trading.".into());
        }
        if let Some(e) = self.trade_blocker(&req) {
            return self.ticket_error(e);
        }
        let Some(id) = self.trading.ticket.as_ref().map(|t| t.id) else { return };
        match self.broker.mode() {
            BrokerMode::Setup => self.ticket_error("Connect a brokerage first (Settings › Brokerage).".into()),
            BrokerMode::Simulated => {
                let result = match self.broker.portfolio.as_deref() {
                    Some(pf) => trading::simulate_preview(pf, &req, quote, chrono::Utc::now()).map(Box::new),
                    None => Err("The sample portfolio is still loading.".into()),
                };
                if let Some(t) = self.trading.ticket.as_mut() {
                    t.review = true;
                }
                self.on_trade(TradeResponse::Preview { ticket: id, result });
            }
            BrokerMode::Live => {
                let t = self.trading.ticket.as_mut().expect("checked above");
                t.previewing = true;
                t.review = true;
                t.error = None;
                t.done = None;
                self.worker.send(Request::Trade(TradeRequest::Preview {
                    creds: self.broker.creds.clone(),
                    req,
                    ticket: id,
                }));
            }
        }
    }

    /// Why this account cannot take this order, from its brokerage and connection.
    fn trade_blocker(&self, req: &OrderRequest) -> Option<String> {
        let Some(cap) = self.capability(&req.account_id) else {
            return Some(if self.broker.mode() == BrokerMode::Live {
                self.caps_pending_note()
            } else {
                "This account's connection was not found; refresh with PORT REFRESH.".into()
            });
        };
        if let Some(b) = cap.blocker() {
            return Some(b);
        }
        match req.amount {
            Amount::Units(u) if u.fract() != 0.0 && !cap.fractional => {
                Some(format!("{} does not support fractional shares through SnapTrade.", cap.brokerage))
            }
            _ => None,
        }
    }

    fn ticket_error(&mut self, e: String) {
        if let Some(t) = &mut self.trading.ticket {
            t.error = Some(e);
            t.review = false;
        }
    }

    /// The user confirmed: send the previewed order. Every precondition is checked again.
    fn ticket_place(&mut self) {
        let Some(t) = self.trading.ticket.as_mut() else { return };
        t.confirming = false;
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let quote = self.ticket_quote(t);
        let preview = match t.current_preview() {
            _ if !self.trading_enabled() => Err("Live orders are off.".to_string()),
            _ if self.demo_mode() => Err("The demo account is selected; switch to LIVE to send this order.".into()),
            None => Err("The ticket changed since the preview; review again.".into()),
            Some(p) if p.expired(chrono::Utc::now()) => Err("The preview expired; review again.".into()),
            Some(p) => match self.trade_blocker(&p.request) {
                Some(b) => Err(b),
                None => self.over_limit(p, quote).map_or(Ok(Box::new(p.clone())), Err),
            },
        };
        let preview = match preview {
            Ok(p) => p,
            Err(e) => return self.ticket_error(e),
        };
        let id = t.id;
        // The last gate before anything is sent (kill switch, hard limits).
        let gate = match self.submit_gate(&preview.request, quote, true) {
            Ok(a) => a,
            Err(e) => return self.ticket_error(e.text().to_string()),
        };
        self.set_live_ctx(id, &preview.request, &gate, quote);
        if preview.simulated {
            let result = match self.broker.portfolio.as_deref_mut() {
                Some(pf) if pf.simulated => {
                    self.trading.sim_trades = true;
                    trading::simulate_place(pf, &preview, chrono::Utc::now()).map(Some)
                }
                _ => Err("The sample portfolio is not loaded.".into()),
            }
            .map_err(PlaceError::NotSent);
            return self.on_trade(TradeResponse::Placed {
                ticket: id,
                preview,
                result: result.map(|o| o.map(Box::new)),
            });
        }
        if self.broker.mode() != BrokerMode::Live {
            return self.ticket_error("The brokerage connection changed; review again.".into());
        }
        let t = self.trading.ticket.as_mut().expect("checked above");
        t.placing = true;
        t.error = None;
        // Consume the preview as it is sent: whatever the outcome, this trade id is never sent
        // again from the ticket.
        t.preview = None;
        self.status = format!("Sending LIVE order: {}", preview.request.summary());
        self.worker.send(Request::Trade(TradeRequest::Place { creds: self.broker.creds.clone(), preview, ticket: id }));
    }

    /// Cancel an open order (from `ORD`). Demo orders cancel at once; live ones are confirmed.
    pub(super) fn request_cancel(&mut self, account: &str, order_id: &str) {
        if account == demo::ACCOUNT_ID {
            return self.demo_cancel(order_id);
        }
        if !self.trading_enabled() {
            self.status = "Live orders are off: enable them in Settings › Trading to cancel orders here".into();
            return;
        }
        let order = self
            .broker
            .portfolio
            .as_ref()
            .and_then(|pf| pf.orders.iter().find(|o| o.account_id == account && o.id == order_id))
            .cloned();
        match order {
            Some(o) if o.is_open() && !o.id.is_empty() => {
                if let Some(b) = self.capability(account).and_then(|c| c.blocker()) {
                    self.status = b;
                    return;
                }
                self.ensure_capabilities(false);
                self.trading.cancel = Some(CancelPrompt { order: o, busy: false, error: None })
            }
            Some(o) => self.status = format!("Order is {} and cannot be cancelled", o.status.to_ascii_lowercase()),
            None => self.status = "Order not found; refresh with PORT REFRESH".into(),
        }
    }

    fn confirm_cancel(&mut self) {
        let Some(c) = &self.trading.cancel else { return };
        let (account, order) = (c.order.account_id.clone(), c.order.id.clone());
        let error = |app: &mut Self, e: &str| {
            if let Some(c) = &mut app.trading.cancel {
                c.error = Some(e.into());
            }
        };
        if !self.trading_enabled() {
            return error(self, "Live orders are off.");
        }
        let simulated = self.broker.portfolio.as_ref().is_some_and(|p| p.simulated);
        if simulated {
            self.trading.sim_trades = true;
            let pf = self.broker.portfolio.as_deref_mut().expect("checked above");
            let result = trading::simulate_cancel(pf, &account, &order);
            self.on_trade(TradeResponse::Cancelled { account, order, result });
        } else if self.broker.mode() == BrokerMode::Live {
            if let Some(c) = &mut self.trading.cancel {
                c.busy = true;
                c.error = None;
            }
            self.worker.send(Request::Trade(TradeRequest::Cancel { creds: self.broker.creds.clone(), account, order }));
        } else {
            error(self, "No brokerage connection.");
        }
    }

    pub(super) fn on_trade(&mut self, r: TradeResponse) {
        match r {
            TradeResponse::Preview { ticket, result } => {
                let quote = self.trading.ticket.as_ref().and_then(|t| self.ticket_quote(t));
                let over = match &result {
                    Ok(p) => self.over_limit(p, quote),
                    Err(_) => None,
                };
                let Some(t) = self.trading.ticket.as_mut().filter(|t| t.id == ticket) else { return };
                t.previewing = false;
                let review = std::mem::take(&mut t.review);
                match result {
                    Ok(p) => {
                        let current = t.request().ok().as_ref() == Some(&p.request);
                        t.preview = Some(p);
                        t.error = over.clone();
                        // One step: the broker's estimate opens the confirmation directly.
                        t.confirming = review && current && over.is_none();
                    }
                    Err(e) => {
                        t.preview = None;
                        t.error = Some(explain(&e));
                    }
                }
            }
            TradeResponse::Placed { ticket, preview, result } => {
                let summary = preview.request.summary();
                let ok = result.is_ok();
                self.audit_live_result(ticket, &preview.request, &result);
                let msg = match &result {
                    Ok(Some(o)) => format!(
                        "Order {}: {summary}{}",
                        o.status.to_ascii_lowercase().replace('_', " "),
                        if o.id.is_empty() { String::new() } else { format!(" (#{})", o.id) }
                    ),
                    Ok(None) => format!("Order sent: {summary}"),
                    Err(e) if e.uncertain() => format!(
                        "Order status unknown: {summary}: {}. It may have reached your broker; check ORD before placing it again.",
                        e.message()
                    ),
                    Err(PlaceError::NotSent(e)) => format!("Order not sent: {summary}: {e}"),
                    Err(e) => format!("Order rejected: {summary}: {}", explain(e.message())),
                };
                self.trade_toast(msg.clone(), ok);
                let uncertain = result.as_ref().err().is_some_and(PlaceError::uncertain);
                if let Some(t) = self.trading.ticket.as_mut().filter(|t| t.id == ticket) {
                    t.placing = false;
                    // A sent preview is never offered again, whatever the answer.
                    t.preview = None;
                    match result {
                        Ok(_) => {
                            t.done = Some(msg);
                            t.error = None;
                        }
                        Err(_) => t.error = Some(msg),
                    }
                }
                if !preview.simulated {
                    self.refresh_portfolio();
                    if uncertain {
                        // Open the orders view so the user can see whether it went through.
                        self.portfolio_command(crate::command::PortView::Orders, "", None);
                    }
                }
            }
            TradeResponse::Cancelled { account, order, result } => {
                let msg = match &result {
                    Ok(()) => format!("Cancel requested for order #{order}"),
                    Err(e) => format!("Could not cancel order #{order}: {}", explain(e)),
                };
                self.trade_toast(msg.clone(), result.is_ok());
                match result {
                    Ok(()) => {
                        self.trading.cancel = None;
                        if self.broker.portfolio.as_ref().is_some_and(|p| !p.simulated) {
                            self.refresh_portfolio();
                        }
                    }
                    Err(_) => {
                        if let Some(c) = self
                            .trading
                            .cancel
                            .as_mut()
                            .filter(|c| c.order.id == order && c.order.account_id == account)
                        {
                            c.busy = false;
                            c.error = Some(msg);
                        }
                    }
                }
            }
            TradeResponse::Capabilities(result) => self.on_capabilities(result),
            TradeResponse::Portal(result) => {
                self.trading.portal_busy = false;
                self.trading.message = Some(match result {
                    Ok(url) => match open_external(&url) {
                        Ok(()) => {
                            self.broker.connecting_since = Some(Instant::now());
                            ("Connection Portal opened: reconnect your broker and allow trading.".into(), false)
                        }
                        Err(e) => (format!("Could not open a browser ({e})."), true),
                    },
                    Err(e) => (format!("Could not open the Connection Portal: {e}"), true),
                });
            }
        }
    }

    // ---- windows ------------------------------------------------------------------------

    /// The ticket window and the confirmation dialogs.
    pub(super) fn trading_windows(&mut self, ctx: &egui::Context) {
        self.trading_periodic(ctx);
        self.safety_periodic();
        if self.trading.ticket.is_some() {
            self.ticket_window(ctx);
        }
        if self.trading.ticket.as_ref().is_some_and(|t| t.confirming) {
            self.confirm_dialog(ctx);
        }
        if self.trading.cancel.is_some() {
            self.cancel_dialog(ctx);
        }
        if self.trading.enable_prompt {
            self.enable_dialog(ctx);
        }
        if self.trading.live_prompt {
            self.live_dialog(ctx);
        }
    }
}

enum TicketAct {
    /// `confirmed`: the user clicked the button while warnings were showing.
    Submit {
        confirmed: bool,
    },
    EnableLive,
    Mode(AccountMode),
}

/// Enter was pressed this frame (not an auto-repeat of a held key).
fn fresh_enter(ui: &Ui) -> bool {
    ui.input(|i| {
        i.events.iter().any(|e| {
            matches!(e, egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat: false, modifiers, .. } if modifiers.is_none())
        })
    })
}

/// A coloured action button, plain when disabled so it does not look clickable.
fn filled(label: &str, fill: Color32, enabled: bool) -> egui::Button<'static> {
    if enabled {
        egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(fill)
    } else {
        egui::Button::new(RichText::new(label).strong())
    }
}

/// The DEMO / LIVE pill.
pub fn mode_badge(ui: &mut Ui, live: bool) -> egui::Response {
    let (label, fill) = if live { ("LIVE", theme::down()) } else { ("DEMO", theme::accent()) };
    ui.add(
        egui::Button::new(RichText::new(label).font(mono(11.0)).strong().color(Color32::WHITE))
            .fill(fill)
            .corner_radius(9.0)
            .sense(egui::Sense::hover()),
    )
}

/// `Individual · Robinhood ••••8443`.
fn account_line(a: &Account) -> String {
    let mut s = format!("{} · {} {}", a.label(), a.institution, a.masked_number());
    if a.is_paper {
        s += " · paper";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        let mut t = Ticket::new(1);
        t.account = Some("sim-brokerage".into());
        t.apply(&TradeArgs {
            buy: Some(true),
            symbol: Some("AAPL US Equity".into()),
            units: Some(10.0),
            ..Default::default()
        });
        t
    }

    #[test]
    fn ticket_builds_requests() {
        let t = ticket();
        let r = t.request().unwrap();
        assert_eq!((r.symbol.as_str(), r.side, r.order_type), ("AAPL", Side::Buy, OrderType::Market));
        assert_eq!(r.amount, Amount::Units(10.0));
        assert_eq!(t.market_key().as_deref(), Some("AAPL US Equity"));

        let mut l = t.clone();
        l.apply(&TradeArgs { buy: Some(false), limit: Some(190.5), ..Default::default() });
        let r = l.request().unwrap();
        assert_eq!((r.side, r.order_type, r.limit), (Side::Sell, OrderType::Limit, Some(190.5)));

        let mut sl = t.clone();
        sl.apply(&TradeArgs { stop: Some(180.0), limit: Some(181.0), ..Default::default() });
        assert_eq!(sl.order_type, OrderType::StopLimit);
        assert!(sl.advanced, "stop orders show the advanced fields");

        let mut n = t.clone();
        n.apply(&TradeArgs { notional: Some(500.0), ..Default::default() });
        assert_eq!(n.request().unwrap().amount, Amount::Notional(500.0));

        let mut bad = t.clone();
        bad.account = None;
        assert!(bad.request().unwrap_err().contains("account"));
        bad = t.clone();
        bad.qty = "ten".into();
        assert!(bad.request().unwrap_err().contains("quantity"));
        bad = t.clone();
        bad.order_type = OrderType::Limit;
        bad.limit.clear();
        assert!(bad.request().unwrap_err().contains("limit"));
    }

    #[test]
    fn editing_the_ticket_invalidates_the_preview() {
        let mut pf = tr_broker::sample::portfolio();
        let mut t = ticket();
        let p = trading::simulate_preview(&pf, &t.request().unwrap(), Some(200.0), chrono::Utc::now()).unwrap();
        t.preview = Some(Box::new(p));
        assert!(t.current_preview().is_some());
        t.qty = "11".into();
        assert!(t.current_preview().is_none(), "a changed quantity needs a new preview");
        t.qty = "10".into();
        assert!(t.current_preview().is_some());
        t.side = Side::Sell;
        assert!(t.current_preview().is_none());
        t.side = Side::Buy;
        let placed = trading::simulate_place(&mut pf, t.current_preview().unwrap(), chrono::Utc::now()).unwrap();
        assert_eq!(placed.status, "EXECUTED");
    }

    #[test]
    fn demo_tickets_place_in_the_demo_book() {
        let mut t = ticket();
        t.account = Some(demo::ACCOUNT_ID.into());
        let mut book = demo::DemoBook::new(chrono::Utc::now());
        book.reset(100_000.0, chrono::Utc::now());
        let o = book.submit(&t.request().unwrap(), Some(200.0), chrono::Utc::now()).unwrap();
        assert_eq!(o.status, "EXECUTED");
        assert_eq!(book.held("AAPL"), 10.0);
    }

    #[test]
    fn read_only_errors_point_to_reconnecting() {
        assert!(explain("SnapTrade: User does not have permissions to place trades (403)").contains("Reconnect"));
        assert_eq!(explain("SnapTrade: Insufficient buying power"), "SnapTrade: Insufficient buying power");
    }

    #[test]
    fn caps_cache_round_trips() {
        let mut caps = HashMap::new();
        caps.insert(
            "conn-1".to_string(),
            Capability {
                brokerage: "Wealthsimple".into(),
                broker_trading: true,
                connection_trading: true,
                fractional: true,
                disabled: false,
            },
        );
        let c = CapsCache { fetched: Some(chrono::Utc::now()), caps };
        let back: CapsCache = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}
