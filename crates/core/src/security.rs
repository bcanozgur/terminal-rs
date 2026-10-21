//! Securities across asset classes, addressed with Bloomberg-style market keys:
//! `AAPL US Equity`, `SPX Index`, `BTCUSD Curncy`, `EURUSD Curncy`, `US10Y Govt`, `CL1 Comdty`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::catalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AssetClass {
    Equity,
    Etf,
    Index,
    Fx,
    Crypto,
    Govt,
    Cmdty,
}

/// The Bloomberg "yellow key" a security lives under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum YellowKey {
    Equity,
    Index,
    Curncy,
    Govt,
    Comdty,
}

impl YellowKey {
    pub const ALL: [YellowKey; 5] =
        [YellowKey::Equity, YellowKey::Index, YellowKey::Curncy, YellowKey::Govt, YellowKey::Comdty];

    /// Accepts `<CRNCY>`, `CURNCY`, `crncy`, `CMDTY`, `COMDTY`, `GOVT`, `INDEX`, `EQUITY`, `EQ`, `ETF`.
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().trim_start_matches('<').trim_end_matches('>').to_ascii_uppercase();
        Some(match t.as_str() {
            "EQUITY" | "EQ" | "EQTY" | "ETF" => YellowKey::Equity,
            "INDEX" | "IDX" => YellowKey::Index,
            "CRNCY" | "CURNCY" | "CURRENCY" | "CCY" | "CRYPTO" | "FX" => YellowKey::Curncy,
            "GOVT" | "GOV" | "RATES" => YellowKey::Govt,
            "CMDTY" | "COMDTY" | "COMMODITY" | "FUT" => YellowKey::Comdty,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            YellowKey::Equity => "Equity",
            YellowKey::Index => "Index",
            YellowKey::Curncy => "Curncy",
            YellowKey::Govt => "Govt",
            YellowKey::Comdty => "Comdty",
        }
    }
}

impl AssetClass {
    pub fn yellow_key(self) -> YellowKey {
        match self {
            AssetClass::Equity | AssetClass::Etf => YellowKey::Equity,
            AssetClass::Index => YellowKey::Index,
            AssetClass::Fx | AssetClass::Crypto => YellowKey::Curncy,
            AssetClass::Govt => YellowKey::Govt,
            AssetClass::Cmdty => YellowKey::Comdty,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AssetClass::Equity => "Equity",
            AssetClass::Etf => "ETF",
            AssetClass::Index => "Index",
            AssetClass::Fx => "FX",
            AssetClass::Crypto => "Crypto",
            AssetClass::Govt => "Govt",
            AssetClass::Cmdty => "Comdty",
        }
    }

    /// Annualised volatility used for synthetic sample data.
    pub fn sample_vol(self) -> f64 {
        match self {
            AssetClass::Equity => 0.30,
            AssetClass::Etf | AssetClass::Index => 0.18,
            AssetClass::Fx => 0.08,
            AssetClass::Crypto => 0.60,
            AssetClass::Govt => 0.20,
            AssetClass::Cmdty => 0.30,
        }
    }

    /// Exchanges trade crypto around the clock; everything else has sessions.
    pub fn trades_24_7(self) -> bool {
        self == AssetClass::Crypto
    }
}

const FIAT: [&str; 26] = [
    "USD", "EUR", "JPY", "GBP", "CHF", "AUD", "CAD", "NZD", "SEK", "NOK", "DKK", "CNH", "CNY", "HKD", "SGD", "MXN",
    "BRL", "ZAR", "TRY", "INR", "KRW", "PLN", "HUF", "CZK", "ILS", "THB",
];

pub fn is_fiat(code: &str) -> bool {
    FIAT.contains(&code)
}

/// A six-letter fiat currency pair such as `EURUSD`.
pub fn is_pair(t: &str) -> bool {
    t.len() == 6 && t.is_ascii() && is_fiat(&t[..3]) && is_fiat(&t[3..])
}

/// A resolved security.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Security {
    pub ticker: String,
    pub class: AssetClass,
}

impl Security {
    pub fn new(ticker: impl Into<String>, class: AssetClass) -> Self {
        Self { ticker: ticker.into().to_ascii_uppercase(), class }
    }

    pub fn equity(ticker: &str) -> Self {
        Self::new(ticker, AssetClass::Equity)
    }

    /// Canonical market key, e.g. `AAPL US Equity`, `BTCUSD Curncy`.
    pub fn key(&self) -> String {
        if self.is_formula() {
            return self.ticker.clone();
        }
        match self.class {
            AssetClass::Equity | AssetClass::Etf => format!("{} US Equity", self.ticker),
            c => format!("{} {}", self.ticker, c.yellow_key().label()),
        }
    }

    /// A formula security (`=XU100/USDTRY`, see [`crate::formula`]); its market key is its ticker.
    pub fn formula(expr: &str) -> Self {
        Self::new(format!("={expr}"), AssetClass::Index)
    }

    pub fn is_formula(&self) -> bool {
        self.ticker.starts_with('=')
    }

    /// The expression behind a formula security or named synthetic such as `GRAMALTIN`.
    pub fn as_formula(&self) -> Option<crate::formula::Formula> {
        if self.is_formula() || (self.class == AssetClass::Index && crate::formula::named(&self.ticker).is_some()) {
            crate::formula::from_symbol(&self.key())?.ok()
        } else {
            None
        }
    }

    /// Resolve user input such as `BTCUSD <CRNCY>`, `aapl us equity`, `US10Y GOVT`, `CL1`.
    /// Returns the security and how many input tokens it consumed.
    pub fn resolve_tokens(tokens: &[&str]) -> Option<(Self, usize)> {
        let (&head, rest) = tokens.split_first()?;
        if let Some((expr, used)) = crate::formula::span(tokens) {
            return Some((Self::formula(&expr), used));
        }
        // `BTC/USD` is a pair; `BRK/B` is a share class.
        let up = head.trim().to_ascii_uppercase();
        let ticker = match up.split_once('/') {
            Some((a, b)) if a.len() >= 3 && b.len() >= 3 => format!("{a}{b}"),
            Some((a, b)) => format!("{a}.{b}"),
            None => up,
        };
        if !looks_like_ticker(&ticker) {
            return None;
        }
        // Optional market/country word (`US`) then a yellow key.
        let mut used = 1;
        let mut key = None;
        for t in rest.iter().take(2) {
            if let Some(k) = YellowKey::parse(t) {
                key = Some(k);
                used += 1;
                break;
            }
            if t.eq_ignore_ascii_case("US") {
                used += 1;
                continue;
            }
            break;
        }
        if key.is_none() && used > 1 {
            used = 1; // a stray `US` without a yellow key belongs to the next function
        }
        Some((Self::classify(&ticker, key), used))
    }

    /// Parse a full key or bare ticker (e.g. persisted settings).
    pub fn parse(input: &str) -> Option<Self> {
        let tokens: Vec<&str> = input.split_whitespace().collect();
        Self::resolve_tokens(&tokens).map(|(s, _)| s)
    }

    fn classify(ticker: &str, key: Option<YellowKey>) -> Self {
        if crate::formula::named(ticker).is_some() && key.is_none_or(|k| k == YellowKey::Index) {
            return Self::new(ticker, AssetClass::Index);
        }
        let listed = catalog::lookup(ticker, key);
        if let Some(l) = listed {
            return Self::new(l.ticker, l.class);
        }
        let class = match key {
            Some(YellowKey::Index) => AssetClass::Index,
            Some(YellowKey::Govt) => AssetClass::Govt,
            Some(YellowKey::Comdty) => AssetClass::Cmdty,
            Some(YellowKey::Curncy) => {
                if ticker.len() == 6 && is_fiat(&ticker[..3]) && is_fiat(&ticker[3..]) {
                    AssetClass::Fx
                } else {
                    AssetClass::Crypto
                }
            }
            Some(YellowKey::Equity) => AssetClass::Equity,
            None if ticker.len() == 6 && is_fiat(&ticker[..3]) && is_fiat(&ticker[3..]) => AssetClass::Fx,
            None => AssetClass::Equity,
        };
        Self::new(ticker, class)
    }

    /// Display name from the catalog, if known.
    pub fn name(&self) -> Option<&'static str> {
        if self.class == AssetClass::Index
            && let Some(n) = crate::formula::named(&self.ticker)
        {
            return Some(n.name);
        }
        catalog::lookup(&self.ticker, Some(self.class.yellow_key())).filter(|l| l.class == self.class).map(|l| l.name)
    }

    /// Base/quote split for currency pairs (`BTCUSD` -> `BTC`, `USD`).
    pub fn pair(&self) -> Option<(&str, &str)> {
        if !matches!(self.class, AssetClass::Fx | AssetClass::Crypto) {
            return None;
        }
        for q in ["USDT", "USDC", "USD", "EUR", "GBP", "JPY", "BTC", "ETH"] {
            if let Some(base) = self.ticker.strip_suffix(q).filter(|b| !b.is_empty()) {
                return Some((base, q));
            }
        }
        None
    }

    /// Price decimals appropriate for the instrument and level.
    pub fn decimals(&self, price: f64) -> usize {
        match self.class {
            AssetClass::Govt => 3,
            AssetClass::Fx if self.ticker.ends_with("JPY") => 3,
            AssetClass::Fx => 4,
            _ => match price.abs() {
                p if p >= 1000.0 => 2,
                p if p >= 1.0 => 2,
                p if p >= 0.01 => 4,
                _ => 6,
            },
        }
    }

    /// Whether congressional (STOCK Act) disclosures can reference this security.
    pub fn has_congress_trades(&self) -> bool {
        matches!(self.class, AssetClass::Equity | AssetClass::Etf)
    }
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

pub fn looks_like_ticker(s: &str) -> bool {
    let core = s.trim_start_matches('^');
    !core.is_empty()
        && core.len() <= 14
        && core.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '='))
        && core.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Security {
        Security::parse(s).unwrap()
    }

    #[test]
    fn market_keys() {
        assert_eq!(r("BTCUSD <CRNCY>"), Security::new("BTCUSD", AssetClass::Crypto));
        assert_eq!(r("EURUSD <CRNCY>"), Security::new("EURUSD", AssetClass::Fx));
        assert_eq!(r("US10Y <GOVT>"), Security::new("US10Y", AssetClass::Govt));
        assert_eq!(r("CL1 <CMDTY>"), Security::new("CL1", AssetClass::Cmdty));
        assert_eq!(r("SPX <INDEX>"), Security::new("SPX", AssetClass::Index));
        assert_eq!(r("AAPL <EQUITY>"), Security::new("AAPL", AssetClass::Equity));
        assert_eq!(r("aapl us equity"), Security::new("AAPL", AssetClass::Equity));
        assert_eq!(r("CL1 Comdty").key(), "CL1 Comdty");
        assert_eq!(r("SPY").class, AssetClass::Etf);
    }

    #[test]
    fn bare_tickers_use_catalog_then_heuristics() {
        assert_eq!(r("BTCUSD").class, AssetClass::Crypto);
        assert_eq!(r("EURUSD").class, AssetClass::Fx);
        assert_eq!(r("AUDNZD").class, AssetClass::Fx);
        assert_eq!(r("SPX").class, AssetClass::Index);
        assert_eq!(r("US10Y").class, AssetClass::Govt);
        assert_eq!(r("ZZZQ").class, AssetClass::Equity);
        assert_eq!(r("PEPEUSD CRNCY").class, AssetClass::Crypto);
        assert_eq!(r("btc/usd").ticker, "BTCUSD");
        assert_eq!(r("brk/b").ticker, "BRK.B");
    }

    #[test]
    fn formulas_and_named_synthetics() {
        let x = r("xu100 / usdtry");
        assert!(x.is_formula() && x.key() == "=XU100/USDTRY" && x.class == AssetClass::Index);
        assert_eq!(r(&x.key()), x); // the market key round-trips
        assert_eq!(x.as_formula().unwrap().leg_keys(), ["XU100 Index", "USDTRY Curncy"]);
        assert_eq!(r("(AAPL+MSFT)/2").key(), "=(AAPL+MSFT)/2");
        let g = r("gramaltin");
        assert_eq!((g.key().as_str(), g.name()), ("GRAMALTIN Index", Some("Gram Gold (TRY)")));
        assert_eq!(g.as_formula().unwrap().canonical(), "GC1*USDTRY/31.1035");
        assert_eq!(r(&g.key()), g);
        // Existing pair / share-class spellings are untouched.
        assert_eq!(r("btc/usd").ticker, "BTCUSD");
        assert_eq!(r("brk/b").ticker, "BRK.B");
    }

    #[test]
    fn tokens_consumed() {
        let t = ["AAPL", "US", "EQUITY", "RSI"];
        assert_eq!(Security::resolve_tokens(&t).unwrap().1, 3);
        let t = ["AAPL", "RSI"];
        assert_eq!(Security::resolve_tokens(&t).unwrap().1, 1);
        let t = ["AAPL", "US", "CONG"];
        assert_eq!(Security::resolve_tokens(&t).unwrap().1, 1);
    }

    #[test]
    fn keys_and_helpers() {
        assert_eq!(Security::new("BTCUSD", AssetClass::Crypto).key(), "BTCUSD Curncy");
        assert_eq!(Security::new("US10Y", AssetClass::Govt).key(), "US10Y Govt");
        assert_eq!(Security::new("BTCUSDT", AssetClass::Crypto).pair(), Some(("BTC", "USDT")));
        assert_eq!(Security::new("USDJPY", AssetClass::Fx).decimals(150.0), 3);
        assert!(!Security::new("BTCUSD", AssetClass::Crypto).has_congress_trades());
        assert_eq!(r("BTCUSD Curncy").name(), Some("Bitcoin / US Dollar"));
    }
}
