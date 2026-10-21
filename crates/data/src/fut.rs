//! Futures curve, VIX term structure and sector heatmap (`FUT`, `VIX`, `HEAT`).
//!
//! Sources, all free and without login:
//! - Yahoo chart endpoint for futures contracts (`CLZ26.NYM`), the Cboe volatility indices
//!   (`^VIX9D` ... `^VIX1Y`) and the SPDR sector / country ETFs. Quotes are delayed.
//! - Cboe's VX futures settlement CSV (`www.cboe.com/us/futures/market_statistics/settlement/csv`):
//!   end-of-day settlement prices of the monthly VIX futures. Cboe's delayed-quote JSON for the
//!   individual VX contracts answers 403 without a browser session, so there is no intraday strip.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, NaiveDate};
use futures::StreamExt;
use serde::Deserialize;

const CHART: &str = "https://query1.finance.yahoo.com/v8/finance/chart";
const VX_CSV: &str = "https://www.cboe.com/us/futures/market_statistics/settlement/csv";

/// Contracts shown on the curve.
pub const CURVE_LEN: usize = 12;
/// Parallel requests (polite to Yahoo).
const CONCURRENCY: usize = 6;
/// A contract whose last trade is older than this has expired or does not trade.
const STALE_SECS: i64 = 10 * 86_400;

// ---- futures roots and contract months -------------------------------------------------

/// A futures product: its root symbol, Yahoo exchange suffix and listed contract months.
#[derive(Debug, PartialEq)]
pub struct Root {
    pub code: &'static str,
    pub name: &'static str,
    /// Yahoo suffix, e.g. `NYM` for `CLZ26.NYM`.
    pub exchange: &'static str,
    /// Listed month codes (`FGHJKMNQUVXZ`), in calendar order.
    pub months: &'static str,
    /// Quote unit, shown next to the price.
    pub unit: &'static str,
}

const ALL_MONTHS: &str = "FGHJKMNQUVXZ";

/// Selectable roots, in chip order.
pub const ROOTS: &[Root] = &[
    Root { code: "ES", name: "E-mini S&P 500", exchange: "CME", months: "HMUZ", unit: "index pts" },
    Root { code: "NQ", name: "E-mini Nasdaq-100", exchange: "CME", months: "HMUZ", unit: "index pts" },
    Root { code: "YM", name: "E-mini Dow", exchange: "CBT", months: "HMUZ", unit: "index pts" },
    Root { code: "RTY", name: "E-mini Russell 2000", exchange: "CME", months: "HMUZ", unit: "index pts" },
    Root { code: "CL", name: "WTI crude oil", exchange: "NYM", months: ALL_MONTHS, unit: "USD/bbl" },
    Root { code: "NG", name: "Natural gas", exchange: "NYM", months: ALL_MONTHS, unit: "USD/MMBtu" },
    Root { code: "GC", name: "Gold", exchange: "CMX", months: "GJMQVZ", unit: "USD/oz" },
    Root { code: "SI", name: "Silver", exchange: "CMX", months: "HKNUZ", unit: "USD/oz" },
    Root { code: "HG", name: "Copper", exchange: "CMX", months: "HKNUZ", unit: "USD/lb" },
    Root { code: "ZB", name: "U.S. Treasury bond", exchange: "CBT", months: "HMUZ", unit: "points" },
    Root { code: "ZN", name: "10-year T-note", exchange: "CBT", months: "HMUZ", unit: "points" },
    Root { code: "ZC", name: "Corn", exchange: "CBT", months: "HKNUZ", unit: "USc/bu" },
    Root { code: "ZS", name: "Soybeans", exchange: "CBT", months: "FHKNQUX", unit: "USc/bu" },
    Root { code: "ZW", name: "Chicago wheat", exchange: "CBT", months: "HKNUZ", unit: "USc/bu" },
    Root { code: "6E", name: "Euro FX", exchange: "CME", months: "HMUZ", unit: "USD/EUR" },
    Root { code: "KC", name: "Coffee", exchange: "NYB", months: "HKNUZ", unit: "USc/lb" },
];

/// The root named `code` (case-insensitive).
pub fn root(code: &str) -> Option<&'static Root> {
    let c = code.trim();
    ROOTS.iter().find(|r| r.code.eq_ignore_ascii_case(c))
}

/// Month code letter for a calendar month 1-12.
pub fn month_code(month: u32) -> Option<char> {
    ALL_MONTHS.chars().nth(month.checked_sub(1)? as usize)
}

/// Calendar month 1-12 for a month code letter.
pub fn month_from_code(c: char) -> Option<u32> {
    ALL_MONTHS.chars().position(|m| m == c.to_ascii_uppercase()).map(|i| i as u32 + 1)
}

pub fn month_name(month: u32) -> &'static str {
    const N: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    N.get(month.wrapping_sub(1) as usize).copied().unwrap_or("?")
}

/// Yahoo ticker of one contract: `CLZ26.NYM`.
pub fn contract_symbol(root: &Root, month: u32, year: i32) -> Option<String> {
    Some(format!("{}{}{:02}.{}", root.code, month_code(month)?, year.rem_euclid(100), root.exchange))
}

fn is_weekday(d: NaiveDate) -> bool {
    d.weekday().number_from_monday() <= 5
}

/// `n` weekdays before `d` (holidays are not modelled, so a rule can land a day late).
fn weekdays_before(mut d: NaiveDate, n: u32) -> NaiveDate {
    let mut left = n;
    while left > 0 {
        d = d.pred_opt().unwrap_or(d);
        if is_weekday(d) {
            left -= 1;
        }
    }
    d
}

fn last_weekday_of(year: i32, month: u32) -> Option<NaiveDate> {
    let first_next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let mut d = first_next.pred_opt()?;
    while !is_weekday(d) {
        d = d.pred_opt()?;
    }
    Some(d)
}

/// The `nth` (1-based) `weekday` of a month.
fn nth_weekday(year: i32, month: u32, weekday: chrono::Weekday, nth: u32) -> Option<NaiveDate> {
    NaiveDate::from_weekday_of_month_opt(year, month, weekday, nth as u8)
}

/// Approximate last trading day of a contract by the exchange's rule for the product.
/// Yahoo keeps quoting an expired contract for days with its final settlement, so the curve
/// would otherwise start with a dead month (CLX26 stopped on 20 Oct 2026 but still showed
/// as the front on the 21st).
pub fn last_trade(code: &str, month: u32, year: i32) -> Option<NaiveDate> {
    use chrono::Weekday::{Fri, Wed};
    let (py, pm) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
    Some(match code {
        // Third Friday of the contract month.
        "ES" | "NQ" | "YM" | "RTY" => nth_weekday(year, month, Fri, 3)?,
        // Three business days before the 25th of the prior month (before the business day
        // preceding the 25th when that is not one).
        "CL" => {
            let mut d = NaiveDate::from_ymd_opt(py, pm, 25)?;
            while !is_weekday(d) {
                d = d.pred_opt()?;
            }
            weekdays_before(d, 3)
        }
        // Three business days before the first day of the contract month.
        "NG" => weekdays_before(NaiveDate::from_ymd_opt(year, month, 1)?, 3),
        // Third last business day of the contract month.
        "GC" | "SI" | "HG" => weekdays_before(last_weekday_of(year, month)?, 2),
        // Seventh business day before the last business day of the contract month.
        "ZB" | "ZN" => weekdays_before(last_weekday_of(year, month)?, 7),
        // Business day before the 15th of the contract month.
        "ZC" | "ZS" | "ZW" => weekdays_before(NaiveDate::from_ymd_opt(year, month, 15)?, 1),
        // Two business days before the third Wednesday.
        "6E" => weekdays_before(nth_weekday(year, month, Wed, 3)?, 2),
        // One business day before first notice: eight before the last business day.
        "KC" => weekdays_before(last_weekday_of(year, month)?, 8),
        _ => return None,
    })
}

/// Contract months to probe, starting with the current month: the next `n` months in the
/// root's listing cycle as `(month, year)`.
pub fn candidates(root: &Root, today: NaiveDate, n: usize) -> Vec<(u32, i32)> {
    let mut out = Vec::new();
    let (mut y, mut m) = (today.year(), today.month());
    for _ in 0..120 {
        if out.len() >= n {
            break;
        }
        if month_code(m).is_some_and(|c| root.months.contains(c)) {
            out.push((m, y));
        }
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    out
}

// ---- Yahoo chart parsing ---------------------------------------------------------------

/// The symbol does not exist (an unlisted contract month): not an error worth showing.
#[derive(Debug)]
pub struct NotFound(pub String);

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: not found", self.0)
    }
}

impl std::error::Error for NotFound {}

/// Daily history and the latest quote of one Yahoo symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub symbol: String,
    pub name: String,
    pub currency: String,
    /// Latest price.
    pub price: f64,
    /// Unix seconds of the latest trade.
    pub time: i64,
    /// Exchange offset from UTC in seconds, to find calendar days.
    pub gmtoffset: i64,
    pub volume: Option<f64>,
    /// `(unix seconds, close)` oldest first, nulls dropped.
    pub bars: Vec<(i64, f64)>,
}

#[derive(Deserialize)]
struct ChartResp {
    chart: ChartBody,
}

#[derive(Deserialize)]
struct ChartBody {
    result: Option<Vec<ChartResult>>,
    error: Option<ChartError>,
}

#[derive(Deserialize)]
struct ChartError {
    code: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ChartResult {
    meta: Meta,
    #[serde(default)]
    timestamp: Vec<i64>,
    indicators: Indicators,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    symbol: Option<String>,
    long_name: Option<String>,
    short_name: Option<String>,
    currency: Option<String>,
    regular_market_price: Option<f64>,
    regular_market_time: Option<i64>,
    regular_market_volume: Option<f64>,
    gmtoffset: Option<i64>,
}

#[derive(Deserialize)]
struct Indicators {
    #[serde(default)]
    quote: Vec<QuoteArrays>,
}

#[derive(Deserialize)]
struct QuoteArrays {
    #[serde(default)]
    close: Vec<Option<f64>>,
}

/// Parse a Yahoo chart response. A missing symbol is reported as [`NotFound`].
pub fn parse_series(json: &str, symbol: &str) -> Result<Series> {
    let r: ChartResp = serde_json::from_str(json).with_context(|| format!("yahoo {symbol}: unexpected response"))?;
    if let Some(e) = r.chart.error {
        let code = e.code.unwrap_or_default();
        if code.eq_ignore_ascii_case("Not Found") {
            return Err(NotFound(symbol.to_string()).into());
        }
        bail!("yahoo {symbol}: {}", e.description.filter(|d| !d.is_empty()).unwrap_or(code));
    }
    let res =
        r.chart.result.and_then(|v| v.into_iter().next()).ok_or_else(|| anyhow!("yahoo {symbol}: empty result"))?;
    let closes = res.indicators.quote.into_iter().next().map(|q| q.close).unwrap_or_default();
    let bars: Vec<(i64, f64)> =
        res.timestamp.iter().zip(closes).filter_map(|(&t, c)| c.filter(|c| c.is_finite()).map(|c| (t, c))).collect();
    let m = res.meta;
    let price = m.regular_market_price.filter(|p| p.is_finite()).or_else(|| bars.last().map(|b| b.1));
    let price = price.ok_or_else(|| anyhow!("yahoo {symbol}: no price"))?;
    let time = m.regular_market_time.or_else(|| bars.last().map(|b| b.0)).unwrap_or(0);
    Ok(Series {
        symbol: m.symbol.unwrap_or_else(|| symbol.to_string()),
        name: m.long_name.or(m.short_name).unwrap_or_default(),
        currency: m.currency.unwrap_or_default(),
        price,
        time,
        gmtoffset: m.gmtoffset.unwrap_or(0),
        volume: m.regular_market_volume.filter(|v| *v > 0.0),
        bars,
    })
}

impl Series {
    fn day_of(&self, ts: i64) -> i64 {
        (ts + self.gmtoffset).div_euclid(86_400)
    }

    /// Calendar date of the latest trade in the exchange's time zone.
    pub fn last_date(&self) -> Option<NaiveDate> {
        NaiveDate::from_num_days_from_ce_opt(self.day_of(self.time) as i32 + 719_163)
    }

    /// The last close on a day before the latest trade's day.
    pub fn prev_close(&self) -> Option<f64> {
        let today = self.day_of(self.time);
        self.bars.iter().rev().find(|b| self.day_of(b.0) < today).map(|b| b.1)
    }

    /// The last close on or before `date`.
    fn close_on_or_before(&self, date: NaiveDate) -> Option<f64> {
        let day = i64::from(date.num_days_from_ce()) - 719_163;
        self.bars.iter().rev().find(|b| self.day_of(b.0) <= day).map(|b| b.1)
    }

    /// The last close of a year before `year`.
    fn last_close_before_year(&self, year: i32) -> Option<f64> {
        self.bars
            .iter()
            .rev()
            .find(|b| {
                NaiveDate::from_num_days_from_ce_opt(self.day_of(b.0) as i32 + 719_163).is_some_and(|d| d.year() < year)
            })
            .map(|b| b.1)
    }
}

fn pct(now: f64, then: Option<f64>) -> Option<f64> {
    then.filter(|t| *t > 0.0).map(|t| (now / t - 1.0) * 100.0)
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; terminal-rs)")
        .timeout(Duration::from_secs(20))
        .build()?)
}

async fn fetch_series(client: &reqwest::Client, symbol: &str, range: &str) -> Result<Series> {
    let url = format!("{CHART}/{}", symbol.replace('^', "%5E"));
    let resp = client
        .get(url)
        .query(&[("range", range), ("interval", "1d")])
        .send()
        .await
        .with_context(|| format!("yahoo {symbol}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    match parse_series(&body, symbol) {
        Ok(s) => Ok(s),
        Err(e) if status.is_success() || e.is::<NotFound>() => Err(e),
        Err(_) => bail!("yahoo {symbol}: HTTP {status}"),
    }
}

/// Fetch many symbols, at most [`CONCURRENCY`] in flight; results keep the input order.
async fn fetch_many(client: &reqwest::Client, symbols: &[String], range: &str) -> Vec<Result<Series>> {
    // Owned inputs per request keep the future `Send` for any lifetime (tokio::spawn).
    let reqs = symbols.iter().cloned().map(|s| {
        let (c, range) = (client.clone(), range.to_string());
        async move { fetch_series(&c, &s, &range).await }
    });
    futures::stream::iter(reqs).buffered(CONCURRENCY).collect().await
}

// ---- futures curve ---------------------------------------------------------------------

/// One contract month on the curve.
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub symbol: String,
    pub month: u32,
    pub year: i32,
    pub price: f64,
    pub prev_close: Option<f64>,
    pub volume: Option<f64>,
}

impl Contract {
    /// `Dec 26`.
    pub fn label(&self) -> String {
        format!("{} {:02}", month_name(self.month), self.year.rem_euclid(100))
    }

    pub fn change(&self) -> Option<f64> {
        self.prev_close.map(|p| self.price - p)
    }

    pub fn change_pct(&self) -> Option<f64> {
        pct(self.price, self.prev_close)
    }

    /// Months from `other` to this contract.
    pub fn months_after(&self, other: &Contract) -> i32 {
        (self.year - other.year) * 12 + self.month as i32 - other.month as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Contango,
    Backwardation,
    Flat,
}

/// Front-to-back summary of a curve.
#[derive(Debug, Clone, PartialEq)]
pub struct Slope {
    pub front: String,
    pub back: String,
    pub months: i32,
    pub spread: f64,
    pub spread_pct: f64,
    /// Back over front, linearly annualised, in percent. Positive in contango.
    pub annualised_pct: f64,
    pub shape: Shape,
}

impl Slope {
    /// What a long holder earns per year from rolling the front into the next contract, in
    /// percent, other things equal: negative in contango, positive in backwardation.
    pub fn roll_yield_pct(&self) -> f64 {
        -self.annualised_pct
    }
}

/// Below this annualised slope (percent) a curve counts as flat.
const FLAT_PCT: f64 = 0.5;

/// Slope of a curve between its first and last contract; `None` below two contracts.
pub fn slope(points: &[Contract]) -> Option<Slope> {
    let (front, back) = (points.first()?, points.last()?);
    let months = back.months_after(front);
    if points.len() < 2 || months <= 0 || front.price <= 0.0 {
        return None;
    }
    let spread = back.price - front.price;
    let spread_pct = (back.price / front.price - 1.0) * 100.0;
    let annualised_pct = spread_pct * 12.0 / f64::from(months);
    let shape = if annualised_pct.abs() < FLAT_PCT {
        Shape::Flat
    } else if annualised_pct > 0.0 {
        Shape::Contango
    } else {
        Shape::Backwardation
    };
    Some(Slope { front: front.label(), back: back.label(), months, spread, spread_pct, annualised_pct, shape })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    pub root: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    pub points: Vec<Contract>,
    /// Probed contract months that Yahoo does not list or that no longer trade.
    pub missing: Vec<String>,
    /// Probes that failed for another reason (network, HTTP).
    pub failed: usize,
    /// Unix seconds.
    pub fetched: i64,
}

impl Curve {
    pub fn slope(&self) -> Option<Slope> {
        slope(&self.points)
    }
}

/// Keep the live contracts, sorted by expiry month, at most [`CURVE_LEN`]; the rest is
/// reported as missing. `probes` is `(month, year, result)`.
pub fn build_curve(root: &'static Root, probes: Vec<(u32, i32, Result<Series>)>, now: i64) -> Result<Curve> {
    let mut points = Vec::new();
    let mut missing = Vec::new();
    let mut failed = 0;
    let mut first_err = None;
    let today = chrono::DateTime::from_timestamp(now, 0).map(|t| t.date_naive());
    for (month, year, res) in probes {
        let label = format!("{} {:02}", month_name(month), year.rem_euclid(100));
        let expired = today.zip(last_trade(root.code, month, year)).is_some_and(|(t, last)| last < t);
        match res {
            Ok(_) if expired => missing.push(label),
            Ok(s) if now - s.time <= STALE_SECS || s.time == 0 => points.push(Contract {
                prev_close: s.prev_close(),
                volume: s.volume,
                symbol: s.symbol,
                month,
                year,
                price: s.price,
            }),
            Ok(_) => missing.push(label),
            Err(e) if e.is::<NotFound>() => missing.push(label),
            Err(e) => {
                failed += 1;
                first_err.get_or_insert(e);
            }
        }
    }
    points.sort_by_key(|c| (c.year, c.month));
    if points.len() > CURVE_LEN {
        points.truncate(CURVE_LEN);
    }
    if points.is_empty() {
        return Err(match first_err {
            Some(e) => e.context(format!("{} futures: no contract could be fetched", root.code)),
            None => anyhow!("{} futures: Yahoo lists no live contracts", root.code),
        });
    }
    Ok(Curve { root: root.code, name: root.name, unit: root.unit, points, missing, failed, fetched: now })
}

/// Fetch the term structure of a futures root.
pub async fn fetch_curve(root: &'static Root) -> Result<Curve> {
    let now = chrono::Utc::now();
    // A few spare months cover contracts that are missing or expired.
    let cands = candidates(root, now.date_naive(), CURVE_LEN + 3);
    let symbols: Vec<String> = cands.iter().filter_map(|&(m, y)| contract_symbol(root, m, y)).collect();
    let c = client()?;
    let results = fetch_many(&c, &symbols, "5d").await;
    let probes = cands.into_iter().zip(results).map(|((m, y), r)| (m, y, r)).collect();
    build_curve(root, probes, now.timestamp())
}

// ---- VIX term structure ----------------------------------------------------------------

/// The Cboe volatility indices on Yahoo: ticker, label, horizon in calendar days.
pub const VIX_INDICES: [(&str, &str, f64); 5] = [
    ("^VIX9D", "VIX9D", 9.0),
    ("^VIX", "VIX", 30.0),
    ("^VIX3M", "VIX3M", 93.0),
    ("^VIX6M", "VIX6M", 186.0),
    ("^VIX1Y", "VIX1Y", 365.0),
];

#[derive(Debug, Clone, PartialEq)]
pub struct VixIndex {
    pub label: &'static str,
    /// Horizon of the index in calendar days.
    pub days: f64,
    pub value: f64,
    pub prev_close: Option<f64>,
    pub time: i64,
}

impl VixIndex {
    pub fn change_pct(&self) -> Option<f64> {
        pct(self.value, self.prev_close)
    }
}

/// One monthly VX future with its end-of-day settlement.
#[derive(Debug, Clone, PartialEq)]
pub struct VxFuture {
    /// `VX/V6`.
    pub symbol: String,
    pub expiry: NaiveDate,
    pub settle: f64,
}

/// Parse Cboe's settlement CSV (`Product,Symbol,Expiration Date,Price`), keeping the monthly
/// VX contracts (`VX/V6`) and skipping weeklies (`VX43/V6`), mini VIX futures and other products.
pub fn parse_vx_settlements(csv: &str) -> Result<Vec<VxFuture>> {
    let mut lines = csv.lines();
    let header = lines.next().unwrap_or_default();
    if !header.starts_with("Product,Symbol") {
        bail!("cboe settlement csv: unexpected header");
    }
    let mut out: Vec<VxFuture> = lines
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 4 || f[0] != "VX" || !f[1].starts_with("VX/") {
                return None;
            }
            let expiry = NaiveDate::parse_from_str(f[2], "%Y-%m-%d").ok()?;
            let settle: f64 = f[3].parse().ok().filter(|p: &f64| *p > 0.0)?;
            Some(VxFuture { symbol: f[1].to_string(), expiry, settle })
        })
        .collect();
    out.sort_by_key(|f| f.expiry);
    if out.is_empty() {
        bail!("cboe settlement csv: no VX rows");
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct VixTerm {
    pub indices: Vec<VixIndex>,
    /// Monthly VX futures; empty when the Cboe CSV could not be fetched.
    pub futures: Vec<VxFuture>,
    pub futures_error: Option<String>,
    /// Unix seconds.
    pub fetched: i64,
    pub today: NaiveDate,
}

impl VixTerm {
    pub fn index(&self, label: &str) -> Option<&VixIndex> {
        self.indices.iter().find(|i| i.label == label)
    }

    /// `a / b` of two index values.
    pub fn ratio(&self, a: &str, b: &str) -> Option<f64> {
        let (a, b) = (self.index(a)?, self.index(b)?);
        (b.value > 0.0).then(|| a.value / b.value)
    }

    /// Futures that have not expired yet.
    pub fn live_futures(&self) -> impl Iterator<Item = &VxFuture> {
        self.futures.iter().filter(|f| f.expiry >= self.today)
    }

    /// Plain-language reading of the curve.
    pub fn interpretation(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = self.ratio("VIX", "VIX3M") {
            out.push(if r >= 1.0 {
                format!("VIX/VIX3M {r:.2}: spot volatility is above 3-month volatility (inverted). The market is paying up for near-term protection, which usually goes with stress or a sell-off.")
            } else if r >= 0.9 {
                format!("VIX/VIX3M {r:.2}: the curve is nearly flat. Near-term and 3-month volatility are priced alike, a transition zone between calm and stress.")
            } else {
                format!("VIX/VIX3M {r:.2}: normal upward-sloping curve (contango). Little near-term fear is priced in.")
            });
        }
        if let Some(r) = self.ratio("VIX9D", "VIX") {
            out.push(if r > 1.0 {
                format!("VIX9D/VIX {r:.2}: the next nine days are priced more volatile than the next 30, which points to an imminent event or an ongoing shock.")
            } else {
                format!("VIX9D/VIX {r:.2}: the very short end is calmer than the 30-day horizon.")
            });
        }
        let mut live = self.live_futures();
        if let (Some(f1), Some(f2)) = (live.next(), live.next()) {
            let d = (f2.settle / f1.settle - 1.0) * 100.0;
            out.push(if d >= 0.0 {
                format!(
                    "VX futures: {} settled {:.2}, the next month {:.2} ({d:+.1}%). Contango means a long VIX futures position loses to roll-down if spot stays put.",
                    f1.symbol, f1.settle, f2.settle
                )
            } else {
                format!(
                    "VX futures: {} settled {:.2}, the next month {:.2} ({d:+.1}%). Backwardation: the futures price an easing of today's volatility.",
                    f1.symbol, f1.settle, f2.settle
                )
            });
        }
        out
    }
}

/// Fetch the five volatility indices and, best effort, the Cboe VX settlements.
pub async fn fetch_vix() -> Result<VixTerm> {
    let c = client()?;
    let symbols: Vec<String> = VIX_INDICES.iter().map(|i| i.0.to_string()).collect();
    let (series, csv) = tokio::join!(fetch_many(&c, &symbols, "5d"), fetch_vx_csv(&c));
    let mut indices = Vec::new();
    let mut first_err = None;
    for (&(_, label, days), res) in VIX_INDICES.iter().zip(series) {
        match res {
            Ok(s) => indices.push(VixIndex { label, days, value: s.price, prev_close: s.prev_close(), time: s.time }),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    if indices.is_empty() {
        return Err(first_err.unwrap_or_else(|| anyhow!("yahoo: no volatility index data")));
    }
    let (futures, futures_error) = match csv {
        Ok(f) => (f, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let now = chrono::Utc::now();
    Ok(VixTerm { indices, futures, futures_error, fetched: now.timestamp(), today: now.date_naive() })
}

async fn fetch_vx_csv(client: &reqwest::Client) -> Result<Vec<VxFuture>> {
    let resp = client.get(VX_CSV).send().await.context("cboe settlement csv")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("cboe settlement csv: HTTP {status}");
    }
    parse_vx_settlements(&resp.text().await?)
}

// ---- sector / region heatmap -----------------------------------------------------------

/// One heatmap tile: an ETF with an approximate weight (tile area).
#[derive(Debug, PartialEq)]
pub struct TileDef {
    pub symbol: &'static str,
    pub name: &'static str,
    pub weight: f64,
}

/// The 11 SPDR sector ETFs with approximate S&P 500 sector weights in percent. Fixed table,
/// not live market caps.
pub const SECTORS: &[TileDef] = &[
    TileDef { symbol: "XLK", name: "Technology", weight: 32.0 },
    TileDef { symbol: "XLF", name: "Financials", weight: 13.5 },
    TileDef { symbol: "XLY", name: "Cons. Discretionary", weight: 10.5 },
    TileDef { symbol: "XLC", name: "Communication", weight: 9.5 },
    TileDef { symbol: "XLV", name: "Health Care", weight: 9.0 },
    TileDef { symbol: "XLI", name: "Industrials", weight: 8.0 },
    TileDef { symbol: "XLP", name: "Cons. Staples", weight: 5.5 },
    TileDef { symbol: "XLE", name: "Energy", weight: 3.0 },
    TileDef { symbol: "XLU", name: "Utilities", weight: 2.5 },
    TileDef { symbol: "XLRE", name: "Real Estate", weight: 2.0 },
    TileDef { symbol: "XLB", name: "Materials", weight: 2.0 },
];

/// Country ETFs of the major equity markets, approximate share of world (ex-US) market cap
/// in percent. Fixed table.
pub const WORLD: &[TileDef] = &[
    TileDef { symbol: "EWJ", name: "Japan", weight: 14.0 },
    TileDef { symbol: "FXI", name: "China", weight: 10.0 },
    TileDef { symbol: "EWU", name: "UK", weight: 9.0 },
    TileDef { symbol: "EWC", name: "Canada", weight: 8.0 },
    TileDef { symbol: "INDA", name: "India", weight: 7.5 },
    TileDef { symbol: "EWQ", name: "France", weight: 6.0 },
    TileDef { symbol: "EWG", name: "Germany", weight: 6.0 },
    TileDef { symbol: "EWT", name: "Taiwan", weight: 5.5 },
    TileDef { symbol: "EWY", name: "South Korea", weight: 4.0 },
    TileDef { symbol: "EWA", name: "Australia", weight: 4.0 },
    TileDef { symbol: "EWL", name: "Switzerland", weight: 3.5 },
    TileDef { symbol: "EWZ", name: "Brazil", weight: 2.5 },
    TileDef { symbol: "EWW", name: "Mexico", weight: 1.0 },
    TileDef { symbol: "TUR", name: "Turkey", weight: 0.5 },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    Month,
    Ytd,
}

impl Period {
    pub const ALL: [Period; 4] = [Period::Day, Period::Week, Period::Month, Period::Ytd];

    pub fn label(self) -> &'static str {
        match self {
            Period::Day => "1D",
            Period::Week => "1W",
            Period::Month => "1M",
            Period::Ytd => "YTD",
        }
    }

    /// Change (percent) at which the colour scale saturates.
    pub fn scale(self) -> f64 {
        match self {
            Period::Day => 3.0,
            Period::Week => 5.0,
            Period::Month => 10.0,
            Period::Ytd => 25.0,
        }
    }
}

/// Percent changes of one ETF over each period; `None` where the history is too short.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Perf {
    pub day: Option<f64>,
    pub week: Option<f64>,
    pub month: Option<f64>,
    pub ytd: Option<f64>,
}

impl Perf {
    pub fn get(&self, p: Period) -> Option<f64> {
        match p {
            Period::Day => self.day,
            Period::Week => self.week,
            Period::Month => self.month,
            Period::Ytd => self.ytd,
        }
    }
}

/// Changes over every period from about a year of daily closes. 1W is seven calendar days,
/// 1M one calendar month, YTD since the last close of the previous year; each against the last
/// close on or before that date.
pub fn perf(s: &Series) -> Perf {
    let Some(today) = s.last_date() else { return Perf::default() };
    let week = today.checked_sub_days(chrono::Days::new(7));
    let month = today.checked_sub_months(chrono::Months::new(1));
    Perf {
        day: pct(s.price, s.prev_close()),
        week: pct(s.price, week.and_then(|d| s.close_on_or_before(d))),
        month: pct(s.price, month.and_then(|d| s.close_on_or_before(d))),
        ytd: pct(s.price, s.last_close_before_year(today.year())),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeatTile {
    pub symbol: &'static str,
    pub name: &'static str,
    pub weight: f64,
    /// `None` when this ETF could not be fetched.
    pub price: Option<f64>,
    pub perf: Perf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Heat {
    pub world: bool,
    pub tiles: Vec<HeatTile>,
    pub failed: usize,
    pub fetched: i64,
}

/// Fetch the sector (or world) ETFs. Fails only when none could be fetched.
pub async fn fetch_heat(world: bool) -> Result<Heat> {
    let defs = if world { WORLD } else { SECTORS };
    let symbols: Vec<String> = defs.iter().map(|d| d.symbol.to_string()).collect();
    let c = client()?;
    let results = fetch_many(&c, &symbols, "1y").await;
    let mut first_err = None;
    let mut failed = 0;
    let tiles: Vec<HeatTile> = defs
        .iter()
        .zip(results)
        .map(|(d, r)| match r {
            Ok(s) => {
                HeatTile { symbol: d.symbol, name: d.name, weight: d.weight, price: Some(s.price), perf: perf(&s) }
            }
            Err(e) => {
                failed += 1;
                first_err.get_or_insert(e);
                HeatTile { symbol: d.symbol, name: d.name, weight: d.weight, price: None, perf: Perf::default() }
            }
        })
        .collect();
    if failed == tiles.len() {
        return Err(first_err.unwrap_or_else(|| anyhow!("yahoo: no ETF data")));
    }
    Ok(Heat { world, tiles, failed, fetched: chrono::Utc::now().timestamp() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLZ26: &str = include_str!("../tests/fixtures/yahoo_fut_clz26.json");
    const VX: &str = include_str!("../tests/fixtures/cboe_vx_settle.csv");

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn month_codes_round_trip() {
        assert_eq!(month_code(1), Some('F'));
        assert_eq!(month_code(12), Some('Z'));
        assert_eq!(month_code(13), None);
        assert_eq!(month_from_code('z'), Some(12));
        assert_eq!(month_from_code('Q'), Some(8));
        assert_eq!(month_from_code('A'), None);
        for m in 1..=12 {
            assert_eq!(month_from_code(month_code(m).unwrap()), Some(m));
        }
    }

    #[test]
    fn contract_symbols() {
        let cl = root("cl").unwrap();
        assert_eq!(contract_symbol(cl, 12, 2026).as_deref(), Some("CLZ26.NYM"));
        assert_eq!(contract_symbol(root("ES").unwrap(), 3, 2027).as_deref(), Some("ESH27.CME"));
        assert_eq!(contract_symbol(root("6E").unwrap(), 6, 2030).as_deref(), Some("6EM30.CME"));
        assert!(root("XX").is_none());
    }

    #[test]
    fn candidates_follow_the_listing_cycle() {
        let es = candidates(root("ES").unwrap(), date(2026, 10, 17), 4);
        assert_eq!(es, vec![(12, 2026), (3, 2027), (6, 2027), (9, 2027)]);
        let cl = candidates(root("CL").unwrap(), date(2026, 11, 3), 3);
        assert_eq!(cl, vec![(11, 2026), (12, 2026), (1, 2027)]);
        let gc = candidates(root("GC").unwrap(), date(2026, 12, 30), 3);
        assert_eq!(gc, vec![(12, 2026), (2, 2027), (4, 2027)]);
    }

    #[test]
    fn parses_yahoo_future() {
        let s = parse_series(CLZ26, "CLZ26.NYM").unwrap();
        assert_eq!(s.symbol, "CLZ26.NYM");
        assert_eq!(s.name, "Crude Oil Dec 26");
        assert!((s.price - 98.9).abs() < 1e-9);
        assert_eq!(s.bars.len(), 5);
        assert!(s.volume.is_some_and(|v| v > 0.0));
        // The last bar is the same day as the latest trade, so the previous close is the bar before.
        assert!((s.prev_close().unwrap() - 96.5).abs() < 1e-9);
        assert_eq!(s.last_date(), Some(date(2026, 10, 16)));
    }

    #[test]
    fn missing_symbol_is_not_found() {
        let e = parse_series(
            r#"{"chart":{"result":null,"error":{"code":"Not Found","description":"No data found, symbol may be delisted"}}}"#,
            "CLX99.NYM",
        )
        .unwrap_err();
        assert!(e.is::<NotFound>());
        let e =
            parse_series(r#"{"chart":{"result":null,"error":{"code":"Bad","description":"nope"}}}"#, "X").unwrap_err();
        assert!(!e.is::<NotFound>() && format!("{e}").contains("nope"));
        assert!(parse_series("<html>", "X").is_err());
    }

    fn contract(month: u32, year: i32, price: f64) -> Contract {
        Contract { symbol: format!("T{month}"), month, year, price, prev_close: Some(price - 1.0), volume: None }
    }

    #[test]
    fn contango_slope() {
        let pts = [contract(12, 2026, 100.0), contract(12, 2027, 106.0)];
        let s = slope(&pts).unwrap();
        assert_eq!(s.months, 12);
        assert_eq!(s.shape, Shape::Contango);
        assert!((s.spread - 6.0).abs() < 1e-9);
        assert!((s.annualised_pct - 6.0).abs() < 1e-9);
        assert!((s.roll_yield_pct() + 6.0).abs() < 1e-9);
        assert_eq!((s.front.as_str(), s.back.as_str()), ("Dec 26", "Dec 27"));
    }

    #[test]
    fn backwardation_slope_is_annualised() {
        // 6 months out, 5% below the front: -10% a year.
        let pts = [contract(1, 2027, 100.0), contract(3, 2027, 98.0), contract(7, 2027, 95.0)];
        let s = slope(&pts).unwrap();
        assert_eq!(s.months, 6);
        assert_eq!(s.shape, Shape::Backwardation);
        assert!((s.annualised_pct + 10.0).abs() < 1e-9);
        assert!((s.roll_yield_pct() - 10.0).abs() < 1e-9);
        let flat = [contract(1, 2027, 100.0), contract(12, 2027, 100.2)];
        assert_eq!(slope(&flat).unwrap().shape, Shape::Flat);
        assert!(slope(&pts[..1]).is_none());
        assert!(slope(&[]).is_none());
    }

    #[test]
    fn contract_changes() {
        let c = contract(12, 2026, 99.0);
        assert_eq!(c.label(), "Dec 26");
        assert_eq!(c.change(), Some(1.0));
        assert!((c.change_pct().unwrap() - (99.0 / 98.0 - 1.0) * 100.0).abs() < 1e-9);
        assert_eq!(contract(3, 2028, 1.0).months_after(&c), 15);
    }

    fn series(sym: &str, time: i64, price: f64) -> Series {
        Series {
            symbol: sym.into(),
            name: sym.into(),
            currency: "USD".into(),
            price,
            time,
            gmtoffset: 0,
            volume: Some(10.0),
            bars: vec![(time - 86_400, price - 1.0), (time, price)],
        }
    }

    #[test]
    fn curve_skips_missing_and_stale_contracts() {
        let now = 1_792_000_000;
        let cl = root("CL").unwrap();
        let probes = vec![
            (10, 2026, Ok(series("CLV26.NYM", now - 40 * 86_400, 90.0))), // expired, stale
            (12, 2026, Ok(series("CLZ26.NYM", now - 3600, 99.0))),
            (11, 2026, Ok(series("CLX26.NYM", now - 3600, 98.0))),
            (1, 2027, Err(NotFound("CLF27.NYM".into()).into())),
            (2, 2027, Err(anyhow!("timeout"))),
        ];
        let c = build_curve(cl, probes, now).unwrap();
        let syms: Vec<&str> = c.points.iter().map(|p| p.symbol.as_str()).collect();
        assert_eq!(syms, ["CLX26.NYM", "CLZ26.NYM"], "sorted by expiry");
        assert_eq!(c.missing, ["Oct 26", "Jan 27"]);
        assert_eq!(c.failed, 1);
        assert_eq!(c.points[0].prev_close, Some(97.0));
    }

    #[test]
    fn last_trading_days() {
        // Published CME / ICE dates.
        assert_eq!(last_trade("CL", 11, 2026), Some(date(2026, 10, 20)));
        assert_eq!(last_trade("CL", 12, 2026), Some(date(2026, 11, 20)));
        assert_eq!(last_trade("NG", 11, 2026), Some(date(2026, 10, 28)));
        assert_eq!(last_trade("ES", 12, 2026), Some(date(2026, 12, 18)));
        assert_eq!(last_trade("GC", 10, 2026), Some(date(2026, 10, 28)));
        assert_eq!(last_trade("ZN", 9, 2026), Some(date(2026, 9, 21)));
        assert_eq!(last_trade("ZC", 12, 2026), Some(date(2026, 12, 14)));
        assert_eq!(last_trade("6E", 12, 2026), Some(date(2026, 12, 14)));
        assert_eq!(last_trade("XX", 12, 2026), None);
    }

    #[test]
    fn expired_front_contract_is_dropped() {
        // 21 Oct 2026: CLX26 stopped trading on the 20th but Yahoo still quotes it.
        let now = 1_792_540_800 + 3600;
        let cl = root("CL").unwrap();
        let probes = vec![
            (11, 2026, Ok(series("CLX26.NYM", now - 20 * 3600, 101.0))),
            (12, 2026, Ok(series("CLZ26.NYM", now - 3600, 98.0))),
        ];
        let c = build_curve(cl, probes, now).unwrap();
        assert_eq!(c.points[0].symbol, "CLZ26.NYM");
        assert_eq!(c.missing, ["Nov 26"]);
    }

    #[test]
    fn curve_without_contracts_is_an_error() {
        let cl = root("CL").unwrap();
        let e = build_curve(cl, vec![(1, 2027, Err(anyhow!("HTTP 429")))], 0).unwrap_err();
        assert!(format!("{e:#}").contains("429"));
        let e = build_curve(cl, vec![(1, 2027, Err(NotFound("x".into()).into()))], 0).unwrap_err();
        assert!(format!("{e}").contains("no live contracts"));
    }

    #[test]
    fn curve_is_capped_at_twelve() {
        let now = 1_792_000_000;
        let probes: Vec<_> = (0..15)
            .map(|i| {
                let (m, y) = ((i % 12) as u32 + 1, 2027 + i / 12);
                (m, y, Ok(series(&format!("CL{i}"), now, 80.0 + f64::from(i))))
            })
            .collect();
        let c = build_curve(root("CL").unwrap(), probes, now).unwrap();
        assert_eq!(c.points.len(), CURVE_LEN);
        assert_eq!(c.points[0].label(), "Jan 27");
    }

    #[test]
    fn parses_vx_settlements() {
        let f = parse_vx_settlements(VX).unwrap();
        assert_eq!(f.len(), 9, "monthlies only: weeklies like VX43/V6 are skipped");
        assert_eq!(f[0].symbol, "VX/V6");
        assert_eq!(f[0].expiry, date(2026, 10, 21));
        assert!((f[0].settle - 19.9075).abs() < 1e-9);
        assert_eq!(f[8].symbol, "VX/M7");
        assert!(parse_vx_settlements("<html>").is_err());
        assert!(parse_vx_settlements("Product,Symbol,Expiration Date,Price\nVA,VA/V6,2026-10-16,143.25\n").is_err());
    }

    fn term(vix: f64, vix3m: f64, vix9d: f64) -> VixTerm {
        let idx = |label, value| VixIndex { label, days: 30.0, value, prev_close: Some(value - 1.0), time: 0 };
        VixTerm {
            indices: vec![idx("VIX9D", vix9d), idx("VIX", vix), idx("VIX3M", vix3m)],
            futures: parse_vx_settlements(VX).unwrap(),
            futures_error: None,
            fetched: 0,
            today: date(2026, 10, 19),
        }
    }

    #[test]
    fn vix_ratio_and_interpretation() {
        let calm = term(15.0, 18.0, 13.0);
        assert!((calm.ratio("VIX", "VIX3M").unwrap() - 15.0 / 18.0).abs() < 1e-9);
        let text = calm.interpretation().join(" ");
        assert!(text.contains("contango") && text.contains("calmer"), "{text}");
        let stress = term(25.0, 22.0, 30.0).interpretation().join(" ");
        assert!(stress.contains("inverted") && stress.contains("nine days"), "{stress}");
        assert!(term(20.0, 20.5, 20.0).interpretation()[0].contains("flat"));
        assert!(calm.ratio("VIX", "NOPE").is_none());
    }

    #[test]
    fn live_futures_drop_expired() {
        let mut t = term(15.0, 18.0, 13.0);
        t.today = date(2026, 10, 22);
        assert_eq!(t.live_futures().next().unwrap().symbol, "VX/X6");
    }

    /// Daily closes ending on a Friday, `n` days of history, price rising by 1 a day.
    fn history(last: NaiveDate, n: i64) -> Series {
        let end = last.and_hms_opt(14, 0, 0).unwrap().and_utc().timestamp();
        let bars: Vec<(i64, f64)> = (0..n).rev().map(|i| (end - i * 86_400, 1000.0 - i as f64)).collect();
        Series {
            symbol: "T".into(),
            name: "T".into(),
            currency: "USD".into(),
            price: 1000.0,
            time: end,
            gmtoffset: 0,
            volume: None,
            bars,
        }
    }

    #[test]
    fn period_changes() {
        // Last trade 2026-10-16; every calendar day has a bar, close = 200 - days back.
        let s = history(date(2026, 10, 16), 400);
        let p = perf(&s);
        assert!((p.day.unwrap() - (1000.0 / 999.0 - 1.0) * 100.0).abs() < 1e-9);
        assert!((p.week.unwrap() - (1000.0 / 993.0 - 1.0) * 100.0).abs() < 1e-9);
        // 2026-09-16 is 30 days back.
        assert!((p.month.unwrap() - (1000.0 / 970.0 - 1.0) * 100.0).abs() < 1e-9);
        // Last close of 2025 is 2025-12-31, 289 days back.
        assert!((p.ytd.unwrap() - (1000.0 / 711.0 - 1.0) * 100.0).abs() < 1e-9);
        assert_eq!(p.get(Period::Week), p.week);
    }

    #[test]
    fn short_history_leaves_periods_empty() {
        let s = history(date(2026, 10, 16), 3);
        let p = perf(&s);
        assert!(p.day.is_some());
        assert!(p.week.is_none() && p.month.is_none() && p.ytd.is_none());
    }

    #[test]
    fn ytd_uses_last_close_of_previous_year() {
        let mut s = history(date(2026, 1, 6), 12);
        // Bars: 2025-12-26 .. 2026-01-06. Last 2025 bar is 2025-12-31 (6 days back, close 994).
        s.price = 994.0 * 1.1;
        let p = perf(&s);
        assert!((p.ytd.unwrap() - 10.0).abs() < 1e-6, "{:?}", p.ytd);
    }

    #[test]
    fn tables_are_sane() {
        assert_eq!(SECTORS.len(), 11);
        let w: f64 = SECTORS.iter().map(|t| t.weight).sum();
        assert!((w - 99.0).abs() < 2.0, "sector weights roughly sum to 100, got {w}");
        assert!(WORLD.iter().all(|t| t.weight > 0.0));
        assert!(ROOTS.iter().all(|r| r.months.chars().all(|c| month_from_code(c).is_some())));
    }

    #[tokio::test]
    #[ignore = "live: Yahoo and Cboe"]
    async fn live_curve_vix_heat() {
        for code in ["ES", "CL", "GC", "ZN"] {
            let c = fetch_curve(root(code).unwrap()).await.unwrap();
            println!("{code}: {} contracts, missing {:?}, slope {:?}", c.points.len(), c.missing, c.slope());
            assert!(c.points.len() >= 2, "{code}");
        }
        let v = fetch_vix().await.unwrap();
        println!("{:?}\n{:?}\n{:?}", v.indices, v.futures_error, v.interpretation());
        assert!(v.index("VIX").is_some());
        assert!(!v.futures.is_empty(), "Cboe CSV: {:?}", v.futures_error);
        let h = fetch_heat(false).await.unwrap();
        assert_eq!(h.failed, 0);
        assert!(h.tiles.iter().all(|t| t.perf.day.is_some() && t.perf.ytd.is_some()));
        let w = fetch_heat(true).await.unwrap();
        println!("world failed: {}", w.failed);
    }
}
