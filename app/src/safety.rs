//! Order-entry safety rails, as pure logic: pre-trade checks against user-set limits, the
//! kill switch state machine, and the append-only audit log (JSONL, one file per month).
//!
//! The app glue (`app/safety_view.rs`) gathers the facts about an order and calls
//! [`assess`]; every order path — demo and live — goes through that one function and records
//! its outcome with [`append`].
//!
//! Each finding is a [`Verdict`]: `Warn` needs an explicit confirmation from the user, `Block`
//! refuses the order. The defaults are meant to stay out of the way of ordinary trading: the
//! hard limits are far above a typical retail order and everything judgement-based only warns.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tr_broker::trading::{OrderType, Side};

use crate::mktcal::{MarketStatus, Phase};

/// Two orders for the same thing this close together are probably a double click.
pub const DUPLICATE_WINDOW: Duration = Duration::seconds(5);
/// Orders remembered for the fat-finger average and the daily count.
const LEDGER_CAP: usize = 200;
/// The fat-finger check needs at least this many earlier orders to know what is usual.
const FAT_FINGER_MIN_HISTORY: usize = 3;
/// …and averages over at most this many of the latest ones.
const FAT_FINGER_WINDOW: usize = 20;

// ---- limits -----------------------------------------------------------------------------

/// User-configurable limits. `None` switches a check off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Block an order whose estimated value exceeds this (in the account currency).
    pub max_order_notional: Option<f64>,
    /// Block an order for more than this many shares or contracts.
    pub max_order_qty: Option<f64>,
    /// Warn when a buy would take one position above this share of the portfolio value.
    pub max_position_pct: Option<f64>,
    /// Block new orders after this many in the last 24 hours.
    pub max_daily_orders: Option<u32>,
    /// Block a limit price further than this many percent from the last price.
    pub price_collar_pct: Option<f64>,
    /// Warn when an order is more than this many times the average of recent orders.
    pub fat_finger_mult: Option<f64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_order_notional: Some(100_000.0),
            max_order_qty: Some(10_000.0),
            max_position_pct: Some(40.0),
            max_daily_orders: Some(100),
            price_collar_pct: Some(10.0),
            fat_finger_mult: Some(5.0),
        }
    }
}

// ---- kill switch ------------------------------------------------------------------------

/// The kill switch: while on, no new order is accepted (cancels still are).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kill {
    pub on: bool,
    pub since: Option<DateTime<Utc>>,
    /// Also cancel every open order when it is switched on.
    pub cancel_open: bool,
}

/// What a kill-switch command did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillChange {
    /// It was switched on; the caller should cancel open orders when `cancel_open`.
    Engaged {
        cancel_open: bool,
    },
    Released,
    /// Already in the requested state.
    Unchanged,
}

impl Kill {
    pub fn engage(&mut self, now: DateTime<Utc>, cancel_open: bool) -> KillChange {
        if self.on {
            return KillChange::Unchanged;
        }
        self.on = true;
        self.since = Some(now);
        KillChange::Engaged { cancel_open }
    }

    pub fn release(&mut self) -> KillChange {
        if !self.on {
            return KillChange::Unchanged;
        }
        self.on = false;
        self.since = None;
        KillChange::Released
    }
}

// ---- persisted settings ----------------------------------------------------------------

/// `<data dir>/safety.json`: limits and the kill switch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyFile {
    pub limits: Limits,
    pub kill: Kill,
}

pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join("safety.json")
}

/// Load the saved settings. A missing file gives the defaults. A damaged file also gives the
/// defaults, except that the kill switch then comes up **on**: when in doubt, no orders.
pub fn load_settings(dir: &Path) -> (SafetyFile, Option<String>) {
    let path = settings_path(dir);
    match std::fs::read(&path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(f) => (f, None),
            Err(e) => {
                let mut f = SafetyFile::default();
                f.kill.on = true;
                f.kill.since = Some(Utc::now());
                (
                    f,
                    Some(format!(
                        "{} could not be read ({e}); the kill switch is ON until you check it.",
                        path.display()
                    )),
                )
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (SafetyFile::default(), None),
        Err(e) => {
            let mut f = SafetyFile::default();
            f.kill.on = true;
            f.kill.since = Some(Utc::now());
            (f, Some(format!("{} could not be read ({e}); the kill switch is ON until you check it.", path.display())))
        }
    }
}

pub fn save_settings(dir: &Path, f: &SafetyFile) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("safety.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(f).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, settings_path(dir))
}

// ---- recent orders ----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Recent {
    pub at: DateTime<Utc>,
    pub symbol: String,
    pub side: Side,
    /// Size as entered, for the duplicate guard.
    pub size_key: String,
    /// Estimated value, for the fat-finger average.
    pub notional: Option<f64>,
}

/// Orders that went through recently (kept in memory, rebuilt from the audit log at start).
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    pub orders: VecDeque<Recent>,
}

impl Ledger {
    pub fn push(&mut self, r: Recent) {
        self.orders.push_back(r);
        while self.orders.len() > LEDGER_CAP {
            self.orders.pop_front();
        }
    }

    fn last_24h(&self, now: DateTime<Utc>) -> usize {
        self.orders.iter().filter(|o| o.at > now - Duration::hours(24) && o.at <= now).count()
    }
}

// ---- pre-trade checks -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Warn,
    Block,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub check: &'static str,
    pub verdict: Verdict,
    pub reason: String,
}

/// What the checks need to know about one order.
#[derive(Debug, Clone)]
pub struct Facts {
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    /// Shares or contracts (estimated for dollar orders).
    pub units: Option<f64>,
    /// Estimated value, positive.
    pub notional: Option<f64>,
    /// The size as entered (`10`, `$500`), used to spot duplicates.
    pub size_key: String,
    pub limit: Option<f64>,
    pub last: Option<f64>,
    pub held_units: f64,
    /// Market value of the current position in this symbol.
    pub position_value: Option<f64>,
    pub portfolio_value: Option<f64>,
    pub buying_power: Option<f64>,
    /// The account may be short (live margin accounts); the demo account may not.
    pub allow_short: bool,
    pub option: bool,
}

/// The result of all checks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Assessment {
    /// Only warnings and blocks; an empty list is a pass.
    pub findings: Vec<Finding>,
}

impl Assessment {
    pub fn verdict(&self) -> Verdict {
        self.findings.iter().map(|f| f.verdict).max().unwrap_or(Verdict::Pass)
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.verdict == Verdict::Block)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.verdict == Verdict::Warn)
    }

    /// All block reasons in one line.
    pub fn block_text(&self) -> String {
        self.blocks().map(|f| f.reason.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// `check: reason` strings for the audit log.
    pub fn summary(&self) -> Vec<String> {
        self.findings
            .iter()
            .map(|f| {
                format!("{} {}: {}", if f.verdict == Verdict::Block { "BLOCK" } else { "WARN" }, f.check, f.reason)
            })
            .collect()
    }
}

fn money(x: f64) -> String {
    let s = format!("{:.0}", x.abs());
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("${}", out.chars().rev().collect::<String>())
}

fn qty_text(x: f64) -> String {
    let s = format!("{x:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Run every check. The kill switch comes first and, being a block, makes the rest moot, but
/// all findings are still returned so the user sees everything wrong at once.
pub fn assess(
    limits: &Limits,
    kill: &Kill,
    ledger: &Ledger,
    f: &Facts,
    market: Option<&MarketStatus>,
    now: DateTime<Utc>,
) -> Assessment {
    let mut out = Vec::new();
    let mut add = |check: &'static str, verdict, reason: String| out.push(Finding { check, verdict, reason });

    if kill.on {
        add(
            "kill switch",
            Verdict::Block,
            "Kill switch is ON: new orders are blocked (command KILL OFF releases it).".into(),
        );
    }
    if f.option {
        add("instrument", Verdict::Block, "Option orders are not supported by the order ticket.".into());
    }

    if let (Some(max), Some(n)) = (limits.max_order_notional, f.notional) {
        if n > max {
            add(
                "max order value",
                Verdict::Block,
                format!("Order value about {} is over your {} limit (Settings › Safety).", money(n), money(max)),
            );
        }
    } else if limits.max_order_notional.is_some() && f.notional.is_none() {
        add(
            "max order value",
            Verdict::Warn,
            "Cannot estimate this order's value, so the order value limit was not checked.".into(),
        );
    }

    if let (Some(max), Some(u)) = (limits.max_order_qty, f.units)
        && u > max
    {
        add(
            "max quantity",
            Verdict::Block,
            format!("{} units is over your {} per-order limit (Settings › Safety).", qty_text(u), qty_text(max)),
        );
    }

    if let (Some(max), Side::Buy, Some(n), Some(pv)) = (limits.max_position_pct, f.side, f.notional, f.portfolio_value)
        && pv > 0.0
    {
        let after = f.position_value.unwrap_or(0.0).max(0.0) + n;
        let pct = after / pv * 100.0;
        if pct > max {
            add(
                "position size",
                Verdict::Warn,
                format!(
                    "{} would be {pct:.0}% of the portfolio ({} of {}), over your {max:.0}% limit.",
                    f.symbol,
                    money(after),
                    money(pv)
                ),
            );
        }
    }

    if let Some(max) = limits.max_daily_orders {
        let n = ledger.last_24h(now);
        if n >= max as usize {
            add(
                "daily order count",
                Verdict::Block,
                format!("{n} orders in the last 24 hours reached your limit of {max} (Settings › Safety)."),
            );
        }
    }

    if let (Some(pct), Some(limit), Some(last)) = (limits.price_collar_pct, f.limit, f.last)
        && matches!(f.order_type, OrderType::Limit | OrderType::StopLimit)
        && last > 0.0
    {
        let away = (limit - last).abs() / last * 100.0;
        if away > pct {
            add(
                "price collar",
                Verdict::Block,
                format!(
                    "Limit price {limit:.2} is {away:.1}% away from the last price {last:.2}, more than your {pct:.0}% collar. Check for a typo."
                ),
            );
        }
    }

    if let (Some(mult), Some(n)) = (limits.fat_finger_mult, f.notional) {
        let prior: Vec<f64> = ledger.orders.iter().rev().filter_map(|o| o.notional).take(FAT_FINGER_WINDOW).collect();
        if prior.len() >= FAT_FINGER_MIN_HISTORY {
            let avg = prior.iter().sum::<f64>() / prior.len() as f64;
            if avg > 0.0 && n > mult * avg {
                add(
                    "fat finger",
                    Verdict::Warn,
                    format!(
                        "This order (about {}) is {:.1}x your recent average of {}.",
                        money(n),
                        n / avg,
                        money(avg)
                    ),
                );
            }
        }
    }

    if ledger.orders.iter().any(|o| {
        o.symbol.eq_ignore_ascii_case(&f.symbol)
            && o.side == f.side
            && o.size_key == f.size_key
            && now - o.at < DUPLICATE_WINDOW
            && o.at <= now
    }) {
        add(
            "duplicate",
            Verdict::Warn,
            format!("The same order ({} {} {}) was sent less than 5 seconds ago.", f.side.api(), f.size_key, f.symbol),
        );
    }

    if let (Side::Buy, Some(n), Some(bp)) = (f.side, f.notional, f.buying_power)
        && n > bp
    {
        add(
            "buying power",
            Verdict::Block,
            format!("Not enough buying power: this costs about {} and you have {}.", money(n), money(bp)),
        );
    }

    if f.side == Side::Sell
        && let Some(u) = f.units
        && u > f.held_units + 1e-9
    {
        if f.allow_short {
            add(
                "short sale",
                Verdict::Warn,
                format!("You hold {} {}; this sells short.", qty_text(f.held_units.max(0.0)), f.symbol),
            );
        } else {
            add(
                "short sale",
                Verdict::Block,
                format!("You hold {} {}; this account cannot sell short.", qty_text(f.held_units.max(0.0)), f.symbol),
            );
        }
    }

    if let Some(m) = market
        && m.phase != Phase::Regular
    {
        let what = match m.phase {
            Phase::Pre => "pre-market",
            Phase::After => "after-hours",
            _ => "closed",
        };
        add(
            "market hours",
            Verdict::Warn,
            format!(
                "{} A market order may wait or fill at an unexpected price ({what}).",
                m.note.trim_end_matches('.').to_string() + "."
            ),
        );
    }

    Assessment { findings: out }
}

// ---- audit log --------------------------------------------------------------------------

/// One line of the audit log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub at: DateTime<Utc>,
    /// `order`, `kill` or `limits`.
    pub kind: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub side: String,
    /// Size as entered (`10`, `$500`).
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub order_type: String,
    #[serde(default)]
    pub price: Option<f64>,
    /// Estimated order value.
    #[serde(default)]
    pub value: Option<f64>,
    /// Findings of the pre-trade checks (empty = all passed).
    #[serde(default)]
    pub checks: Vec<String>,
    /// The user confirmed warnings (or the live confirmation dialog) before sending.
    #[serde(default)]
    pub confirmed: bool,
    /// `placed`, `rejected`, `blocked`, `unknown`, `not_sent`, `kill_on`, `kill_off`,
    /// `limits_changed`.
    pub outcome: String,
    #[serde(default)]
    pub detail: String,
}

impl AuditRecord {
    pub fn event(at: DateTime<Utc>, kind: &str, outcome: &str, detail: impl Into<String>) -> Self {
        Self {
            at,
            kind: kind.into(),
            account: String::new(),
            symbol: String::new(),
            side: String::new(),
            qty: String::new(),
            order_type: String::new(),
            price: None,
            value: None,
            checks: Vec::new(),
            confirmed: false,
            outcome: outcome.into(),
            detail: detail.into(),
        }
    }

    /// Counts toward the daily total and the fat-finger average.
    pub fn went_through(&self) -> bool {
        self.kind == "order" && matches!(self.outcome.as_str(), "placed" | "unknown")
    }
}

pub fn audit_dir(dir: &Path) -> PathBuf {
    dir.join("audit")
}

fn month_file(dir: &Path, at: DateTime<Utc>) -> PathBuf {
    audit_dir(dir).join(format!("audit-{}.jsonl", at.format("%Y-%m")))
}

/// Append one record to its month's file (created on first use; never rewritten).
pub fn append(dir: &Path, rec: &AuditRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(audit_dir(dir))?;
    let mut line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(month_file(dir, rec.at))?;
    f.write_all(line.as_bytes())
}

/// Every record on disk, oldest first, and the number of unreadable lines skipped.
pub fn read_all(dir: &Path) -> (Vec<AuditRecord>, usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(audit_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    let mut bad = 0;
    for p in files {
        let Ok(text) = std::fs::read_to_string(&p) else {
            bad += 1;
            continue;
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<AuditRecord>(line) {
                Ok(r) => out.push(r),
                Err(_) => bad += 1,
            }
        }
    }
    out.sort_by_key(|r| r.at);
    (out, bad)
}

/// A ledger rebuilt from the orders that went through in the last 24 hours.
pub fn ledger_from(records: &[AuditRecord], now: DateTime<Utc>) -> Ledger {
    let mut l = Ledger::default();
    for r in records.iter().filter(|r| r.went_through() && r.at > now - Duration::hours(24) && r.at <= now) {
        l.push(Recent {
            at: r.at,
            symbol: r.symbol.clone(),
            side: if r.side.eq_ignore_ascii_case("sell") { Side::Sell } else { Side::Buy },
            size_key: r.qty.clone(),
            notional: r.value,
        });
    }
    l
}

fn csv_field(s: &str) -> String {
    // Also neutralise spreadsheet formulas.
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

/// The records as CSV, in the order given.
pub fn to_csv(records: &[&AuditRecord]) -> String {
    let mut s =
        String::from("time_utc,kind,account,symbol,side,qty,type,price,value,checks,confirmed,outcome,detail\n");
    for r in records {
        let row = [
            r.at.format("%Y-%m-%d %H:%M:%S").to_string(),
            r.kind.clone(),
            r.account.clone(),
            r.symbol.clone(),
            r.side.clone(),
            r.qty.clone(),
            r.order_type.clone(),
            r.price.map_or(String::new(), |p| format!("{p}")),
            r.value.map_or(String::new(), |v| format!("{v:.2}")),
            r.checks.join(" | "),
            r.confirmed.to_string(),
            r.outcome.clone(),
            r.detail.clone(),
        ];
        s.push_str(&row.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        s.push('\n');
    }
    s
}

/// Filter by symbol (substring, case-insensitive) and outcome (exact; empty = all).
pub fn matches(r: &AuditRecord, symbol: &str, outcome: &str) -> bool {
    let sym = symbol.trim();
    (sym.is_empty() || r.symbol.to_ascii_uppercase().contains(&sym.to_ascii_uppercase()))
        && (outcome.is_empty() || r.outcome == outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mktcal::{Exchange, status};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        // Wednesday 14 Oct 2026, 11:00 ET: the NYSE is open.
        Utc.with_ymd_and_hms(2026, 10, 14, 15, 0, 0).unwrap()
    }

    fn facts() -> Facts {
        Facts {
            symbol: "AAPL".into(),
            side: Side::Buy,
            order_type: OrderType::Market,
            units: Some(10.0),
            notional: Some(2_000.0),
            size_key: "10".into(),
            limit: None,
            last: Some(200.0),
            held_units: 0.0,
            position_value: None,
            portfolio_value: Some(100_000.0),
            buying_power: Some(50_000.0),
            allow_short: false,
            option: false,
        }
    }

    fn run(l: &Limits, k: &Kill, led: &Ledger, f: &Facts) -> Assessment {
        assess(l, k, led, f, Some(&status(Exchange::Nyse, now())), now())
    }

    fn checks(a: &Assessment) -> Vec<&'static str> {
        a.findings.iter().map(|f| f.check).collect()
    }

    fn recent(secs_ago: i64, notional: f64, key: &str) -> Recent {
        Recent {
            at: now() - Duration::seconds(secs_ago),
            symbol: "AAPL".into(),
            side: Side::Buy,
            size_key: key.into(),
            notional: Some(notional),
        }
    }

    #[test]
    fn ordinary_order_passes() {
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &facts());
        assert_eq!(a.verdict(), Verdict::Pass);
        assert!(a.findings.is_empty());
    }

    #[test]
    fn kill_switch_blocks_everything() {
        let mut k = Kill::default();
        k.engage(now(), false);
        let a = run(&Limits::default(), &k, &Ledger::default(), &facts());
        assert_eq!(a.verdict(), Verdict::Block);
        assert!(a.block_text().contains("Kill switch"));
    }

    #[test]
    fn kill_state_machine() {
        let mut k = Kill::default();
        assert_eq!(k.release(), KillChange::Unchanged);
        assert_eq!(k.engage(now(), true), KillChange::Engaged { cancel_open: true });
        assert_eq!(k.since, Some(now()));
        assert_eq!(k.engage(now(), true), KillChange::Unchanged);
        assert_eq!(k.release(), KillChange::Released);
        assert!(!k.on && k.since.is_none());
    }

    #[test]
    fn notional_and_quantity_limits_block() {
        let mut f = facts();
        f.notional = Some(150_000.0);
        f.buying_power = None;
        f.portfolio_value = None;
        assert_eq!(checks(&run(&Limits::default(), &Kill::default(), &Ledger::default(), &f)), ["max order value"]);
        f.notional = Some(1_000.0);
        f.units = Some(20_000.0);
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(checks(&a), ["max quantity"]);
        assert_eq!(a.verdict(), Verdict::Block);
    }

    #[test]
    fn limits_can_be_switched_off() {
        let off = Limits {
            max_order_notional: None,
            max_order_qty: None,
            max_position_pct: None,
            max_daily_orders: None,
            price_collar_pct: None,
            fat_finger_mult: None,
        };
        let mut f = facts();
        f.notional = Some(9e9);
        f.units = Some(9e9);
        f.buying_power = None;
        assert!(run(&off, &Kill::default(), &Ledger::default(), &f).findings.is_empty());
    }

    #[test]
    fn position_size_warns() {
        let mut f = facts();
        f.notional = Some(30_000.0);
        f.position_value = Some(20_000.0);
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(checks(&a), ["position size"]);
        assert_eq!(a.verdict(), Verdict::Warn);
        // Selling never trips it.
        f.side = Side::Sell;
        f.held_units = 1000.0;
        assert!(run(&Limits::default(), &Kill::default(), &Ledger::default(), &f).findings.is_empty());
    }

    #[test]
    fn daily_order_count() {
        let l = Limits { max_daily_orders: Some(3), ..Limits::default() };
        let mut led = Ledger::default();
        for i in 0..3 {
            led.push(Recent { symbol: format!("S{i}"), ..recent(3600 * (i + 1), 1000.0, "1") });
        }
        assert_eq!(checks(&run(&l, &Kill::default(), &led, &facts())), ["daily order count"]);
        // Older than 24 hours does not count.
        led.orders[0].at = now() - Duration::hours(25);
        assert!(run(&l, &Kill::default(), &led, &facts()).findings.is_empty());
    }

    #[test]
    fn price_collar() {
        let mut f = facts();
        f.order_type = OrderType::Limit;
        f.limit = Some(240.0);
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert!(checks(&a).contains(&"price collar"), "{:?}", a);
        assert_eq!(a.verdict(), Verdict::Block);
        f.limit = Some(205.0);
        assert!(!checks(&run(&Limits::default(), &Kill::default(), &Ledger::default(), &f)).contains(&"price collar"));
        // A market order has no limit price to check.
        f.order_type = OrderType::Market;
        f.limit = Some(1.0);
        assert!(run(&Limits::default(), &Kill::default(), &Ledger::default(), &f).findings.is_empty());
    }

    #[test]
    fn fat_finger_needs_history_then_warns() {
        let mut f = facts();
        f.notional = Some(20_000.0);
        f.size_key = "100".into();
        let mut led = Ledger::default();
        led.push(recent(600, 1_000.0, "5"));
        led.push(recent(500, 1_000.0, "5"));
        // Two prior orders are too little to know what is usual.
        assert!(run(&Limits::default(), &Kill::default(), &led, &f).findings.is_empty());
        led.push(recent(400, 1_000.0, "5"));
        let a = run(&Limits::default(), &Kill::default(), &led, &f);
        assert_eq!(checks(&a), ["fat finger"]);
        assert_eq!(a.verdict(), Verdict::Warn);
        f.notional = Some(4_000.0);
        assert!(run(&Limits::default(), &Kill::default(), &led, &f).findings.is_empty());
    }

    #[test]
    fn duplicate_within_five_seconds() {
        let mut led = Ledger::default();
        led.push(recent(3, 2_000.0, "10"));
        let a = run(&Limits::default(), &Kill::default(), &led, &facts());
        assert_eq!(checks(&a), ["duplicate"]);
        assert_eq!(a.verdict(), Verdict::Warn);
        led.orders[0].at = now() - Duration::seconds(6);
        assert!(run(&Limits::default(), &Kill::default(), &led, &facts()).findings.is_empty());
        // Different size or side is a different order.
        led.orders[0].at = now() - Duration::seconds(1);
        led.orders[0].size_key = "11".into();
        assert!(run(&Limits::default(), &Kill::default(), &led, &facts()).findings.is_empty());
    }

    #[test]
    fn buying_power_blocks_buys_only() {
        let mut f = facts();
        f.buying_power = Some(500.0);
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(checks(&a), ["buying power"]);
        assert!(a.block_text().contains("Not enough buying power"));
        f.side = Side::Sell;
        f.held_units = 10.0;
        assert!(run(&Limits::default(), &Kill::default(), &Ledger::default(), &f).findings.is_empty());
    }

    #[test]
    fn short_rules() {
        let mut f = facts();
        f.side = Side::Sell;
        f.buying_power = None;
        f.held_units = 4.0;
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(a.verdict(), Verdict::Block);
        f.allow_short = true;
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(checks(&a), ["short sale"]);
        assert_eq!(a.verdict(), Verdict::Warn);
    }

    #[test]
    fn option_orders_are_refused() {
        let mut f = facts();
        f.option = true;
        assert_eq!(run(&Limits::default(), &Kill::default(), &Ledger::default(), &f).verdict(), Verdict::Block);
    }

    #[test]
    fn closed_market_warns() {
        let sat = Utc.with_ymd_and_hms(2026, 10, 17, 15, 0, 0).unwrap();
        let a = assess(
            &Limits::default(),
            &Kill::default(),
            &Ledger::default(),
            &facts(),
            Some(&status(Exchange::Nyse, sat)),
            sat,
        );
        assert_eq!(checks(&a), ["market hours"]);
        assert_eq!(a.verdict(), Verdict::Warn);
        assert!(a.findings[0].reason.contains("weekend"));
        let after = Utc.with_ymd_and_hms(2026, 10, 14, 21, 0, 0).unwrap();
        let a = assess(
            &Limits::default(),
            &Kill::default(),
            &Ledger::default(),
            &facts(),
            Some(&status(Exchange::Nyse, after)),
            after,
        );
        assert!(a.findings[0].reason.contains("after-hours"));
    }

    #[test]
    fn unknown_value_warns_instead_of_guessing() {
        let mut f = facts();
        f.notional = None;
        f.buying_power = None;
        let a = run(&Limits::default(), &Kill::default(), &Ledger::default(), &f);
        assert_eq!(a.verdict(), Verdict::Warn);
    }

    #[test]
    fn worst_verdict_wins() {
        let mut f = facts();
        f.notional = Some(150_000.0);
        f.buying_power = None;
        let sat = Utc.with_ymd_and_hms(2026, 10, 17, 15, 0, 0).unwrap();
        let a = assess(
            &Limits::default(),
            &Kill::default(),
            &Ledger::default(),
            &f,
            Some(&status(Exchange::Nyse, sat)),
            sat,
        );
        assert_eq!(a.verdict(), Verdict::Block);
        assert!(a.warnings().count() >= 1 && a.blocks().count() == 1);
        assert_eq!(a.summary().len(), a.findings.len());
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tr-safety-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn settings_round_trip_and_damage() {
        let dir = tmp("settings");
        let (f, note) = load_settings(&dir);
        assert!(note.is_none() && !f.kill.on && f.limits == Limits::default());
        let mut f = f;
        f.kill.engage(now(), true);
        f.limits.max_order_notional = None;
        f.limits.max_daily_orders = Some(7);
        save_settings(&dir, &f).unwrap();
        let (g, note) = load_settings(&dir);
        assert!(note.is_none());
        assert_eq!(g, f);
        std::fs::write(settings_path(&dir), b"{ not json").unwrap();
        let (h, note) = load_settings(&dir);
        assert!(h.kill.on, "a damaged file must fail closed");
        assert!(note.unwrap().contains("kill switch is ON"));
        // Missing fields get defaults.
        std::fs::write(settings_path(&dir), br#"{"kill":{"on":true}}"#).unwrap();
        let (i, _) = load_settings(&dir);
        assert!(i.kill.on && i.limits == Limits::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn order_rec(at: DateTime<Utc>, sym: &str, outcome: &str) -> AuditRecord {
        AuditRecord {
            account: "demo-paper".into(),
            symbol: sym.into(),
            side: "BUY".into(),
            qty: "10".into(),
            order_type: "Market".into(),
            checks: vec!["WARN market hours: closed".into()],
            confirmed: true,
            value: Some(2000.5),
            ..AuditRecord::event(at, "order", outcome, "")
        }
    }

    #[test]
    fn audit_appends_per_month_and_reads_back_in_order() {
        let dir = tmp("audit");
        let jan = Utc.with_ymd_and_hms(2026, 1, 31, 23, 59, 0).unwrap();
        let feb = Utc.with_ymd_and_hms(2026, 2, 1, 0, 1, 0).unwrap();
        append(&dir, &order_rec(feb, "MSFT", "placed")).unwrap();
        append(&dir, &order_rec(jan, "AAPL", "blocked")).unwrap();
        append(&dir, &AuditRecord::event(feb, "kill", "kill_on", "KILL command")).unwrap();
        let files: Vec<_> = std::fs::read_dir(audit_dir(&dir))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(
            files.contains(&"audit-2026-01.jsonl".to_string()) && files.contains(&"audit-2026-02.jsonl".to_string())
        );
        let (recs, bad) = read_all(&dir);
        assert_eq!(bad, 0);
        assert_eq!(recs.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(), ["AAPL", "MSFT", ""]);
        assert_eq!(recs[0].outcome, "blocked");
        // Appending never rewrites earlier lines.
        append(&dir, &order_rec(feb, "NVDA", "rejected")).unwrap();
        let text = std::fs::read_to_string(audit_dir(&dir).join("audit-2026-02.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().next().unwrap().contains("MSFT"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reader_skips_damaged_lines() {
        let dir = tmp("damaged");
        append(&dir, &order_rec(now(), "AAPL", "placed")).unwrap();
        let file = audit_dir(&dir).join("audit-2026-10.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"garbage\n\n{\"at\":1}\n").unwrap();
        let (recs, bad) = read_all(&dir);
        assert_eq!((recs.len(), bad), (1, 2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ledger_rebuilds_from_recent_orders_that_went_through() {
        let recs = vec![
            order_rec(now() - Duration::hours(30), "OLD", "placed"),
            order_rec(now() - Duration::hours(2), "AAPL", "placed"),
            order_rec(now() - Duration::hours(1), "MSFT", "blocked"),
            AuditRecord::event(now() - Duration::minutes(5), "kill", "kill_on", ""),
            order_rec(now() - Duration::minutes(1), "NVDA", "unknown"),
        ];
        let l = ledger_from(&recs, now());
        let syms: Vec<_> = l.orders.iter().map(|o| o.symbol.as_str()).collect();
        assert_eq!(syms, ["AAPL", "NVDA"]);
        assert_eq!(l.orders[0].notional, Some(2000.5));
        assert_eq!(l.orders[0].size_key, "10");
    }

    #[test]
    fn csv_escapes_and_defuses_formulas() {
        let mut r = order_rec(now(), "AAPL", "placed");
        r.detail = "said \"hi\", ok".into();
        r.checks = vec!["=cmd()".into()];
        let csv = to_csv(&[&r]);
        let mut lines = csv.lines();
        assert!(lines.next().unwrap().starts_with("time_utc,kind"));
        let row = lines.next().unwrap();
        assert!(row.contains("\"said \"\"hi\"\", ok\""), "{row}");
        assert!(row.contains("'=cmd()"), "{row}");
        assert!(row.starts_with("2026-10-14 15:00:00,order,demo-paper,AAPL,BUY,10,Market,,2000.50,"));
    }

    #[test]
    fn filter_matches_symbol_and_outcome() {
        let r = order_rec(now(), "AAPL", "blocked");
        assert!(matches(&r, "", ""));
        assert!(matches(&r, "aap", "blocked"));
        assert!(!matches(&r, "msft", ""));
        assert!(!matches(&r, "", "placed"));
    }

    #[test]
    fn ledger_is_capped() {
        let mut l = Ledger::default();
        for i in 0..(LEDGER_CAP + 50) {
            l.push(recent(i as i64, 1.0, "1"));
        }
        assert_eq!(l.orders.len(), LEDGER_CAP);
    }
}
