//! Price and indicator alerts (Bloomberg ALRT): rules such as `AAPL >= 250`, `NVDA RSI(14) >= 70`
//! or `MSFT crosses above SMA(50)`, evaluated by the worker against quotes and live ticks.
//!
//! Every rule remembers the last state it saw (the condition's truth, or which side of a line
//! the price is on), so it fires on the change, never twice for one move, and a restart picks
//! up where it left off. One-shot rules switch themselves off after firing; repeating rules
//! re-arm once the condition stops holding.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tr_core::{Security, Timeframe, indicators};

/// History of fired alerts kept in the book.
const HISTORY: usize = 200;
/// A repeating rule fires at most once per this many seconds (a price hovering on a level).
pub const REPEAT_COOLDOWN_SECS: i64 = 60;
/// Marks a toast as a price alert (Congress alerts have no prefix).
pub const TOAST_PREFIX: &str = "\u{1}price:";

/// What a rule watches.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Metric {
    /// Last price.
    Price,
    /// Change on the day, percent of the previous close.
    ChangePct,
    /// RSI of daily closes (including the live price).
    Rsi(usize),
    /// Pre- or post-market move, percent of the regular session's close.
    ExtMovePct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// The value is at or above the target (fires as soon as it holds).
    Above,
    /// The value is at or below the target.
    Below,
    /// The value moves from below the target to above it.
    CrossAbove,
    /// The value moves from above the target to below it.
    CrossBelow,
    /// Either crossing.
    Cross,
}

impl Op {
    fn is_cross(self) -> bool {
        matches!(self, Op::CrossAbove | Op::CrossBelow | Op::Cross)
    }
}

/// What the value is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Target {
    Level(f64),
    /// Simple moving average of daily closes.
    Sma(usize),
    /// Exponential moving average of daily closes.
    Ema(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub metric: Metric,
    pub op: Op,
    pub target: Target,
}

/// Extended-hours state of a security.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Ext {
    #[default]
    Unknown,
    /// The regular session is open (or the security has no extended hours): no extended move.
    Regular,
    /// Percent move of the pre/post-market price from the regular close.
    Move(f64),
}

/// What the worker knows about one security when it evaluates rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Market {
    pub last: Option<f64>,
    pub prev_close: Option<f64>,
    /// Daily closes, oldest first; the newest is replaced by `last` when evaluating.
    pub closes: Vec<f64>,
    pub ext: Ext,
}

impl Market {
    fn live_closes(&self) -> Vec<f64> {
        let mut c = self.closes.clone();
        if let (Some(last), Some(x)) = (self.last, c.last_mut()) {
            *x = last;
        }
        c
    }
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

/// `250`, `250.5`, `0.0012`: the shortest form of a level.
fn num(x: f64) -> String {
    format!("{x}")
}

fn signed_pct(x: f64) -> String {
    format!("{}{}%", if x > 0.0 { "+" } else { "" }, num(x))
}

impl Condition {
    /// Daily closes are needed (indicators).
    pub fn needs_closes(&self) -> bool {
        matches!(self.metric, Metric::Rsi(_)) || !matches!(self.target, Target::Level(_))
    }

    pub fn needs_ext(&self) -> bool {
        self.metric == Metric::ExtMovePct
    }

    fn metric_label(&self) -> String {
        match self.metric {
            Metric::Price => "Last".into(),
            Metric::ChangePct => "Chg%".into(),
            Metric::Rsi(n) => format!("RSI({n})"),
            Metric::ExtMovePct => "Ext-hrs move".into(),
        }
    }

    fn target_label(&self) -> String {
        let pct = matches!(self.metric, Metric::ChangePct | Metric::ExtMovePct);
        match self.target {
            Target::Level(x) if pct => signed_pct(x),
            Target::Level(x) => num(x),
            Target::Sma(n) => format!("SMA({n})"),
            Target::Ema(n) => format!("EMA({n})"),
        }
    }

    /// `Last >= 250`, `RSI(14) crosses above 70`, `Last crosses below SMA(50)`.
    pub fn label(&self) -> String {
        let op = match self.op {
            Op::Above => ">=",
            Op::Below => "<=",
            Op::CrossAbove => "crosses above",
            Op::CrossBelow => "crosses below",
            Op::Cross => "crosses",
        };
        format!("{} {op} {}", self.metric_label(), self.target_label())
    }

    /// Current value of the watched metric.
    pub fn value(&self, m: &Market) -> Option<f64> {
        match self.metric {
            Metric::Price => finite(m.last),
            Metric::ChangePct => {
                let prev = finite(m.prev_close).filter(|p| *p != 0.0)?;
                finite(m.last).map(|l| (l / prev - 1.0) * 100.0)
            }
            Metric::Rsi(n) => finite(indicators::rsi(&m.live_closes(), n).last().copied()),
            Metric::ExtMovePct => match m.ext {
                Ext::Move(x) => finite(Some(x)),
                _ => None,
            },
        }
    }

    fn target_value(&self, m: &Market) -> Option<f64> {
        match self.target {
            Target::Level(x) => Some(x),
            Target::Sma(n) => finite(indicators::sma(&m.live_closes(), n).last().copied()),
            Target::Ema(n) => finite(indicators::ema(&m.live_closes(), n).last().copied()),
        }
    }

    /// The state a rule tracks: for level rules whether the condition holds, for crossings
    /// whether the value is on the upper side of the target. `None` = not enough data.
    pub fn state(&self, m: &Market) -> Option<bool> {
        // Outside extended hours an extended-move condition simply does not hold.
        if self.needs_ext() && m.ext == Ext::Regular && !self.op.is_cross() {
            return Some(false);
        }
        let (v, t) = (self.value(m)?, self.target_value(m)?);
        Some(match self.op {
            Op::Above => v >= t,
            Op::Below => v <= t,
            Op::CrossAbove | Op::CrossBelow | Op::Cross => v >= t,
        })
    }

    /// Whether moving from state `prev` to `now` triggers the condition.
    pub fn triggers(&self, prev: Option<bool>, now: bool) -> bool {
        match self.op {
            Op::Above | Op::Below => now && prev != Some(true),
            Op::CrossAbove => prev == Some(false) && now,
            Op::CrossBelow => prev == Some(true) && !now,
            Op::Cross => prev.is_some_and(|p| p != now),
        }
    }

    /// Format a value of this condition's metric for messages.
    pub fn format_value(&self, symbol: &str, v: f64) -> String {
        match self.metric {
            Metric::Price => {
                let d = Security::parse(symbol).map_or(2, |s| s.decimals(v));
                format!("{v:.d$}")
            }
            Metric::ChangePct | Metric::ExtMovePct => format!("{v:+.2}%"),
            Metric::Rsi(_) => format!("{v:.1}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: u64,
    /// Market key (`AAPL US Equity`).
    pub symbol: String,
    pub cond: Condition,
    pub enabled: bool,
    /// Re-arm after firing instead of switching off.
    pub repeat: bool,
    pub created: i64,
    #[serde(default)]
    pub last_fired: Option<i64>,
    #[serde(default)]
    pub fired: u32,
    /// Last observed state (see [`Condition::state`]); persisted so a restart never re-fires.
    #[serde(default)]
    pub state: Option<bool>,
    /// Chart timeframe active when the alert was created on a chart (`None` for older rules
    /// and alerts typed on the command line).
    #[serde(default)]
    pub tf: Option<Timeframe>,
    /// Free-text note shown on the chart's alert line.
    #[serde(default)]
    pub note: String,
}

impl Rule {
    pub fn ticker(&self) -> String {
        Security::parse(&self.symbol).map_or_else(|| self.symbol.clone(), |s| s.ticker)
    }

    /// Update the rule from market data. Returns the metric's value when the rule fires.
    pub fn check(&mut self, m: &Market, now: i64) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        let state = self.cond.state(m)?;
        let prev = self.state.replace(state);
        if !self.cond.triggers(prev, state) {
            return None;
        }
        if self.repeat && self.last_fired.is_some_and(|t| now - t < REPEAT_COOLDOWN_SECS) {
            return None;
        }
        self.last_fired = Some(now);
        self.fired += 1;
        if !self.repeat {
            self.enabled = false;
        }
        Some(self.cond.value(m).unwrap_or(f64::NAN))
    }
}

/// A rule that fired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fired {
    pub rule: u64,
    pub symbol: String,
    pub message: String,
    pub ts: i64,
    #[serde(default)]
    pub seen: bool,
}

/// Changes to the rule book, sent from the UI to the worker.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleOp {
    Add(Spec),
    Enable(u64, bool),
    Repeat(u64, bool),
    Delete(u64),
    /// Change a rule's condition and note (re-arms it: its tracked state starts fresh).
    Edit(u64, Condition, String),
    /// Native (macOS) notifications on or off.
    Notify(bool),
    MarkSeen,
    ClearFired,
}

/// Rules, fired history and options, persisted as JSON in the data directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertBook {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub fired: Vec<Fired>,
    /// Also post a native notification when a rule fires.
    #[serde(default = "yes")]
    pub notify: bool,
    #[serde(default)]
    next_id: u64,
}

fn yes() -> bool {
    true
}

impl Default for AlertBook {
    fn default() -> Self {
        Self { rules: Vec::new(), fired: Vec::new(), notify: true, next_id: 1 }
    }
}

impl AlertBook {
    /// Load the book; a missing file is an empty book, an unreadable one is set aside.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        match serde_json::from_slice::<AlertBook>(&bytes) {
            Ok(mut b) => {
                b.next_id = b.next_id.max(b.rules.iter().map(|r| r.id + 1).max().unwrap_or(1));
                b
            }
            Err(e) => {
                tracing::warn!("alert rules {} unreadable ({e}); starting empty", path.display());
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::layouts::write_atomic(path, &json)
    }

    pub fn add(&mut self, symbol: &str, cond: Condition, repeat: bool, now: i64) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.rules.push(Rule {
            id,
            symbol: symbol.to_string(),
            cond,
            enabled: true,
            repeat,
            created: now,
            last_fired: None,
            fired: 0,
            state: None,
            tf: None,
            note: String::new(),
        });
        id
    }

    fn rule_mut(&mut self, id: u64) -> Option<&mut Rule> {
        self.rules.iter_mut().find(|r| r.id == id)
    }

    /// Apply a change. Returns whether anything changed.
    pub fn apply(&mut self, op: RuleOp, now: i64) -> bool {
        match op {
            RuleOp::Add(spec) => {
                let Some(symbol) = spec.symbol else { return false };
                let id = self.add(&symbol, spec.cond, spec.repeat, now);
                if let Some(r) = self.rule_mut(id) {
                    r.tf = spec.tf;
                    r.note = spec.note;
                }
            }
            RuleOp::Edit(id, cond, note) => {
                let Some(r) = self.rule_mut(id) else { return false };
                if r.cond == cond && r.note == note {
                    return false;
                }
                r.cond = cond;
                r.note = note;
                r.state = None;
            }
            RuleOp::Enable(id, on) => {
                let Some(r) = self.rule_mut(id) else { return false };
                if r.enabled == on {
                    return false;
                }
                r.enabled = on;
                // Re-enabled rules start fresh: a level rule fires if it already holds.
                r.state = None;
            }
            RuleOp::Repeat(id, on) => {
                let Some(r) = self.rule_mut(id) else { return false };
                r.repeat = on;
            }
            RuleOp::Delete(id) => {
                let n = self.rules.len();
                self.rules.retain(|r| r.id != id);
                return self.rules.len() != n;
            }
            RuleOp::Notify(on) => self.notify = on,
            RuleOp::MarkSeen => {
                if self.fired.iter().all(|f| f.seen) {
                    return false;
                }
                self.fired.iter_mut().for_each(|f| f.seen = true);
            }
            RuleOp::ClearFired => self.fired.clear(),
        }
        true
    }

    /// Symbols with enabled rules, and whether any of them needs daily closes / extended hours.
    pub fn watched(&self) -> Vec<(String, bool, bool)> {
        let mut out: Vec<(String, bool, bool)> = Vec::new();
        for r in self.rules.iter().filter(|r| r.enabled) {
            match out.iter_mut().find(|(s, ..)| *s == r.symbol) {
                Some((_, c, e)) => {
                    *c |= r.cond.needs_closes();
                    *e |= r.cond.needs_ext();
                }
                None => out.push((r.symbol.clone(), r.cond.needs_closes(), r.cond.needs_ext())),
            }
        }
        out
    }

    pub fn watches(&self, symbol: &str) -> bool {
        self.rules.iter().any(|r| r.enabled && r.symbol == symbol)
    }

    /// Evaluate `symbol`'s rules. Returns the alerts that fired and whether any rule changed
    /// (and the book should be saved).
    pub fn evaluate(&mut self, symbol: &str, m: &Market, now: i64) -> (Vec<Fired>, bool) {
        let mut fired = Vec::new();
        let mut changed = false;
        for r in self.rules.iter_mut().filter(|r| r.enabled && r.symbol == symbol) {
            let before = r.state;
            if let Some(v) = r.check(m, now) {
                let message = format!("{}: {} (now {})", r.ticker(), r.cond.label(), r.cond.format_value(symbol, v));
                fired.push(Fired { rule: r.id, symbol: symbol.to_string(), message, ts: now, seen: false });
            }
            changed |= r.state != before;
        }
        changed |= !fired.is_empty();
        for f in &fired {
            self.fired.insert(0, f.clone());
        }
        self.fired.truncate(HISTORY);
        (fired, changed)
    }

    /// `symbol`'s enabled price-level rules, drawn as lines on its charts.
    pub fn chart_alerts(&self, symbol: &str) -> Vec<ChartAlert> {
        self.rules
            .iter()
            .filter(|r| r.enabled && r.symbol == symbol && r.cond.metric == Metric::Price)
            .filter_map(|r| match r.cond.target {
                Target::Level(level) => Some(ChartAlert { id: r.id, level, op: r.cond.op, note: r.note.clone() }),
                _ => None,
            })
            .collect()
    }

    pub fn unseen(&self) -> usize {
        self.fired.iter().filter(|f| !f.seen).count()
    }
}

/// An armed price-level alert as a chart draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartAlert {
    pub id: u64,
    pub level: f64,
    pub op: Op,
    pub note: String,
}

/// The condition a chart suggests for a new alert at `price`: crossing up when the price is
/// above the last price, crossing down when below.
pub fn default_chart_op(price: f64, last: Option<f64>) -> Op {
    if last.is_some_and(|l| price < l) { Op::CrossBelow } else { Op::CrossAbove }
}

/// A rule typed on the command line: `ALRT AAPL > 250`, `ALRT NVDA RSI > 70 REPEAT`.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// Market key; `None` = the focused chart's security.
    pub symbol: Option<String>,
    pub cond: Condition,
    pub repeat: bool,
    /// Chart timeframe the alert was created on, if any.
    pub tf: Option<Timeframe>,
    pub note: String,
}

fn metric_word(w: &str) -> Option<Metric> {
    Some(match w {
        "RSI" => Metric::Rsi(14),
        "CHG" | "CHANGE" | "CHG%" | "%" | "PCT" => Metric::ChangePct,
        "EXT" | "EXTENDED" | "AH" => Metric::ExtMovePct,
        _ => return None,
    })
}

fn op_word(w: &str) -> Option<Op> {
    Some(match w {
        ">" | ">=" | "=>" | "ABOVE" => Op::Above,
        "<" | "<=" | "=<" | "BELOW" => Op::Below,
        "CROSS" | "CROSSES" | "XOVER" => Op::Cross,
        _ => return None,
    })
}

/// Split `>250` and `AAPL>=250` into separate words.
fn split_ops(words: &[&str]) -> Vec<String> {
    let mut s = String::new();
    for w in words {
        s.push(' ');
        let mut in_op = false;
        for c in w.chars() {
            let op = matches!(c, '<' | '>' | '=');
            if op != in_op {
                s.push(' ');
                in_op = op;
            }
            s.push(c);
        }
    }
    s.split_whitespace().map(|w| w.to_ascii_uppercase()).collect()
}

fn period(w: Option<&String>) -> Option<usize> {
    w.and_then(|w| w.parse::<usize>().ok()).filter(|n| (2..=1000).contains(n))
}

/// Parse the words after `ALRT`. Unknown or contradictory words make the rule invalid so a
/// typo never arms something unintended.
pub fn parse(words: &[&str]) -> Option<Spec> {
    let mut repeat = false;
    let mut t: Vec<String> = Vec::new();
    for w in split_ops(words) {
        match w.as_str() {
            "REPEAT" | "RPT" | "REPEATING" | "ALWAYS" => repeat = true,
            "ONCE" | "ONESHOT" | "ONE-SHOT" => repeat = false,
            _ => t.push(w),
        }
    }
    let mut i = 0;
    // A security unless the first word is a metric or operator (`RSI US EQUITY` is a ticker).
    let keyed = t.get(1).is_some_and(|w| w == "US" || tr_core::YellowKey::parse(w).is_some());
    let mut symbol = None;
    if let Some(first) = t.first()
        && (keyed || (metric_word(first).is_none() && op_word(first).is_none()))
    {
        let refs: Vec<&str> = t.iter().map(String::as_str).collect();
        let (sec, used) = Security::resolve_tokens(&refs)?;
        symbol = Some(sec.key());
        i = used;
    }
    let mut metric = Metric::Price;
    if let Some(m) = t.get(i).and_then(|w| metric_word(w)) {
        metric = m;
        i += 1;
        if let Metric::Rsi(_) = m
            && t.get(i + 1).is_some_and(|w| op_word(w).is_some())
            && let Some(n) = period(t.get(i))
        {
            metric = Metric::Rsi(n);
            i += 1;
        }
    }
    let mut op = op_word(t.get(i)?)?;
    i += 1;
    if op == Op::Cross {
        match t.get(i).and_then(|w| op_word(w)) {
            Some(Op::Above) => (op, i) = (Op::CrossAbove, i + 1),
            Some(Op::Below) => (op, i) = (Op::CrossBelow, i + 1),
            Some(Op::Cross) => return None,
            _ => {}
        }
    }
    let word = t.get(i)?;
    i += 1;
    let target = match word.as_str() {
        "SMA" | "EMA" => {
            let default = if word == "SMA" { 50 } else { 20 };
            let n = match t.get(i) {
                Some(_) => {
                    i += 1;
                    period(t.get(i - 1))?
                }
                None => default,
            };
            // A moving average is a line the price crosses.
            op = match op {
                Op::Above => Op::CrossAbove,
                Op::Below => Op::CrossBelow,
                o => o,
            };
            if word == "SMA" { Target::Sma(n) } else { Target::Ema(n) }
        }
        w => {
            let pct = w.ends_with('%');
            let x = w.trim_start_matches(['$', '+']).trim_end_matches('%').replace(',', "").parse::<f64>().ok()?;
            if !x.is_finite() {
                return None;
            }
            // `AAPL > 5%` is a move on the day.
            if pct && metric == Metric::Price {
                metric = Metric::ChangePct;
            }
            Target::Level(x)
        }
    };
    if i != t.len() {
        return None;
    }
    let cond = Condition { metric, op, target };
    let valid = match (metric, target) {
        (Metric::Price, Target::Level(x)) => x > 0.0,
        (Metric::Price, _) => true,
        (_, Target::Sma(_) | Target::Ema(_)) => false,
        (Metric::Rsi(_), Target::Level(x)) => (0.0..=100.0).contains(&x),
        (Metric::ChangePct, Target::Level(_)) => true,
        // Extended-hours moves are levels: up by at least x%, or down by at least x%.
        (Metric::ExtMovePct, Target::Level(x)) => match op {
            Op::Above => x > 0.0,
            Op::Below => x < 0.0,
            _ => false,
        },
    };
    valid.then_some(Spec { symbol, cond, repeat, tf: None, note: String::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> Option<Spec> {
        parse(&s.split_whitespace().collect::<Vec<_>>())
    }

    fn cond(metric: Metric, op: Op, target: Target) -> Condition {
        Condition { metric, op, target }
    }

    fn px(last: f64) -> Market {
        Market { last: Some(last), prev_close: Some(100.0), ..Default::default() }
    }

    #[test]
    fn parses_rules() {
        let s = spec("AAPL > 250").unwrap();
        assert_eq!(s.symbol.as_deref(), Some("AAPL US Equity"));
        assert_eq!(s.cond, cond(Metric::Price, Op::Above, Target::Level(250.0)));
        assert!(!s.repeat);
        assert_eq!(spec("AAPL>250"), spec("AAPL > 250"));
        assert_eq!(spec("aapl >= 250"), spec("AAPL > 250"));
        assert_eq!(spec("AAPL ABOVE $250"), spec("AAPL > 250"));
        let b = spec("BTCUSD CRNCY < 60,000").unwrap();
        assert_eq!(b.symbol.as_deref(), Some("BTCUSD Curncy"));
        assert_eq!(b.cond, cond(Metric::Price, Op::Below, Target::Level(60_000.0)));
        assert_eq!(spec("BTCUSD Curncy < 60000"), Some(b));
        let r = spec("NVDA RSI > 70 REPEAT").unwrap();
        assert_eq!(r.cond, cond(Metric::Rsi(14), Op::Above, Target::Level(70.0)));
        assert!(r.repeat);
        assert_eq!(spec("NVDA RSI 9 < 30").unwrap().cond, cond(Metric::Rsi(9), Op::Below, Target::Level(30.0)));
        assert_eq!(spec("TSLA CHG < -3").unwrap().cond, cond(Metric::ChangePct, Op::Below, Target::Level(-3.0)));
        assert_eq!(spec("TSLA > 5%").unwrap().cond, cond(Metric::ChangePct, Op::Above, Target::Level(5.0)));
        assert_eq!(spec("AAPL EXT > 2").unwrap().cond, cond(Metric::ExtMovePct, Op::Above, Target::Level(2.0)));
        // Moving averages are crossings.
        assert_eq!(spec("MSFT > SMA 50").unwrap().cond, cond(Metric::Price, Op::CrossAbove, Target::Sma(50)));
        assert_eq!(spec("MSFT CROSS EMA").unwrap().cond, cond(Metric::Price, Op::Cross, Target::Ema(20)));
        assert_eq!(spec("MSFT CROSS BELOW SMA 200").unwrap().cond.op, Op::CrossBelow);
        assert_eq!(spec("MSFT CROSSES ABOVE 400").unwrap().cond.op, Op::CrossAbove);
        // No security: the focused chart's.
        assert_eq!(spec("> 250").unwrap().symbol, None);
        assert_eq!(spec("RSI > 70").unwrap().symbol, None);
        // A yellow key makes a metric word a ticker.
        assert_eq!(spec("RSI US EQUITY > 10").unwrap().symbol.as_deref(), Some("RSI US Equity"));
        assert_eq!(spec("X > 30").unwrap().symbol.as_deref(), Some("X US Equity"));
    }

    #[test]
    fn rejects_malformed_rules() {
        for bad in [
            "AAPL",
            "AAPL 250",
            "AAPL > ",
            "AAPL > abc",
            "AAPL > 250 300",
            "AAPL > -5",
            "AAPL RSI > 120",
            "AAPL RSI > SMA 50",
            "AAPL EXT < 2",
            "AAPL EXT > -2",
            "AAPL EXT CROSS 2",
            "AAPL > SMA 1",
            "AAPL CROSS CROSS 5",
            "AAPL > 250 NOW",
        ] {
            assert_eq!(spec(bad), None, "{bad}");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(spec("AAPL > 250").unwrap().cond.label(), "Last >= 250");
        assert_eq!(spec("AAPL > 250.5").unwrap().cond.label(), "Last >= 250.5");
        assert_eq!(spec("NVDA RSI > 70").unwrap().cond.label(), "RSI(14) >= 70");
        assert_eq!(spec("MSFT < SMA 50").unwrap().cond.label(), "Last crosses below SMA(50)");
        assert_eq!(spec("TSLA CHG > 5").unwrap().cond.label(), "Chg% >= +5%");
        assert_eq!(spec("AAPL EXT < -3").unwrap().cond.label(), "Ext-hrs move <= -3%");
    }

    #[test]
    fn level_rule_fires_once() {
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", cond(Metric::Price, Op::Above, Target::Level(250.0)), false, 0);
        assert!(b.evaluate("AAPL US Equity", &px(240.0), 1).0.is_empty());
        let (fired, changed) = b.evaluate("AAPL US Equity", &px(251.0), 2);
        assert_eq!(fired.len(), 1);
        assert!(changed);
        assert_eq!(fired[0].message, "AAPL: Last >= 250 (now 251.00)");
        // One-shot: switched off, never fires again.
        assert!(!b.rules[0].enabled);
        assert!(b.evaluate("AAPL US Equity", &px(240.0), 100).0.is_empty());
        assert!(b.evaluate("AAPL US Equity", &px(260.0), 200).0.is_empty());
        assert_eq!(b.rules[0].fired, 1);
        assert_eq!(b.unseen(), 1);
        assert!(b.apply(RuleOp::MarkSeen, 0));
        assert_eq!(b.unseen(), 0);
    }

    #[test]
    fn level_rule_already_met_fires_on_first_quote() {
        let mut r = Rule {
            id: 1,
            symbol: "AAPL US Equity".into(),
            cond: cond(Metric::Price, Op::Below, Target::Level(250.0)),
            enabled: true,
            repeat: false,
            created: 0,
            last_fired: None,
            fired: 0,
            state: None,
            tf: None,
            note: String::new(),
        };
        assert_eq!(r.check(&px(240.0), 1), Some(240.0));
        assert!(r.check(&px(240.0), 2).is_none());
    }

    #[test]
    fn repeating_rule_rearms_without_double_firing() {
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", cond(Metric::Price, Op::Above, Target::Level(250.0)), true, 0);
        let mut t = 0;
        let mut fire = |b: &mut AlertBook, p: f64, dt: i64| {
            t += dt;
            b.evaluate("AAPL US Equity", &px(p), t).0.len()
        };
        assert_eq!(fire(&mut b, 251.0, 1), 1);
        // Still above: no second alert for the same move, however many ticks arrive.
        assert_eq!(fire(&mut b, 252.0, 1), 0);
        assert_eq!(fire(&mut b, 253.0, 500), 0);
        // Back below re-arms; above again fires again.
        assert_eq!(fire(&mut b, 249.0, 1), 0);
        assert_eq!(fire(&mut b, 251.0, 1), 1);
        // A quick dip and recovery inside the cooldown does not fire.
        assert_eq!(fire(&mut b, 249.0, 1), 0);
        assert_eq!(fire(&mut b, 251.0, 1), 0);
        assert!(b.rules[0].enabled);
        assert_eq!(b.rules[0].fired, 2);
    }

    #[test]
    fn cross_needs_a_transition() {
        let c = cond(Metric::Price, Op::CrossAbove, Target::Level(100.0));
        assert!(!c.triggers(None, true), "already above when created is not a cross");
        assert!(c.triggers(Some(false), true));
        assert!(!c.triggers(Some(true), true));
        assert!(!c.triggers(Some(true), false));
        let down = cond(Metric::Price, Op::CrossBelow, Target::Level(100.0));
        assert!(down.triggers(Some(true), false));
        assert!(!down.triggers(Some(false), true));
        let any = cond(Metric::Price, Op::Cross, Target::Level(100.0));
        assert!(any.triggers(Some(true), false) && any.triggers(Some(false), true));
        assert!(!any.triggers(None, false));
    }

    #[test]
    fn crosses_moving_average() {
        // 60 closes at 100, then the live price decides the side of the 50-day SMA.
        let base = Market { closes: vec![100.0; 60], prev_close: Some(100.0), ..Default::default() };
        let at = |p: f64| Market { last: Some(p), ..base.clone() };
        let mut b = AlertBook::default();
        b.add("MSFT US Equity", cond(Metric::Price, Op::CrossAbove, Target::Sma(50)), false, 0);
        assert!(b.evaluate("MSFT US Equity", &at(99.0), 1).0.is_empty());
        assert_eq!(b.rules[0].state, Some(false));
        let fired = b.evaluate("MSFT US Equity", &at(103.0), 2).0;
        assert_eq!(fired.len(), 1, "crossed above the SMA");
        assert!(fired[0].message.starts_with("MSFT: Last crosses above SMA(50)"));
        // Too little history: no state, nothing fires.
        let mut short = AlertBook::default();
        short.add("MSFT US Equity", cond(Metric::Price, Op::Cross, Target::Sma(50)), true, 0);
        let thin = Market { last: Some(103.0), closes: vec![100.0; 10], ..Default::default() };
        assert!(short.evaluate("MSFT US Equity", &thin, 1).0.is_empty());
        assert_eq!(short.rules[0].state, None);
    }

    #[test]
    fn rsi_and_day_change() {
        let rising: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let m = Market { last: Some(140.0), prev_close: Some(138.0), closes: rising, ext: Ext::Unknown };
        let rsi = cond(Metric::Rsi(14), Op::Above, Target::Level(70.0));
        assert!(rsi.value(&m).unwrap() > 70.0);
        assert_eq!(rsi.state(&m), Some(true));
        let chg = cond(Metric::ChangePct, Op::Above, Target::Level(1.0));
        assert!((chg.value(&m).unwrap() - 1.449).abs() < 0.01);
        assert_eq!(chg.state(&m), Some(true));
        assert_eq!(chg.state(&Market { prev_close: None, ..m }), None);
    }

    #[test]
    fn extended_hours_move() {
        let c = cond(Metric::ExtMovePct, Op::Below, Target::Level(-3.0));
        let ext = |e| Market { last: Some(100.0), ext: e, ..Default::default() };
        assert_eq!(c.state(&ext(Ext::Unknown)), None);
        assert_eq!(c.state(&ext(Ext::Regular)), Some(false));
        assert_eq!(c.state(&ext(Ext::Move(-1.0))), Some(false));
        assert_eq!(c.state(&ext(Ext::Move(-4.2))), Some(true));
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", c, true, 0);
        assert_eq!(b.evaluate("AAPL US Equity", &ext(Ext::Move(-4.2)), 10).0.len(), 1);
        // The regular session re-arms it for the next session's move.
        assert!(b.evaluate("AAPL US Equity", &ext(Ext::Regular), 1000).0.is_empty());
        let f = b.evaluate("AAPL US Equity", &ext(Ext::Move(-3.5)), 2000).0;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].message, "AAPL: Ext-hrs move <= -3% (now -3.50%)");
    }

    #[test]
    fn missing_data_keeps_state() {
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", cond(Metric::Price, Op::CrossAbove, Target::Level(250.0)), false, 0);
        b.evaluate("AAPL US Equity", &px(240.0), 1);
        let (f, changed) = b.evaluate("AAPL US Equity", &Market::default(), 2);
        assert!(f.is_empty() && !changed);
        assert_eq!(b.rules[0].state, Some(false));
        // Other symbols' quotes never touch the rule.
        assert!(b.evaluate("MSFT US Equity", &px(300.0), 3).0.is_empty());
        assert_eq!(b.evaluate("AAPL US Equity", &px(255.0), 4).0.len(), 1);
    }

    #[test]
    fn ops_and_reenable() {
        let mut b = AlertBook::default();
        let s = spec("AAPL > 250").unwrap();
        assert!(b.apply(RuleOp::Add(s.clone()), 5));
        assert!(!b.apply(RuleOp::Add(Spec { symbol: None, ..s }), 5), "needs a security");
        let id = b.rules[0].id;
        b.evaluate("AAPL US Equity", &px(260.0), 6);
        assert!(!b.rules[0].enabled);
        // Re-enabling starts fresh, so a condition that still holds fires again.
        assert!(b.apply(RuleOp::Enable(id, true), 7));
        assert_eq!(b.rules[0].state, None);
        assert_eq!(b.evaluate("AAPL US Equity", &px(260.0), 8).0.len(), 1);
        assert!(b.apply(RuleOp::Repeat(id, true), 9));
        assert!(b.rules[0].repeat);
        assert_eq!(b.watched(), vec![]);
        assert!(b.apply(RuleOp::Enable(id, true), 10));
        assert_eq!(b.watched(), vec![("AAPL US Equity".to_string(), false, false)]);
        assert!(b.apply(RuleOp::Delete(id), 11));
        assert!(!b.apply(RuleOp::Delete(id), 11));
        assert!(b.rules.is_empty());
        // Ids are never reused.
        b.add("MSFT US Equity", s_cond(), false, 12);
        assert!(b.rules[0].id > id);
    }

    fn s_cond() -> Condition {
        cond(Metric::Price, Op::Above, Target::Level(1.0))
    }

    #[test]
    fn book_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("tr-alerts-{}", std::process::id()));
        let path = dir.join("alerts.json");
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", cond(Metric::Price, Op::Above, Target::Level(250.0)), true, 1);
        b.add("NVDA US Equity", cond(Metric::Rsi(14), Op::Below, Target::Level(30.0)), false, 2);
        b.evaluate("AAPL US Equity", &px(251.0), 3);
        b.notify = false;
        b.save(&path).unwrap();
        let loaded = AlertBook::load(&path);
        assert_eq!(loaded, b);
        // State survives, so the restart does not fire the same move again.
        let mut again = loaded;
        assert!(again.evaluate("AAPL US Equity", &px(252.0), 10_000).0.is_empty());
        let id = again.add("MSFT US Equity", s_cond(), false, 4);
        assert_eq!(id, 3);
        // A corrupt file is set aside rather than crashing or being overwritten blindly.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(AlertBook::load(&path), AlertBook::default());
        assert!(path.with_extension("json.bad").exists());
        assert_eq!(AlertBook::load(&dir.join("missing.json")), AlertBook::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_rules_without_timeframe_or_note_still_load() {
        let mut b = AlertBook::default();
        b.add("AAPL US Equity", cond(Metric::Price, Op::Above, Target::Level(250.0)), false, 1);
        let mut v = serde_json::to_value(&b).unwrap();
        let rule = v["rules"][0].as_object_mut().unwrap();
        assert!(rule.remove("tf").is_some() && rule.remove("note").is_some());
        let old: AlertBook = serde_json::from_value(v).unwrap();
        assert_eq!(old.rules[0].tf, None);
        assert_eq!(old.rules[0].note, "");
        assert_eq!(old.rules[0].cond, b.rules[0].cond);
    }

    #[test]
    fn chart_alert_keeps_timeframe_and_note_and_can_be_edited() {
        let mut b = AlertBook::default();
        let spec = Spec {
            symbol: Some("AAPL US Equity".into()),
            cond: cond(Metric::Price, Op::CrossAbove, Target::Level(260.0)),
            repeat: false,
            tf: Some(Timeframe::H1),
            note: "breakout".into(),
        };
        assert!(b.apply(RuleOp::Add(spec), 1));
        assert_eq!(b.rules[0].tf, Some(Timeframe::H1));
        let shown = b.chart_alerts("AAPL US Equity");
        assert_eq!(shown.len(), 1);
        assert_eq!((shown[0].level, shown[0].note.as_str()), (260.0, "breakout"));
        let id = b.rules[0].id;
        let moved = cond(Metric::Price, Op::CrossBelow, Target::Level(240.0));
        assert!(b.apply(RuleOp::Edit(id, moved, "stop".into()), 2));
        assert_eq!(b.chart_alerts("AAPL US Equity")[0].level, 240.0);
        assert!(!b.apply(RuleOp::Edit(id, moved, "stop".into()), 3), "no change");
        // Triggered alerts stop drawing.
        b.rules[0].enabled = false;
        assert!(b.chart_alerts("AAPL US Equity").is_empty());
    }

    #[test]
    fn chart_default_condition_follows_last_price() {
        assert_eq!(default_chart_op(110.0, Some(100.0)), Op::CrossAbove);
        assert_eq!(default_chart_op(90.0, Some(100.0)), Op::CrossBelow);
        assert_eq!(default_chart_op(90.0, None), Op::CrossAbove);
    }
}
