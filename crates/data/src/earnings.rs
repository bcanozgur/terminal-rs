//! Earnings calendar and history (ERN / EVTS / EE) from free sources.
//!
//! - **Nasdaq's public calendar** (`api.nasdaq.com`, unofficial and undocumented; it wants
//!   browser-like headers): who reports on a given day (pre-market / after-hours), the
//!   consensus EPS estimate, the prior-year EPS and market cap, plus per-ticker the last four
//!   reported quarters with estimate and surprise. Any failure is returned as an error for the
//!   caller to show; nothing here tries to get around bot protection.
//! - **SEC EDGAR** supplies the reported side: the 8-K Item 2.02 ("Results of Operations")
//!   filing dates and quarterly diluted EPS from XBRL company facts. The EDGAR client itself is
//!   not in this module: [`EdgarSource`] is the small raw-JSON interface it needs, and the
//!   parsers below work on the documents it returns.
//!
//! [`EarningsStore`] caches calendar days and per-ticker histories in SQLite.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tr_core::Bar;

/// When in the trading day a company reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Timing {
    PreMarket,
    AfterHours,
    #[default]
    Unknown,
}

impl Timing {
    pub fn label(self) -> &'static str {
        match self {
            Timing::PreMarket => "BMO",
            Timing::AfterHours => "AMC",
            Timing::Unknown => "—",
        }
    }
}

/// One company reporting on one day, from the Nasdaq calendar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarRow {
    pub date: NaiveDate,
    pub symbol: String,
    pub company: String,
    pub timing: Timing,
    /// Consensus EPS estimate.
    pub eps_estimate: Option<f64>,
    /// EPS of the same quarter last year (upcoming rows only).
    pub eps_last_year: Option<f64>,
    /// Reported EPS (rows for days already reported).
    pub eps_actual: Option<f64>,
    pub surprise_pct: Option<f64>,
    pub market_cap: Option<f64>,
    /// Fiscal quarter the report covers, e.g. `Sep/2026`.
    pub fiscal_quarter: String,
    pub estimates: Option<u32>,
}

/// One reported quarter of a ticker's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EarningsEvent {
    pub date: NaiveDate,
    pub timing: Timing,
    /// Fiscal period end (EDGAR) when known.
    pub period_end: Option<NaiveDate>,
    pub fiscal_quarter: String,
    /// Diluted EPS as filed with the SEC (GAAP).
    pub eps_gaap: Option<f64>,
    /// EPS the consensus is compared against (Nasdaq; usually adjusted).
    pub eps_actual: Option<f64>,
    pub eps_estimate: Option<f64>,
    pub surprise_pct: Option<f64>,
    /// Close-to-close move around the report, percent.
    pub move_pct: Option<f64>,
}

/// A ticker's earnings history plus its next date, if known.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TickerEarnings {
    /// Newest first.
    pub events: Vec<EarningsEvent>,
    pub next: Option<NextEarnings>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextEarnings {
    pub date: NaiveDate,
    pub timing: Timing,
    pub eps_estimate: Option<f64>,
    /// Projected from the reporting cadence rather than announced.
    pub projected: bool,
}

// ---- Nasdaq parsing ---------------------------------------------------------------------

/// `"$1.31"`, `"($0.12)"`, `"-$0.12"`, `"$2,230,197,160,000"` → number; `N/A`/empty → `None`.
pub fn parse_money(s: &str) -> Option<f64> {
    let t = s.trim();
    let neg = t.starts_with('(') || t.starts_with('-');
    let digits: String = t.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let v: f64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn parse_pct(s: &str) -> Option<f64> {
    s.trim().trim_end_matches('%').replace(',', "").parse().ok()
}

fn timing_of(s: &str) -> Timing {
    match s {
        "time-pre-market" => Timing::PreMarket,
        "time-after-hours" => Timing::AfterHours,
        _ => Timing::Unknown,
    }
}

fn status_ok(v: &Value) -> Result<()> {
    match v.pointer("/status/rCode").and_then(Value::as_i64) {
        Some(200) | None => Ok(()),
        Some(c) => bail!("Nasdaq returned status {c}: {}", v.pointer("/status/bCodeMessage").unwrap_or(&Value::Null)),
    }
}

/// Parse `calendar/earnings?date=` for `date`.
pub fn parse_calendar(json: &str, date: NaiveDate) -> Result<Vec<CalendarRow>> {
    let v: Value = serde_json::from_str(json).context("Nasdaq calendar is not JSON")?;
    status_ok(&v)?;
    let rows = v.pointer("/data/rows").and_then(Value::as_array);
    let Some(rows) = rows else { return Ok(Vec::new()) }; // a day with no reports has `rows: null`
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    Ok(rows
        .iter()
        .filter_map(|r| {
            let symbol = text(r, "symbol");
            if symbol.is_empty() {
                return None;
            }
            // Before a report the `eps` column is last year's figure; once reported (a surprise
            // exists) it is the actual.
            let surprise_pct = parse_pct(&text(r, "surprise"));
            let eps = parse_money(&text(r, "eps"));
            let reported = surprise_pct.is_some();
            Some(CalendarRow {
                date,
                symbol,
                company: text(r, "name"),
                timing: timing_of(&text(r, "time")),
                eps_estimate: parse_money(&text(r, "epsForecast")),
                eps_last_year: if reported { None } else { eps },
                eps_actual: if reported { eps } else { None },
                surprise_pct,
                market_cap: parse_money(&text(r, "marketCap")),
                fiscal_quarter: text(r, "fiscalQuarterEnding"),
                estimates: text(r, "noOfEsts").parse().ok(),
            })
        })
        .collect())
}

/// One quarter from `company/<T>/earnings-surprise` (the last four).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    pub date: NaiveDate,
    pub fiscal_quarter: String,
    pub eps: Option<f64>,
    pub estimate: Option<f64>,
    pub surprise_pct: Option<f64>,
}

pub fn parse_surprise(json: &str) -> Result<Vec<Reported>> {
    let v: Value = serde_json::from_str(json).context("Nasdaq surprise is not JSON")?;
    status_ok(&v)?;
    let rows = v.pointer("/data/earningsSurpriseTable/rows").and_then(Value::as_array);
    let Some(rows) = rows else { return Ok(Vec::new()) };
    Ok(rows
        .iter()
        .filter_map(|r| {
            let date = NaiveDate::parse_from_str(r.get("dateReported")?.as_str()?, "%m/%d/%Y").ok()?;
            let num = |k: &str| match r.get(k)? {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => parse_money(s),
                _ => None,
            };
            Some(Reported {
                date,
                fiscal_quarter: r.get("fiscalQtrEnd").and_then(Value::as_str).unwrap_or("").into(),
                eps: num("eps"),
                estimate: num("consensusForecast"),
                surprise_pct: r.get("percentageSurprise").and_then(Value::as_str).and_then(parse_pct),
            })
        })
        .collect())
}

// ---- EDGAR parsing -----------------------------------------------------------------------

/// The raw SEC documents this module needs, supplied by the EDGAR client (which owns the
/// ticker→CIK map, User-Agent, rate limit and cache).
#[async_trait]
pub trait EdgarSource: Send + Sync {
    /// `https://data.sec.gov/api/xbrl/companyfacts/CIK##########.json` for `ticker`.
    async fn company_facts(&self, ticker: &str) -> Result<Value>;
    /// `https://data.sec.gov/submissions/CIK##########.json` for `ticker`.
    async fn submissions(&self, ticker: &str) -> Result<Value>;
}

/// Stand-in used when no EDGAR client is wired in: history falls back to Nasdaq's last four
/// quarters.
pub struct NoEdgar;

#[async_trait]
impl EdgarSource for NoEdgar {
    async fn company_facts(&self, _: &str) -> Result<Value> {
        bail!("EDGAR client not available")
    }
    async fn submissions(&self, _: &str) -> Result<Value> {
        bail!("EDGAR client not available")
    }
}

/// Filing dates of 8-Ks carrying Item 2.02 (Results of Operations), newest first.
pub fn release_dates(submissions: &Value) -> Vec<NaiveDate> {
    let r = &submissions["filings"]["recent"];
    let col = |k: &str| r.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let (forms, items, filed) = (col("form"), col("items"), col("filingDate"));
    let mut out: Vec<NaiveDate> = (0..forms.len())
        .filter(|&i| forms[i].as_str().is_some_and(|f| f == "8-K"))
        .filter(|&i| items.get(i).and_then(Value::as_str).is_some_and(|s| s.split(',').any(|x| x.trim() == "2.02")))
        .filter_map(|i| NaiveDate::parse_from_str(filed.get(i)?.as_str()?, "%Y-%m-%d").ok())
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.dedup();
    out
}

/// A quarter's diluted EPS from XBRL.
#[derive(Debug, Clone, PartialEq)]
pub struct EpsQuarter {
    pub end: NaiveDate,
    pub eps: f64,
    /// The fourth quarter is not filed on its own: it is the fiscal year less the nine-month
    /// figure (approximate, as EPS is not exactly additive).
    pub derived: bool,
}

/// Quarterly diluted EPS (newest first) from company facts.
pub fn quarterly_eps(facts: &Value) -> Vec<EpsQuarter> {
    let arr = facts
        .pointer("/facts/us-gaap/EarningsPerShareDiluted/units/USD~1shares")
        .or_else(|| facts.pointer("/facts/us-gaap/EarningsPerShareDiluted/units").and_then(|u| u.get("USD/shares")))
        .and_then(Value::as_array);
    let Some(arr) = arr else { return Vec::new() };
    struct F {
        start: NaiveDate,
        end: NaiveDate,
        val: f64,
        filed: NaiveDate,
    }
    let d = |x: &Value, k: &str| x.get(k)?.as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let all: Vec<F> = arr
        .iter()
        .filter_map(|x| {
            Some(F { start: d(x, "start")?, end: d(x, "end")?, val: x.get("val")?.as_f64()?, filed: d(x, "filed")? })
        })
        .collect();
    // The latest filing wins when a period was restated.
    let latest = |start: NaiveDate, end: NaiveDate| {
        all.iter().filter(|f| f.start == start && f.end == end).max_by_key(|f| f.filed).map(|f| f.val)
    };
    let mut out: Vec<EpsQuarter> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for f in &all {
        let days = (f.end - f.start).num_days();
        if (80..=100).contains(&days) && seen.insert(f.end) {
            out.push(EpsQuarter { end: f.end, eps: latest(f.start, f.end).unwrap_or(f.val), derived: false });
        }
    }
    // Fourth quarters: a ~year-long period less the nine-month period sharing its start.
    for f in all.iter().filter(|f| (350..=380).contains(&(f.end - f.start).num_days())) {
        if seen.contains(&f.end) {
            continue;
        }
        let nine = all.iter().find(|n| {
            n.start == f.start
                && (260..=290).contains(&(n.end - n.start).num_days())
                && (f.end - n.end).num_days() < 110
        });
        if let (Some(n), Some(year)) = (nine, latest(f.start, f.end)) {
            let nine_val = latest(n.start, n.end).unwrap_or(n.val);
            seen.insert(f.end);
            out.push(EpsQuarter { end: f.end, eps: ((year - nine_val) * 100.0).round() / 100.0, derived: true });
        }
    }
    out.sort_by_key(|q| std::cmp::Reverse(q.end));
    out
}

// ---- merging -----------------------------------------------------------------------------

/// Combine Nasdaq's reported quarters (estimate, surprise) with EDGAR's quarters and 8-K dates
/// into one history, newest first. Each EDGAR quarter is dated by the first 2.02 8-K filed
/// after its period end; it joins a Nasdaq quarter reported within 4 days of that date.
pub fn merge_history(
    nasdaq: &[Reported],
    eps: &[EpsQuarter],
    releases: &[NaiveDate],
    timing: Timing,
    limit: usize,
) -> Vec<EarningsEvent> {
    let mut events: Vec<EarningsEvent> = nasdaq
        .iter()
        .map(|r| EarningsEvent {
            date: r.date,
            timing,
            period_end: None,
            fiscal_quarter: r.fiscal_quarter.clone(),
            eps_gaap: None,
            eps_actual: r.eps,
            eps_estimate: r.estimate,
            surprise_pct: r.surprise_pct,
            move_pct: None,
        })
        .collect();
    for q in eps {
        let Some(date) = releases.iter().copied().filter(|d| *d > q.end && (*d - q.end).num_days() <= 100).min() else {
            continue;
        };
        match events.iter_mut().find(|e| (e.date - date).num_days().abs() <= 4) {
            Some(e) => {
                e.eps_gaap = Some(q.eps);
                e.period_end = Some(q.end);
                e.date = date.min(e.date);
            }
            None => events.push(EarningsEvent {
                date,
                timing,
                period_end: Some(q.end),
                fiscal_quarter: format!("{}/{}", month_abbr(q.end.month()), q.end.year()),
                eps_gaap: Some(q.eps),
                eps_actual: None,
                eps_estimate: None,
                surprise_pct: None,
                move_pct: None,
            }),
        }
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.date));
    events.dedup_by_key(|e| e.date);
    events.truncate(limit);
    events
}

fn month_abbr(m: u32) -> &'static str {
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m as usize + 11) % 12]
}

/// Surprise percent from an actual and an estimate (relative to the estimate's magnitude).
pub fn surprise(actual: f64, estimate: f64) -> Option<f64> {
    (estimate != 0.0).then(|| (actual - estimate) / estimate.abs() * 100.0)
}

/// Close-to-close move (percent) around a report on `date` from ascending daily `bars`:
/// before the open → that day's close vs the prior close; after the close (or unknown, most
/// often after the close) → the next day's close vs the report day's close.
pub fn price_reaction(bars: &[Bar], date: NaiveDate, timing: Timing) -> Option<f64> {
    let day = |b: &Bar| chrono::DateTime::from_timestamp(b.ts, 0).map(|t| t.date_naive());
    // First bar on or after the report date.
    let i = bars.partition_point(|b| day(b).is_some_and(|d| d < date));
    let b = bars.get(i)?;
    day(b)?; // sanity
    let (from, to) = match timing {
        Timing::PreMarket => (bars.get(i.checked_sub(1)?)?, b),
        _ => (b, bars.get(i + 1)?),
    };
    // The report day itself must have traded (else the "first bar" is a later session).
    if day(b)? != date && timing != Timing::PreMarket {
        return None;
    }
    (from.close != 0.0).then(|| (to.close / from.close - 1.0) * 100.0)
}

/// A projected next report date: the last one plus the usual quarterly cadence, rolled
/// forward past `today`.
pub fn project_next(last: NaiveDate, today: NaiveDate) -> NaiveDate {
    let mut d = last;
    while d <= today {
        d += chrono::Duration::days(91);
    }
    // Land on a weekday.
    while d.weekday().number_from_monday() > 5 {
        d += chrono::Duration::days(1);
    }
    d
}

// ---- Nasdaq client -----------------------------------------------------------------------

pub struct Nasdaq {
    client: reqwest::Client,
    base: String,
}

impl Nasdaq {
    pub fn new() -> Result<Self> {
        Self::with_base("https://api.nasdaq.com")
    }

    pub fn with_base(base: &str) -> Result<Self> {
        use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, ORIGIN, REFERER};
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_static("application/json, text/plain, */*"));
        h.insert(ORIGIN, HeaderValue::from_static("https://www.nasdaq.com"));
        h.insert(REFERER, HeaderValue::from_static("https://www.nasdaq.com/"));
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
            .default_headers(h)
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self { client, base: base.trim_end_matches('/').into() })
    }

    async fn get(&self, path: &str) -> Result<String> {
        let resp = self.client.get(format!("{}{path}", self.base)).send().await.context("Nasdaq request failed")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Nasdaq earnings data unavailable (HTTP {status})");
        }
        resp.text().await.context("reading Nasdaq response")
    }

    pub async fn calendar(&self, date: NaiveDate) -> Result<Vec<CalendarRow>> {
        parse_calendar(&self.get(&format!("/api/calendar/earnings?date={date}")).await?, date)
    }

    /// The last four reported quarters of `ticker`.
    pub async fn reported(&self, ticker: &str) -> Result<Vec<Reported>> {
        parse_surprise(&self.get(&format!("/api/company/{}/earnings-surprise", ticker.to_ascii_lowercase())).await?)
    }
}

// ---- SQLite cache ------------------------------------------------------------------------

/// Calendar days and per-ticker histories, so panes paint from disk and the unofficial API is
/// hit sparingly. Days in the past never change once cached; today and the future expire.
pub struct EarningsStore {
    conn: Connection,
}

/// Seconds a not-yet-final calendar day stays fresh.
const DAY_TTL: i64 = 3 * 3600;
/// Seconds a cached ticker history stays fresh.
const HISTORY_TTL: i64 = 6 * 3600;

impl EarningsStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS earnings_days (
                day TEXT PRIMARY KEY, fetched INTEGER NOT NULL, rows TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS earnings_history (
                ticker TEXT PRIMARY KEY, fetched INTEGER NOT NULL, doc TEXT NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    pub fn put_day(&self, day: NaiveDate, rows: &[CalendarRow], now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO earnings_days (day, fetched, rows) VALUES (?1, ?2, ?3)",
            params![day.to_string(), now, serde_json::to_string(rows)?],
        )?;
        Ok(())
    }

    /// Cached rows for `day`; `fresh_only` drops a stale entry for a day that can still change.
    pub fn day(&self, day: NaiveDate, today: NaiveDate, now: i64, fresh_only: bool) -> Option<Vec<CalendarRow>> {
        let (fetched, rows): (i64, String) = self
            .conn
            .query_row("SELECT fetched, rows FROM earnings_days WHERE day = ?1", params![day.to_string()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok()?;
        // A past day only counts as final if it was fetched after it had passed.
        let final_day = day < today && fetched >= day_end(day);
        if fresh_only && !final_day && now - fetched > DAY_TTL {
            return None;
        }
        serde_json::from_str(&rows).ok()
    }

    pub fn put_history(&self, ticker: &str, doc: &TickerEarnings, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO earnings_history (ticker, fetched, doc) VALUES (?1, ?2, ?3)",
            params![ticker.to_ascii_uppercase(), now, serde_json::to_string(doc)?],
        )?;
        Ok(())
    }

    /// The cached history and whether it is still fresh.
    pub fn history(&self, ticker: &str, now: i64) -> Option<(TickerEarnings, bool)> {
        let (fetched, doc): (i64, String) = self
            .conn
            .query_row(
                "SELECT fetched, doc FROM earnings_history WHERE ticker = ?1",
                params![ticker.to_ascii_uppercase()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        Some((serde_json::from_str(&doc).ok()?, now - fetched <= HISTORY_TTL))
    }
}

fn day_end(d: NaiveDate) -> i64 {
    d.succ_opt().and_then(|n| n.and_hms_opt(0, 0, 0)).map_or(i64::MAX, |t| t.and_utc().timestamp())
}

// ---- service -----------------------------------------------------------------------------

/// Today's date in New York (approximated as UTC-4), the clock earnings dates are quoted in.
pub fn today_et() -> NaiveDate {
    (chrono::Utc::now() - chrono::Duration::hours(4)).date_naive()
}

/// Weekdays Monday..Friday of the week containing `d`.
pub fn week_days(d: NaiveDate) -> Vec<NaiveDate> {
    let mon = d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64);
    (0..5).map(|i| mon + chrono::Duration::days(i)).collect()
}

/// Rows for `day`, from the cache when fresh, else the network (falling back to a stale copy
/// when the network fails). Weekends have no reports and are not requested.
pub async fn calendar_day(
    store: &std::sync::Mutex<EarningsStore>,
    nasdaq: &Nasdaq,
    day: NaiveDate,
    today: NaiveDate,
    now: i64,
) -> Result<Vec<CalendarRow>> {
    if day.weekday().number_from_monday() > 5 {
        return Ok(Vec::new());
    }
    if let Some(rows) = store.lock().unwrap().day(day, today, now, true) {
        return Ok(rows);
    }
    match nasdaq.calendar(day).await {
        Ok(rows) => {
            let _ = store.lock().unwrap().put_day(day, &rows, now);
            Ok(rows)
        }
        Err(e) => store.lock().unwrap().day(day, today, now, false).ok_or(e),
    }
}

/// A ticker's earnings history (cached) with price reactions computed from daily `bars`.
/// EDGAR is optional: without it the history is Nasdaq's last four quarters.
pub async fn ticker_history(
    store: &std::sync::Mutex<EarningsStore>,
    nasdaq: &Nasdaq,
    edgar: &dyn EdgarSource,
    ticker: &str,
    today: NaiveDate,
    now: i64,
) -> Result<TickerEarnings> {
    let cached = store.lock().unwrap().history(ticker, now);
    if let Some((doc, true)) = &cached {
        return Ok(doc.clone());
    }
    let (reported, facts, subs) =
        tokio::join!(nasdaq.reported(ticker), edgar.company_facts(ticker), edgar.submissions(ticker));
    if let (Err(e), true) = (&reported, facts.is_err()) {
        return match cached {
            Some((doc, _)) => Ok(doc),
            None => Err(anyhow::anyhow!("{e:#}")),
        };
    }
    let reported = reported.unwrap_or_default();
    let eps = facts.as_ref().map(quarterly_eps).unwrap_or_default();
    let releases = subs.as_ref().map(release_dates).unwrap_or_default();
    // Nasdaq's per-ticker history has no time of day; the calendar row of the latest report does.
    let timing = match reported.first() {
        Some(r) => calendar_day(store, nasdaq, r.date, today, now)
            .await
            .ok()
            .and_then(|rows| rows.into_iter().find(|c| c.symbol.eq_ignore_ascii_case(ticker)))
            .map_or(Timing::Unknown, |c| c.timing),
        None => Timing::Unknown,
    };
    let events = merge_history(&reported, &eps, &releases, timing, 12);
    let next = find_next(store, nasdaq, ticker, events.first().map(|e| e.date), today, now).await;
    let doc = TickerEarnings { events, next };
    let _ = store.lock().unwrap().put_history(ticker, &doc, now);
    Ok(doc)
}

/// The announced next date if it appears in the calendar near the projected one, else the
/// projection.
async fn find_next(
    store: &std::sync::Mutex<EarningsStore>,
    nasdaq: &Nasdaq,
    ticker: &str,
    last: Option<NaiveDate>,
    today: NaiveDate,
    now: i64,
) -> Option<NextEarnings> {
    let last = last?;
    let proj = project_next(last, today);
    // Scan a window around the projection (cached days are free); stop at the first hit.
    let mut days: Vec<NaiveDate> =
        (-10..=21).map(|i| proj + chrono::Duration::days(i)).filter(|d| *d >= today).collect();
    days.sort();
    let mut failed = 0;
    for d in days {
        match calendar_day(store, nasdaq, d, today, now).await {
            Ok(rows) => {
                if let Some(r) = rows.into_iter().find(|r| r.symbol.eq_ignore_ascii_case(ticker)) {
                    return Some(NextEarnings {
                        date: d,
                        timing: r.timing,
                        eps_estimate: r.eps_estimate,
                        projected: false,
                    });
                }
            }
            Err(_) => {
                failed += 1;
                if failed >= 2 {
                    break;
                }
            }
        }
    }
    Some(NextEarnings { date: proj, timing: Timing::Unknown, eps_estimate: None, projected: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAL: &str = include_str!("../tests/fixtures/nasdaq_earnings.json");
    const SURPRISE: &str = include_str!("../tests/fixtures/nasdaq_surprise_aapl.json");
    const FACTS: &str = include_str!("../tests/fixtures/sec_eps_aapl.json");
    const SUBS: &str = include_str!("../tests/fixtures/sec_submissions_aapl.json");

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn money_formats() {
        assert_eq!(parse_money("$1.31"), Some(1.31));
        assert_eq!(parse_money("($0.12)"), Some(-0.12));
        assert_eq!(parse_money("-$0.12"), Some(-0.12));
        assert_eq!(parse_money("$2,230,197,160,000"), Some(2_230_197_160_000.0));
        assert_eq!(parse_money("N/A"), None);
        assert_eq!(parse_money(""), None);
    }

    #[test]
    fn calendar_fixture() {
        let rows = parse_calendar(CAL, d(2026, 10, 15)).unwrap();
        assert!(rows.len() >= 6);
        let tsm = rows.iter().find(|r| r.symbol == "TSM").unwrap();
        assert_eq!(tsm.timing, Timing::PreMarket);
        assert_eq!(tsm.eps_estimate, Some(4.45));
        assert_eq!(tsm.eps_last_year, Some(2.92));
        assert_eq!(tsm.eps_actual, None);
        assert!(tsm.market_cap.unwrap() > 2e12);
        assert_eq!(tsm.fiscal_quarter, "Sep/2026");
        assert_eq!(tsm.estimates, Some(6));
    }

    #[test]
    fn empty_and_error_days() {
        let none = r#"{"data":{"rows":null},"status":{"rCode":200}}"#;
        assert!(parse_calendar(none, d(2026, 10, 17)).unwrap().is_empty());
        assert!(parse_calendar("<html>blocked</html>", d(2026, 10, 17)).is_err());
        assert!(parse_calendar(r#"{"status":{"rCode":503,"bCodeMessage":"x"}}"#, d(2026, 10, 17)).is_err());
    }

    #[test]
    fn reported_rows_carry_actuals() {
        let json = r#"{"data":{"rows":[{"eps":"$1.91","surprise":"1.6","time":"time-not-supplied","symbol":"AAPL","name":"Apple Inc.","marketCap":"$5,005,219,700,000","fiscalQuarterEnding":"Jun/2026","epsForecast":"$1.88","noOfEsts":"11"}]},"status":{"rCode":200}}"#;
        let r = &parse_calendar(json, d(2026, 7, 30)).unwrap()[0];
        assert_eq!((r.eps_actual, r.surprise_pct, r.eps_last_year), (Some(1.91), Some(1.6), None));
        assert_eq!(r.timing, Timing::Unknown);
    }

    #[test]
    fn surprise_fixture() {
        let r = parse_surprise(SURPRISE).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].date, d(2026, 7, 30));
        assert_eq!((r[0].eps, r[0].estimate, r[0].surprise_pct), (Some(1.91), Some(1.88), Some(1.6)));
    }

    #[test]
    fn edgar_release_dates() {
        let subs: Value = serde_json::from_str(SUBS).unwrap();
        let dates = release_dates(&subs);
        assert_eq!(dates[..2], [d(2026, 7, 30), d(2026, 4, 30)]);
    }

    #[test]
    fn edgar_quarterly_eps() {
        let facts: Value = serde_json::from_str(FACTS).unwrap();
        let q = quarterly_eps(&facts);
        let get = |y, m, day| q.iter().find(|x| x.end == d(y, m, day)).unwrap();
        assert_eq!(get(2026, 6, 27).eps, 2.02);
        assert_eq!(get(2026, 3, 28).eps, 2.01);
        assert_eq!(get(2025, 12, 27).eps, 2.84);
        // FY2025 Q4 = 7.46 - 5.62, derived.
        let q4 = get(2025, 9, 27);
        assert!(q4.derived);
        assert!((q4.eps - 1.84).abs() < 1e-9);
        assert!(q.windows(2).all(|w| w[0].end > w[1].end));
    }

    #[test]
    fn merge_joins_sources() {
        let facts: Value = serde_json::from_str(FACTS).unwrap();
        let subs: Value = serde_json::from_str(SUBS).unwrap();
        let h = merge_history(
            &parse_surprise(SURPRISE).unwrap(),
            &quarterly_eps(&facts),
            &release_dates(&subs),
            Timing::AfterHours,
            12,
        );
        assert_eq!(h[0].date, d(2026, 7, 30));
        assert_eq!((h[0].eps_gaap, h[0].eps_actual, h[0].eps_estimate), (Some(2.02), Some(1.91), Some(1.88)));
        assert_eq!(h[0].period_end, Some(d(2026, 6, 27)));
        assert!(h.windows(2).all(|w| w[0].date > w[1].date));
    }

    #[test]
    fn merge_without_edgar_uses_nasdaq() {
        let h = merge_history(&parse_surprise(SURPRISE).unwrap(), &[], &[], Timing::Unknown, 12);
        assert_eq!(h.len(), 4);
        assert_eq!(h[0].eps_gaap, None);
    }

    fn bars(closes: &[(NaiveDate, f64)]) -> Vec<Bar> {
        closes
            .iter()
            .map(|(dt, c)| Bar {
                ts: dt.and_hms_opt(14, 30, 0).unwrap().and_utc().timestamp(),
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 1.0,
            })
            .collect()
    }

    #[test]
    fn reaction_depends_on_timing() {
        let b = bars(&[(d(2026, 7, 29), 100.0), (d(2026, 7, 30), 102.0), (d(2026, 7, 31), 110.0)]);
        let bmo = price_reaction(&b, d(2026, 7, 30), Timing::PreMarket).unwrap();
        assert!((bmo - 2.0).abs() < 1e-9);
        let amc = price_reaction(&b, d(2026, 7, 30), Timing::AfterHours).unwrap();
        assert!((amc - (110.0 / 102.0 - 1.0) * 100.0).abs() < 1e-9);
        // No following session yet.
        assert_eq!(price_reaction(&b, d(2026, 7, 31), Timing::AfterHours), None);
        // Before the history starts.
        assert_eq!(price_reaction(&b, d(2026, 7, 29), Timing::PreMarket), None);
    }

    #[test]
    fn surprise_and_projection() {
        assert!((surprise(1.1, 1.0).unwrap() - 10.0).abs() < 1e-9);
        assert!((surprise(-0.5, -1.0).unwrap() - 50.0).abs() < 1e-9);
        assert_eq!(surprise(1.0, 0.0), None);
        let p = project_next(d(2026, 7, 30), d(2026, 10, 16));
        assert_eq!(p, d(2026, 10, 29));
        assert!(p.weekday().number_from_monday() <= 5);
    }

    #[test]
    fn weeks_are_monday_to_friday() {
        let w = week_days(d(2026, 10, 16));
        assert_eq!((w[0], w[4]), (d(2026, 10, 12), d(2026, 10, 16)));
    }

    #[test]
    fn store_roundtrip_and_freshness() {
        let s = EarningsStore::open_in_memory().unwrap();
        let rows = parse_calendar(CAL, d(2026, 10, 15)).unwrap();
        let day = d(2026, 10, 15);
        let now = 1_792_000_000; // 2026-10-14..16 range, irrelevant to the TTL arithmetic
        s.put_day(day, &rows, now).unwrap();
        assert_eq!(s.day(day, day, now + 60, true).unwrap().len(), rows.len());
        // Today's entry goes stale; the stale copy is still available as a fallback.
        assert!(s.day(day, day, now + 4 * 3600, true).is_none());
        assert!(s.day(day, day, now + 4 * 3600, false).is_some());
        // A past day fetched after it ended is final.
        let fin = day_end(day) + 10;
        s.put_day(day, &rows, fin).unwrap();
        assert!(s.day(day, d(2026, 10, 20), fin + 10 * 86_400, true).is_some());

        let doc = TickerEarnings::default();
        s.put_history("aapl", &doc, now).unwrap();
        assert_eq!(s.history("AAPL", now + 10), Some((doc.clone(), true)));
        assert_eq!(s.history("AAPL", now + 7 * 3600), Some((doc, false)));
    }
}
