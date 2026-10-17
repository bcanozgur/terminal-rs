//! US economic calendar (Bloomberg `ECO`) and release history (`ECOF`) from free FRED sources.
//!
//! - Release dates: the FRED API (`fred/releases/dates`, free key in `FRED_API_KEY` or the
//!   settings) or, without a key, FRED's public release-calendar page for each release. FOMC
//!   decisions come from the Fed's published meeting schedule ([`FOMC_DECISIONS`]).
//! - Actual and prior: ALFRED's keyless `alfredgraph.csv` with `vintage_date` = the release
//!   day, so each row shows what was published that day, not today's revised numbers.
//! - History: FRED's keyless `fredgraph.csv`.
//!
//! There is no consensus/forecast column: survey expectations are not available for free.
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
}

const HOUR: i64 = 3600;
/// Requests in flight at once; FRED's edge drops clients that burst.
const CONCURRENCY: usize = 2;

impl Eco {
    pub fn new(cache: Option<EcoCache>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("terminal-rs/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client, cache })
    }

    /// GET `url` as text, served from the cache when younger than `max_age` seconds. A stale
    /// copy is used when the network fails. `label` replaces the URL in errors and cache keys
    /// (the URL may carry the API key).
    async fn text(&self, url: &str, label: &str, now: i64, max_age: Option<i64>) -> Result<String> {
        if let Some(body) = self.cache.as_ref().and_then(|c| c.get(label, now, max_age)) {
            return Ok(body);
        }
        let fetched = async {
            let resp = self.client.get(url).send().await.map_err(|e| anyhow!("{label}: {:?}", e.without_url()))?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| anyhow!("{label}: {:?}", e.without_url()))?;
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
            Err(e) => self.cache.as_ref().and_then(|c| c.get(label, now, None)).ok_or(e),
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

    /// Observations of `series` from `from`, as published on `vintage` (ALFRED), or the
    /// current revised data when `vintage` is `None`.
    pub async fn observations(
        &self,
        series: &str,
        from: NaiveDate,
        vintage: Option<NaiveDate>,
        now: i64,
    ) -> Result<Vec<(NaiveDate, f64)>> {
        let (url, label, max_age) = match vintage {
            // Past vintages never change; today's may still gain the release.
            Some(v) => (
                format!("{ALFRED_CSV}?id={series}&cosd={from}&vintage_date={v}"),
                format!("alfred {series} {from} @{v}"),
                Some(10 * 60),
            ),
            None => (format!("{FRED_CSV}?id={series}&cosd={from}"), format!("fred {series} {from}"), Some(HOUR)),
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
        let jobs: Vec<_> = rows.iter_mut().map(|r| self.fill(r, now)).collect();
        futures::stream::iter(jobs).buffer_unordered(CONCURRENCY).collect::<Vec<()>>().await;
        let latest = if source == DateSource::Unavailable { self.latest(now).await } else { Vec::new() };
        EcoWeek { monday, rows, source, latest, notes }
    }

    /// Fill a row's values once its release time has passed.
    async fn fill(&self, row: &mut EcoRow, now: i64) {
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
            self.observations(ev.series, from, None, now).await.map(|obs| fomc_values(&obs, row.date))
        } else {
            let day = self.observations(ev.series, from, Some(row.date), now).await;
            // On the day itself, compare with the day before to tell a posted release from one
            // FRED has not picked up yet; later, the day's vintage is enough.
            let before = if now - row.ts < 24 * HOUR {
                self.observations(ev.series, from, Some(row.date - chrono::Days::new(1)), now).await
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
    pub async fn latest(&self, now: i64) -> Vec<Latest> {
        let today = chrono::DateTime::from_timestamp(now, 0).map_or(NaiveDate::MIN, |t| t.date_naive());
        let from = today - chrono::Days::new(800);
        let jobs: Vec<_> = EVENTS.iter().map(|e| self.observations(e.series, from, None, now)).collect();
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
    pub async fn series(&self, ev: &'static EcoEvent, now: i64) -> Result<EcoSeries> {
        let today = chrono::DateTime::from_timestamp(now, 0).map_or(NaiveDate::MIN, |t| t.date_naive());
        let raw = self.observations(ev.series, today - chrono::Days::new(15 * 366), None, now).await?;
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
