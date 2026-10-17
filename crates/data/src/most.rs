//! Market movers (`MOST`): US day gainers, day losers and most active stocks from Yahoo's
//! predefined screeners (`/v1/finance/screener/predefined/saved`). The endpoint answers with
//! only the browser-like user agent the rest of the Yahoo code uses: no crumb or cookie.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

const URL: &str = "https://query1.finance.yahoo.com/v1/finance/screener/predefined/saved";

/// Rows requested per list (Yahoo allows up to 250).
pub const COUNT: usize = 100;

/// The three predefined screeners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Gainers,
    Losers,
    Active,
}

impl List {
    pub const ALL: [List; 3] = [List::Gainers, List::Losers, List::Active];

    pub fn id(self) -> &'static str {
        match self {
            List::Gainers => "day_gainers",
            List::Losers => "day_losers",
            List::Active => "most_actives",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            List::Gainers => "Gainers",
            List::Losers => "Losers",
            List::Active => "Most Active",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mover {
    pub symbol: String,
    pub name: String,
    pub price: f64,
    pub change: f64,
    pub change_pct: f64,
    pub volume: Option<f64>,
    /// Three month average daily volume.
    pub avg_volume: Option<f64>,
    pub market_cap: Option<f64>,
    pub exchange: String,
    /// Quote delay in minutes as reported by Yahoo.
    pub delay_min: Option<u32>,
    /// `REGULAR`, `PRE`, `POST`, `CLOSED`, ...
    pub market_state: String,
}

impl Mover {
    /// Today's volume as a multiple of the three month average.
    pub fn rel_volume(&self) -> Option<f64> {
        match (self.volume, self.avg_volume) {
            (Some(v), Some(a)) if a > 0.0 => Some(v / a),
            _ => None,
        }
    }
}

/// One fetch of all three lists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Board {
    pub gainers: Vec<Mover>,
    pub losers: Vec<Mover>,
    pub active: Vec<Mover>,
    /// Unix seconds.
    pub fetched: i64,
}

impl Board {
    pub fn list(&self, l: List) -> &[Mover] {
        match l {
            List::Gainers => &self.gainers,
            List::Losers => &self.losers,
            List::Active => &self.active,
        }
    }

    /// The quote delay Yahoo reports for the most rows, if any.
    pub fn delay_min(&self) -> Option<u32> {
        let mut counts: std::collections::HashMap<u32, usize> = Default::default();
        for m in self.gainers.iter().chain(&self.losers).chain(&self.active) {
            if let Some(d) = m.delay_min {
                *counts.entry(d).or_default() += 1;
            }
        }
        counts.into_iter().max_by_key(|&(d, n)| (n, std::cmp::Reverse(d))).map(|(d, _)| d)
    }

    /// The market state of the first row (`REGULAR`, `PRE`, `POST`, `CLOSED`).
    pub fn market_state(&self) -> Option<&str> {
        self.gainers
            .iter()
            .chain(&self.active)
            .chain(&self.losers)
            .map(|m| m.market_state.as_str())
            .find(|s| !s.is_empty())
    }
}

#[derive(Deserialize)]
struct Resp {
    finance: Finance,
}

#[derive(Deserialize)]
struct Finance {
    result: Option<Vec<Screen>>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Screen {
    #[serde(default)]
    quotes: Vec<Quote>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quote {
    symbol: Option<String>,
    short_name: Option<String>,
    long_name: Option<String>,
    regular_market_price: Option<f64>,
    regular_market_change: Option<f64>,
    regular_market_change_percent: Option<f64>,
    regular_market_volume: Option<f64>,
    average_daily_volume3_month: Option<f64>,
    market_cap: Option<f64>,
    full_exchange_name: Option<String>,
    exchange_data_delayed_by: Option<u32>,
    market_state: Option<String>,
}

/// Parse one screener response. Rows without a symbol or price are skipped.
pub fn parse_screener(json: &str) -> Result<Vec<Mover>> {
    let r: Resp = serde_json::from_str(json).context("yahoo screener: unexpected response")?;
    if let Some(e) = r.finance.error {
        bail!("yahoo screener: {}", e.description.or(e.code).unwrap_or_else(|| "error".into()));
    }
    let screen =
        r.finance.result.and_then(|v| v.into_iter().next()).ok_or_else(|| anyhow!("yahoo screener: empty result"))?;
    Ok(screen
        .quotes
        .into_iter()
        .filter_map(|q| {
            let symbol = q.symbol.filter(|s| !s.is_empty())?;
            let price = q.regular_market_price.filter(|p| p.is_finite())?;
            Some(Mover {
                name: q.long_name.or(q.short_name).unwrap_or_default(),
                price,
                change: q.regular_market_change.unwrap_or(0.0),
                change_pct: q.regular_market_change_percent.unwrap_or(0.0),
                volume: q.regular_market_volume,
                avg_volume: q.average_daily_volume3_month,
                market_cap: q.market_cap.filter(|c| *c > 0.0),
                exchange: q.full_exchange_name.unwrap_or_default(),
                delay_min: q.exchange_data_delayed_by,
                market_state: q.market_state.unwrap_or_default(),
                symbol,
            })
        })
        .collect())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; terminal-rs)")
        .timeout(Duration::from_secs(20))
        .build()?)
}

async fn fetch_list(client: &reqwest::Client, list: List) -> Result<Vec<Mover>> {
    let resp = client
        .get(URL)
        .query(&[
            ("scrIds", list.id()),
            ("count", &COUNT.to_string()),
            ("formatted", "false"),
            ("lang", "en-US"),
            ("region", "US"),
        ])
        .send()
        .await
        .with_context(|| format!("yahoo screener {}", list.id()))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("yahoo screener {}: HTTP {status}", list.id());
    }
    parse_screener(&body)
}

/// Fetch all three lists. Fails with the first error: a half-filled board would pass off a
/// missing list as an empty market.
pub async fn fetch() -> Result<Board> {
    let c = client()?;
    let (g, l, a) =
        tokio::join!(fetch_list(&c, List::Gainers), fetch_list(&c, List::Losers), fetch_list(&c, List::Active));
    Ok(Board { gainers: g?, losers: l?, active: a?, fetched: chrono::Utc::now().timestamp() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = include_str!("../tests/fixtures/yahoo_screener.json");

    #[test]
    fn parses_rows_and_skips_incomplete() {
        let rows = parse_screener(FIX).unwrap();
        assert_eq!(rows.len(), 3, "BAD has no price");
        let m = &rows[0];
        assert_eq!(m.symbol, "BSP");
        assert_eq!(m.name, "Bending Spoons S.p.A.");
        assert!((m.price - 47.01).abs() < 1e-9);
        assert!(m.change < 0.0 && m.change_pct < -15.0);
        assert_eq!(m.delay_min, Some(15));
        assert_eq!(m.exchange, "NasdaqGS");
        let rel = m.rel_volume().unwrap();
        assert!((rel - 4_867_393.0 / 2_716_300.0).abs() < 1e-9);
    }

    #[test]
    fn api_error_is_reported() {
        let e = parse_screener(r#"{"finance":{"result":null,"error":{"code":"Bad Request","description":"nope"}}}"#)
            .unwrap_err();
        assert!(format!("{e}").contains("nope"));
        assert!(parse_screener(r#"{"finance":{"result":[],"error":null}}"#).is_err());
        assert!(parse_screener("<html>").is_err());
    }

    #[test]
    fn board_delay_and_state() {
        let rows = parse_screener(FIX).unwrap();
        let b = Board { gainers: rows.clone(), losers: vec![], active: rows, fetched: 0 };
        assert_eq!(b.delay_min(), Some(15));
        assert_eq!(b.market_state(), Some("REGULAR"));
        assert_eq!(b.list(List::Losers).len(), 0);
    }
}
