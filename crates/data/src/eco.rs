//! US economic calendar (Bloomberg `ECO`) and release history (`ECOF`) from free FRED sources.
//!
//! - Release dates: the FRED API (`fred/releases/dates`, free key in `FRED_API_KEY` or the
//!   settings) or, without a key, FRED's public release-calendar page for each release. FOMC
//!   decisions come from the Fed's published meeting schedule ([`FOMC_DECISIONS`]).
//! - Actual and prior: with a key, `fred/series/observations` with the release day as the
//!   real-time period (what ALFRED calls a vintage); without one, ALFRED's keyless `alfredgraph.csv` with `vintage_date` = the release
//!   day, so each row shows what was published that day, not today's revised numbers.
//! - History: `fred/series/observations` with a key, FRED's keyless `fredgraph.csv` without.
//! - Consensus and the world calendar: ForexFactory's free weekly calendar export
//!   (`ff_calendar_thisweek.json`, no key; `nextweek` and `lastweek` when published). US rows
//!   take their forecast from it; every other country's events are listed as [`GlobalRow`]s.
//!   The export is rate limited and answers with HTML when throttled, so bodies are validated
//!   before they are cached.
//!
//! Everything fetched is cached in SQLite; settled releases are cached for good.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, NaiveDate, Weekday};
use futures::StreamExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

const FRED_API: &str = "https://api.stlouisfed.org/fred";
const CALENDAR: &str = "https://fred.stlouisfed.org/releases/calendar";
const ALFRED_CSV: &str = "https://alfred.stlouisfed.org/graph/alfredgraph.csv";
const FRED_CSV: &str = "https://fred.stlouisfed.org/graph/fredgraph.csv";
/// ForexFactory's public calendar export for the current week (forecasts included).
const FF_THIS: &str = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";
/// Published on weekends only (404 otherwise); `lastweek` likewise.
const FF_NEXT: &str = "https://nfs.faireconomy.media/ff_calendar_nextweek.json";
const FF_LAST: &str = "https://nfs.faireconomy.media/ff_calendar_lastweek.json";
/// Environment variable holding a FRED API key (overrides the stored key).
pub const ENV_KEY: &str = "FRED_API_KEY";
/// Where to get a free key.
pub const KEY_URL: &str = "https://fred.stlouisfed.org/docs/api/api_key.html";

/// How a release's headline number is derived from its FRED series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// The value itself (rates, levels).
    Level,
    /// Difference from the previous observation (payrolls).
    Change,
    /// Percent change from the previous observation (MoM/QoQ).
    PctChange,
    /// Percent change from a year earlier.
    YoyPct,
}

/// One market-moving US release.
#[derive(Debug, PartialEq)]
pub struct EcoEvent {
    /// Short code used by `ECOF <code>`.
    pub code: &'static str,
    pub name: &'static str,
    /// FRED release id (0 = FOMC, dated from [`FOMC_DECISIONS`]).
    pub release_id: u32,
    pub series: &'static str,
    pub transform: Transform,
    /// Multiplier applied before display (e.g. claims in thousands).
    pub scale: f64,
    pub unit: &'static str,
    pub decimals: usize,
    /// 1 (low) to 3 (high).
    pub importance: u8,
    /// Usual release time, US Eastern (hour, minute).
    pub et: (u32, u32),
    /// Other names accepted by `ECOF`.
    pub aliases: &'static [&'static str],
}

impl EcoEvent {
    /// The headline number formatted with its unit.
    pub fn fmt(&self, v: f64) -> String {
        format!("{:.*}{}", self.decimals, v, self.unit)
    }

    /// A change between two headline numbers, signed.
    pub fn fmt_change(&self, d: f64) -> String {
        let unit = if self.unit == "%" { "pp" } else { self.unit };
        format!("{:+.*}{}", self.decimals, d, unit)
    }

    /// What the headline measures, for the series view.
    pub fn measure(&self) -> &'static str {
        match self.transform {
            Transform::Level => "level",
            Transform::Change => "change from previous period",
            Transform::PctChange => "% change from previous period",
            Transform::YoyPct => "% change from a year earlier",
        }
    }
}

macro_rules! ev {
    ($code:expr, $name:expr, $rid:expr, $series:expr, $t:ident, $scale:expr, $unit:expr, $dec:expr, $imp:expr, $h:expr, $m:expr, [$($a:expr),*]) => {
        EcoEvent {
            code: $code,
            name: $name,
            release_id: $rid,
            series: $series,
            transform: Transform::$t,
            scale: $scale,
            unit: $unit,
            decimals: $dec,
            importance: $imp,
            et: ($h, $m),
            aliases: &[$($a),*],
        }
    };
}

/// The curated release set. ISM PMIs are not on FRED (proprietary), so industrial production
/// stands in for manufacturing activity.
pub static EVENTS: &[EcoEvent] = &[
    ev!("CPI", "CPI MoM", 10, "CPIAUCSL", PctChange, 1.0, "%", 1, 3, 8, 30, ["INFLATION"]),
    ev!("CPIYOY", "CPI YoY", 10, "CPIAUCSL", YoyPct, 1.0, "%", 1, 3, 8, 30, ["CPI YOY"]),
    ev!("CORECPI", "Core CPI MoM", 10, "CPILFESL", PctChange, 1.0, "%", 1, 3, 8, 30, ["CORE CPI", "CPI CORE"]),
    ev!("PPI", "PPI Final Demand MoM", 46, "PPIFIS", PctChange, 1.0, "%", 1, 2, 8, 30, []),
    ev!("PCE", "PCE Price Index MoM", 54, "PCEPI", PctChange, 1.0, "%", 1, 2, 8, 30, ["PCE PRICES"]),
    ev!("COREPCE", "Core PCE Price Index MoM", 54, "PCEPILFE", PctChange, 1.0, "%", 1, 3, 8, 30, ["CORE PCE"]),
    ev!("NFP", "Nonfarm Payrolls", 50, "PAYEMS", Change, 1.0, "K", 0, 3, 8, 30, ["PAYROLLS", "JOBS"]),
    ev!("UNRATE", "Unemployment Rate", 50, "UNRATE", Level, 1.0, "%", 1, 3, 8, 30, ["UNEMPLOYMENT", "UR"]),
    ev!("CLAIMS", "Initial Jobless Claims", 180, "ICSA", Level, 0.001, "K", 0, 2, 8, 30, ["JOBLESS", "ICSA"]),
    ev!("GDP", "GDP QoQ Annualized", 53, "A191RL1Q225SBEA", Level, 1.0, "%", 1, 3, 8, 30, []),
    ev!("RETAIL", "Retail Sales MoM", 9, "RSAFS", PctChange, 1.0, "%", 1, 3, 8, 30, ["RETAIL SALES", "RSX"]),
    ev!("INDPRO", "Industrial Production MoM", 13, "INDPRO", PctChange, 1.0, "%", 1, 2, 9, 15, ["IP"]),
    ev!("HOUST", "Housing Starts", 27, "HOUST", Level, 1.0, "K", 0, 2, 8, 30, ["HOUSING", "STARTS"]),
    ev!("UMICH", "UMich Consumer Sentiment", 91, "UMCSENT", Level, 1.0, "", 1, 2, 10, 0, ["SENTIMENT", "UMCSENT"]),
    ev!("JOLTS", "JOLTS Job Openings", 192, "JTSJOL", Level, 0.001, "M", 2, 2, 10, 0, ["OPENINGS"]),
    ev!(
        "FOMC",
        "FOMC Rate Decision (Upper Bound)",
        0,
        "DFEDTARU",
        Level,
        1.0,
        "%",
        2,
        3,
        14,
        0,
        ["FED", "FFR", "RATE"]
    ),
];

/// Look an event up by code, alias or name (case-insensitive).
pub fn event(name: &str) -> Option<&'static EcoEvent> {
    let n = name.trim().to_ascii_uppercase();
    let squash = |s: &str| s.to_ascii_uppercase().replace([' ', '-', '_'], "");
    EVENTS
        .iter()
        .find(|e| e.code == n || e.aliases.iter().any(|a| *a == n))
        .or_else(|| EVENTS.iter().find(|e| squash(e.code) == squash(&n) || squash(e.name) == squash(&n)))
}

/// FOMC decision days (the second day of each meeting; statement at 2:00 pm ET), from
/// <https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm> (last updated
/// October 7, 2026). 2027 dates are tentative until confirmed at the preceding meeting.
pub const FOMC_DECISIONS: &[(i32, u32, u32)] = &[
    (2026, 1, 28),
    (2026, 3, 18),
    (2026, 4, 29),
    (2026, 6, 17),
    (2026, 7, 29),
    (2026, 9, 16),
    (2026, 10, 28),
    (2026, 12, 9),
    (2027, 1, 27),
    (2027, 3, 17),
    (2027, 4, 28),
    (2027, 6, 9),
    (2027, 7, 28),
    (2027, 9, 15),
    (2027, 10, 27),
    (2027, 12, 8),
];

pub fn fomc_dates() -> impl Iterator<Item = NaiveDate> {
    FOMC_DECISIONS.iter().filter_map(|&(y, m, d)| NaiveDate::from_ymd_opt(y, m, d))
}

// ---- time ---------------------------------------------------------------------------------

/// US daylight saving time is in effect on `d` (second Sunday of March to the first Sunday
/// of November; releases are never between midnight and 2 am, so the day is enough).
pub fn us_dst(d: NaiveDate) -> bool {
    let start = NaiveDate::from_weekday_of_month_opt(d.year(), 3, Weekday::Sun, 2);
    let end = NaiveDate::from_weekday_of_month_opt(d.year(), 11, Weekday::Sun, 1);
    matches!((start, end), (Some(s), Some(e)) if d >= s && d < e)
}

/// Unix seconds of `h:m` US Eastern on `d`.
pub fn et_to_utc(d: NaiveDate, (h, m): (u32, u32)) -> i64 {
    let offset = if us_dst(d) { 4 } else { 5 };
    d.and_hms_opt(h, m, 0).map_or(0, |t| t.and_utc().timestamp()) + offset * 3600
}

/// The Monday of `d`'s week.
pub fn monday(d: NaiveDate) -> NaiveDate {
    d - chrono::Days::new(d.weekday().num_days_from_monday() as u64)
}

// ---- parsing ------------------------------------------------------------------------------

/// A release date from either source, with the time when the source gives one (US Eastern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseDate {
    pub release_id: u32,
    pub date: NaiveDate,
    pub et: Option<(u32, u32)>,
}

#[derive(Deserialize)]
struct ApiDates {
    #[serde(default)]
    release_dates: Vec<ApiDate>,
}

#[derive(Deserialize)]
struct ApiDate {
    release_id: u32,
    date: String,
}

/// Parse a FRED API `fred/releases/dates` (or `fred/release/dates`) JSON response.
pub fn parse_api_dates(json: &str) -> Result<Vec<ReleaseDate>> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(msg) = v.get("error_message").and_then(|m| m.as_str())
    {
        bail!("FRED: {msg}");
    }
    let r: ApiDates = serde_json::from_str(json).context("FRED release dates")?;
    Ok(r.release_dates
        .into_iter()
        .filter_map(|d| {
            Some(ReleaseDate {
                release_id: d.release_id,
                date: NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").ok()?,
                et: None,
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct ApiObs {
    #[serde(default)]
    observations: Vec<ApiOb>,
}

#[derive(Deserialize)]
struct ApiOb {
    date: String,
    value: String,
}

/// Parse a FRED API `fred/series/observations` JSON response; missing values (`.`) are
/// skipped. Ascending by date.
pub fn parse_api_observations(json: &str) -> Result<Vec<(NaiveDate, f64)>> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(msg) = v.get("error_message").and_then(|m| m.as_str())
    {
        bail!("FRED: {msg}");
    }
    let r: ApiObs = serde_json::from_str(json).context("FRED observations")?;
    let mut v: Vec<(NaiveDate, f64)> = r
        .observations
        .into_iter()
        .filter_map(|o| Some((NaiveDate::parse_from_str(&o.date, "%Y-%m-%d").ok()?, o.value.trim().parse().ok()?)))
        .collect();
    v.sort_by_key(|o| o.0);
    Ok(v)
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `7:30 am` -> (7, 30).
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let s = s.trim().to_ascii_lowercase();
    let (hm, pm) = match (s.strip_suffix("am"), s.strip_suffix("pm")) {
        (Some(x), _) => (x.trim(), false),
        (_, Some(x)) => (x.trim(), true),
        _ => return None,
    };
    let (h, m) = hm.split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    if !(1..=12).contains(&h) || m > 59 {
        return None;
    }
    Some((h % 12 + if pm { 12 } else { 0 }, m))
}

/// Parse FRED's public release-calendar page (`/releases/calendar?rid=…`). Its times are
/// US Central (St. Louis); they are returned as Eastern.
pub fn parse_calendar_html(html: &str) -> Vec<ReleaseDate> {
    let start = html.find("release-dates-pager").unwrap_or(0);
    let mut out = Vec::new();
    let mut date: Option<NaiveDate> = None;
    for row in html[start..].split("<tr").skip(1) {
        if let Some(i) = row.find("font-weight: bold;\">") {
            let text = &row[i + "font-weight: bold;\">".len()..];
            let text = &text[..text.find('<').unwrap_or(text.len())];
            date = NaiveDate::parse_from_str(text.trim(), "%A %B %d, %Y").ok();
            continue;
        }
        let (Some(d), Some(i)) = (date, row.find("release?rid=")) else { continue };
        let digits: String = row[i + "release?rid=".len()..].chars().take_while(char::is_ascii_digit).collect();
        let Ok(release_id) = digits.parse() else { continue };
        let first_cell = row.split("</td>").next().unwrap_or_default();
        let et = parse_clock(&strip_tags(first_cell)).map(|(h, m)| ((h + 1) % 24, m));
        out.push(ReleaseDate { release_id, date: d, et });
    }
    out
}

/// Parse FRED/ALFRED graph CSV (`observation_date,SERIES[_vintage]`); missing values (empty
/// or `.`) are skipped. Ascending by date.
pub fn parse_obs_csv(csv: &str) -> Result<Vec<(NaiveDate, f64)>> {
    let mut lines = csv.lines();
    let header = lines.next().ok_or_else(|| anyhow!("empty FRED CSV"))?;
    if !header.to_ascii_lowercase().contains("date") {
        bail!("unexpected FRED CSV header: {}", header.chars().take(80).collect::<String>());
    }
    let mut v: Vec<(NaiveDate, f64)> = lines
        .filter_map(|l| {
            let (d, x) = l.split_once(',')?;
            Some((NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()?, x.trim().parse().ok()?))
        })
        .collect();
    v.sort_by_key(|o| o.0);
    Ok(v)
}

/// The headline series for `ev` from raw observations (ascending), scaled for display.
pub fn derive(ev: &EcoEvent, obs: &[(NaiveDate, f64)]) -> Vec<(NaiveDate, f64)> {
    let out: Vec<(NaiveDate, f64)> = match ev.transform {
        Transform::Level => obs.to_vec(),
        Transform::Change => obs.windows(2).map(|w| (w[1].0, w[1].1 - w[0].1)).collect(),
        Transform::PctChange => {
            obs.windows(2).filter(|w| w[0].1 != 0.0).map(|w| (w[1].0, (w[1].1 / w[0].1 - 1.0) * 100.0)).collect()
        }
        Transform::YoyPct => {
            let by_date: HashMap<NaiveDate, f64> = obs.iter().copied().collect();
            obs.iter()
                .filter_map(|&(d, x)| {
                    let base = by_date.get(&d.with_year(d.year() - 1)?).filter(|b| **b != 0.0)?;
                    Some((d, (x / base - 1.0) * 100.0))
                })
                .collect()
        }
    };
    out.into_iter().map(|(d, x)| (d, x * ev.scale)).collect()
}

// ---- rows ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// Scheduled in the future.
    Upcoming,
    /// Published; `actual` and `prior` are as of the release day.
    Released,
    /// The release time has passed but FRED has not posted the number yet.
    Pending,
    /// Published, but the values could not be fetched.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EcoRow {
    pub code: &'static str,
    pub date: NaiveDate,
    /// Release time, US Eastern.
    pub et: (u32, u32),
    /// Release time, Unix seconds.
    pub ts: i64,
    /// Observation period the actual refers to.
    pub period: Option<NaiveDate>,
    pub actual: Option<f64>,
    pub prior: Option<f64>,
    pub state: RowState,
    /// Consensus forecast as published (`0.4%`, `199K`), current week only.
    pub consensus: Option<String>,
}

impl EcoRow {
    pub fn event(&self) -> &'static EcoEvent {
        event(self.code).unwrap_or(&EVENTS[0])
    }

    pub fn change(&self) -> Option<f64> {
        Some(self.actual? - self.prior?)
    }

    /// The observation period as Bloomberg shows it (`Sep`, `Q3`, `Oct 10`).
    pub fn period_label(&self) -> String {
        let Some(p) = self.period else { return String::new() };
        let e = self.event();
        if e.series.starts_with("A191") {
            format!("Q{}", (p.month() - 1) / 3 + 1)
        } else if e.code == "CLAIMS" || e.code == "FOMC" {
            p.format("%b %d").to_string()
        } else {
            p.format("%b").to_string()
        }
    }
}

/// Latest actual/prior for one event when no release dates are available.
#[derive(Debug, Clone, PartialEq)]
pub struct Latest {
    pub code: &'static str,
    pub period: NaiveDate,
    pub actual: f64,
    pub prior: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSource {
    /// Official FRED API (key set).
    Api,
    /// FRED's public release-calendar pages (no key).
    Calendar,
    /// No release dates: only FOMC and the latest values are shown.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EcoWeek {
    pub monday: NaiveDate,
    pub rows: Vec<EcoRow>,
    pub source: DateSource,
    /// Latest values per event, filled when release dates are unavailable.
    pub latest: Vec<Latest>,
    pub notes: Vec<String>,
    /// Other countries' events (and US ones FRED does not cover), sorted by time.
    pub global: Vec<GlobalRow>,
    /// The ForexFactory export covers this week; `false` = no world data for it.
    pub global_covered: bool,
}

/// History of one event for the `ECOF` view.
#[derive(Debug, Clone, PartialEq)]
pub struct EcoSeries {
    pub code: &'static str,
    /// Headline values (after [`derive`]), ascending.
    pub values: Vec<(NaiveDate, f64)>,
    /// Raw series values, ascending.
    pub raw: Vec<(NaiveDate, f64)>,
}

/// Actual, prior and period of a release from the vintage published that day (`day`) and the
/// one before (`before`). `None` when the series did not change, i.e. not posted yet.
pub fn released_values(
    ev: &EcoEvent,
    day: &[(NaiveDate, f64)],
    before: &[(NaiveDate, f64)],
) -> Option<(NaiveDate, f64, Option<f64>)> {
    if day.is_empty() || day == before {
        return None;
    }
    let d = derive(ev, day);
    let (period, actual) = *d.last()?;
    let prior = d.len().checked_sub(2).map(|i| d[i].1);
    Some((period, actual, prior))
}

/// FOMC decision from the daily target series: the rate on the decision day vs the day before.
/// `None` until the day's value is posted.
pub fn fomc_values(obs: &[(NaiveDate, f64)], day: NaiveDate) -> Option<(NaiveDate, f64, Option<f64>)> {
    let i = obs.iter().rposition(|o| o.0 <= day)?;
    if obs[i].0 < day {
        return None;
    }
    let prior = i.checked_sub(1).map(|j| obs[j].1);
    Some((day, obs[i].1, prior))
}

/// Rows for the week starting `monday` from release dates (plus FOMC), sorted by time.
pub fn schedule(monday: NaiveDate, dates: &[ReleaseDate]) -> Vec<EcoRow> {
    let sunday = monday + chrono::Days::new(6);
    let in_week = |d: NaiveDate| d >= monday && d <= sunday;
    let mut rows = Vec::new();
    for e in EVENTS {
        let mut push = |date: NaiveDate, et: (u32, u32)| {
            if !rows.iter().any(|r: &EcoRow| r.code == e.code && r.date == date) {
                rows.push(EcoRow {
                    code: e.code,
                    date,
                    et,
                    ts: et_to_utc(date, et),
                    period: None,
                    actual: None,
                    prior: None,
                    state: RowState::Upcoming,
                    consensus: None,
                });
            }
        };
        if e.release_id == 0 {
            fomc_dates().filter(|d| in_week(*d)).for_each(|d| push(d, e.et));
        } else {
            for d in dates.iter().filter(|d| d.release_id == e.release_id && in_week(d.date)) {
                push(d.date, d.et.unwrap_or(e.et));
            }
        }
    }
    rows.sort_by(|a, b| a.ts.cmp(&b.ts).then(b.event().importance.cmp(&a.event().importance)));
    rows
}

// ---- consensus ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FfEvent {
    pub title: String,
    pub country: String,
    /// RFC 3339 with the US Eastern offset.
    pub date: String,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub forecast: String,
    #[serde(default)]
    pub previous: String,
    /// Not in the public export today; used when it appears.
    #[serde(default)]
    pub actual: Option<String>,
}

/// Whether a body looks like the export (a JSON array). Throttled requests get an HTML page,
/// sometimes with a 200.
fn looks_like_ff(body: &str) -> bool {
    body.trim_start().starts_with('[')
}

/// Parse ForexFactory's weekly calendar export. Throttled requests get an HTML page instead
/// of JSON; that is reported as such.
pub fn parse_ff(json: &str) -> Result<Vec<FfEvent>> {
    if !looks_like_ff(json) {
        bail!(
            "ForexFactory returned {} instead of the calendar (rate limited?)",
            if json.trim_start().starts_with('<') { "a web page" } else { "an unexpected response" }
        );
    }
    serde_json::from_str(json).context("ForexFactory calendar")
}

/// Whether a ForexFactory title is the release behind event `code`.
fn ff_matches(code: &str, title: &str) -> bool {
    let t = title.trim();
    match code {
        "CPI" => t == "CPI m/m",
        "CPIYOY" => t == "CPI y/y",
        "CORECPI" => t == "Core CPI m/m",
        "PPI" => t == "PPI m/m",
        "PCE" => t == "PCE Price Index m/m",
        "COREPCE" => t == "Core PCE Price Index m/m",
        "NFP" => t == "Non-Farm Employment Change",
        "UNRATE" => t == "Unemployment Rate",
        "CLAIMS" => t == "Unemployment Claims",
        // Advance, Prelim and Final estimates.
        "GDP" => t.ends_with("GDP q/q"),
        "RETAIL" => t == "Retail Sales m/m",
        "INDPRO" => t == "Industrial Production m/m",
        "HOUST" => t == "Housing Starts",
        "UMICH" => t.ends_with("UoM Consumer Sentiment"),
        "JOLTS" => t == "JOLTS Job Openings",
        "FOMC" => t == "Federal Funds Rate",
        _ => false,
    }
}

/// Put US consensus forecasts on the rows released the same (Eastern) day.
pub fn apply_consensus(rows: &mut [EcoRow], ff: &[FfEvent]) {
    for r in rows {
        let hit = ff.iter().find(|f| {
            f.country == "USD"
                && !f.forecast.trim().is_empty()
                && ff_matches(r.code, &f.title)
                && chrono::DateTime::parse_from_rfc3339(&f.date).is_ok_and(|d| d.date_naive() == r.date)
        });
        if let Some(f) = hit {
            r.consensus = Some(f.forecast.trim().to_string());
        }
    }
}

// ---- world calendar -----------------------------------------------------------------------

/// A country in the calendar. `ff` = covered by the ForexFactory export. `TR` has no source
/// yet (planned: TCMB / TUIK) and gets a filter chip only once rows with its code exist.
#[derive(Debug, PartialEq)]
pub struct Country {
    pub code: &'static str,
    pub currency: &'static str,
    pub name: &'static str,
    pub ff: bool,
    /// Other names accepted by `ECO <country>`.
    pub aliases: &'static [&'static str],
}

pub const COUNTRIES: &[Country] = &[
    Country { code: "US", currency: "USD", name: "United States", ff: true, aliases: &["USA"] },
    Country { code: "EU", currency: "EUR", name: "Euro area", ff: true, aliases: &["EZ", "EMU"] },
    Country { code: "GB", currency: "GBP", name: "United Kingdom", ff: true, aliases: &["UK"] },
    Country { code: "JP", currency: "JPY", name: "Japan", ff: true, aliases: &[] },
    Country { code: "CN", currency: "CNY", name: "China", ff: true, aliases: &[] },
    Country { code: "CA", currency: "CAD", name: "Canada", ff: true, aliases: &[] },
    Country { code: "AU", currency: "AUD", name: "Australia", ff: true, aliases: &[] },
    Country { code: "NZ", currency: "NZD", name: "New Zealand", ff: true, aliases: &[] },
    Country { code: "CH", currency: "CHF", name: "Switzerland", ff: true, aliases: &[] },
    // Extension point: a Turkish source (TCMB / TUIK) emits `GlobalRow`s with this code.
    Country { code: "TR", currency: "TRY", name: "Turkey", ff: false, aliases: &["TUR"] },
];

/// Events that belong to no single country (ForexFactory's `All`: IMF meetings, G20, ...).
pub const GLOBAL_CODE: &str = "GL";

/// The country code for a ForexFactory currency (`EUR` -> `EU`); unknown currencies keep
/// their own code so nothing is dropped.
pub fn country_of_currency(cur: &str) -> String {
    let c = cur.trim().to_ascii_uppercase();
    if c == "ALL" {
        return GLOBAL_CODE.into();
    }
    COUNTRIES.iter().find(|k| k.currency == c).map_or(c, |k| k.code.to_string())
}

/// Resolve a user-typed country (`US`, `usd`, `UK`, `euro area`) to its code.
pub fn parse_country(s: &str) -> Option<&'static str> {
    let n = s.trim().to_ascii_uppercase();
    COUNTRIES
        .iter()
        .find(|k| k.code == n || k.currency == n || k.aliases.contains(&n.as_str()) || k.name.to_ascii_uppercase() == n)
        .map(|k| k.code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impact {
    High,
    Medium,
    Low,
    Holiday,
}

impl Impact {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Self::High,
            "medium" => Self::Medium,
            "holiday" => Self::Holiday,
            _ => Self::Low,
        }
    }

    /// Importance bars: 3 high, 2 medium, 1 low, 0 holiday.
    pub fn importance(self) -> u8 {
        match self {
            Self::High => 3,
            Self::Medium => 2,
            Self::Low => 1,
            Self::Holiday => 0,
        }
    }
}

/// One non-FRED event (any country) from the ForexFactory export. Values are the strings as
/// published (`0.3%`, `-775M`).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalRow {
    /// A [`COUNTRIES`] code, or [`GLOBAL_CODE`].
    pub country: String,
    pub title: String,
    /// Date in US Eastern.
    pub date: NaiveDate,
    /// Release time, US Eastern.
    pub et: (u32, u32),
    /// Release time, Unix seconds.
    pub ts: i64,
    pub impact: Impact,
    /// Holidays carry no time.
    pub all_day: bool,
    pub forecast: Option<String>,
    pub previous: Option<String>,
    pub actual: Option<String>,
}

impl GlobalRow {
    pub fn importance(&self) -> u8 {
        self.impact.importance()
    }
}

/// The part of the export a Monday-based week uses: events from the Sunday before at 18:00 ET
/// (Asia's Monday morning) up to the next Sunday 18:00 ET.
pub fn week_window(monday: NaiveDate) -> (i64, i64) {
    let sunday = monday + chrono::Days::new(6);
    (et_to_utc(monday - chrono::Days::new(1), (18, 0)), et_to_utc(sunday, (18, 0)))
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The export's events in `monday`'s week as [`GlobalRow`]s, sorted by time. US events that
/// one of `us_rows` (the FRED rows) already covers are left out; US events FRED does not
/// cover stay.
pub fn global_rows(ff: &[FfEvent], monday: NaiveDate, us_rows: &[EcoRow]) -> Vec<GlobalRow> {
    use chrono::Timelike;
    let (from, to) = week_window(monday);
    let mut out: Vec<GlobalRow> = ff
        .iter()
        .filter_map(|f| {
            let t = chrono::DateTime::parse_from_rfc3339(&f.date).ok()?;
            let (ts, date) = (t.timestamp(), t.date_naive());
            if !(from..to).contains(&ts) {
                return None;
            }
            let country = country_of_currency(&f.country);
            if country == "US" && us_rows.iter().any(|r| r.date == date && ff_matches(r.code, &f.title)) {
                return None;
            }
            let impact = Impact::parse(&f.impact);
            Some(GlobalRow {
                country,
                title: f.title.trim().to_string(),
                date,
                et: (t.hour(), t.minute()),
                ts,
                impact,
                all_day: impact == Impact::Holiday,
                forecast: non_empty(&f.forecast),
                previous: non_empty(&f.previous),
                actual: f.actual.as_deref().and_then(non_empty),
            })
        })
        .collect();
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then(b.importance().cmp(&a.importance())).then(a.title.cmp(&b.title)));
    out.dedup_by(|a, b| a.ts == b.ts && a.country == b.country && a.title == b.title);
    out
}

/// A published figure as a number (`0.3%` -> 0.3, `-775M` -> -775); `None` for ranges and text.
pub fn figure(s: &str) -> Option<f64> {
    s.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%').parse().ok()
}

/// A consensus figure in the event's own unit, when the two can be compared (`0.4%` for a
/// percent release, `199K` for claims).
pub fn consensus_value(ev: &EcoEvent, s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() || c == '%' => (&s[..i], &s[i..]),
        _ => (s, ""),
    };
    (unit == ev.unit).then(|| num.parse().ok()).flatten()
}

// ---- key ----------------------------------------------------------------------------------

/// The FRED API key: `FRED_API_KEY` first, then the owner-only file `<data_dir>/secrets/fred.key`.
pub struct KeyStore {
    path: PathBuf,
}

impl KeyStore {
    pub fn new(data_dir: &Path) -> Self {
        Self { path: data_dir.join("secrets").join("fred.key") }
    }

    pub fn from_env() -> Option<String> {
        std::env::var(ENV_KEY).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    }

    pub fn stored(&self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    }

    /// The effective key (environment wins).
    pub fn key(&self) -> Option<String> {
        Self::from_env().or_else(|| self.stored())
    }

    pub fn describe(&self) -> String {
        format!("{} (owner-only)", self.path.display())
    }

    /// Save a key (0600 file in a 0700 directory). FRED keys are 32 lowercase alphanumerics.
    pub fn save(&self, key: &str) -> Result<()> {
        use std::io::Write;
        let key = key.trim();
        if key.len() != 32 || !key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            bail!("a FRED API key is 32 lowercase letters and digits");
        }
        let dir = self.path.parent().context("key path has no directory")?;
        std::fs::create_dir_all(dir)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            opts.mode(0o600);
        }
        let tmp = self.path.with_extension("tmp");
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(key.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Mask a key for display: `••••••••3f9a`.
pub fn mask(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 8 {
        "••••••••".into()
    } else {
        format!("••••••••{}", &k[k.len() - 4..])
    }
}

// ---- cache --------------------------------------------------------------------------------

/// SQLite cache of fetched bodies (with their fetch time) and settled release values.
pub struct EcoCache {
    conn: Mutex<Connection>,
}

impl EcoCache {
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
            "CREATE TABLE IF NOT EXISTS http (key TEXT PRIMARY KEY, fetched INTEGER NOT NULL, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS released (
                code TEXT NOT NULL, date TEXT NOT NULL, period TEXT NOT NULL, actual REAL NOT NULL, prior REAL,
                PRIMARY KEY (code, date)
             ) WITHOUT ROWID;",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// A cached body no older than `max_age` seconds (any age when `None`).
    pub fn get(&self, key: &str, now: i64, max_age: Option<i64>) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        let (fetched, body): (i64, String) = conn
            .query_row("SELECT fetched, body FROM http WHERE key = ?1", params![key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .ok()??;
        max_age.is_none_or(|a| now - fetched <= a).then_some(body)
    }

    pub fn put(&self, key: &str, now: i64, body: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO http (key, fetched, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET fetched = excluded.fetched, body = excluded.body",
                params![key, now, body],
            );
        }
    }

    pub fn released(&self, code: &str, date: NaiveDate) -> Option<(NaiveDate, f64, Option<f64>)> {
        let conn = self.conn.lock().ok()?;
        let (p, a, pr): (String, f64, Option<f64>) = conn
            .query_row(
                "SELECT period, actual, prior FROM released WHERE code = ?1 AND date = ?2",
                params![code, date.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()??;
        Some((NaiveDate::parse_from_str(&p, "%Y-%m-%d").ok()?, a, pr))
    }

    pub fn store_released(&self, code: &str, date: NaiveDate, v: (NaiveDate, f64, Option<f64>)) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO released (code, date, period, actual, prior) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![code, date.to_string(), v.0.to_string(), v.1, v.2],
            );
        }
    }
}

// ---- client -------------------------------------------------------------------------------

/// Fetches and caches calendar data. Cheap to share; the key is passed per call so a key
/// saved in the settings applies immediately.
pub struct Eco {
    client: reqwest::Client,
    cache: Option<EcoCache>,
    /// Hosts that recently failed to connect or answer, with the time they may be tried again.
    down: Mutex<HashMap<String, i64>>,
    /// Cache labels whose last answer was unusable (HTML, 404), with the time they may be
    /// asked again: the ForexFactory export is rate limited.
    backoff: Mutex<HashMap<String, i64>>,
}

const HOUR: i64 = 3600;
/// Requests in flight at once; FRED's edge drops clients that burst.
const CONCURRENCY: usize = 2;
/// How long a host that timed out is skipped, so one dead host costs one timeout, not one
/// per request.
const DOWN_FOR: i64 = 120;
/// The User-Agent every FRED / ALFRED request must send (see [`Eco::new`]).
pub(crate) const FRED_USER_AGENT: &str =
    concat!("curl/8.7.1 (compatible; terminal-rs/", env!("CARGO_PKG_VERSION"), ")");

impl Eco {
    pub fn new(cache: Option<EcoCache>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // FRED's edge silently stalls clients whose User-Agent does not start with a
            // download tool's name (`terminal-rs/…` and browser strings time out on every
            // fred/alfred host since Oct 2026); `curl/…` is let through.
            .user_agent(FRED_USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self { client, cache, down: Mutex::new(HashMap::new()), backoff: Mutex::new(HashMap::new()) })
    }

    /// GET `url` as text, served from the cache when younger than `max_age` seconds. A stale
    /// copy is used when the network fails. `label` replaces the URL in errors and cache keys
    /// (the URL may carry the API key).
    async fn text(&self, url: &str, label: &str, now: i64, max_age: Option<i64>) -> Result<String> {
        self.text_checked(url, label, now, max_age, None).await
    }

    /// [`Self::text`] that only accepts bodies passing `valid`, and after an unusable answer
    /// leaves the URL alone for the given number of seconds (serving the last good copy).
    async fn text_checked(
        &self,
        url: &str,
        label: &str,
        now: i64,
        max_age: Option<i64>,
        check: Option<(fn(&str) -> bool, i64)>,
    ) -> Result<String> {
        let ok = |b: &String| check.is_none_or(|(v, _)| v(b));
        if let Some(body) = self.cache.as_ref().and_then(|c| c.get(label, now, max_age)).filter(ok) {
            return Ok(body);
        }
        let backed_off = self.backoff.lock().ok().and_then(|b| b.get(label).copied()).is_some_and(|t| t > now);
        let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default();
        let fetched = async {
            if backed_off {
                bail!("{label}: asked recently, not asking again yet");
            }
            if self.down.lock().ok().and_then(|d| d.get(&host).copied()).is_some_and(|t| t > now) {
                bail!("{host} is unreachable");
            }
            let resp = match self.client.get(url).send().await {
                Ok(r) => r,
                Err(e) => {
                    if (e.is_timeout() || e.is_connect())
                        && let Ok(mut d) = self.down.lock()
                    {
                        d.insert(host.clone(), chrono::Utc::now().timestamp() + DOWN_FOR);
                    }
                    bail!("{host} did not answer ({})", if e.is_timeout() { "timed out" } else { "connection failed" });
                }
            };
            let status = resp.status();
            let body = resp.text().await.map_err(|e| anyhow!("{label}: {:?}", e.without_url()))?;
            let unusable = !status.is_success() || !ok(&body);
            if unusable
                && let Some((_, secs)) = check
                && let Ok(mut b) = self.backoff.lock()
            {
                b.insert(label.to_string(), chrono::Utc::now().timestamp() + secs);
            }
            if status.is_success() && !ok(&body) {
                bail!("{label}: unexpected response (rate limited?)");
            }
            if !status.is_success() {
                // FRED API errors carry a JSON message worth showing.
                if let Err(e) = parse_api_dates(&body) {
                    bail!("{label}: HTTP {status}: {e:#}");
                }
                bail!("{label}: HTTP {status}");
            }
            Ok(body)
        }
        .await;
        match fetched {
            Ok(body) => {
                if let Some(c) = &self.cache {
                    c.put(label, now, &body);
                }
                Ok(body)
            }
            Err(e) => self.cache.as_ref().and_then(|c| c.get(label, now, None)).filter(ok).ok_or(e),
        }
    }

    /// Release dates for `[from, to]` from the FRED API, including scheduled future dates.
    pub async fn api_dates(&self, key: &str, from: NaiveDate, to: NaiveDate, now: i64) -> Result<Vec<ReleaseDate>> {
        let url = format!(
            "{FRED_API}/releases/dates?realtime_start={from}&realtime_end={to}&include_release_dates_with_no_data=true&limit=1000&file_type=json&api_key={key}"
        );
        let body = self.text(&url, &format!("fred/releases/dates {from}..{to}"), now, Some(6 * HOUR)).await?;
        parse_api_dates(&body)
    }

    /// Release dates for one release in the half-year containing `day`, from FRED's public
    /// calendar page (one page holds 50 dates, more than a half-year of weekly claims).
    pub async fn calendar_dates(&self, rid: u32, day: NaiveDate, now: i64) -> Result<Vec<ReleaseDate>> {
        let (from, to) = if day.month() <= 6 {
            ((day.year(), 1, 1), (day.year(), 6, 30))
        } else {
            ((day.year(), 7, 1), (day.year(), 12, 31))
        };
        let ymd = |(y, m, d): (i32, u32, u32)| NaiveDate::from_ymd_opt(y, m, d).unwrap_or(day);
        let (from, to) = (ymd(from), ymd(to));
        let url = format!("{CALENDAR}?rid={rid}&vs={from}&ve={to}");
        let body = self.text(&url, &format!("fred calendar rid={rid} {from}..{to}"), now, Some(24 * HOUR)).await?;
        if !body.contains("release-dates-pager") {
            bail!("FRED release calendar (rid {rid}): unexpected page");
        }
        Ok(parse_calendar_html(&body).into_iter().filter(|d| d.release_id == rid).collect())
    }

    /// Observations of `series` from `from`, as published on `vintage` (real-time period of
    /// that day, ALFRED's "vintage date"), or the current revised data when `vintage` is
    /// `None`. With a `key` this uses the FRED API, otherwise the keyless CSV downloads.
    pub async fn observations(
        &self,
        key: Option<&str>,
        series: &str,
        from: NaiveDate,
        vintage: Option<NaiveDate>,
        now: i64,
    ) -> Result<Vec<(NaiveDate, f64)>> {
        // Past vintages never change; today's may still gain the release.
        let max_age = if vintage.is_some() { Some(10 * 60) } else { Some(HOUR) };
        if let Some(k) = key {
            let real = vintage.map(|v| format!("&realtime_start={v}&realtime_end={v}")).unwrap_or_default();
            let url = format!(
                "{FRED_API}/series/observations?series_id={series}&observation_start={from}{real}&file_type=json&api_key={k}"
            );
            let label = match vintage {
                Some(v) => format!("fred api {series} {from} @{v}"),
                None => format!("fred api {series} {from}"),
            };
            return parse_api_observations(&self.text(&url, &label, now, max_age).await?);
        }
        let (url, label) = match vintage {
            Some(v) => (
                format!("{ALFRED_CSV}?id={series}&cosd={from}&vintage_date={v}"),
                format!("alfred {series} {from} @{v}"),
            ),
            None => (format!("{FRED_CSV}?id={series}&cosd={from}"), format!("fred {series} {from}")),
        };
        parse_obs_csv(&self.text(&url, &label, now, max_age).await?)
    }

    /// The calendar for the week starting `monday` (a Monday), with values for every release
    /// whose time has passed.
    pub async fn week(&self, key: Option<&str>, monday: NaiveDate, now: i64) -> EcoWeek {
        let sunday = monday + chrono::Days::new(6);
        let mut notes = Vec::new();
        let mut source = DateSource::Unavailable;
        let mut dates = Vec::new();
        if let Some(k) = key {
            match self.api_dates(k, monday, sunday, now).await {
                Ok(d) => {
                    dates = d;
                    source = DateSource::Api;
                }
                Err(e) => notes.push(format!("FRED API: {e:#}")),
            }
        }
        if source == DateSource::Unavailable {
            let mut rids: Vec<u32> = EVENTS.iter().map(|e| e.release_id).filter(|&r| r != 0).collect();
            rids.sort_unstable();
            rids.dedup();
            // The week may straddle two half-years.
            let mut halves = vec![monday];
            if (sunday.month() <= 6) != (monday.month() <= 6) || sunday.year() != monday.year() {
                halves.push(sunday);
            }
            let jobs: Vec<(u32, NaiveDate)> = rids.iter().flat_map(|&r| halves.iter().map(move |&h| (r, h))).collect();
            let jobs: Vec<_> = jobs.into_iter().map(|(r, h)| self.calendar_dates(r, h, now)).collect();
            let results: Vec<Result<Vec<ReleaseDate>>> =
                futures::stream::iter(jobs).buffer_unordered(CONCURRENCY).collect().await;
            let failed = results.iter().filter(|r| r.is_err()).count();
            if failed < results.len() {
                source = DateSource::Calendar;
                dates = results.into_iter().filter_map(Result::ok).flatten().collect();
                if failed > 0 {
                    notes.push(format!("{failed} release calendars could not be loaded; some rows may be missing"));
                }
            } else if let Some(Err(e)) = results.into_iter().next() {
                notes.push(format!("release calendar: {e:#}"));
            }
        }
        let mut rows = schedule(monday, &dates);
        let jobs: Vec<_> = rows.iter_mut().map(|r| self.fill(key, r, now)).collect();
        futures::stream::iter(jobs).buffer_unordered(CONCURRENCY).collect::<Vec<()>>().await;
        // The export covers the current week, plus the next and last when it publishes them.
        let (mut global, mut global_covered) = (Vec::new(), false);
        match self.ff(monday, now).await {
            Ok(Some(ff)) => {
                apply_consensus(&mut rows, &ff);
                global = global_rows(&ff, monday, &rows);
                global_covered = !global.is_empty() || ff.iter().any(|f| f.country == "USD");
            }
            Ok(None) => {}
            Err(e) => notes.push(format!("world calendar: {e:#}")),
        }
        let latest = if source == DateSource::Unavailable { self.latest(key, now).await } else { Vec::new() };
        if let Ok(d) = self.down.lock() {
            let mut hosts: Vec<&String> = d.iter().filter(|&(_, &t)| t > now).map(|(h, _)| h).collect();
            hosts.sort();
            for h in hosts {
                notes.push(format!("{h} is not answering from this network; values from it are unavailable"));
            }
        }
        EcoWeek { monday, rows, source, latest, notes, global, global_covered }
    }

    /// The ForexFactory export that covers the week of `monday`: this, next or last week's
    /// (cached, and not asked again soon after a throttled or missing answer). `None` when no
    /// export can cover it.
    async fn ff(&self, monday: NaiveDate, now: i64) -> Result<Option<Vec<FfEvent>>> {
        // The export's weeks run on US Eastern dates.
        let utc = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default();
        let today = (utc - chrono::Duration::hours(if us_dst(utc.date_naive()) { 4 } else { 5 })).date_naive();
        let (url, label, ttl) = match (monday - self::monday(today)).num_days() {
            0 => (FF_THIS, "forexfactory thisweek", HOUR),
            7 => (FF_NEXT, "forexfactory nextweek", 6 * HOUR),
            -7 => (FF_LAST, "forexfactory lastweek", 6 * HOUR),
            _ => return Ok(None),
        };
        // A missing or throttled export is not asked again for ten minutes.
        match self.text_checked(url, label, now, Some(ttl), Some((looks_like_ff, 600))).await {
            Ok(b) => parse_ff(&b).map(Some),
            // nextweek / lastweek simply do not exist most of the time (404).
            Err(_) if url != FF_THIS => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Fill a row's values once its release time has passed.
    async fn fill(&self, key: Option<&str>, row: &mut EcoRow, now: i64) {
        if row.ts > now {
            row.state = RowState::Upcoming;
            return;
        }
        let ev = row.event();
        if let Some(v) = self.cache.as_ref().and_then(|c| c.released(ev.code, row.date)) {
            row.set(v);
            return;
        }
        // Two years of history covers year-over-year and quarterly priors.
        let from = row.date - chrono::Days::new(800);
        let values = if ev.release_id == 0 {
            self.observations(key, ev.series, from, None, now).await.map(|obs| fomc_values(&obs, row.date))
        } else {
            let day = self.observations(key, ev.series, from, Some(row.date), now).await;
            // On the day itself, compare with the day before to tell a posted release from one
            // FRED has not picked up yet; later, the day's vintage is enough.
            let before = if now - row.ts < 24 * HOUR {
                self.observations(key, ev.series, from, Some(row.date - chrono::Days::new(1)), now).await
            } else {
                Ok(Vec::new())
            };
            match (day, before) {
                (Ok(d), Ok(b)) => Ok(released_values(ev, &d, &b)),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        };
        match values {
            Ok(Some(v)) => {
                row.set(v);
                if let Some(c) = &self.cache {
                    c.store_released(ev.code, row.date, v);
                }
            }
            // FRED usually posts within the hour; after a few days, stop expecting it.
            Ok(None) if now - row.ts < 3 * 24 * HOUR => row.state = RowState::Pending,
            Ok(None) => row.state = RowState::Unavailable,
            Err(e) => {
                tracing::debug!("eco {} {}: {e:#}", ev.code, row.date);
                row.state = RowState::Unavailable;
            }
        }
    }

    /// Latest actual/prior of every event from current data (no dates needed).
    pub async fn latest(&self, key: Option<&str>, now: i64) -> Vec<Latest> {
        let today = chrono::DateTime::from_timestamp(now, 0).map_or(NaiveDate::MIN, |t| t.date_naive());
        let from = today - chrono::Days::new(800);
        let jobs: Vec<_> = EVENTS.iter().map(|e| self.observations(key, e.series, from, None, now)).collect();
        let fetched: Vec<_> = futures::stream::iter(jobs).buffered(CONCURRENCY).collect().await;
        EVENTS
            .iter()
            .zip(fetched)
            .filter_map(|(e, obs)| {
                let d = derive(e, &obs.ok()?);
                let &(period, actual) = d.last()?;
                Some(Latest { code: e.code, period, actual, prior: d.len().checked_sub(2).map(|i| d[i].1) })
            })
            .collect()
    }

    /// History for `ECOF`: about 15 years of the event's series.
    pub async fn series(&self, key: Option<&str>, ev: &'static EcoEvent, now: i64) -> Result<EcoSeries> {
        let today = chrono::DateTime::from_timestamp(now, 0).map_or(NaiveDate::MIN, |t| t.date_naive());
        let raw = self.observations(key, ev.series, today - chrono::Days::new(15 * 366), None, now).await?;
        if raw.is_empty() {
            bail!("no observations for {}", ev.series);
        }
        Ok(EcoSeries { code: ev.code, values: derive(ev, &raw), raw })
    }
}

impl EcoRow {
    fn set(&mut self, (period, actual, prior): (NaiveDate, f64, Option<f64>)) {
        self.period = Some(period);
        self.actual = Some(actual);
        self.prior = prior;
        self.state = RowState::Released;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consensus_from_forexfactory() {
        let json = r#"[
            {"title":"CPI m/m","country":"USD","date":"2026-10-14T08:30:00-04:00","impact":"High","forecast":"0.6%","previous":"0.4%"},
            {"title":"CPI m/m","country":"GBP","date":"2026-10-14T02:00:00-04:00","impact":"High","forecast":"0.9%","previous":"0.4%"},
            {"title":"Unemployment Claims","country":"USD","date":"2026-10-15T08:30:00-04:00","impact":"Medium","forecast":"199K","previous":"197K"},
            {"title":"Prelim UoM Consumer Sentiment","country":"USD","date":"2026-10-16T10:00:00-04:00","impact":"High","forecast":"","previous":"55.1"}
        ]"#;
        let ff = parse_ff(json).unwrap();
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        let mut rows = schedule(
            d(12),
            &[
                ReleaseDate { release_id: 10, date: d(14), et: None },
                ReleaseDate { release_id: 180, date: d(15), et: None },
                ReleaseDate { release_id: 91, date: d(16), et: None },
            ],
        );
        apply_consensus(&mut rows, &ff);
        let cons = |code: &str| rows.iter().find(|r| r.code == code).and_then(|r| r.consensus.clone());
        assert_eq!(cons("CPI").as_deref(), Some("0.6%"));
        assert_eq!(cons("CPIYOY"), None);
        assert_eq!(cons("CLAIMS").as_deref(), Some("199K"));
        assert_eq!(cons("UMICH"), None);
        assert_eq!(consensus_value(event("CPI").unwrap(), "0.6%"), Some(0.6));
        assert_eq!(consensus_value(event("CLAIMS").unwrap(), "199K"), Some(199.0));
        assert_eq!(consensus_value(event("HOUST").unwrap(), "1.31M"), None);
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn events_are_unique_and_resolvable() {
        for (i, e) in EVENTS.iter().enumerate() {
            assert!(EVENTS[..i].iter().all(|o| o.code != e.code), "{}", e.code);
            assert_eq!(event(e.code), Some(e));
            assert!((1..=3).contains(&e.importance));
        }
        assert_eq!(event("core cpi").unwrap().code, "CORECPI");
        assert_eq!(event("payrolls").unwrap().code, "NFP");
        assert_eq!(event("Core-PCE").unwrap().code, "COREPCE");
        assert_eq!(event("nonfarm payrolls").unwrap().code, "NFP");
        assert!(event("ISM").is_none());
    }

    #[test]
    fn eastern_time_follows_us_dst() {
        assert!(!us_dst(d(2026, 3, 7)) && us_dst(d(2026, 3, 8)), "starts the second Sunday of March");
        assert!(us_dst(d(2026, 10, 31)) && !us_dst(d(2026, 11, 1)), "ends the first Sunday of November");
        // CPI 2026-10-14 08:30 EDT = 12:30 UTC; 2026-12-10 08:30 EST = 13:30 UTC.
        assert_eq!(
            et_to_utc(d(2026, 10, 14), (8, 30)),
            d(2026, 10, 14).and_hms_opt(12, 30, 0).unwrap().and_utc().timestamp()
        );
        assert_eq!(
            et_to_utc(d(2026, 12, 10), (8, 30)),
            d(2026, 12, 10).and_hms_opt(13, 30, 0).unwrap().and_utc().timestamp()
        );
        assert_eq!(monday(d(2026, 10, 16)), d(2026, 10, 12));
        assert_eq!(monday(d(2026, 10, 12)), d(2026, 10, 12));
        assert_eq!(monday(d(2026, 10, 18)), d(2026, 10, 12));
    }

    #[test]
    fn parses_public_release_calendar() {
        let dates = parse_calendar_html(include_str!("../tests/fixtures/fred_calendar_cpi_2026.html"));
        assert_eq!(dates.len(), 12, "{dates:?}");
        assert!(dates.iter().all(|r| r.release_id == 10));
        assert_eq!(dates[0], ReleaseDate { release_id: 10, date: d(2026, 1, 13), et: Some((8, 30)) });
        assert!(dates.iter().any(|r| r.date == d(2026, 10, 14)));
        assert!(dates.windows(2).all(|w| w[0].date < w[1].date));
        assert_eq!(parse_clock("12:00 pm"), Some((12, 0)));
        assert_eq!(parse_clock("12:15 am"), Some((0, 15)));
        assert_eq!(parse_clock("1:00 pm"), Some((13, 0)));
        assert_eq!(parse_clock("noon"), None);
    }

    #[test]
    fn parses_api_release_dates() {
        let dates = parse_api_dates(include_str!("../tests/fixtures/fred_releases_dates.json")).unwrap();
        assert!(dates.iter().any(|r| r.release_id == 10 && r.date == d(2026, 10, 14)));
        assert!(dates.iter().all(|r| r.et.is_none()));
        let err = parse_api_dates(
            r#"{"error_code":400,"error_message":"Bad Request.  The value for variable api_key is not registered."}"#,
        );
        assert!(format!("{:#}", err.unwrap_err()).contains("not registered"));
    }

    #[test]
    fn schedule_builds_sorted_week_with_fomc() {
        let dates = parse_api_dates(include_str!("../tests/fixtures/fred_releases_dates.json")).unwrap();
        let rows = schedule(d(2026, 10, 12), &dates);
        let codes: Vec<&str> = rows.iter().map(|r| r.code).collect();
        assert!(codes.contains(&"CPI") && codes.contains(&"CORECPI") && codes.contains(&"CLAIMS"), "{codes:?}");
        assert!(!codes.contains(&"FOMC"));
        assert!(rows.windows(2).all(|w| w[0].ts <= w[1].ts));
        assert!(rows.iter().all(|r| r.date >= d(2026, 10, 12) && r.date <= d(2026, 10, 18)));
        let next = schedule(d(2026, 10, 26), &[]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].code, "FOMC");
        assert_eq!(next[0].date, d(2026, 10, 28));
        assert_eq!(next[0].ts, d(2026, 10, 28).and_hms_opt(18, 0, 0).unwrap().and_utc().timestamp());
    }

    const FF_SAMPLE: &str = include_str!("../tests/fixtures/ff_calendar_thisweek.json");

    #[test]
    fn parses_ff_export_and_rejects_html() {
        let ff = parse_ff(FF_SAMPLE).unwrap();
        assert_eq!(ff.len(), 43);
        assert_eq!(ff.last().and_then(|f| f.actual.as_deref()), Some("0.2%"));
        assert_eq!(ff[0].actual, None);
        // Throttled / missing exports are HTML, sometimes with a 200.
        let e = parse_ff("<html><body>Too many requests</body></html>").unwrap_err();
        assert!(format!("{e:#}").contains("web page"), "{e:#}");
        assert!(parse_ff("").is_err());
        assert!(parse_ff("{\"error\":1}").is_err());
        assert!(looks_like_ff(" \n[]") && !looks_like_ff("<html>"));
    }

    #[test]
    fn countries_resolve() {
        assert_eq!(country_of_currency("EUR"), "EU");
        assert_eq!(country_of_currency("GBP"), "GB");
        assert_eq!(country_of_currency("All"), GLOBAL_CODE);
        assert_eq!(country_of_currency("XYZ"), "XYZ");
        assert_eq!(parse_country("uk"), Some("GB"));
        assert_eq!(parse_country("usd"), Some("US"));
        assert_eq!(parse_country("EU"), Some("EU"));
        assert_eq!(parse_country("Turkey"), Some("TR"));
        assert_eq!(parse_country("ALL"), None);
        assert!(COUNTRIES.iter().filter(|k| !k.ff).all(|k| k.code == "TR"));
        assert_eq!(Impact::parse("High").importance(), 3);
        assert_eq!(Impact::parse("Medium").importance(), 2);
        assert_eq!(Impact::parse("Low").importance(), 1);
        assert_eq!(Impact::parse("Holiday").importance(), 0);
        assert_eq!(Impact::parse("").importance(), 1);
    }

    #[test]
    fn global_rows_cover_the_week_and_skip_us_rows_fred_has() {
        let ff = parse_ff(FF_SAMPLE).unwrap();
        let dates = [
            ReleaseDate { release_id: 27, date: d(2026, 10, 20), et: None },
            ReleaseDate { release_id: 180, date: d(2026, 10, 22), et: None },
            ReleaseDate { release_id: 91, date: d(2026, 10, 23), et: None },
        ];
        let mut us = schedule(d(2026, 10, 19), &dates);
        apply_consensus(&mut us, &ff);
        let g = global_rows(&ff, d(2026, 10, 19), &us);
        assert!(g.windows(2).all(|w| w[0].ts <= w[1].ts));
        let has = |c: &str, t: &str| g.iter().any(|r| r.country == c && r.title == t);
        // FRED covers these: no duplicates.
        assert!(!has("US", "Housing Starts"));
        assert!(!has("US", "Unemployment Claims"));
        assert!(!has("US", "Revised UoM Consumer Sentiment"));
        // FRED does not: kept.
        assert!(has("US", "Flash Manufacturing PMI"));
        assert!(has("US", "Building Permits"));
        // Other countries.
        assert!(has("CA", "CPI m/m") && has("GB", "CPI y/y") && has("NZ", "CPI q/q"));
        assert!(has("EU", "German Flash Services PMI") && has("JP", "National Core CPI y/y"));
        assert!(has("CN", "GDP q/y") && has("AU", "Flash Services PMI"));
        // Before Sunday 18:00 ET of the week: left to the previous week.
        assert!(!g.iter().any(|r| r.title == "IMF Meetings"));
        let gb = g.iter().find(|r| r.country == "GB" && r.title == "CPI y/y").unwrap();
        assert_eq!((gb.date, gb.et, gb.impact, gb.importance()), (d(2026, 10, 21), (2, 0), Impact::High, 3));
        assert_eq!(gb.forecast.as_deref(), Some("3.4%"));
        assert_eq!(gb.previous.as_deref(), Some("3.1%"));
        assert_eq!(gb.actual, None);
        assert_eq!(gb.ts, et_to_utc(d(2026, 10, 21), (2, 0)));
        // Empty figures are None; holidays have no time of day.
        let api = g.iter().find(|r| r.title == "API Weekly Statistical Bulletin").unwrap();
        assert_eq!((&api.forecast, &api.previous), (&None, &None));
        let hol = g.iter().find(|r| r.country == "EU" && r.impact == Impact::Holiday).unwrap();
        assert!(hol.all_day && hol.importance() == 0);
        // The Sunday-evening Asian prints belong to this week, not the one before.
        assert!(has("CN", "Unemployment Rate"));
        let before = global_rows(&ff, d(2026, 10, 12), &us);
        assert!(before.iter().all(|r| r.ts < et_to_utc(d(2026, 10, 18), (18, 0))));
        // An optional actual comes through.
        let chf = g.iter().find(|r| r.country == "CH" && r.title == "CPI m/m").unwrap();
        assert_eq!(chf.actual.as_deref(), Some("0.2%"));
        // With no FRED rows (no key), the US events stay as FF-only rows.
        let all = global_rows(&ff, d(2026, 10, 19), &[]);
        assert!(all.iter().any(|r| r.country == "US" && r.title == "Housing Starts"));
        // A week outside the export has nothing.
        assert!(global_rows(&ff, d(2026, 11, 2), &[]).is_empty());
    }

    #[test]
    fn global_rows_drop_exact_duplicates_and_bad_dates() {
        let ev = |title: &str, date: &str| FfEvent {
            title: title.into(),
            country: "EUR".into(),
            date: date.into(),
            impact: "High".into(),
            forecast: " 1.0% ".into(),
            previous: String::new(),
            actual: Some("  ".into()),
        };
        let ff = [ev("X", "2026-10-21T03:00:00-04:00"), ev("X", "2026-10-21T03:00:00-04:00"), ev("Y", "not a date")];
        let g = global_rows(&ff, d(2026, 10, 19), &[]);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].forecast.as_deref(), Some("1.0%"));
        assert_eq!((g[0].previous.as_deref(), g[0].actual.as_deref()), (None, None));
    }

    #[test]
    fn figures_compare() {
        assert_eq!(figure("0.3%"), Some(0.3));
        assert_eq!(figure("-775M"), Some(-775.0));
        assert_eq!(figure("1.30M"), Some(1.3));
        assert_eq!(figure("2.1|3"), None);
        assert_eq!(figure(""), None);
    }

    #[test]
    fn derives_headlines() {
        let obs = parse_obs_csv(include_str!("../tests/fixtures/alfred_cpi_20261014.csv")).unwrap();
        let cpi = event("CPI").unwrap();
        let mom = derive(cpi, &obs);
        let (p, v) = *mom.last().unwrap();
        assert_eq!(p, d(2026, 9, 1));
        assert!((v - (336.298 / 334.131 - 1.0) * 100.0).abs() < 1e-9);
        let yoy = derive(event("CPIYOY").unwrap(), &obs);
        assert_eq!(yoy.first().unwrap().0, d(2026, 1, 1), "year-over-year starts a year in");
        let claims = event("CLAIMS").unwrap();
        assert_eq!(derive(claims, &[(d(2026, 10, 10), 199_000.0)]), vec![(d(2026, 10, 10), 199.0)]);
        let nfp = event("NFP").unwrap();
        assert_eq!(derive(nfp, &[(d(2026, 8, 1), 100.0), (d(2026, 9, 1), 150.0)]), vec![(d(2026, 9, 1), 50.0)]);
        assert_eq!(cpi.fmt(0.2567), "0.3%");
        assert_eq!(cpi.fmt_change(-0.1), "-0.1pp");
        assert_eq!(claims.fmt(199.0), "199K");
    }

    #[test]
    fn release_values_come_from_the_release_day_vintage() {
        let cpi = event("CPI").unwrap();
        let day = parse_obs_csv(include_str!("../tests/fixtures/alfred_cpi_20261014.csv")).unwrap();
        let before = parse_obs_csv(include_str!("../tests/fixtures/alfred_cpi_20261013.csv")).unwrap();
        let (period, actual, prior) = released_values(cpi, &day, &before).unwrap();
        assert_eq!(period, d(2026, 9, 1));
        assert!((actual - 0.6485).abs() < 1e-3, "{actual}");
        assert!((prior.unwrap() - (334.131 / 332.813 - 1.0) * 100.0).abs() < 1e-9);
        assert_eq!(released_values(cpi, &before, &before), None, "unchanged vintage = not posted yet");
    }

    #[test]
    fn parses_api_observations() {
        let obs = parse_api_observations(include_str!("../tests/fixtures/fred_observations_cpi.json")).unwrap();
        assert_eq!(obs.len(), 3, "the `.` row is skipped");
        assert_eq!(obs[0], (d(2026, 7, 1), 332.813));
        assert_eq!(obs[2], (d(2026, 9, 1), 334.131));
        let err = parse_api_observations(
            r#"{"error_code":400,"error_message":"Bad Request. The value for variable api_key is not registered."}"#,
        );
        assert!(format!("{:#}", err.unwrap_err()).contains("not registered"));
    }

    #[test]
    fn fomc_decision_needs_the_days_value() {
        let obs = vec![(d(2026, 9, 15), 4.25), (d(2026, 9, 16), 4.0), (d(2026, 9, 17), 4.0)];
        assert_eq!(fomc_values(&obs, d(2026, 9, 16)), Some((d(2026, 9, 16), 4.0, Some(4.25))));
        assert_eq!(fomc_values(&obs, d(2026, 9, 18)), None);
        assert!(fomc_dates().all(|d| d.weekday() == Weekday::Wed));
    }

    #[test]
    fn cache_round_trip_and_ttl() {
        let c = EcoCache::open_in_memory().unwrap();
        c.put("k", 1_000, "body");
        assert_eq!(c.get("k", 1_500, Some(600)).as_deref(), Some("body"));
        assert_eq!(c.get("k", 2_000, Some(600)), None);
        assert_eq!(c.get("k", 9_999, None).as_deref(), Some("body"));
        c.store_released("CPI", d(2026, 10, 14), (d(2026, 9, 1), 0.6, Some(0.4)));
        assert_eq!(c.released("CPI", d(2026, 10, 14)), Some((d(2026, 9, 1), 0.6, Some(0.4))));
        assert_eq!(c.released("CPI", d(2026, 9, 11)), None);
    }

    #[test]
    fn key_store_is_private_and_validated() {
        let dir = std::env::temp_dir().join(format!("tr-eco-key-{}", std::process::id()));
        let ks = KeyStore::new(&dir);
        assert_eq!(ks.stored(), None);
        assert!(ks.save("not-a-key").is_err());
        ks.save(" abcdefghijklmnopqrstuvwxyz123456 ").unwrap();
        assert_eq!(ks.stored().as_deref(), Some("abcdefghijklmnopqrstuvwxyz123456"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("secrets/fred.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(mask("abcdefghijklmnopqrstuvwxyz123456"), "••••••••3456");
        ks.clear().unwrap();
        ks.clear().unwrap();
        assert_eq!(ks.stored(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
