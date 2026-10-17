//! Where a quote came from, how late it is, and whether it has gone stale.
//!
//! Every [`Quote`] records its [`QuoteSource`]; [`Timeliness`] says what that source
//! documents about delay, and [`Quote::health`] compares the last trade with an
//! asset-class threshold *while the market is open*. Closed markets are never stale.
//!
//! Session rules are deliberately simple: US stocks 09:30-16:00 ET Mon-Fri, Borsa Istanbul
//! 10:00-18:00 Istanbul time Mon-Fri, crypto 24/7, FX and futures Sun 17:00 ET - Fri 17:00 ET.
//! Exchange holidays are ignored (a holiday reads as an open market with a stale quote), and
//! so are lunch breaks, auctions and the CME daily maintenance break.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tr_core::AssetClass;

use crate::{Quote, security, yahoo};

/// The provider that produced a quote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QuoteSource {
    /// Quotes cached before sources were recorded.
    #[default]
    Unknown,
    Binance,
    Coinbase,
    Yahoo,
    Alpaca,
    Treasury,
    Fred,
    /// Synthetic prices: offline mode or a source that was unreachable.
    Sample,
}

impl QuoteSource {
    pub fn name(self) -> &'static str {
        match self {
            QuoteSource::Unknown => "Unknown",
            QuoteSource::Binance => "Binance",
            QuoteSource::Coinbase => "Coinbase",
            QuoteSource::Yahoo => "Yahoo",
            QuoteSource::Alpaca => "Alpaca",
            QuoteSource::Treasury => "Treasury.gov",
            QuoteSource::Fred => "FRED",
            QuoteSource::Sample => "Sample",
        }
    }

    /// The source a market key is normally served by (what a sample quote stands in for).
    pub fn intended(symbol: &str) -> QuoteSource {
        match security(symbol).class {
            AssetClass::Crypto => QuoteSource::Binance,
            AssetClass::Govt => QuoteSource::Treasury,
            _ => QuoteSource::Yahoo,
        }
    }
}

/// How current a source's prices are, as the source documents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeliness {
    Live,
    /// Delayed by about this many minutes; `None` when the delay varies by exchange.
    Delayed(Option<u32>),
    EndOfDay,
    Simulated,
    Unknown,
}

impl Timeliness {
    pub fn label(self) -> String {
        match self {
            Timeliness::Live => "live".into(),
            Timeliness::Delayed(Some(m)) => format!("{m}m delayed"),
            Timeliness::Delayed(None) => "delayed".into(),
            Timeliness::EndOfDay => "end of day".into(),
            Timeliness::Simulated => "simulated".into(),
            Timeliness::Unknown => String::new(),
        }
    }
}

/// The trading calendar a market key follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Market {
    Crypto,
    UsEquity,
    Bist,
    Fx,
    Futures,
    Govt,
    /// Other exchanges and indices: no session rules, never flagged stale.
    Other,
}

const US_INDICES: &[&str] = &["^GSPC", "^DJI", "^IXIC", "^NDX", "^RUT", "^VIX", "^SP500TR"];

pub fn market_of(symbol: &str) -> Market {
    let sec = security(symbol);
    let y = yahoo::yahoo_symbol(&sec);
    match sec.class {
        AssetClass::Crypto => Market::Crypto,
        AssetClass::Fx => Market::Fx,
        AssetClass::Cmdty => Market::Futures,
        AssetClass::Govt => Market::Govt,
        AssetClass::Equity | AssetClass::Etf | AssetClass::Index if y.ends_with(".IS") => Market::Bist,
        AssetClass::Index if US_INDICES.contains(&y.as_str()) => Market::UsEquity,
        AssetClass::Index => Market::Other,
        // `SAP.DE`, `VOD.L`: a foreign listing follows its own calendar.
        AssetClass::Equity | AssetClass::Etf if y.rsplit_once('.').is_some_and(|(_, s)| s.len() >= 2) => Market::Other,
        AssetClass::Equity | AssetClass::Etf => Market::UsEquity,
    }
}

/// Seconds without a trade after which an open market's quote counts as stale.
pub fn stale_after(m: Market) -> Option<i64> {
    match m {
        Market::Crypto => Some(120),
        Market::UsEquity | Market::Bist | Market::Futures => Some(20 * 60),
        Market::Fx => Some(10 * 60),
        // Published once a day; five days covers a long weekend plus a holiday.
        Market::Govt => Some(5 * 86_400),
        Market::Other => None,
    }
}

enum Session {
    Closed,
    /// Open since this time (Unix seconds; 0 = always open).
    Open(i64),
}

/// `now` in a zone `shift` seconds from UTC: (weekday with Monday = 0, seconds of day, UTC
/// time of local midnight).
fn zoned(now: i64, shift: i64) -> (i64, i64, i64) {
    let local = now + shift;
    let days = local.div_euclid(86_400);
    (((days + 3).rem_euclid(7)), local.rem_euclid(86_400), days * 86_400 - shift)
}

/// Eastern time's offset from UTC in seconds (the day-level DST rule is enough here).
fn et_shift(now: i64) -> i64 {
    let date = chrono::DateTime::from_timestamp(now - 5 * 3600, 0).map(|d| d.date_naive());
    if date.is_some_and(|d: NaiveDate| crate::eco::us_dst(d)) { -4 * 3600 } else { -5 * 3600 }
}

fn session(m: Market, now: i64) -> Session {
    const H: i64 = 3600;
    let weekday_window = |shift: i64, open: i64, close: i64| {
        let (wd, sod, midnight) = zoned(now, shift);
        if wd < 5 && (open..close).contains(&sod) { Session::Open(midnight + open) } else { Session::Closed }
    };
    match m {
        Market::Crypto => Session::Open(0),
        Market::UsEquity => weekday_window(et_shift(now), 9 * H + 1800, 16 * H),
        Market::Bist => weekday_window(3 * H, 10 * H, 18 * H),
        Market::Fx | Market::Futures => {
            let (wd, sod, midnight) = zoned(now, et_shift(now));
            let open = match wd {
                0..=3 => true,
                4 => sod < 17 * H,
                5 => false,
                _ => sod >= 17 * H,
            };
            // The week's open: Sunday 17:00 ET.
            let back = if wd == 6 { 0 } else { wd + 1 };
            if open { Session::Open(midnight - back * 86_400 + 17 * H) } else { Session::Closed }
        }
        Market::Govt | Market::Other => Session::Closed,
    }
}

/// Whether the market's regular session is open at `now` (Unix seconds).
pub fn is_open(m: Market, now: i64) -> bool {
    matches!(session(m, now), Session::Open(_))
}

/// What the quote's age says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Current enough for its source (or its age cannot be told).
    Fresh,
    /// The market is open but nothing newer than `age` seconds has arrived.
    Stale {
        age: i64,
    },
    /// The market is shut; the quote is the last price of the session.
    Closed,
    /// Published once a day.
    EndOfDay,
    Simulated,
}

impl Quote {
    pub fn timeliness(&self) -> Timeliness {
        match self.source {
            QuoteSource::Unknown => Timeliness::Unknown,
            QuoteSource::Binance | QuoteSource::Coinbase | QuoteSource::Alpaca => Timeliness::Live,
            QuoteSource::Treasury | QuoteSource::Fred => Timeliness::EndOfDay,
            QuoteSource::Sample => Timeliness::Simulated,
            // Yahoo reports each instrument's delay (`exchangeDataDelayedBy`); 0 is not trusted to
            // mean real-time, so it stays "delayed". Without it, NASDAQ/NYSE and Borsa Istanbul
            // are taken as ~15 minutes and the rest (FX, futures, other indices) as unknown.
            QuoteSource::Yahoo => match self.delay_min {
                Some(n) if n > 0 => Timeliness::Delayed(Some(n)),
                Some(_) => Timeliness::Delayed(None),
                None => match market_of(&self.symbol) {
                    Market::UsEquity | Market::Bist => Timeliness::Delayed(Some(15)),
                    _ => Timeliness::Delayed(None),
                },
            },
        }
    }

    /// The moment the price was last updated: the last trade when the source reports it,
    /// otherwise the quote's own timestamp.
    pub fn as_of(&self) -> i64 {
        self.traded.unwrap_or(self.ts)
    }

    /// Staleness at `now` (Unix seconds).
    pub fn health(&self, now: i64) -> Health {
        match self.timeliness() {
            Timeliness::Simulated => return Health::Simulated,
            Timeliness::EndOfDay => {
                let age = now - self.ts;
                return if stale_after(Market::Govt).is_some_and(|l| age > l) {
                    Health::Stale { age }
                } else {
                    Health::EndOfDay
                };
            }
            _ => {}
        }
        let m = market_of(&self.symbol);
        let Session::Open(since) = session(m, now) else {
            return if m == Market::Other { Health::Fresh } else { Health::Closed };
        };
        // Only a reported trade time can be judged; a session's first minutes are measured
        // from its open, since the last trade is from the day before.
        // A delayed feed's trade times are already that old, so its delay adds to the limit.
        let delay = match self.timeliness() {
            Timeliness::Delayed(Some(n)) => i64::from(n) * 60,
            _ => 0,
        };
        match (self.traded, stale_after(m)) {
            (Some(t), Some(limit)) if now - t.max(since) > limit + delay => Health::Stale { age: now - t.max(since) },
            _ => Health::Fresh,
        }
    }
}

/// Compact age: `45s`, `25m`, `3h`, `2d`.
pub fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", (s + 30) / 60),
        5400..=129_599 => format!("{}h", (s + 1800) / 3600),
        _ => format!("{}d", (s + 43_200) / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap().and_utc().timestamp()
    }

    fn q(symbol: &str, source: QuoteSource, traded: Option<i64>) -> Quote {
        Quote {
            symbol: symbol.into(),
            last: 1.0,
            prev_close: 1.0,
            ts: traded.unwrap_or_default(),
            spark: vec![],
            extended: None,
            source,
            traded,
            delay_min: None,
        }
    }

    // 2026-10-14 is a Wednesday and US daylight time (EDT = UTC-4).
    const AAPL: &str = "AAPL US Equity";

    #[test]
    fn us_session_follows_eastern_time() {
        assert!(!is_open(Market::UsEquity, utc(2026, 10, 14, 13, 29)));
        assert!(is_open(Market::UsEquity, utc(2026, 10, 14, 13, 30)));
        assert!(is_open(Market::UsEquity, utc(2026, 10, 14, 19, 59)));
        assert!(!is_open(Market::UsEquity, utc(2026, 10, 14, 20, 0)));
        // Standard time (EST = UTC-5): 2026-12-16.
        assert!(!is_open(Market::UsEquity, utc(2026, 12, 16, 14, 29)));
        assert!(is_open(Market::UsEquity, utc(2026, 12, 16, 14, 30)));
        assert!(!is_open(Market::UsEquity, utc(2026, 12, 16, 21, 0)));
    }

    #[test]
    fn weekends_are_closed() {
        let sat = utc(2026, 10, 17, 15, 0);
        assert!(!is_open(Market::UsEquity, sat));
        assert!(!is_open(Market::Bist, sat));
        assert!(!is_open(Market::Fx, sat));
        assert!(is_open(Market::Crypto, sat));
    }

    #[test]
    fn bist_session_is_istanbul_time() {
        assert!(!is_open(Market::Bist, utc(2026, 10, 14, 6, 59)));
        assert!(is_open(Market::Bist, utc(2026, 10, 14, 7, 0)));
        assert!(is_open(Market::Bist, utc(2026, 10, 14, 14, 59)));
        assert!(!is_open(Market::Bist, utc(2026, 10, 14, 15, 0)));
    }

    #[test]
    fn fx_runs_sunday_evening_to_friday_evening() {
        assert!(!is_open(Market::Fx, utc(2026, 10, 18, 20, 59))); // Sun 16:59 EDT
        assert!(is_open(Market::Fx, utc(2026, 10, 18, 21, 0))); // Sun 17:00 EDT
        assert!(is_open(Market::Fx, utc(2026, 10, 14, 3, 0)));
        assert!(is_open(Market::Fx, utc(2026, 10, 16, 20, 59))); // Fri 16:59 EDT
        assert!(!is_open(Market::Fx, utc(2026, 10, 16, 21, 0)));
    }

    #[test]
    fn market_routing() {
        assert_eq!(market_of(AAPL), Market::UsEquity);
        assert_eq!(market_of("THYAO TR Equity"), Market::Bist);
        assert_eq!(market_of("THYAO.IS"), Market::Bist);
        assert_eq!(market_of("XU100 Index"), Market::Bist);
        assert_eq!(market_of("BTCUSD Curncy"), Market::Crypto);
        assert_eq!(market_of("EURUSD Curncy"), Market::Fx);
        assert_eq!(market_of("US10Y Govt"), Market::Govt);
        assert_eq!(market_of("CL1 Comdty"), Market::Futures);
        assert_eq!(market_of("SPX Index"), Market::UsEquity);
        assert_eq!(market_of("DAX Index"), Market::Other);
        assert_eq!(market_of("SAP.DE Equity"), Market::Other);
    }

    #[test]
    fn delay_classes() {
        let t = |s: &str, src| q(s, src, Some(0)).timeliness();
        assert_eq!(t("BTCUSD Curncy", QuoteSource::Binance), Timeliness::Live);
        assert_eq!(t("BTCUSD Curncy", QuoteSource::Coinbase), Timeliness::Live);
        assert_eq!(t(AAPL, QuoteSource::Alpaca), Timeliness::Live);
        assert_eq!(t(AAPL, QuoteSource::Yahoo), Timeliness::Delayed(Some(15)));
        assert_eq!(t("THYAO TR Equity", QuoteSource::Yahoo), Timeliness::Delayed(Some(15)));
        assert_eq!(t("EURUSD Curncy", QuoteSource::Yahoo), Timeliness::Delayed(None));
        assert_eq!(t("US10Y Govt", QuoteSource::Treasury), Timeliness::EndOfDay);
        assert_eq!(t("US10Y Govt", QuoteSource::Fred), Timeliness::EndOfDay);
        assert_eq!(t(AAPL, QuoteSource::Sample), Timeliness::Simulated);
        assert_eq!(t(AAPL, QuoteSource::Unknown), Timeliness::Unknown);
        assert_eq!(Timeliness::Delayed(Some(15)).label(), "15m delayed");
    }

    #[test]
    fn reported_delay_wins_over_the_table() {
        let with = |s: &str, d| Quote { delay_min: d, ..q(s, QuoteSource::Yahoo, Some(0)) };
        assert_eq!(with("CL1 Comdty", Some(10)).timeliness(), Timeliness::Delayed(Some(10)));
        assert_eq!(with(AAPL, Some(20)).timeliness(), Timeliness::Delayed(Some(20)));
        // Zero is not taken as proof of real-time.
        assert_eq!(with("SPY US Equity", Some(0)).timeliness(), Timeliness::Delayed(None));
        assert_eq!(with(AAPL, None).timeliness(), Timeliness::Delayed(Some(15)));
        // A 20m feed tolerates 20m more quiet than a real-time one.
        let now = utc(2026, 10, 14, 17, 0);
        let age =
            |d, mins: i64| Quote { delay_min: d, ..q(AAPL, QuoteSource::Yahoo, Some(now - mins * 60)) }.health(now);
        assert_eq!(age(Some(20), 39), Health::Fresh);
        assert!(matches!(age(Some(20), 41), Health::Stale { .. }));
    }

    #[test]
    fn stale_only_while_the_market_is_open() {
        let now = utc(2026, 10, 14, 17, 0); // Wed 13:00 EDT
        let fresh = q(AAPL, QuoteSource::Yahoo, Some(now - 30 * 60)); // 15m feed delay + 15m quiet
        assert_eq!(fresh.health(now), Health::Fresh);
        let stale = q(AAPL, QuoteSource::Yahoo, Some(now - 40 * 60));
        assert_eq!(stale.health(now), Health::Stale { age: 40 * 60 });
        // After the close the same old trade is simply the closing price.
        let after = utc(2026, 10, 14, 22, 0);
        assert_eq!(q(AAPL, QuoteSource::Yahoo, Some(utc(2026, 10, 14, 20, 0))).health(after), Health::Closed);
        // Saturday.
        let sat = utc(2026, 10, 17, 15, 0);
        assert_eq!(q(AAPL, QuoteSource::Yahoo, Some(utc(2026, 10, 16, 20, 0))).health(sat), Health::Closed);
    }

    #[test]
    fn opening_minutes_are_measured_from_the_open() {
        // 13:40 EDT-less: 09:40 ET, the last trade is still yesterday's close.
        let now = utc(2026, 10, 14, 13, 40);
        let prev_close = utc(2026, 10, 13, 20, 0);
        assert_eq!(q(AAPL, QuoteSource::Yahoo, Some(prev_close)).health(now), Health::Fresh);
        // Past the limit plus the feed delay, still nothing: stale, aged from the open.
        let later = utc(2026, 10, 14, 14, 10);
        assert_eq!(q(AAPL, QuoteSource::Yahoo, Some(prev_close)).health(later), Health::Stale { age: 40 * 60 });
    }

    #[test]
    fn thresholds_per_class() {
        let now = utc(2026, 10, 14, 12, 0); // Wed: BIST 15:00, FX and crypto open
        let at = |sym: &str, src, age: i64| q(sym, src, Some(now - age)).health(now);
        assert_eq!(at("BTCUSD Curncy", QuoteSource::Binance, 100), Health::Fresh);
        assert_eq!(at("BTCUSD Curncy", QuoteSource::Binance, 130), Health::Stale { age: 130 });
        assert_eq!(at("EURUSD Curncy", QuoteSource::Yahoo, 9 * 60), Health::Fresh);
        assert_eq!(at("EURUSD Curncy", QuoteSource::Yahoo, 11 * 60), Health::Stale { age: 660 });
        assert_eq!(at("THYAO TR Equity", QuoteSource::Yahoo, 34 * 60), Health::Fresh);
        assert_eq!(at("THYAO TR Equity", QuoteSource::Yahoo, 36 * 60), Health::Stale { age: 2160 });
        // Crypto never closes, even on a weekend.
        let sat = utc(2026, 10, 17, 12, 0);
        assert_eq!(q("BTCUSD Curncy", QuoteSource::Binance, Some(sat - 600)).health(sat), Health::Stale { age: 600 });
    }

    #[test]
    fn unknown_trade_time_is_never_stale() {
        let now = utc(2026, 10, 14, 17, 0);
        assert_eq!(q(AAPL, QuoteSource::Yahoo, None).health(now), Health::Fresh);
    }

    #[test]
    fn end_of_day_and_sample() {
        let now = utc(2026, 10, 19, 12, 0);
        let mut t = q("US10Y Govt", QuoteSource::Treasury, None);
        t.ts = utc(2026, 10, 16, 0, 0); // Friday's close read on Monday
        assert_eq!(t.health(now), Health::EndOfDay);
        t.ts = utc(2026, 10, 9, 0, 0);
        assert!(matches!(t.health(now), Health::Stale { .. }));
        assert_eq!(q(AAPL, QuoteSource::Sample, None).health(now), Health::Simulated);
    }

    #[test]
    fn age_format() {
        assert_eq!(fmt_age(45), "45s");
        assert_eq!(fmt_age(25 * 60), "25m");
        assert_eq!(fmt_age(3 * 3600), "3h");
        assert_eq!(fmt_age(3 * 86_400), "3d");
    }

    #[test]
    fn old_cache_rows_still_deserialize() {
        let q: Quote = serde_json::from_str(r#"{"symbol":"X","last":1.0,"prev_close":1.0,"ts":5}"#).unwrap();
        assert_eq!(q.source, QuoteSource::Unknown);
        assert_eq!(q.traded, None);
    }
}
