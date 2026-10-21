//! Formula (synthetic) securities: arithmetic over catalog securities, e.g. `XU100/USDTRY`,
//! `GC1*USDTRY/31.1035`, `(AAPL+MSFT)/2`, `CL1-CO1`.
//!
//! A formula is `+ - * /` over numeric constants, parentheses and security tickers (resolved
//! like any command-line ticker; a ticker cannot carry a yellow key and cannot contain `-`).
//! A formula security is a [`Security`] of class Index whose ticker is `=<canonical
//! expression>`, so its market key is `=XU100/USDTRY`. Named synthetics ([`NAMED`], e.g.
//! `GRAMALTIN`) resolve to an expression too.
//!
//! Bars: each leg's history is joined on the bar open time (for daily and longer bars on the
//! calendar day, so a `.IS` bar and a US bar of the same date line up). Open, high, low and
//! close are each the expression evaluated over the legs' same field, then high/low are
//! widened to contain open and close. That is exact for a single leg and for sums, and an
//! approximation for ratios and spreads (the true extreme of a ratio inside a bar is not
//! knowable from the legs' OHLC). Volume is zero. A bar where the expression is not finite
//! (division by zero) is skipped.

use std::collections::BTreeMap;
use std::fmt;

use crate::{Bar, Security, Timeframe, catalog};

/// A built-in named synthetic.
pub struct Named {
    pub ticker: &'static str,
    pub name: &'static str,
    pub expr: &'static str,
    /// Currency the result is quoted in.
    pub ccy: &'static str,
}

/// Built-in synthetics. A troy ounce is 31.1035 g; `CEYREK` is the pure gold in a quarter coin
/// (1.6065 g), without the jeweller's premium.
pub const NAMED: &[Named] = &[
    Named { ticker: "GRAMALTIN", name: "Gram Gold (TRY)", expr: "GC1*USDTRY/31.1035", ccy: "TRY" },
    Named { ticker: "CEYREK", name: "Quarter Gold Coin, gold value (TRY)", expr: "GC1*USDTRY/31.1035*1.6065", ccy: "TRY" },
    Named { ticker: "XU100USD", name: "BIST 100 in USD", expr: "XU100/USDTRY", ccy: "USD" },
    Named { ticker: "BTCTRY", name: "Bitcoin / Turkish Lira", expr: "BTCUSD*USDTRY", ccy: "TRY" },
];

pub fn named(ticker: &str) -> Option<&'static Named> {
    let t = ticker.trim().to_ascii_uppercase();
    NAMED.iter().find(|n| n.ticker == t)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaError(pub String);

impl fmt::Display for FormulaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FormulaError {}

fn err<T>(m: impl Into<String>) -> Result<T, FormulaError> {
    Err(FormulaError(m.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

impl Op {
    fn prec(self) -> u8 {
        match self {
            Op::Add | Op::Sub => 1,
            Op::Mul | Op::Div => 2,
        }
    }

    fn ch(self) -> char {
        match self {
            Op::Add => '+',
            Op::Sub => '-',
            Op::Mul => '*',
            Op::Div => '/',
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Num(f64),
    /// Index into [`Formula::legs`].
    Leg(usize),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

/// A parsed formula.
#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    expr: Expr,
    legs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Id(String),
    Op(Op),
    LParen,
    RParen,
}

fn lex(src: &str) -> Result<Vec<Tok>, FormulaError> {
    let cs: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let simple = match c {
            '+' => Some(Tok::Op(Op::Add)),
            '-' => Some(Tok::Op(Op::Sub)),
            '*' => Some(Tok::Op(Op::Mul)),
            '/' => Some(Tok::Op(Op::Div)),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            _ => None,
        };
        if let Some(t) = simple {
            out.push(t);
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let s = i;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            if i < cs.len() && cs[i].is_ascii_alphabetic() {
                return err(format!("bad number near '{}'", cs[s..=i].iter().collect::<String>()));
            }
            let t: String = cs[s..i].iter().collect();
            match t.parse::<f64>() {
                Ok(v) => out.push(Tok::Num(v)),
                Err(_) => return err(format!("bad number '{t}'")),
            }
        } else if c.is_ascii_alphabetic() {
            let s = i;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '.' || cs[i] == '_') {
                i += 1;
            }
            out.push(Tok::Id(cs[s..i].iter().collect::<String>().to_ascii_uppercase()));
        } else {
            return err(format!("unexpected '{c}'"));
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    legs: Vec<String>,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn expr(&mut self) -> Result<Expr, FormulaError> {
        let mut l = self.term()?;
        while let Some(Tok::Op(op @ (Op::Add | Op::Sub))) = self.peek().cloned() {
            self.pos += 1;
            let r = self.term()?;
            l = Expr::Bin(op, l.into(), r.into());
        }
        Ok(l)
    }

    fn term(&mut self) -> Result<Expr, FormulaError> {
        let mut l = self.unary()?;
        while let Some(Tok::Op(op @ (Op::Mul | Op::Div))) = self.peek().cloned() {
            self.pos += 1;
            let r = self.unary()?;
            l = Expr::Bin(op, l.into(), r.into());
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Expr, FormulaError> {
        match self.peek() {
            Some(Tok::Op(Op::Sub)) => {
                self.pos += 1;
                Ok(Expr::Neg(self.unary()?.into()))
            }
            Some(Tok::Op(Op::Add)) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<Expr, FormulaError> {
        let t = self.peek().cloned();
        self.pos += 1;
        match t {
            Some(Tok::Num(v)) => Ok(Expr::Num(v)),
            Some(Tok::Id(id)) => {
                if let Some(n) = named(&id) {
                    // Named synthetics expand in place; their legs are plain tickers. The
                    // `0 +` wrapper keeps the expansion grouped and is dropped by `simplify`.
                    let sub = Formula::parse(n.expr)?;
                    let map: Vec<usize> = sub.legs.iter().map(|l| self.leg(l)).collect();
                    return Ok(Expr::Bin(Op::Add, Expr::Num(0.0).into(), remap(&sub.expr, &map).into()));
                }
                Ok(Expr::Leg(self.leg(&id)))
            }
            Some(Tok::LParen) => {
                let e = self.expr()?;
                match self.peek() {
                    Some(Tok::RParen) => {
                        self.pos += 1;
                        Ok(e)
                    }
                    _ => err("missing ')'"),
                }
            }
            Some(Tok::RParen) => err("unexpected ')'"),
            Some(Tok::Op(o)) => err(format!("unexpected '{}'", o.ch())),
            None => err("expression ends early"),
        }
    }

    fn leg(&mut self, id: &str) -> usize {
        match self.legs.iter().position(|l| l == id) {
            Some(i) => i,
            None => {
                self.legs.push(id.to_string());
                self.legs.len() - 1
            }
        }
    }
}

fn remap(e: &Expr, map: &[usize]) -> Expr {
    match e {
        Expr::Num(v) => Expr::Num(*v),
        Expr::Leg(i) => Expr::Leg(map[*i]),
        Expr::Neg(x) => Expr::Neg(remap(x, map).into()),
        Expr::Bin(o, a, b) => Expr::Bin(*o, remap(a, map).into(), remap(b, map).into()),
    }
}

/// Drop the `0 + ` wrapper used to keep an expanded named synthetic grouped.
fn simplify(e: Expr) -> Expr {
    match e {
        Expr::Bin(Op::Add, a, b) if *a == Expr::Num(0.0) => simplify(*b),
        Expr::Bin(o, a, b) => Expr::Bin(o, simplify(*a).into(), simplify(*b).into()),
        Expr::Neg(x) => Expr::Neg(simplify(*x).into()),
        e => e,
    }
}

impl Formula {
    /// Parse an expression. It needs at least one security.
    pub fn parse(src: &str) -> Result<Formula, FormulaError> {
        let toks = lex(src)?;
        if toks.is_empty() {
            return err("empty expression");
        }
        let mut p = Parser { toks, pos: 0, legs: Vec::new() };
        let expr = p.expr()?;
        if p.pos < p.toks.len() {
            return err("unexpected input after the expression");
        }
        if p.legs.is_empty() {
            return err("an expression needs at least one security");
        }
        Ok(Formula { expr: simplify(expr), legs: p.legs })
    }

    /// Tickers of the legs, in order of first appearance.
    pub fn legs(&self) -> &[String] {
        &self.legs
    }

    /// Market keys of the legs.
    pub fn leg_keys(&self) -> Vec<String> {
        self.legs.iter().map(|l| leg_key(l)).collect()
    }

    /// Normalised text: upper case, no spaces, minimal parentheses.
    pub fn canonical(&self) -> String {
        let mut s = String::new();
        self.write(&self.expr, &mut s);
        s
    }

    fn write(&self, e: &Expr, out: &mut String) {
        match e {
            Expr::Num(v) => out.push_str(&v.to_string()),
            Expr::Leg(i) => out.push_str(&self.legs[*i]),
            Expr::Neg(x) => {
                out.push('-');
                if matches!(**x, Expr::Bin(..)) {
                    out.push('(');
                    self.write(x, out);
                    out.push(')');
                } else {
                    self.write(x, out);
                }
            }
            Expr::Bin(op, a, b) => {
                let prec = |x: &Expr| if let Expr::Bin(o, ..) = x { o.prec() } else { 9 };
                let lp = prec(a) < op.prec();
                // Operators are left-associative, so an equal-precedence right operand keeps
                // its parentheses and the text re-parses to the same tree.
                let rp = prec(b) <= op.prec();
                if lp {
                    out.push('(');
                }
                self.write(a, out);
                if lp {
                    out.push(')');
                }
                out.push(op.ch());
                if rp {
                    out.push('(');
                }
                self.write(b, out);
                if rp {
                    out.push(')');
                }
            }
        }
    }

    /// Evaluate with one value per leg (in [`Formula::legs`] order). `None` when the result
    /// is not finite (division by zero).
    pub fn eval(&self, vals: &[f64]) -> Option<f64> {
        fn go(e: &Expr, v: &[f64]) -> f64 {
            match e {
                Expr::Num(x) => *x,
                Expr::Leg(i) => v[*i],
                Expr::Neg(x) => -go(x, v),
                Expr::Bin(op, a, b) => {
                    let (a, b) = (go(a, v), go(b, v));
                    match op {
                        Op::Add => a + b,
                        Op::Sub => a - b,
                        Op::Mul => a * b,
                        Op::Div if b == 0.0 => f64::NAN,
                        Op::Div => a / b,
                    }
                }
            }
        }
        if vals.len() != self.legs.len() {
            return None;
        }
        Some(go(&self.expr, vals)).filter(|x| x.is_finite())
    }

    /// One bar from one aligned bar per leg (see the module docs for how OHLC is derived).
    pub fn combine_bars(&self, bars: &[Bar]) -> Option<Bar> {
        if bars.len() != self.legs.len() {
            return None;
        }
        let field = |f: fn(&Bar) -> f64| self.eval(&bars.iter().map(f).collect::<Vec<_>>());
        let (o, c) = (field(|b| b.open)?, field(|b| b.close)?);
        let (h, l) = (field(|b| b.high), field(|b| b.low));
        let all = [Some(o), Some(c), h, l].into_iter().flatten();
        let (hi, lo) = all.fold((f64::MIN, f64::MAX), |(hi, lo), x| (hi.max(x), lo.min(x)));
        Some(Bar::new(bars.iter().map(|b| b.ts).min()?, o, hi, lo, c, 0.0))
    }

    /// Join the legs' histories (one `Vec<Bar>` per leg, in leg order) and evaluate. Bars
    /// missing from any leg, and bars where the expression is not finite, are skipped.
    pub fn combine(&self, tf: Timeframe, series: &[Vec<Bar>]) -> Vec<Bar> {
        if series.len() != self.legs.len() || series.is_empty() {
            return Vec::new();
        }
        let maps: Vec<BTreeMap<i64, Bar>> =
            series.iter().map(|s| s.iter().map(|b| (align_key(tf, b.ts), *b)).collect()).collect();
        let mut out = Vec::new();
        'keys: for k in maps[0].keys() {
            let mut row = Vec::with_capacity(maps.len());
            for m in &maps {
                match m.get(k) {
                    Some(b) => row.push(*b),
                    None => continue 'keys,
                }
            }
            if let Some(b) = self.combine_bars(&row) {
                out.push(b);
            }
        }
        out.sort_by_key(|b| b.ts);
        out
    }
}

fn leg_key(ticker: &str) -> String {
    Security::parse(ticker).map_or_else(|| ticker.to_string(), |s| s.key())
}

/// The join key of a bar: its open time for intraday, its calendar day for daily and longer.
/// Exchanges stamp a day's bar at their own local open (`.IS` 07:00 UTC, US 14:30 UTC, Sydney
/// 23:00 UTC the evening before); shifting by 6h puts every open on its trading date.
pub fn align_key(tf: Timeframe, ts: i64) -> i64 {
    if tf.is_intraday() { ts } else { (ts + 6 * 3600).div_euclid(86_400) }
}

/// The formula behind a market key: `=EXPR` or a named synthetic (`GRAMALTIN`, `GRAMALTIN Index`).
pub fn from_symbol(symbol: &str) -> Option<Result<Formula, FormulaError>> {
    let s = symbol.trim();
    if let Some(e) = s.strip_prefix('=') {
        return Some(Formula::parse(e));
    }
    let mut words = s.split_whitespace();
    let head = words.next()?;
    let rest_ok = words.all(|w| w.eq_ignore_ascii_case("INDEX"));
    named(head).filter(|_| rest_ok).map(|n| Formula::parse(n.expr))
}

fn is_op_char(c: char) -> bool {
    matches!(c, '+' | '-' | '*' | '/')
}

fn known(ticker: &str) -> bool {
    catalog::lookup(ticker, None).is_some() || named(ticker).is_some()
}

/// Recognise a formula at the start of command-line tokens. Returns the canonical expression
/// and how many tokens it spans (spaces around operators and inside parentheses are allowed:
/// `( AAPL + MSFT ) / 2`). Plain tickers and pairs (`BTC/USD`, `BRK/B`) are not formulas; a lone
/// `A/B` or `A-B` is one only when both sides are known catalog tickers (`SPX/XU100`, `CL1-CO1`).
pub fn span(tokens: &[&str]) -> Option<(String, usize)> {
    let first = *tokens.first()?;
    let explicit = first.starts_with('=');
    let mut expr = first.trim_start_matches('=').to_string();
    let mut used = 1;
    while let Some(next) = tokens.get(used) {
        let depth = expr.matches('(').count() as i64 - expr.matches(')').count() as i64;
        let ends_op = expr.chars().last().is_some_and(|c| is_op_char(c) || c == '(');
        let starts_op = next.chars().next().is_some_and(|c| is_op_char(c) || c == ')');
        if depth > 0 || ends_op || starts_op {
            expr.push_str(next);
            used += 1;
        } else {
            break;
        }
    }
    if !expr.chars().any(|c| is_op_char(c) || c == '(') {
        return None;
    }
    let f = Formula::parse(&expr).ok()?;
    // A lone `A/B` or `A-B` between two plain tickers may be a pair or a share class.
    if !explicit
        && !expr.contains(['(', '+', '*'])
        && f.legs.len() == 2
        && expr.matches(['/', '-']).count() == 1
        && let Expr::Bin(op, a, b) = &f.expr
        && matches!((&**a, &**b), (Expr::Leg(_), Expr::Leg(_)))
    {
        let (x, y) = (&f.legs[0], &f.legs[1]);
        let pair = format!("{x}{y}");
        if !(known(x) && known(y)) || (*op == Op::Div && (known(&pair) || crate::security::is_pair(&pair))) {
            return None;
        }
    }
    Some((f.canonical(), used))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Formula {
        Formula::parse(s).unwrap()
    }

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar::new(ts, o, h, l, c, 10.0)
    }

    #[test]
    fn parsing_and_precedence() {
        assert_eq!(f("xu100 / usdtry").canonical(), "XU100/USDTRY");
        assert!(Formula::parse("2+3*4").is_err()); // no security
        assert_eq!(f("A+B*C").eval(&[1.0, 2.0, 3.0]), Some(7.0));
        assert_eq!(f("(A+B)*C").eval(&[1.0, 2.0, 3.0]), Some(9.0));
        assert_eq!(f("A-B-C").eval(&[10.0, 3.0, 2.0]), Some(5.0));
        assert_eq!(f("A/B/C").eval(&[24.0, 3.0, 2.0]), Some(4.0));
        assert_eq!(f("-A+B").eval(&[1.0, 5.0]), Some(4.0));
        assert_eq!(f("A*-2").eval(&[3.0]), Some(-6.0));
        assert_eq!(f("GC1*USDTRY/31.1035").legs(), ["GC1", "USDTRY"]);
        assert_eq!(f("(AAPL+MSFT)/2").canonical(), "(AAPL+MSFT)/2");
        assert_eq!(f("((A))+(B*C)").canonical(), "A+B*C");
        assert_eq!(f("A-(B-C)").canonical(), "A-(B-C)");
        assert_eq!(f("A/(B*C)").canonical(), "A/(B*C)");
        assert_eq!(f("A*(B/C)").canonical(), "A*(B/C)");
        for bad in ["", "A+", "(A", "A)", "A B", "A+*B", "3M/A", "A$B", "()"] {
            assert!(Formula::parse(bad).is_err(), "{bad}");
        }
        // Canonical text re-parses to the same formula.
        for s in ["A-(B-C)", "A/(B/C)", "-(A+B)*C", "(A+B)/2"] {
            assert_eq!(f(&f(s).canonical()), f(s));
        }
    }

    #[test]
    fn named_synthetics_expand() {
        assert_eq!(f("BTCTRY*2").canonical(), "BTCUSD*USDTRY*2");
        assert_eq!(f("GRAMALTIN/XU100USD").legs(), ["GC1", "USDTRY", "XU100"]);
        for n in NAMED {
            assert!(Formula::parse(n.expr).is_ok(), "{}", n.ticker);
        }
        assert!(from_symbol("GRAMALTIN Index").is_some() && from_symbol("GRAMALTIN").is_some());
        assert!(from_symbol("AAPL US Equity").is_none());
    }

    #[test]
    fn division_by_zero_skips_the_bar() {
        assert_eq!(f("A/B").eval(&[1.0, 0.0]), None);
        let a = vec![bar(0, 1.0, 2.0, 1.0, 2.0), bar(86_400, 1.0, 2.0, 1.0, 2.0)];
        let b = vec![bar(0, 2.0, 2.0, 2.0, 2.0), bar(86_400, 0.0, 0.0, 0.0, 0.0)];
        let out = f("A/B").combine(Timeframe::D1, &[a, b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].close, 1.0);
    }

    #[test]
    fn ohlc_is_valid_and_volume_zero() {
        let usd = bar(0, 40.0, 41.0, 39.0, 40.5);
        let idx = bar(0, 100.0, 110.0, 95.0, 105.0);
        let b = f("A/B").combine_bars(&[idx, usd]).unwrap();
        assert!(b.high >= b.open.max(b.close) && b.low <= b.open.min(b.close));
        assert_eq!(b.volume, 0.0);
        assert!((b.close - 105.0 / 40.5).abs() < 1e-12 && (b.open - 2.5).abs() < 1e-12);
        // A single leg scaled keeps its shape exactly.
        let g = f("A*2").combine_bars(&[idx]).unwrap();
        assert_eq!((g.open, g.high, g.low, g.close), (200.0, 220.0, 190.0, 210.0));
        // A negative factor flips high and low but stays ordered.
        let n = f("-A").combine_bars(&[idx]).unwrap();
        assert!(n.high == -95.0 && n.low == -110.0);
    }

    #[test]
    fn alignment_inner_join() {
        let h = 3600;
        // Daily: BIST stamps 07:00 UTC, US 14:30 UTC, Sydney 23:00 UTC the evening before.
        let ist = vec![bar(7 * h, 1.0, 1.0, 1.0, 10.0), bar(86_400 + 7 * h, 1.0, 1.0, 1.0, 12.0)];
        let us = vec![bar(14 * h + 1800, 1.0, 1.0, 1.0, 2.0), bar(2 * 86_400 + 14 * h, 1.0, 1.0, 1.0, 3.0)];
        let out = f("A/B").combine(Timeframe::D1, &[ist.clone(), us]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].close, 5.0);
        assert_eq!(out[0].ts, 7 * h);
        let syd = vec![bar(-h, 1.0, 1.0, 1.0, 5.0)]; // 23:00 UTC the previous evening = day 0
        assert_eq!(f("A+B").combine(Timeframe::D1, &[ist.clone(), syd]).len(), 1);
        // Intraday: exact open times only.
        let m5a = vec![bar(0, 1.0, 1.0, 1.0, 4.0), bar(300, 1.0, 1.0, 1.0, 6.0)];
        let m5b = vec![bar(300, 1.0, 1.0, 1.0, 3.0), bar(600, 1.0, 1.0, 1.0, 9.0)];
        let out = f("A/B").combine(Timeframe::M5, &[m5a, m5b]);
        assert_eq!((out.len(), out[0].ts, out[0].close), (1, 300, 2.0));
        assert!(f("A/B").combine(Timeframe::D1, &[ist, vec![]]).is_empty());
    }

    #[test]
    fn command_line_spans() {
        assert_eq!(span(&["XU100/USDTRY"]), Some(("XU100/USDTRY".into(), 1)));
        assert_eq!(span(&["SPX/XU100", "5"]), Some(("SPX/XU100".into(), 1)));
        assert_eq!(span(&["CL1-CO1"]), Some(("CL1-CO1".into(), 1)));
        assert_eq!(span(&["(aapl", "+", "msft)", "/", "2", "1W"]), Some(("(AAPL+MSFT)/2".into(), 5)));
        assert_eq!(span(&["GC1*USDTRY/31.1035"]), Some(("GC1*USDTRY/31.1035".into(), 1)));
        assert_eq!(span(&["=btcusd*usdtry"]), Some(("BTCUSD*USDTRY".into(), 1)));
        // Pairs, share classes and plain tickers stay what they were.
        for t in ["BTC/USD", "EUR/USD", "BRK/B", "BRK-B", "AAPL", "BTCUSD", "EURUSD"] {
            assert_eq!(span(&[t]), None, "{t}");
        }
        assert_eq!(span(&["AAPL", "RSI"]), None);
    }
}
