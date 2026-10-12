//! Order entry through SnapTrade: the order ticket (`BUY`, `SELL`, `TRADE`, Trade on quote and
//! position rows), SnapTrade's order-impact preview, the mandatory confirmation dialog,
//! cancelling open orders from `ORD`, and Settings › Trading.
//!
//! Safety rails, in order:
//! 1. Trading is off until enabled in Settings › Trading, which takes an explicit
//!    acknowledgement. Commands still open the ticket, but nothing can be previewed or sent.
//! 2. An order can only be placed from a SnapTrade preview of exactly the ticket's current
//!    fields; editing anything makes the preview stale. Previews expire before SnapTrade's
//!    five-minute limit.
//! 3. Placing always goes through a confirmation dialog showing account, symbol, side,
//!    quantity and estimated cost. Escape or a click outside cancels.
//! 4. An optional maximum order value refuses large orders before they are sent.
//!
//! With the simulated sample portfolio, previews and orders are computed locally and never
//! leave the machine.

use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use tr_broker::trading::{
    self, Amount, Capability, OrderPreview, OrderRequest, OrderType, PlaceError, Side, TimeInForce,
};
use tr_broker::{Account, InstrumentKind, Order};

use super::TerminalApp;
use super::broker::{BrokerMode, open_external};
use super::portfolio_view::{money, price, qty};
use super::settings_view::SettingsCmd;
use crate::command::TradeArgs;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{Request, TradeRequest, TradeResponse};

/// Marks a toast as coming from order entry (titled "Trading" rather than a Congress alert).
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
    pub preview: Option<Box<OrderPreview>>,
    pub previewing: bool,
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
            preview: None,
            previewing: false,
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
        self.preview = None;
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

#[derive(Default)]
pub struct TradingState {
    pub ticket: Option<Ticket>,
    next_id: u64,
    pub cancel: Option<CancelPrompt>,
    /// Settings › Trading: the enable acknowledgement dialog is open.
    pub enable_prompt: bool,
    pub ack: bool,
    /// Settings › Trading feedback: text and whether it is an error.
    pub message: Option<(String, bool)>,
    pub portal_busy: bool,
    last_quote: Option<Instant>,
    /// Per-account trading capability (live connections), and when it was requested.
    pub caps: Option<std::collections::HashMap<String, Capability>>,
    caps_requested: Option<Instant>,
    /// `Portfolio::fetched` when capabilities were last requested: a newer portfolio (after
    /// reconnecting, say) makes them stale.
    caps_for: Option<chrono::DateTime<chrono::Utc>>,
    pub caps_error: Option<String>,
    /// Simulated orders were placed or cancelled: keep the sample portfolio across refreshes.
    pub sim_trades: bool,
}

/// How often capabilities are re-read while trading UI is open.
const CAPS_TTL: Duration = Duration::from_secs(300);
/// Faster re-reads while the user may be reconnecting with trading access.
const CAPS_CONNECTING: Duration = Duration::from_secs(20);

/// Add a hint to errors that usually mean the connection is read-only.
fn explain(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("403") || l.contains("permission") || l.contains("not allowed") || l.contains("read-only") {
        format!("{e} — the connection may be read-only: Settings › Trading › Reconnect with Trading Access.")
    } else {
        e.to_string()
    }
}

impl TerminalApp {
    fn trading_enabled(&self) -> bool {
        self.prefs.trading.enabled
    }

    /// What `account` can do: everything in the simulated portfolio, the fetched capability
    /// for live accounts (`None` while unknown).
    pub(super) fn capability(&self, account: &str) -> Option<Capability> {
        match self.broker.mode() {
            BrokerMode::Simulated => Some(Capability {
                brokerage: self
                    .broker
                    .portfolio
                    .as_ref()
                    .and_then(|pf| pf.account(account))
                    .map_or("Simulated Broker".into(), |a| a.institution.clone()),
                broker_trading: true,
                connection_trading: true,
                fractional: true,
                disabled: false,
            }),
            BrokerMode::Live => self.trading.caps.as_ref()?.get(account).cloned(),
            BrokerMode::Setup => None,
        }
    }

    /// Re-read capabilities when stale (live connections only): after [`CAPS_TTL`], every
    /// [`CAPS_CONNECTING`] while the user may be reconnecting in the browser, and whenever a
    /// newer portfolio has loaded since the last read.
    pub(super) fn ensure_capabilities(&mut self, force: bool) {
        if self.broker.mode() != BrokerMode::Live {
            return;
        }
        let connecting = self.broker.connecting_since.is_some_and(|t| t.elapsed() < Duration::from_secs(300));
        let ttl = if connecting { CAPS_CONNECTING } else { CAPS_TTL };
        let fetched = self.broker.portfolio.as_ref().and_then(|p| p.fetched);
        let newer_portfolio = fetched.is_some() && fetched != self.trading.caps_for;
        let stale = self.trading.caps_requested.is_none_or(|t| t.elapsed() > ttl);
        // Never more than one request every few seconds, whatever triggers it.
        let recent = self.trading.caps_requested.is_some_and(|t| t.elapsed() < Duration::from_secs(5));
        if force || stale || (newer_portfolio && !recent) {
            self.trading.caps_requested = Some(Instant::now());
            self.trading.caps_for = fetched;
            self.worker.send(Request::Trade(TradeRequest::Capabilities { creds: self.broker.creds.clone() }));
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

    /// Accounts orders can go to.
    fn trade_accounts(&self) -> Vec<Account> {
        self.broker.portfolio.as_ref().map(|p| p.accounts.clone()).unwrap_or_default()
    }

    /// `BUY`/`SELL`/`TRADE` and Trade actions: open (or refill) the ticket.
    pub(super) fn open_ticket(&mut self, args: TradeArgs, account: Option<String>) {
        let t = &mut self.trading;
        let mut ticket = match t.ticket.take() {
            // Keep the account and type choices of an idle ticket.
            Some(old) if !old.placing && !old.previewing => Ticket { id: old.id, ..old },
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
        self.status = match (&args.symbol, self.trading_enabled()) {
            (_, false) => "Order ticket opened · trading is off (Settings › Trading)".into(),
            (Some(k), true) => format!("Order ticket: {} {}", ticket.side.label(), broker_symbol(k)),
            (None, true) => "Order ticket".into(),
        };
        self.trading.ticket = Some(ticket);
        self.trading.last_quote = None;
        self.ensure_capabilities(false);
        if self.broker.portfolio.is_none() && !self.broker.loading {
            self.refresh_portfolio();
        }
    }

    fn ticket_quote(&self, t: &Ticket) -> Option<f64> {
        self.quotes.get(&t.market_key()?).map(|q| q.last).filter(|x| x.is_finite() && *x > 0.0)
    }

    /// Keep the ticket's quote fresh and its expiry countdown ticking.
    pub(super) fn trading_periodic(&mut self, ctx: &egui::Context) {
        if self.trading.ticket.is_none() {
            return;
        }
        self.ensure_capabilities(false);
        let Some(t) = &self.trading.ticket else { return };
        if t.preview.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(k) = t.market_key()
            && self.trading.last_quote.is_none_or(|q| q.elapsed() > Duration::from_secs(15))
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

    fn ticket_preview(&mut self) {
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let quote = self.ticket_quote(t);
        let req = match t.request() {
            Ok(r) => r,
            Err(e) => return self.ticket_error(e),
        };
        if !self.trading_enabled() {
            return self.ticket_error("Trading is off. Enable it in Settings › Trading.".into());
        }
        if let Some(e) = self.trade_blocker(&req) {
            return self.ticket_error(e);
        }
        let id = t.id;
        match self.broker.mode() {
            BrokerMode::Setup => self.ticket_error("Connect a brokerage first (PORT SETUP).".into()),
            BrokerMode::Simulated => {
                let result = match self.broker.portfolio.as_deref() {
                    Some(pf) => trading::simulate_preview(pf, &req, quote, chrono::Utc::now()).map(Box::new),
                    None => Err("The sample portfolio is still loading.".into()),
                };
                self.on_trade(TradeResponse::Preview { ticket: id, result });
            }
            BrokerMode::Live => {
                let t = self.trading.ticket.as_mut().expect("checked above");
                t.previewing = true;
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
            return (self.broker.mode() == BrokerMode::Live && self.trading.caps.is_none())
                .then(|| match &self.trading.caps_error {
                    Some(e) => format!("Could not check the account's trading access: {e}"),
                    None => "Checking the account's trading access…".into(),
                })
                .or(Some("This account's connection was not found; refresh with PORT REFRESH.".into()));
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
        }
    }

    /// Open the confirmation dialog for a current, unexpired preview.
    fn ticket_review(&mut self) {
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let quote = self.ticket_quote(t);
        let check = match t.current_preview() {
            None => Err("Preview the order first.".to_string()),
            Some(p) if p.expired(chrono::Utc::now()) => Err("The preview expired; preview again.".into()),
            Some(p) => self.over_limit(p, quote).map_or(Ok(()), Err),
        };
        match check {
            Ok(()) => {
                let t = self.trading.ticket.as_mut().expect("checked above");
                t.confirming = true;
                t.error = None;
            }
            Err(e) => self.ticket_error(e),
        }
    }

    /// The user confirmed: send the previewed order. Every precondition is checked again.
    fn ticket_place(&mut self) {
        let Some(t) = self.trading.ticket.as_mut() else { return };
        t.confirming = false;
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let quote = self.ticket_quote(t);
        let preview = match t.current_preview() {
            _ if !self.trading_enabled() => Err("Trading is off.".to_string()),
            None => Err("The ticket changed since the preview; preview again.".into()),
            Some(p) if p.expired(chrono::Utc::now()) => Err("The preview expired; preview again.".into()),
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
            return self.ticket_error("The brokerage connection changed; preview again.".into());
        }
        let t = self.trading.ticket.as_mut().expect("checked above");
        t.placing = true;
        t.error = None;
        // Consume the preview as it is sent: whatever the outcome, this trade id is never sent
        // again from the ticket.
        t.preview = None;
        self.status = format!("Sending order: {}", preview.request.summary());
        self.worker.send(Request::Trade(TradeRequest::Place { creds: self.broker.creds.clone(), preview, ticket: id }));
    }

    /// Ask to cancel an open order (from `ORD`).
    pub(super) fn request_cancel(&mut self, account: &str, order_id: &str) {
        if !self.trading_enabled() {
            self.status = "Trading is off: enable it in Settings › Trading to cancel orders here".into();
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
            return error(self, "Trading is off.");
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
                let Some(t) = self.trading.ticket.as_mut().filter(|t| t.id == ticket) else { return };
                t.previewing = false;
                match result {
                    Ok(p) => {
                        t.preview = Some(p);
                        t.error = None;
                    }
                    Err(e) => {
                        t.preview = None;
                        t.error = Some(explain(&e));
                    }
                }
            }
            TradeResponse::Placed { ticket, preview, result } => {
                let summary = preview.request.summary();
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
                tracing::info!("trade: {msg}");
                self.status = msg.clone();
                self.toasts.push((format!("{TOAST_PREFIX}{msg}"), Instant::now()));
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
                self.status = msg.clone();
                self.toasts.push((format!("{TOAST_PREFIX}{msg}"), Instant::now()));
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
            TradeResponse::Capabilities(result) => match result {
                Ok(c) => {
                    self.trading.caps = Some(c);
                    self.trading.caps_error = None;
                }
                Err(e) => {
                    tracing::warn!("trading capabilities: {e}");
                    self.trading.caps_error = Some(e);
                }
            },
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
    }

    fn ticket_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let simulated = self.broker.mode() == BrokerMode::Simulated;
        let title = if simulated { "Order Ticket · Simulated" } else { "Order Ticket" };
        let mut act = None;
        egui::Window::new(RichText::new(title).font(ui_font(13.0)))
            .id(egui::Id::new("order-ticket"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(dp(340.0))
            .default_pos(ctx.content_rect().center() - egui::vec2(dp(170.0), dp(240.0)))
            .show(ctx, |ui| act = self.ticket_ui(ui, simulated));
        if !open {
            self.trading.ticket = None;
            return;
        }
        match act {
            Some(TicketAct::Preview) => self.ticket_preview(),
            Some(TicketAct::Review) => self.ticket_review(),
            Some(TicketAct::Settings) => {
                self.settings_tab = super::settings_view::SettingsTab::Trading;
                self.open_panel(super::PaneKind::Settings);
            }
            None => {}
        }
    }

    fn ticket_ui(&mut self, ui: &mut Ui, simulated: bool) -> Option<TicketAct> {
        let enabled = self.trading_enabled();
        let accounts = self.trade_accounts();
        let pf_loading = self.broker.loading && self.broker.portfolio.is_none();
        let held: Vec<(String, f64)> = self
            .broker
            .portfolio
            .as_ref()
            .map(|pf| pf.positions.iter().map(|p| (format!("{}|{}", p.account_id, p.symbol), p.units)).collect())
            .unwrap_or_default();
        let quote = self.trading.ticket.as_ref().and_then(|t| self.ticket_quote(t));
        let caps: std::collections::HashMap<String, Option<Capability>> =
            accounts.iter().map(|a| (a.id.clone(), self.capability(&a.id))).collect();
        let checking = self.broker.mode() == BrokerMode::Live && self.trading.caps.is_none();
        let caps_error = self.trading.caps_error.clone();
        let t = self.trading.ticket.as_mut()?;
        let mut act = None;
        ui.spacing_mut().item_spacing.y = dp(6.0);

        if !enabled {
            egui::Frame::new().fill(theme::accent_wash()).corner_radius(4.0).inner_margin(dp(6.0)).show(ui, |ui| {
                ui.label(
                    RichText::new("Trading is off. Orders cannot be previewed or sent until you enable it.")
                        .font(ui_font(12.0))
                        .color(amber()),
                );
                if ui.button("Open Settings › Trading").clicked() {
                    act = Some(TicketAct::Settings);
                }
            });
        } else if simulated {
            ui.label(
                RichText::new("Sample portfolio: orders are simulated locally and never sent.")
                    .font(ui_font(11.5))
                    .color(theme::cyan()),
            );
        }
        let busy = t.previewing || t.placing;
        let label = |ui: &mut Ui, s: &str| {
            ui.add_sized([dp(78.0), dp(20.0)], egui::Label::new(RichText::new(s).font(ui_font(12.0)).color(muted())));
        };

        // Account.
        ui.horizontal(|ui| {
            label(ui, "Account");
            let current = t
                .account
                .as_ref()
                .and_then(|id| accounts.iter().find(|a| &a.id == id))
                .map_or("Choose…".to_string(), account_line);
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("ticket-account").width(dp(230.0)).selected_text(current).show_ui(
                    ui,
                    |ui| {
                        for a in &accounts {
                            let cap = caps.get(&a.id).cloned().flatten();
                            let read_only = cap.as_ref().is_some_and(|c| !c.can_trade());
                            let label = format!(
                                "{}{}{}",
                                account_line(a),
                                if read_only { " · read-only" } else { "" },
                                a.buying_power
                                    .or(a.cash)
                                    .map_or(String::new(), |c| format!(" · {} available", money(c, &a.currency)))
                            );
                            ui.selectable_value(&mut t.account, Some(a.id.clone()), label);
                        }
                    },
                );
            });
            if pf_loading {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
        let blocker = match t.account.as_ref().map(|id| caps.get(id).cloned().flatten()) {
            Some(Some(c)) => c.blocker(),
            Some(None) if checking => Some(match &caps_error {
                Some(e) => format!("Could not check trading access: {e}"),
                None => "Checking trading access…".into(),
            }),
            Some(None) => Some("No connection found for this account; refresh with PORT REFRESH.".into()),
            None => None,
        };
        if let Some(b) = &blocker {
            ui.label(RichText::new(b).font(ui_font(11.5)).color(amber()));
        }
        if accounts.is_empty() && !pf_loading {
            ui.label(RichText::new("No brokerage accounts loaded (PORT SETUP).").font(ui_font(11.5)).color(muted()));
        }

        // Side and symbol.
        ui.horizontal(|ui| {
            label(ui, "Side");
            let sel = Some(usize::from(t.side == Side::Sell));
            if let Some(i) = widgets::segmented(ui, "ticket-side", &["Buy", "Sell"], sel, 22.0)
                && !busy
            {
                t.side = if i == 0 { Side::Buy } else { Side::Sell };
            }
        });
        ui.horizontal(|ui| {
            label(ui, "Symbol");
            let r = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut t.symbol).hint_text("AAPL").font(mono(13.0)).desired_width(dp(90.0)),
            );
            if r.changed() {
                t.symbol = t.symbol.to_ascii_uppercase();
            }
            if let Some(q) = quote {
                ui.label(RichText::new(format!("last {}", price(q))).font(mono(12.0)).color(text()));
            }
        });

        // Size.
        let key = format!("{}|{}", t.account.as_deref().unwrap_or(""), t.symbol.trim());
        let position = held.iter().find(|(k, _)| k.eq_ignore_ascii_case(&key)).map(|(_, u)| *u);
        ui.horizontal(|ui| {
            label(ui, if t.dollars { "Amount" } else { "Quantity" });
            ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut t.qty)
                    .hint_text(if t.dollars { "$" } else { "shares" })
                    .font(mono(13.0))
                    .desired_width(dp(90.0)),
            );
            let sel = Some(usize::from(t.dollars));
            if let Some(i) = widgets::segmented(ui, "ticket-size", &["Shares", "Dollars"], sel, 20.0)
                && !busy
            {
                t.dollars = i == 1;
                if t.dollars {
                    t.order_type = OrderType::Market;
                    t.tif = TimeInForce::Day;
                }
            }
        });
        if let Some(units) = position {
            ui.horizontal(|ui| {
                ui.add_space(dp(84.0));
                ui.label(RichText::new(format!("Holding {}", qty(units))).font(ui_font(11.5)).color(muted()));
                if t.side == Side::Sell && units > 0.0 && !busy && ui.small_button("Sell all").clicked() {
                    t.dollars = false;
                    t.qty = fmt_input(units);
                }
            });
        }

        // Type, prices and time in force.
        ui.horizontal(|ui| {
            label(ui, "Type");
            let labels: Vec<&str> = OrderType::ALL.iter().map(|o| o.label()).collect();
            let sel = OrderType::ALL.iter().position(|o| *o == t.order_type);
            if let Some(i) = widgets::segmented(ui, "ticket-type", &labels, sel, 20.0)
                && !busy
            {
                t.order_type = OrderType::ALL[i];
                if t.order_type != OrderType::Market {
                    t.dollars = false;
                }
            }
        });
        if t.order_type.needs_stop() {
            ui.horizontal(|ui| {
                label(ui, "Stop");
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut t.stop)
                        .hint_text("trigger")
                        .font(mono(13.0))
                        .desired_width(dp(90.0)),
                );
            });
        }
        if t.order_type.needs_limit() {
            ui.horizontal(|ui| {
                label(ui, "Limit");
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut t.limit)
                        .hint_text("price")
                        .font(mono(13.0))
                        .desired_width(dp(90.0)),
                );
                if let Some(q) = quote
                    && !busy
                    && ui.small_button("Last").on_hover_text("Use the last price").clicked()
                {
                    t.limit = fmt_input(q);
                }
            });
        }
        ui.horizontal(|ui| {
            label(ui, "Time in force");
            let labels: Vec<&str> = TimeInForce::ALL.iter().map(|x| x.label()).collect();
            let sel = TimeInForce::ALL.iter().position(|x| *x == t.tif);
            if let Some(i) = widgets::segmented(ui, "ticket-tif", &labels, sel, 20.0)
                && !busy
            {
                t.tif = TimeInForce::ALL[i];
            }
        })
        .response
        .on_hover_text(t.tif.describe());

        ui.separator();
        // Preview.
        let now = chrono::Utc::now();
        match (&t.preview, t.current_preview()) {
            (_, Some(p)) => preview_lines(ui, p, quote, now),
            (Some(_), None) => {
                ui.label(
                    RichText::new("The ticket changed since the preview: preview again.")
                        .font(ui_font(11.5))
                        .color(amber()),
                );
            }
            (None, None) => {
                ui.label(
                    RichText::new("Preview asks the broker (through SnapTrade) to check the order and estimate cost and fees. Nothing is sent until you confirm.")
                        .font(ui_font(11.5))
                        .color(muted()),
                );
            }
        }
        if let Some(e) = &t.error {
            ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
        }
        if let Some(d) = &t.done {
            ui.label(RichText::new(d).font(ui_font(12.0)).color(theme::up()));
        }

        let ready = t.request();
        let previewed = t.current_preview().is_some_and(|p| !p.expired(now));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(enabled && ready.is_ok() && blocker.is_none() && !busy, egui::Button::new("Preview"))
                .on_disabled_hover_text(match &ready {
                    Err(e) => e.as_str(),
                    Ok(_) if !enabled => "Trading is off",
                    Ok(_) if blocker.is_some() => "This account cannot trade",
                    Ok(_) => "Working…",
                })
                .clicked()
            {
                act = Some(TicketAct::Preview);
            }
            let (fill, label) = match t.side {
                Side::Buy => (theme::up(), "Place Buy Order…"),
                Side::Sell => (theme::down(), "Place Sell Order…"),
            };
            let can_place = enabled && previewed && blocker.is_none() && !busy;
            if ui
                .add_enabled(can_place, filled(label, fill, can_place))
                .on_disabled_hover_text("Preview the current ticket first")
                .clicked()
            {
                act = Some(TicketAct::Review);
            }
            if busy {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(
                    RichText::new(if t.placing { "Sending…" } else { "Checking…" }).font(ui_font(11.5)).color(muted()),
                );
            }
        });
        act
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let Some(p) = t.current_preview().cloned() else {
            if let Some(t) = self.trading.ticket.as_mut() {
                t.confirming = false;
            }
            return;
        };
        let quote = self.ticket_quote(t);
        let account = self.broker.portfolio.as_ref().and_then(|pf| pf.account(&p.request.account_id)).cloned();
        let now = chrono::Utc::now();
        let expired = p.expired(now);
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("order-confirm")).show(ctx, |ui| {
            ui.set_width(dp(380.0));
            let r = &p.request;
            let side_color = if r.side == Side::Buy { theme::up() } else { theme::down() };
            ui.label(
                RichText::new(format!("Confirm {} order", r.side.label().to_lowercase())).font(ui_font(16.0)).strong(),
            );
            ui.add_space(dp(4.0));
            egui::Grid::new("confirm-grid").num_columns(2).spacing([dp(14.0), dp(5.0)]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: RichText| {
                    ui.label(RichText::new(k).font(ui_font(12.0)).color(muted()));
                    ui.label(v);
                    ui.end_row();
                };
                let acct = account.as_ref().map_or(r.account_id.clone(), |a| {
                    let mut s = format!("{} · {} {}", a.label(), a.institution, a.masked_number());
                    if a.is_paper {
                        s += " · paper";
                    }
                    s
                });
                row(ui, "Account", RichText::new(acct).font(ui_font(12.5)).color(text()));
                let sym = if p.symbol.description.is_empty() {
                    p.symbol.symbol.clone()
                } else {
                    format!("{} — {}", p.symbol.symbol, p.symbol.description)
                };
                row(ui, "Symbol", RichText::new(sym).font(mono(13.0)).color(amber()));
                row(ui, "Side", RichText::new(r.side.api()).font(mono(13.0)).strong().color(side_color));
                let size = match r.amount {
                    Amount::Units(u) => format!("{} shares", qty(p.units.unwrap_or(u))),
                    Amount::Notional(n) => format!("{} (dollar amount)", money(n, &p.currency)),
                };
                row(ui, "Quantity", RichText::new(size).font(mono(13.0)).color(text()));
                let mut order = r.order_type.label().to_string();
                if let Some(s) = r.stop.filter(|_| r.order_type.needs_stop()) {
                    order += &format!(" · stop {}", price(s));
                }
                if let Some(l) = r.limit.filter(|_| r.order_type.needs_limit()) {
                    order += &format!(" · limit {}", price(l));
                }
                order += &format!(" · {}", r.time_in_force.label());
                row(ui, "Order", RichText::new(order).font(ui_font(12.5)).color(text()));
                let cost = if r.side == Side::Buy { "Est. cost" } else { "Est. proceeds" };
                let total =
                    p.estimated_total(quote).map_or("unknown (market price at fill)".into(), |v| money(v, &p.currency));
                row(ui, cost, RichText::new(total).font(mono(14.0)).strong().color(text()));
                if let Some(c) = p.commission {
                    row(ui, "Commission", RichText::new(money(c, &p.currency)).font(mono(12.5)).color(text()));
                }
                if let Some(c) = p.remaining_cash {
                    row(ui, "Cash after", RichText::new(money(c, &p.currency)).font(mono(12.5)).color(text()));
                }
            });
            ui.add_space(dp(6.0));
            let warn = if p.simulated {
                "Simulated: the order is recorded in the sample portfolio only.".to_string()
            } else {
                format!(
                    "This sends a real order to {}. It may fill immediately and a filled order cannot be undone.",
                    account.as_ref().map_or("your broker", |a| a.institution.as_str())
                )
            };
            ui.label(RichText::new(warn).font(ui_font(11.5)).color(if p.simulated { theme::cyan() } else { amber() }));
            if expired {
                ui.label(
                    RichText::new("The preview expired: cancel and preview again.")
                        .font(ui_font(12.0))
                        .color(theme::down()),
                );
            } else {
                let left = (p.expires() - now).num_seconds().max(0);
                ui.label(
                    RichText::new(format!("Preview valid for {}:{:02}", left / 60, left % 60))
                        .font(ui_font(11.0))
                        .color(muted()),
                );
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
                let label = format!("Confirm {}", r.side.label());
                if ui.add_enabled(!expired, filled(&label, side_color, !expired)).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.ticket_place(),
            Some(false) => {
                if let Some(t) = self.trading.ticket.as_mut() {
                    t.confirming = false;
                }
            }
            None => {}
        }
    }

    fn cancel_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.trading.cancel.clone() else { return };
        let account = self.broker.portfolio.as_ref().and_then(|pf| pf.account(&c.order.account_id)).cloned();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("cancel-confirm")).show(ctx, |ui| {
            ui.set_width(dp(340.0));
            let o = &c.order;
            ui.label(RichText::new("Cancel order?").font(ui_font(16.0)).strong());
            ui.add_space(dp(4.0));
            let mut line =
                format!("{} {} {}", o.action.replace('_', " "), o.quantity.map_or("—".into(), qty), o.short_label());
            if let Some(l) = o.limit_price {
                line += &format!(" limit {}", price(l));
            }
            if let Some(s) = o.stop_price {
                line += &format!(" stop {}", price(s));
            }
            ui.label(RichText::new(line).font(mono(13.0)).color(text()));
            ui.label(
                RichText::new(format!(
                    "{} · order #{} · {}",
                    account.as_ref().map_or(o.account_id.clone(), |a| format!("{} {}", a.label(), a.masked_number())),
                    o.id,
                    o.status.to_ascii_lowercase()
                ))
                .font(ui_font(11.5))
                .color(muted()),
            );
            ui.label(RichText::new("Shares that already filled stay filled.").font(ui_font(11.5)).color(muted()));
            if let Some(e) = &c.error {
                ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.add_enabled(!c.busy, egui::Button::new("Keep Order")).clicked() {
                    choice = Some(false);
                }
                let b =
                    egui::Button::new(RichText::new("Cancel Order").strong().color(Color32::WHITE)).fill(theme::down());
                if ui.add_enabled(!c.busy, b).clicked() {
                    choice = Some(true);
                }
                if c.busy {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        });
        if choice.is_none() && !c.busy && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.confirm_cancel(),
            Some(false) => self.trading.cancel = None,
            None => {}
        }
    }

    fn enable_dialog(&mut self, ctx: &egui::Context) {
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("trading-enable")).show(ctx, |ui| {
            ui.set_width(dp(420.0));
            ui.label(RichText::new("Enable live trading?").font(ui_font(16.0)).strong());
            ui.add_space(dp(4.0));
            for line in [
                "Orders you confirm are sent to your real brokerage account through SnapTrade and can execute immediately with real money.",
                "Market orders fill at the prevailing price, which can differ from the preview. Quotes in terminal-rs may be delayed.",
                "terminal-rs is provided as is, without warranty. You are responsible for every order you place.",
                "Your brokerage connection must allow trading (reconnect with trading access if it was connected read-only).",
            ] {
                ui.label(RichText::new(format!("• {line}")).font(ui_font(12.0)).color(text()));
            }
            ui.add_space(dp(6.0));
            ui.checkbox(&mut self.trading.ack, "I understand that orders are real and I am responsible for them");
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Keep Trading Off").clicked() {
                    choice = Some(false);
                }
                if ui.add_enabled(self.trading.ack, filled("Enable Trading", theme::down(), self.trading.ack)).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        if let Some(on) = choice {
            self.trading.enable_prompt = false;
            self.trading.ack = false;
            if on {
                self.prefs.trading.enabled = true;
                self.save_prefs_now();
                self.trading.message = Some(("Trading enabled.".into(), false));
                self.status = "Trading enabled (Settings › Trading)".into();
            }
        }
    }

    /// Settings › Brokerage access line, which depends on whether trading is enabled.
    pub(super) fn brokerage_access_note(&self) -> String {
        if self.trading_enabled() {
            "Trading is enabled (Settings › Trading): confirmed orders are sent to your broker. Connect Brokerage \
             requests read-only access; use Reconnect with Trading Access in Settings › Trading for brokers that trade."
                .into()
        } else {
            "Read-only: terminal-rs reads accounts, balances, positions, orders and activity. Trading is off \
             (Settings › Trading), and the Connection Portal requests read-only access."
                .into()
        }
    }

    fn save_prefs_now(&mut self) {
        if let Err(e) = self.prefs.save(&super::prefs_path()) {
            self.status = format!("Could not save preferences: {e}");
        }
    }

    // ---- settings -------------------------------------------------------------------------

    /// Settings › Trading.
    pub(super) fn trading_tab(&mut self, ui: &mut Ui, cmds: &mut Vec<SettingsCmd>) {
        let section = |ui: &mut Ui, title: &str| {
            ui.add_space(dp(10.0));
            ui.label(RichText::new(title).font(ui_font(13.5)).color(text()));
            ui.separator();
        };
        let note = |ui: &mut Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };

        section(ui, "Live trading (SnapTrade)");
        let on = self.prefs.trading.enabled;
        ui.label(
            RichText::new(if on {
                "Trading is ON: confirmed orders are sent to your broker."
            } else {
                "Trading is OFF."
            })
            .font(ui_font(12.5))
            .color(if on { theme::down() } else { theme::accent() }),
        );
        note(
            ui,
            "When on, BUY / SELL / TRADE and the Trade actions on quotes and positions open an order ticket. Every order is checked by your broker through SnapTrade (preview with estimated cost and fees) and needs a final confirmation. Open orders can be cancelled from ORD.",
        );
        ui.horizontal(|ui| {
            if on {
                if ui.button("Turn Trading Off").clicked() {
                    self.prefs.trading.enabled = false;
                    self.trading.message = Some(("Trading disabled.".into(), false));
                    if let Some(t) = &mut self.trading.ticket {
                        t.confirming = false;
                    }
                    cmds.push(SettingsCmd::SavePrefs);
                }
            } else if ui.button(RichText::new("Enable Trading…").strong()).clicked() {
                self.trading.enable_prompt = true;
                self.trading.ack = false;
            }
        });

        section(ui, "Safety");
        ui.horizontal(|ui| {
            let mut limited = self.prefs.trading.max_order_value.is_some();
            if ui.checkbox(&mut limited, "Maximum order value").changed() {
                self.prefs.trading.max_order_value = limited.then_some(5_000.0);
                cmds.push(SettingsCmd::SavePrefs);
            }
            if let Some(v) = &mut self.prefs.trading.max_order_value {
                let r = ui.add(egui::DragValue::new(v).range(1.0..=10_000_000.0).speed(50.0).prefix("$"));
                if r.changed() {
                    cmds.push(SettingsCmd::SavePrefs);
                }
            }
        });
        note(ui, "Orders whose estimated value is above the limit are refused before they are sent.");
        let accounts = self.trade_accounts();
        if !accounts.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Default account").font(ui_font(12.5)));
                let current = self
                    .prefs
                    .trading
                    .default_account
                    .as_ref()
                    .and_then(|id| accounts.iter().find(|a| &a.id == id))
                    .map_or("Ask each time".to_string(), account_line);
                let before = self.prefs.trading.default_account.clone();
                egui::ComboBox::from_id_salt("trading-default-account").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.prefs.trading.default_account, None, "Ask each time");
                    for a in &accounts {
                        ui.selectable_value(
                            &mut self.prefs.trading.default_account,
                            Some(a.id.clone()),
                            account_line(a),
                        );
                    }
                });
                if self.prefs.trading.default_account != before {
                    cmds.push(SettingsCmd::SavePrefs);
                }
            });
        }

        section(ui, "Brokerage access");
        if self.broker.mode() == BrokerMode::Live {
            self.ensure_capabilities(false);
        }
        let mut by_broker: std::collections::BTreeMap<String, (usize, usize, Option<String>)> = Default::default();
        for a in &accounts {
            if let Some(c) = self.capability(&a.id) {
                let e = by_broker.entry(c.brokerage.clone()).or_default();
                e.0 += 1;
                e.1 += usize::from(c.can_trade());
                e.2 = c.blocker();
            }
        }
        for (broker, (n, ok, why)) in &by_broker {
            let (line, color) = if *ok == *n {
                (format!("{broker}: trading available on {n} account{}", if *n == 1 { "" } else { "s" }), theme::up())
            } else {
                (format!("{broker}: {}", why.clone().unwrap_or_else(|| "read-only".into())), amber())
            };
            ui.label(RichText::new(line).font(ui_font(12.0)).color(color));
        }
        if let Some(e) = &self.trading.caps_error {
            ui.label(
                RichText::new(format!("Could not read trading access: {e}")).font(ui_font(11.5)).color(theme::down()),
            );
        }
        note(
            ui,
            "Connections made for portfolio tracking are read-only. To trade, reconnect the broker through SnapTrade's Connection Portal with trading access (supported brokers only).",
        );
        ui.horizontal(|ui| {
            let live = self.broker.mode() == BrokerMode::Live;
            if ui
                .add_enabled(live && !self.trading.portal_busy, egui::Button::new("Reconnect with Trading Access…"))
                .on_disabled_hover_text("Set up SnapTrade in Settings › Brokerage first")
                .clicked()
            {
                self.trading.portal_busy = true;
                self.trading.message = None;
                let dark = self.applied_theme.as_ref().is_none_or(|t| t.dark);
                self.worker.send(Request::Trade(TradeRequest::Portal { creds: self.broker.creds.clone(), dark }));
            }
            if self.trading.portal_busy {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
        if let Some((m, err)) = &self.trading.message {
            ui.label(RichText::new(m).font(ui_font(12.0)).color(if *err { theme::down() } else { theme::up() }));
        }

        section(ui, "Using it");
        note(ui, "BUY AAPL 10 · SELL MSFT 5 LMT 410 · BUY NVDA $500 · SELL TSLA 3 STP 250 LMT 245 · TRADE · AAPL BUY");
        note(
            ui,
            "Right-click a watchlist quote or a position for Buy / Sell; right-click an open order in ORD to cancel it.",
        );
    }
}

enum TicketAct {
    Preview,
    Review,
    Settings,
}

/// A coloured action button, plain when disabled so it does not look clickable.
fn filled(label: &str, fill: Color32, enabled: bool) -> egui::Button<'static> {
    if enabled {
        egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(fill)
    } else {
        egui::Button::new(RichText::new(label).strong())
    }
}

/// `Individual · Robinhood ••••8443`.
fn account_line(a: &Account) -> String {
    let mut s = format!("{} · {} {}", a.label(), a.institution, a.masked_number());
    if a.is_paper {
        s += " · paper";
    }
    s
}

fn preview_lines(ui: &mut Ui, p: &OrderPreview, quote: Option<f64>, now: chrono::DateTime<chrono::Utc>) {
    let r = &p.request;
    let row = |ui: &mut Ui, k: &str, v: String| {
        ui.horizontal(|ui| {
            ui.add_sized([dp(110.0), dp(16.0)], egui::Label::new(RichText::new(k).font(ui_font(11.5)).color(muted())));
            ui.label(RichText::new(v).font(mono(12.0)).color(text()));
        });
    };
    let title = if p.simulated { "Simulated preview" } else { "Broker preview" };
    ui.label(RichText::new(title).font(ui_font(12.0)).color(theme::accent()));
    if !p.symbol.description.is_empty() {
        row(ui, "Security", format!("{} {}", p.symbol.symbol, p.symbol.description));
    }
    if let Some(px) = p.price {
        row(ui, "Price", price(px));
    }
    if let Some(u) = p.units.filter(|_| matches!(r.amount, Amount::Notional(_))) {
        row(ui, "Est. shares", qty(u));
    }
    let label = if r.side == Side::Buy { "Est. cost" } else { "Est. proceeds" };
    row(ui, label, p.estimated_total(quote).map_or("at market".into(), |v| money(v, &p.currency)));
    if let Some(c) = p.commission {
        row(ui, "Commission", money(c, &p.currency));
    }
    if let Some(f) = p.forex_fees.filter(|f| *f > 0.0) {
        row(ui, "FX fees", money(f, &p.currency));
    }
    if let Some(c) = p.remaining_cash {
        row(ui, "Cash after", money(c, &p.currency));
    }
    if p.expired(now) {
        ui.label(RichText::new("Preview expired: preview again.").font(ui_font(11.5)).color(amber()));
    } else {
        let left = (p.expires() - now).num_seconds().max(0);
        ui.label(RichText::new(format!("Valid for {}:{:02}", left / 60, left % 60)).font(ui_font(11.0)).color(muted()));
    }
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
    fn read_only_errors_point_to_reconnecting() {
        assert!(explain("SnapTrade: User does not have permissions to place trades (403)").contains("Reconnect"));
        assert_eq!(explain("SnapTrade: Insufficient buying power"), "SnapTrade: Insufficient buying power");
    }
}
