//! Smart money (`SMART`): insider transactions, institutional holdings and short interest.
//!
//! Sources, all free and readable without a login:
//! - **Insiders**: SEC EDGAR Form 4. The issuer's `submissions` feed lists its recent Form 4
//!   filings; each filing's raw ownership XML is parsed ([`parse_form4`]).
//! - **Institutions**: SEC EDGAR 13F-HR of a curated list of well-known managers
//!   ([`MANAGERS`]). The filing's information table is aggregated per CUSIP; the latest two
//!   quarters give the change (new / added / reduced / sold). 13F has no tickers, so issuer
//!   names are matched against SEC's `company_tickers.json` ([`TitleIndex`]).
//! - **Short interest**: FINRA's bi-monthly short interest files at
//!   `cdn.finra.org/equity/otcmarket/biweekly/shrt<yyyymmdd>.csv` (all US exchange-listed and
//!   OTC equities, one row per symbol with the previous period beside it) and, as a separate
//!   and honestly labelled series, FINRA's daily Reg SHO short *sale volume* files
//!   (`cdn.finra.org/equity/regsho/daily/CNMSshvol<yyyymmdd>.txt`).
//!
//! SEC fair access: requests go through the shared EDGAR throttle (under 10 per second) with
//! the user's User-Agent. Everything parsed is cached on disk under `<data dir>/smartmoney/`;
//! filings are immutable, so a cached filing is never fetched again.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::edgar;

/// The curated managers: display name, CIK and a short description.
pub const MANAGERS: &[(&str, u64, &str)] = &[
    ("Berkshire Hathaway", 1067983, "Warren Buffett"),
    ("Bridgewater Associates", 1350694, "Ray Dalio's macro fund"),
    ("Renaissance Technologies", 1037389, "Quant (Medallion)"),
    ("Citadel Advisors", 1423053, "Ken Griffin, multi-strategy"),
    ("Soros Fund Management", 1029160, "George Soros, macro"),
    ("Tiger Global", 1167483, "Chase Coleman, growth/tech"),
    ("Pershing Square", 1336528, "Bill Ackman, concentrated activist"),
    ("Appaloosa", 1656456, "David Tepper"),
    ("Third Point", 1040273, "Dan Loeb, event-driven"),
    ("Duquesne Family Office", 1536411, "Stanley Druckenmiller"),
    ("Baupost Group", 1061768, "Seth Klarman, value"),
    ("Elliott Management", 1791786, "Paul Singer, activist"),
    ("Lone Pine Capital", 1061165, "Stephen Mandel, long/short"),
    ("Viking Global", 1103804, "Andreas Halvorsen, long/short"),
    ("Coatue Management", 1135730, "Philippe Laffont, tech"),
    ("Point72", 1603466, "Steve Cohen, multi-strategy"),
    ("D. E. Shaw", 1009207, "Quant / multi-strategy"),
    ("Icahn Enterprises", 921669, "Carl Icahn, activist"),
];

const SUBMISSIONS_TTL: Duration = Duration::from_secs(6 * 3600);
/// A cached manager is re-checked against the SEC after this long.
pub const MANAGER_TTL: i64 = 24 * 3600;
/// Form 4 filings looked back over.
pub const INSIDER_DAYS: i64 = 120;
/// At most this many Form 4 filings are read per ticker (newest first).
const MAX_FORM4: usize = 80;
/// 13F values were reported in thousands of dollars until the schedule changed on this date.
const DOLLARS_FROM: (i32, u32, u32) = (2023, 1, 3);

fn dollars_from() -> NaiveDate {
    NaiveDate::from_ymd_opt(DOLLARS_FROM.0, DOLLARS_FROM.1, DOLLARS_FROM.2).unwrap_or_default()
}

/// The cache directory under the app's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("smartmoney")
}

// ---- small XML / text helpers -------------------------------------------------------------

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Drop XML namespace prefixes from element names (`<ns1:infoTable>` becomes `<infoTable>`);
/// some filing agents prefix every element.
fn strip_ns(xml: &str) -> std::borrow::Cow<'_, str> {
    if !xml.contains("<ns") && !xml.contains("<n1:") && !xml.contains("<ns1:") {
        // Cheap exit for the common unprefixed layout; prefixes other than these are rare.
        let probe = xml.find("infoTable>").and_then(|i| xml[..i].rfind('<').map(|j| &xml[j..i]));
        if probe.is_none_or(|p| !p.contains(':')) {
            return xml.into();
        }
    }
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..=i]);
        rest = &rest[i + 1..];
        if let Some(r) = rest.strip_prefix('/') {
            out.push('/');
            rest = r;
        }
        let name_end =
            rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))).unwrap_or(rest.len());
        let name = &rest[..name_end];
        out.push_str(name.rsplit_once(':').map_or(name, |(_, n)| n));
        rest = &rest[name_end..];
    }
    out.push_str(rest);
    out.into()
}

/// Text of the first `<tag>…</tag>` in `s`.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{name}>"))? + start;
    Some(&s[start..end])
}

/// `<name><value>…</value></name>`, the Form 4 idiom.
fn val<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    tag(tag(s, name)?, "value").map(str::trim).filter(|v| !v.is_empty())
}

fn num(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().replace(',', "").parse::<f64>().ok()).filter(|v| v.is_finite())
}

fn date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim().get(..10)?, "%Y-%m-%d").ok()
}

/// A throttled SEC GET with an explicit `Accept` (directory `index.json` only answers a plain
/// `application/json`).
async fn sec_get(url: &str, ua: &str, accept: &str) -> Result<String> {
    edgar::throttle().await;
    let resp = edgar::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, accept)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("not found on EDGAR ({url})");
    }
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("SEC refused the request ({status}); check the SEC User-Agent (name and email) in Settings › Panels");
    }
    if !status.is_success() {
        bail!("EDGAR returned {status} for {url}");
    }
    Ok(resp.text().await?)
}

fn require_ua(ua: &str) -> Result<()> {
    if edgar::valid_user_agent(ua) {
        Ok(())
    } else {
        bail!("set an SEC User-Agent with a contact email (Settings › Panels, or {})", edgar::UA_ENV)
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string(v) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---- Form 4 -------------------------------------------------------------------------------

/// What a Form 4 transaction code means for the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    /// Open-market or private purchase (`P`).
    Buy,
    /// Open-market or private sale (`S`).
    Sell,
    /// Grant or award (`A`).
    Award,
    /// Option exercise or conversion (`M`, `C`, `X`).
    Exercise,
    /// Shares withheld for taxes (`F`).
    Tax,
    /// Gift (`G`).
    Gift,
    Other,
}

impl TxKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Buy => "Buy",
            Self::Sell => "Sell",
            Self::Award => "Award",
            Self::Exercise => "Exercise",
            Self::Tax => "Tax",
            Self::Gift => "Gift",
            Self::Other => "Other",
        }
    }
}

/// One non-derivative line of a Form 4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsiderTx {
    pub insider: String,
    pub title: String,
    pub date: NaiveDate,
    pub filed: NaiveDate,
    /// SEC transaction code (`P`, `S`, `A`, `M`, `F`, `G`…).
    pub code: String,
    /// Acquired (`A`) rather than disposed (`D`).
    pub acquired: bool,
    pub shares: f64,
    pub price: Option<f64>,
    pub owned_after: Option<f64>,
    /// Held directly rather than through a trust or other entity.
    pub direct: bool,
    /// Made under a Rule 10b5-1 trading plan.
    pub plan: bool,
    pub accession: String,
}

impl InsiderTx {
    pub fn kind(&self) -> TxKind {
        match self.code.as_str() {
            "P" => TxKind::Buy,
            "S" => TxKind::Sell,
            "A" => TxKind::Award,
            "M" | "C" | "X" | "O" => TxKind::Exercise,
            "F" => TxKind::Tax,
            "G" => TxKind::Gift,
            _ => TxKind::Other,
        }
    }

    /// Dollar value (zero-priced awards and gifts have none).
    pub fn value(&self) -> Option<f64> {
        self.price.filter(|p| *p > 0.0).map(|p| p * self.shares)
    }
}

/// Parse the ownership XML of a Form 4. `filed` and `accession` come from the submissions feed.
pub fn parse_form4(xml: &str, accession: &str, filed: NaiveDate) -> Vec<InsiderTx> {
    let xml = &*strip_ns(xml);
    let mut owners: Vec<(String, String)> = Vec::new();
    for chunk in xml.split("<reportingOwner>").skip(1) {
        let body = chunk.split("</reportingOwner>").next().unwrap_or(chunk);
        let name = unescape(tag(body, "rptOwnerName").unwrap_or_default());
        let flag = |n: &str| tag(body, n).is_some_and(|v| matches!(v.trim(), "1" | "true"));
        let officer = unescape(tag(body, "officerTitle").unwrap_or_default());
        let title = if !officer.is_empty() {
            officer
        } else if flag("isDirector") {
            "Director".into()
        } else if flag("isTenPercentOwner") {
            "10% owner".into()
        } else {
            unescape(tag(body, "otherText").unwrap_or("Other"))
        };
        if !name.is_empty() {
            owners.push((name, title));
        }
    }
    let Some((first, title)) = owners.first().cloned() else { return Vec::new() };
    let insider = if owners.len() > 1 { format!("{first} +{}", owners.len() - 1) } else { first };
    let plan = tag(xml, "aff10b5One").is_some_and(|v| matches!(v.trim(), "1" | "true"));
    let mut out = Vec::new();
    for chunk in xml.split("<nonDerivativeTransaction>").skip(1) {
        let body = chunk.split("</nonDerivativeTransaction>").next().unwrap_or(chunk);
        let Some(day) = val(body, "transactionDate").and_then(date) else { continue };
        let coding = tag(body, "transactionCoding").unwrap_or_default();
        let code = tag(coding, "transactionCode").unwrap_or_default().trim().to_ascii_uppercase();
        let Some(shares) = num(val(body, "transactionShares")) else { continue };
        out.push(InsiderTx {
            insider: insider.clone(),
            title: title.clone(),
            date: day,
            filed,
            code,
            acquired: val(body, "transactionAcquiredDisposedCode").is_some_and(|v| v.eq_ignore_ascii_case("A")),
            shares,
            price: num(val(body, "transactionPricePerShare")),
            owned_after: num(val(body, "sharesOwnedFollowingTransaction")),
            direct: val(body, "directOrIndirectOwnership").is_none_or(|v| v.eq_ignore_ascii_case("D")),
            plan,
            accession: accession.to_string(),
        });
    }
    out
}

/// Totals of one side (buys or sells) of open-market activity.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Side {
    pub count: usize,
    pub insiders: usize,
    pub shares: f64,
    pub value: f64,
}

/// Net open-market insider activity over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub days: i64,
    pub buys: Side,
    pub sells: Side,
}

impl Summary {
    pub fn net_value(&self) -> f64 {
        self.buys.value - self.sells.value
    }

    pub fn net_shares(&self) -> f64 {
        self.buys.shares - self.sells.shares
    }
}

/// Open-market purchases (`P`) against sales (`S`) in the `days` before `today`.
pub fn summarize(txs: &[InsiderTx], today: NaiveDate, days: i64) -> Summary {
    let from = today - chrono::Duration::days(days);
    let mut s = Summary { days, ..Summary::default() };
    let (mut buyers, mut sellers): (HashSet<&str>, HashSet<&str>) = (HashSet::new(), HashSet::new());
    for t in txs.iter().filter(|t| t.date >= from && t.date <= today) {
        let (side, who) = match t.kind() {
            TxKind::Buy => (&mut s.buys, &mut buyers),
            TxKind::Sell => (&mut s.sells, &mut sellers),
            _ => continue,
        };
        side.count += 1;
        side.shares += t.shares;
        side.value += t.value().unwrap_or(0.0);
        who.insert(&t.insider);
    }
    s.buys.insiders = buyers.len();
    s.sells.insiders = sellers.len();
    s
}

/// An issuer's recent insider transactions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Insiders {
    pub ticker: String,
    pub issuer: String,
    /// Transactions, newest first.
    pub txs: Vec<InsiderTx>,
    /// Form 4 filings in the look-back window / filings actually read.
    pub filings: usize,
    pub read: usize,
    pub updated: i64,
}

/// The raw XML URL of a Form 4 from its (stylesheet-rendered) primary document URL.
pub fn form4_xml_url(primary: &str) -> String {
    let Some((dir, file)) = primary.rsplit_once('/') else { return primary.to_string() };
    match dir.rsplit_once('/') {
        Some((parent, last)) if last.starts_with("xsl") => format!("{parent}/{file}"),
        _ => primary.to_string(),
    }
}

/// Load an issuer's Form 4 transactions (throttled, cached per filing).
pub async fn load_insiders(data_dir: &Path, ticker: &str, ua: &str) -> Result<Insiders> {
    require_ua(ua)?;
    let edgar_dir = edgar::cache_dir(data_dir);
    let json = edgar::submissions_json(&edgar_dir, ticker, ua).await?;
    let profile = edgar::parse_submissions(&json)?;
    let today = chrono::Local::now().date_naive();
    let from = today - chrono::Duration::days(INSIDER_DAYS);
    let all: Vec<edgar::Filing> = profile.filings_of(&["4"]).filter(|f| f.filed >= from).cloned().collect();
    let dir = cache_dir(data_dir).join("form4");
    let mut txs = Vec::new();
    let mut read = 0;
    for f in all.iter().take(MAX_FORM4) {
        let path = dir.join(format!("{}.json", f.accession));
        let parsed: Option<Vec<InsiderTx>> = read_json(&path);
        let parsed = match parsed {
            Some(p) => p,
            None => match sec_get(&form4_xml_url(&f.url), ua, "application/xml, text/xml, */*").await {
                Ok(xml) => {
                    let p = parse_form4(&xml, &f.accession, f.filed);
                    write_json(&path, &p);
                    p
                }
                Err(e) => {
                    tracing::warn!("Form 4 {}: {e:#}", f.accession);
                    continue;
                }
            },
        };
        read += 1;
        txs.extend(parsed);
    }
    txs.sort_by(|a, b| b.date.cmp(&a.date).then(b.filed.cmp(&a.filed)));
    Ok(Insiders {
        ticker: ticker.to_ascii_uppercase(),
        issuer: profile.name,
        txs,
        filings: all.len(),
        read,
        updated: now(),
    })
}

// ---- 13F ----------------------------------------------------------------------------------

/// One issuer's position in a 13F, summed over share lines (other managers, discretion types).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub cusip: String,
    pub name: String,
    /// Title of class (`COM`, `CL A`…).
    pub class: String,
    pub shares: f64,
    /// Market value in dollars at the period end.
    pub value: f64,
}

/// One manager's 13F-HR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filing13F {
    pub accession: String,
    pub filed: NaiveDate,
    pub period: NaiveDate,
    pub total: f64,
    /// Equity positions, largest first.
    pub positions: Vec<Position>,
}

/// A manager with its latest two 13F-HR filings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manager {
    pub cik: u64,
    pub name: String,
    pub latest: Option<Filing13F>,
    pub prev: Option<Filing13F>,
    pub updated: i64,
}

impl Manager {
    pub fn stale(&self, now: i64) -> bool {
        now - self.updated > MANAGER_TTL
    }
}

/// Parse a 13F information table. `values_in_thousands` is true for filings before 2023.
pub fn parse_info_table(xml: &str, values_in_thousands: bool) -> Vec<Position> {
    let mult = if values_in_thousands { 1000.0 } else { 1.0 };
    let xml = &*strip_ns(xml);
    let mut by_cusip: HashMap<String, Position> = HashMap::new();
    for chunk in xml.split("<infoTable>").skip(1) {
        let body = chunk.split("</infoTable>").next().unwrap_or(chunk);
        // Options are not share positions.
        if tag(body, "putCall").is_some_and(|p| !p.trim().is_empty()) {
            continue;
        }
        let amt = tag(body, "shrsOrPrnAmt").unwrap_or_default();
        if !tag(amt, "sshPrnamtType").is_some_and(|t| t.trim().eq_ignore_ascii_case("SH")) {
            continue;
        }
        let (Some(shares), Some(value)) = (num(tag(amt, "sshPrnamt")), num(tag(body, "value"))) else { continue };
        let cusip = tag(body, "cusip").unwrap_or_default().trim().to_ascii_uppercase();
        if cusip.is_empty() {
            continue;
        }
        let e = by_cusip.entry(cusip.clone()).or_insert_with(|| Position {
            cusip,
            name: unescape(tag(body, "nameOfIssuer").unwrap_or_default()),
            class: unescape(tag(body, "titleOfClass").unwrap_or_default()),
            shares: 0.0,
            value: 0.0,
        });
        e.shares += shares;
        e.value += value * mult;
    }
    let mut out: Vec<Position> = by_cusip.into_values().collect();
    // Some filers still report thousands after the schedule change: a median implied price under
    // $1 a share (values are summed in dollars here, so 1000x too small) gives it away.
    let mut px: Vec<f64> = out.iter().filter(|p| p.shares > 0.0).map(|p| p.value / p.shares / mult).collect();
    px.sort_by(f64::total_cmp);
    if !values_in_thousands && px.get(px.len() / 2).is_some_and(|m| *m < 1.0) {
        for p in &mut out {
            p.value *= 1000.0;
        }
    }
    out.sort_by(|a, b| b.value.total_cmp(&a.value).then_with(|| a.cusip.cmp(&b.cusip)));
    out
}

/// The information table's file name from a filing directory's `index.json`: the largest XML
/// that is not the cover page.
pub fn infotable_name(index_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Item {
        name: String,
        #[serde(default)]
        size: String,
    }
    #[derive(Deserialize)]
    struct Dir {
        item: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Doc {
        directory: Dir,
    }
    let doc: Doc = serde_json::from_str(index_json).ok()?;
    doc.directory
        .item
        .into_iter()
        .filter(|i| i.name.to_ascii_lowercase().ends_with(".xml") && !i.name.eq_ignore_ascii_case("primary_doc.xml"))
        .max_by_key(|i| i.size.trim().parse::<u64>().unwrap_or(0))
        .map(|i| i.name)
}

async fn load_filing(cik: u64, f: &edgar::Filing, ua: &str) -> Result<Filing13F> {
    let period = f.report.ok_or_else(|| anyhow!("13F {} has no period of report", f.accession))?;
    let base = format!("https://www.sec.gov/Archives/edgar/data/{cik}/{}", f.accession.replace('-', ""));
    let index = sec_get(&format!("{base}/index.json"), ua, "application/json").await?;
    let name = infotable_name(&index).ok_or_else(|| anyhow!("13F {} has no information table", f.accession))?;
    let xml = sec_get(&format!("{base}/{name}"), ua, "application/xml, text/xml, */*").await?;
    let positions = parse_info_table(&xml, f.filed < dollars_from());
    if positions.is_empty() {
        bail!("13F {} lists no share positions", f.accession);
    }
    let total = positions.iter().map(|p| p.value).sum();
    Ok(Filing13F { accession: f.accession.clone(), filed: f.filed, period, total, positions })
}

fn manager_path(dir: &Path, cik: u64) -> PathBuf {
    cache_dir(dir).join("13f_v2").join(format!("{cik}.json"))
}

/// A manager from the disk cache.
pub fn read_manager(data_dir: &Path, cik: u64) -> Option<Manager> {
    read_json(&manager_path(data_dir, cik))
}

/// Every curated manager that is cached, in curated order.
pub fn all_cached(data_dir: &Path) -> Vec<Manager> {
    MANAGERS.iter().filter_map(|(_, cik, _)| read_manager(data_dir, *cik)).collect()
}

/// Load a manager's latest two 13F-HR filings; the cache is used unless `force` or stale, and
/// a cached filing is never fetched again.
pub async fn load_manager(data_dir: &Path, cik: u64, ua: &str, force: bool) -> Result<Manager> {
    let cached = read_manager(data_dir, cik);
    if let Some(m) = cached.as_ref().filter(|m| !force && !m.stale(now())) {
        return Ok(m.clone());
    }
    require_ua(ua)?;
    let url = format!("https://data.sec.gov/submissions/CIK{cik:010}.json");
    let sub_dir = cache_dir(data_dir).join("submissions");
    let json =
        edgar::cached(&sub_dir, &format!("{cik}.json"), if force { Duration::ZERO } else { SUBMISSIONS_TTL }, &url, ua)
            .await?;
    let profile = edgar::parse_submissions(&json)?;
    let filings: Vec<&edgar::Filing> = profile.filings_of(&["13F-HR"]).take(2).collect();
    if filings.is_empty() {
        bail!("{} has no 13F-HR filings on EDGAR", profile.name);
    }
    let reuse = |acc: &str| {
        cached.as_ref().and_then(|m| [&m.latest, &m.prev].into_iter().flatten().find(|f| f.accession == acc).cloned())
    };
    let mut got: Vec<Filing13F> = Vec::new();
    for f in filings {
        match reuse(&f.accession) {
            Some(x) => got.push(x),
            None => got.push(load_filing(cik, f, ua).await?),
        }
    }
    let mut it = got.into_iter();
    let m = Manager { cik, name: profile.name, latest: it.next(), prev: it.next(), updated: now() };
    write_json(&manager_path(data_dir, cik), &m);
    Ok(m)
}

/// How a position changed against the previous quarter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    New,
    Added,
    Reduced,
    Sold,
    Unchanged,
}

impl Move {
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "New",
            Self::Added => "Added",
            Self::Reduced => "Reduced",
            Self::Sold => "Sold out",
            Self::Unchanged => "Unchanged",
        }
    }
}

/// A position with its change against the previous filing.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub position: Position,
    pub prev_shares: f64,
    pub mv: Move,
    /// Share change in percent (none for new and sold-out positions).
    pub pct: Option<f64>,
}

fn classify(shares: f64, prev: Option<f64>) -> (Move, Option<f64>) {
    match prev {
        None => (Move::New, None),
        Some(p) if p <= 0.0 => (Move::New, None),
        Some(p) => {
            let pct = (shares - p) / p * 100.0;
            // Rounding in the share count is not a trade.
            if pct.abs() < 0.5 {
                (Move::Unchanged, Some(pct))
            } else if pct > 0.0 {
                (Move::Added, Some(pct))
            } else {
                (Move::Reduced, Some(pct))
            }
        }
    }
}

impl Manager {
    /// The `n` largest positions of the latest filing with their change. Without a previous
    /// filing everything is reported as unchanged-unknown (`Move::Unchanged`, no percentage).
    pub fn top(&self, n: usize) -> Vec<Change> {
        let Some(latest) = &self.latest else { return Vec::new() };
        let prev: Option<HashMap<&str, f64>> =
            self.prev.as_ref().map(|p| p.positions.iter().map(|x| (x.cusip.as_str(), x.shares)).collect());
        latest
            .positions
            .iter()
            .take(n)
            .map(|p| match &prev {
                Some(map) => {
                    let before = map.get(p.cusip.as_str()).copied();
                    let (mv, pct) = classify(p.shares, before);
                    Change { position: p.clone(), prev_shares: before.unwrap_or(0.0), mv, pct }
                }
                None => Change { position: p.clone(), prev_shares: 0.0, mv: Move::Unchanged, pct: None },
            })
            .collect()
    }

    /// The `n` largest positions of the previous filing that the latest one no longer holds.
    pub fn sold_out(&self, n: usize) -> Vec<Change> {
        let (Some(latest), Some(prev)) = (&self.latest, &self.prev) else { return Vec::new() };
        let now: HashSet<&str> = latest.positions.iter().map(|p| p.cusip.as_str()).collect();
        prev.positions
            .iter()
            .filter(|p| !now.contains(p.cusip.as_str()))
            .take(n)
            .map(|p| Change { position: p.clone(), prev_shares: p.shares, mv: Move::Sold, pct: Some(-100.0) })
            .collect()
    }
}

// ---- 13F issuer name → ticker -------------------------------------------------------------

const ABBREVIATIONS: &[(&str, &str)] = &[
    ("finl", "financial"),
    ("intl", "international"),
    ("natl", "national"),
    ("amer", "american"),
    ("mfg", "manufacturing"),
    ("tech", "technologies"),
    ("techs", "technologies"),
    ("technology", "technologies"),
    ("svcs", "services"),
    ("svc", "services"),
    ("sys", "systems"),
    ("ind", "industries"),
    ("inds", "industries"),
    ("pptys", "properties"),
    ("cap", "capital"),
    ("res", "resources"),
    ("pharms", "pharmaceuticals"),
    ("pharmaceutical", "pharmaceuticals"),
    ("labs", "laboratories"),
    ("comms", "communications"),
    ("commun", "communications"),
    ("entmt", "entertainment"),
    ("hlth", "health"),
    ("healthcare", "health"),
    ("bk", "bank"),
    ("pete", "petroleum"),
    ("petrol", "petroleum"),
    ("elec", "electric"),
    ("mtg", "mortgage"),
    ("matls", "materials"),
    ("mgmt", "management"),
    ("entrmt", "entertainment"),
    ("bancorporation", "bancorp"),
];

/// Words that carry no identity in an issuer name.
const NOISE: &[&str] = &[
    "of", "and", "the", "hldgs", "hldg", "grp", "com", "new", "del", "cl", "class", "ord", "shs", "sh", "adr", "ads",
    "stk", "us", "usa", "common", "stock",
];

/// Issuer name as comparable tokens.
fn tokens(name: &str) -> Vec<String> {
    crate::etf::norm_name(name, false)
        .split_whitespace()
        .filter(|w| !NOISE.contains(w))
        .map(|w| ABBREVIATIONS.iter().find(|(a, _)| *a == w).map_or(w, |(_, full)| full).to_string())
        .collect()
}

fn tok_eq(a: &str, b: &str) -> bool {
    a == b || (a.len().min(b.len()) >= 3 && (a.starts_with(b) || b.starts_with(a)))
}

/// Same words, allowing abbreviated later words (`MFG` is in the table, `HLDG`-style truncations
/// are caught by prefixes); the first word must match exactly so `ALPHA` is not `ALPHABET`.
fn names_match(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.concat() == b.concat() {
        return true;
    }
    a.len() == b.len() && a[0] == b[0] && a.iter().zip(b).skip(1).all(|(x, y)| tok_eq(x, y))
}

/// Candidate rows share the first word, but `SIRIUSXM` against `SIRIUS XM` also joins words.
fn bucket(first: &str) -> String {
    first.chars().take(4).collect()
}

/// Not a preferred share, warrant, right or unit (`WFC-PY`, `ACAC-WT`).
fn is_common_ticker(t: &str) -> bool {
    let Some((_, suffix)) = t.rsplit_once('-') else { return true };
    !(suffix.starts_with('P') && suffix.len() <= 2 || matches!(suffix, "WT" | "WS" | "W" | "R" | "RT" | "U" | "UN"))
}

/// The share class named in a title of class or issuer name (`CL A`, `CLASS B`, `CAP STK CL C`).
fn class_letter(s: &str) -> Option<char> {
    let lower = s.to_lowercase();
    let mut words = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    while let Some(w) = words.next() {
        if matches!(w, "class" | "cl") {
            return words.next().and_then(|c| (c.len() == 1).then(|| c.chars().next())).flatten();
        }
    }
    None
}

struct TitleRow {
    tokens: Vec<String>,
    cik: u64,
    ticker: String,
}

/// SEC registrant names → tickers, for matching the issuer names of 13F filings.
pub struct TitleIndex {
    rows: Vec<TitleRow>,
    by_bucket: HashMap<String, Vec<usize>>,
}

impl TitleIndex {
    pub fn parse(json: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Row {
            cik_str: u64,
            ticker: String,
            title: String,
        }
        let raw: HashMap<String, Row> = serde_json::from_str(json).context("company_tickers.json")?;
        let mut raw: Vec<(usize, Row)> = raw.into_iter().map(|(k, r)| (k.parse().unwrap_or(usize::MAX), r)).collect();
        raw.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.ticker.cmp(&b.1.ticker)));
        let mut rows = Vec::new();
        let mut by_bucket: HashMap<String, Vec<usize>> = HashMap::new();
        for (_, r) in raw {
            let ticker = r.ticker.to_ascii_uppercase();
            let toks = tokens(&r.title);
            if toks.is_empty() || !is_common_ticker(&ticker) {
                continue;
            }
            by_bucket.entry(bucket(&toks[0])).or_default().push(rows.len());
            rows.push(TitleRow { tokens: toks, cik: r.cik_str, ticker });
        }
        Ok(Self { rows, by_bucket })
    }

    /// The ticker of the issuer a 13F line names, or none when the name is unknown or shared
    /// by several companies. Dual-class issuers are resolved by the class in the line, else
    /// they get the primary ticker.
    pub fn ticker(&self, issuer: &str, class: &str) -> Option<&str> {
        let toks = tokens(issuer);
        let first = toks.first()?;
        let mut hits: Vec<&TitleRow> = Vec::new();
        for key in [bucket(first)] {
            for &i in self.by_bucket.get(&key)? {
                if names_match(&toks, &self.rows[i].tokens) {
                    hits.push(&self.rows[i]);
                }
            }
        }
        let cik = hits.first()?.cik;
        if hits.iter().any(|h| h.cik != cik) {
            return None;
        }
        if let [one] = hits.as_slice() {
            return Some(&one.ticker);
        }
        if let Some(c) = class_letter(class).or_else(|| class_letter(issuer)) {
            let mut by_class = hits.iter().filter(|h| {
                let t = h.ticker.to_ascii_lowercase();
                t.ends_with(&format!("-{}", c.to_ascii_lowercase()))
            });
            if let (Some(h), None) = (by_class.next(), by_class.next()) {
                return Some(&h.ticker);
            }
            // `GOOGL` / `GOOG` carry no class suffix: class A is the first listed (SEC orders by
            // market cap), the other class the second.
            return match hits.as_slice() {
                [a, ..] if c.eq_ignore_ascii_case(&'a') => Some(&a.ticker),
                [_, b, ..] => Some(&b.ticker),
                _ => None,
            };
        }
        hits.first().map(|h| h.ticker.as_str())
    }

    /// Tickers for a manager's top positions, keyed by CUSIP.
    pub fn resolve(&self, positions: &[Position]) -> HashMap<String, String> {
        positions.iter().filter_map(|p| Some((p.cusip.clone(), self.ticker(&p.name, &p.class)?.to_string()))).collect()
    }
}

/// The ticker index from SEC's `company_tickers.json` (cached 24 h by the EDGAR module).
pub async fn title_index(data_dir: &Path, ua: &str) -> Result<TitleIndex> {
    require_ua(ua)?;
    let json =
        edgar::cached(&edgar::cache_dir(data_dir), "company_tickers.json", edgar::TICKERS_TTL, edgar::TICKERS_URL, ua)
            .await?;
    TitleIndex::parse(&json)
}

/// A curated manager that holds a stock.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    pub manager: String,
    pub cik: u64,
    pub period: NaiveDate,
    pub shares: f64,
    pub value: f64,
    /// Percent of the manager's reported equity portfolio.
    pub weight: f64,
    /// Rank among the manager's positions (1 = largest).
    pub rank: usize,
    pub mv: Move,
    pub pct: Option<f64>,
}

/// Which of `managers` hold `ticker`, largest position first. A manager whose previous
/// filing is missing reports every change as unknown (`Move::Unchanged`, no percentage).
pub fn holders_of(managers: &[Manager], ticker: &str, index: &TitleIndex) -> Vec<Holder> {
    let ticker = ticker.to_ascii_uppercase();
    let mut out = Vec::new();
    for m in managers {
        let Some(latest) = &m.latest else { continue };
        let prev: Option<HashMap<&str, f64>> =
            m.prev.as_ref().map(|p| p.positions.iter().map(|x| (x.cusip.as_str(), x.shares)).collect());
        let mut shares = 0.0;
        let mut value = 0.0;
        let mut before: Option<f64> = None;
        let mut rank = 0;
        let mut found = false;
        for (i, p) in latest.positions.iter().enumerate() {
            if index.ticker(&p.name, &p.class) == Some(ticker.as_str()) {
                if !found {
                    rank = i + 1;
                }
                found = true;
                shares += p.shares;
                value += p.value;
                if let Some(b) = prev.as_ref().and_then(|map| map.get(p.cusip.as_str())) {
                    *before.get_or_insert(0.0) += b;
                }
            }
        }
        if !found {
            continue;
        }
        let (mv, pct) = if prev.is_some() { classify(shares, before) } else { (Move::Unchanged, None) };
        out.push(Holder {
            manager: m.name.clone(),
            cik: m.cik,
            period: latest.period,
            shares,
            value,
            weight: if latest.total > 0.0 { value / latest.total * 100.0 } else { 0.0 },
            rank,
            mv,
            pct,
        });
    }
    out.sort_by(|a, b| b.value.total_cmp(&a.value));
    out
}

// ---- FINRA short interest -----------------------------------------------------------------

const FINRA: &str = "https://cdn.finra.org/equity";
/// Pause between FINRA downloads.
const FINRA_GAP: Duration = Duration::from_millis(200);
/// Short interest periods shown.
const SI_PERIODS: usize = 6;
/// Trading days of short sale volume shown.
const SV_DAYS: usize = 10;

/// One bi-monthly short interest row of a symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShortInterest {
    pub settle: NaiveDate,
    pub short: f64,
    /// Short interest at the previous settlement date.
    pub prev: f64,
    pub avg_volume: f64,
    pub days_to_cover: f64,
}

impl ShortInterest {
    pub fn change(&self) -> f64 {
        self.short - self.prev
    }

    pub fn change_pct(&self) -> Option<f64> {
        (self.prev > 0.0).then(|| self.change() / self.prev * 100.0)
    }
}

/// One day of FINRA-reported short sale volume of a symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShortVolume {
    pub date: NaiveDate,
    pub short: f64,
    pub exempt: f64,
    pub total: f64,
}

impl ShortVolume {
    /// Short sale volume as a percent of total reported volume.
    pub fn ratio(&self) -> Option<f64> {
        (self.total > 0.0).then(|| self.short / self.total * 100.0)
    }
}

/// A symbol's FINRA data. Each series has its own error: either source can be down.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ShortData {
    pub ticker: String,
    /// Newest first.
    pub interest: Vec<ShortInterest>,
    pub interest_error: Option<String>,
    /// Newest first.
    pub volume: Vec<ShortVolume>,
    pub volume_error: Option<String>,
    pub updated: i64,
}

/// Spellings of a ticker across FINRA files (`BRK-B` is `BRKB` in short interest and `BRK/B`
/// in short sale volume).
fn symbol_variants(ticker: &str) -> Vec<String> {
    let t = ticker.to_ascii_uppercase();
    let mut v = vec![t.clone()];
    for sep in ["", "/", ".", " "] {
        let alt = t.replace(['-', '.', '/'], sep);
        if !v.contains(&alt) {
            v.push(alt);
        }
    }
    v
}

fn split_fields(line: &str) -> Vec<&str> {
    line.split('|').map(|f| f.trim().trim_matches('"')).collect()
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty())
}

/// Parse a short interest file into compact tab-separated rows
/// (`symbol cur prev avg_volume days_to_cover`) for the cache.
pub fn compact_short_interest(csv: &str) -> String {
    let mut it = lines(csv);
    let Some(header) = it.next() else { return String::new() };
    let cols = split_fields(header);
    let at = |name: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(sym), Some(cur), Some(prev), Some(adv), Some(dtc)) = (
        at("symbolCode"),
        at("currentShortPositionQuantity"),
        at("previousShortPositionQuantity"),
        at("averageDailyVolumeQuantity"),
        at("daysToCoverQuantity"),
    ) else {
        return String::new();
    };
    let mut out = String::new();
    for line in it {
        let f = split_fields(line);
        let get = |i: usize| f.get(i).copied().unwrap_or_default();
        if get(sym).is_empty() || get(cur).parse::<f64>().is_err() {
            continue;
        }
        out.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", get(sym), get(cur), get(prev), get(adv), get(dtc)));
    }
    out
}

/// A symbol's row in a compact short interest file.
pub fn find_short_interest(compact: &str, settle: NaiveDate, symbols: &[String]) -> Option<ShortInterest> {
    compact.lines().find_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 5 || !symbols.iter().any(|s| s == f[0]) {
            return None;
        }
        Some(ShortInterest {
            settle,
            short: f[1].parse().ok()?,
            prev: f[2].parse().unwrap_or(0.0),
            avg_volume: f[3].parse().unwrap_or(0.0),
            days_to_cover: f[4].parse().unwrap_or(0.0),
        })
    })
}

/// Compact a daily short sale volume file (`symbol short exempt total`).
pub fn compact_short_volume(txt: &str) -> String {
    let mut out = String::new();
    for line in lines(txt).skip(1) {
        let f = split_fields(line);
        if f.len() < 5 || f[2].parse::<f64>().is_err() {
            continue;
        }
        out.push_str(&format!("{}\t{}\t{}\t{}\n", f[1], f[2], f[3], f[4]));
    }
    out
}

pub fn find_short_volume(compact: &str, date: NaiveDate, symbols: &[String]) -> Option<ShortVolume> {
    compact.lines().find_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 4 || !symbols.iter().any(|s| s == f[0]) {
            return None;
        }
        Some(ShortVolume {
            date,
            short: f[1].parse().ok()?,
            exempt: f[2].parse().unwrap_or(0.0),
            total: f[3].parse().ok()?,
        })
    })
}

/// FINRA settlement dates, newest first: the 15th and the last day of each month, moved back to
/// a weekday. Dates later than `today` less `lag_days` are skipped (FINRA publishes about a
/// week after settlement).
pub fn settlement_dates(today: NaiveDate, months: u32, lag_days: u64) -> Vec<NaiveDate> {
    let weekday = |mut d: NaiveDate| {
        while d.weekday().number_from_monday() > 5 {
            d = d.pred_opt().unwrap_or(d);
        }
        d
    };
    let mut out = Vec::new();
    let mut first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
    for _ in 0..=months {
        let next = first.checked_add_months(chrono::Months::new(1)).unwrap_or(first);
        let end = next.pred_opt().unwrap_or(first);
        let mid = first.with_day(15).unwrap_or(first);
        out.push(weekday(end));
        out.push(weekday(mid));
        first = first.checked_sub_months(chrono::Months::new(1)).unwrap_or(first);
    }
    out.sort_by(|a, b| b.cmp(a));
    out.dedup();
    let cutoff = today.checked_sub_days(Days::new(lag_days)).unwrap_or(today);
    out.retain(|d| *d <= cutoff);
    out
}

/// Weekdays up to and including `today`, newest first.
pub fn recent_weekdays(today: NaiveDate, n: usize) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = today;
    while out.len() < n {
        if d.weekday().number_from_monday() <= 5 {
            out.push(d);
        }
        match d.pred_opt() {
            Some(p) => d = p,
            None => break,
        }
    }
    out
}

/// GET a FINRA file; `None` when it does not exist (FINRA's CDN answers 403/404 for a missing key).
async fn finra_get(url: &str) -> Result<Option<String>> {
    tokio::time::sleep(FINRA_GAP).await;
    let resp = edgar::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, "terminal-rs (research terminal)")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if matches!(status.as_u16(), 403 | 404) {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("FINRA returned {status} for {url}");
    }
    Ok(Some(resp.text().await?))
}

/// A FINRA file through a compact on-disk cache (the files are immutable once published).
async fn finra_cached(path: &Path, url: &str, compact: fn(&str) -> String) -> Result<Option<String>> {
    if let Ok(s) = std::fs::read_to_string(path) {
        return Ok(Some(s));
    }
    let Some(raw) = finra_get(url).await? else { return Ok(None) };
    let c = compact(&raw);
    if c.is_empty() {
        bail!("unexpected file layout at {url}");
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &c).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
    Ok(Some(c))
}

async fn load_interest(dir: &Path, symbols: &[String], today: NaiveDate) -> Result<Vec<ShortInterest>> {
    let mut out = Vec::new();
    let mut misses = 0;
    for nominal in settlement_dates(today, 6, 6) {
        if out.len() >= SI_PERIODS {
            break;
        }
        // A holiday moves the settlement date to the business day before.
        let mut tries = vec![nominal];
        if let Some(prev) = nominal.pred_opt() {
            let mut d = prev;
            while d.weekday().number_from_monday() > 5 {
                d = d.pred_opt().unwrap_or(d);
            }
            tries.push(d);
        }
        let mut found = false;
        for d in tries {
            let tag = d.format("%Y%m%d");
            let url = format!("{FINRA}/otcmarket/biweekly/shrt{tag}.csv");
            if let Some(c) = finra_cached(&dir.join(format!("si_{tag}.tsv")), &url, compact_short_interest).await? {
                found = true;
                if let Some(row) = find_short_interest(&c, d, symbols) {
                    out.push(row);
                }
                break;
            }
        }
        if !found {
            misses += 1;
            if misses >= 4 && out.is_empty() {
                bail!("FINRA short interest files were not reachable");
            }
        }
    }
    Ok(out)
}

async fn load_volume(dir: &Path, symbols: &[String], today: NaiveDate) -> Result<Vec<ShortVolume>> {
    let mut out = Vec::new();
    let mut files = 0;
    for d in recent_weekdays(today, SV_DAYS + 8) {
        if files >= SV_DAYS {
            break;
        }
        let tag = d.format("%Y%m%d");
        let url = format!("{FINRA}/regsho/daily/CNMSshvol{tag}.txt");
        if let Some(c) = finra_cached(&dir.join(format!("sv_{tag}.tsv")), &url, compact_short_volume).await? {
            files += 1;
            if let Some(row) = find_short_volume(&c, d, symbols) {
                out.push(row);
            }
        }
    }
    if files == 0 {
        bail!("FINRA Reg SHO daily files were not reachable");
    }
    Ok(out)
}

/// Delete cached FINRA files older than a few months.
fn prune_finra(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut si: BTreeSet<PathBuf> = BTreeSet::new();
    let mut sv: BTreeSet<PathBuf> = BTreeSet::new();
    for e in rd.filter_map(Result::ok) {
        match e.file_name().to_string_lossy().get(..3) {
            Some("si_") => si.insert(e.path()),
            Some("sv_") => sv.insert(e.path()),
            _ => false,
        };
    }
    for (set, keep) in [(si, 12), (sv, 25)] {
        let n = set.len().saturating_sub(keep);
        for p in set.into_iter().take(n) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Load a ticker's FINRA short interest history and recent short sale volume.
pub async fn load_short(data_dir: &Path, ticker: &str) -> ShortData {
    let dir = cache_dir(data_dir).join("finra");
    let today = chrono::Local::now().date_naive();
    let symbols = symbol_variants(ticker);
    let mut out = ShortData { ticker: ticker.to_ascii_uppercase(), updated: now(), ..ShortData::default() };
    match load_interest(&dir, &symbols, today).await {
        Ok(v) if v.is_empty() => {
            out.interest_error = Some(format!("{} is not in FINRA's short interest files", out.ticker))
        }
        Ok(v) => out.interest = v,
        Err(e) => out.interest_error = Some(format!("{e:#}")),
    }
    match load_volume(&dir, &symbols, today).await {
        Ok(v) if v.is_empty() => out.volume_error = Some(format!("{} is not in FINRA's daily files", out.ticker)),
        Ok(v) => out.volume = v,
        Err(e) => out.volume_error = Some(format!("{e:#}")),
    }
    prune_finra(&dir);
    out
}

// ---- tests --------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        date(s).unwrap()
    }

    const FORM4: &str = r#"<?xml version="1.0"?>
<ownershipDocument>
  <documentType>4</documentType>
  <issuer><issuerCik>0000320193</issuerCik><issuerName>Apple Inc.</issuerName><issuerTradingSymbol>AAPL</issuerTradingSymbol></issuer>
  <reportingOwner>
    <reportingOwnerId><rptOwnerCik>0001780525</rptOwnerCik><rptOwnerName>Newstead Jennifer</rptOwnerName></reportingOwnerId>
    <reportingOwnerRelationship><isDirector>false</isDirector><isOfficer>true</isOfficer><officerTitle>SVP, GC &amp; Government Affairs</officerTitle></reportingOwnerRelationship>
  </reportingOwner>
  <aff10b5One>true</aff10b5One>
  <nonDerivativeTable>
    <nonDerivativeTransaction>
      <securityTitle><value>Common Stock</value></securityTitle>
      <transactionDate><value>2026-10-13</value></transactionDate>
      <transactionCoding><transactionFormType>4</transactionFormType><transactionCode>S</transactionCode></transactionCoding>
      <transactionAmounts>
        <transactionShares><value>2399</value></transactionShares>
        <transactionPricePerShare><value>345.00</value></transactionPricePerShare>
        <transactionAcquiredDisposedCode><value>D</value></transactionAcquiredDisposedCode>
      </transactionAmounts>
      <postTransactionAmounts><sharesOwnedFollowingTransaction><value>37194</value></sharesOwnedFollowingTransaction></postTransactionAmounts>
      <ownershipNature><directOrIndirectOwnership><value>D</value></directOrIndirectOwnership></ownershipNature>
    </nonDerivativeTransaction>
    <nonDerivativeTransaction>
      <transactionDate><value>2026-10-10</value></transactionDate>
      <transactionCoding><transactionCode>P</transactionCode></transactionCoding>
      <transactionAmounts>
        <transactionShares><value>1,000</value></transactionShares>
        <transactionPricePerShare><value>340.5</value></transactionPricePerShare>
        <transactionAcquiredDisposedCode><value>A</value></transactionAcquiredDisposedCode>
      </transactionAmounts>
      <postTransactionAmounts><sharesOwnedFollowingTransaction><value>40000</value></sharesOwnedFollowingTransaction></postTransactionAmounts>
      <ownershipNature><directOrIndirectOwnership><value>I</value></directOrIndirectOwnership></ownershipNature>
    </nonDerivativeTransaction>
    <nonDerivativeTransaction>
      <transactionDate><value>2026-10-09</value></transactionDate>
      <transactionCoding><transactionCode>A</transactionCode></transactionCoding>
      <transactionAmounts>
        <transactionShares><value>500</value></transactionShares>
        <transactionPricePerShare><footnoteId id="F1"/></transactionPricePerShare>
        <transactionAcquiredDisposedCode><value>A</value></transactionAcquiredDisposedCode>
      </transactionAmounts>
    </nonDerivativeTransaction>
  </nonDerivativeTable>
</ownershipDocument>"#;

    #[test]
    fn form4_transactions() {
        let t = parse_form4(FORM4, "0001-26-1", d("2026-10-15"));
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].insider, "Newstead Jennifer");
        assert_eq!(t[0].title, "SVP, GC & Government Affairs");
        assert_eq!((t[0].code.as_str(), t[0].kind(), t[0].acquired), ("S", TxKind::Sell, false));
        assert_eq!((t[0].shares, t[0].price, t[0].owned_after), (2399.0, Some(345.0), Some(37194.0)));
        assert!(t[0].plan && t[0].direct && !t[1].direct);
        assert_eq!(t[0].value(), Some(2399.0 * 345.0));
        assert_eq!((t[1].kind(), t[1].shares, t[1].date), (TxKind::Buy, 1000.0, d("2026-10-10")));
        assert_eq!((t[2].kind(), t[2].price, t[2].value()), (TxKind::Award, None, None));
    }

    #[test]
    fn form4_summary_counts_open_market_only() {
        let t = parse_form4(FORM4, "a", d("2026-10-15"));
        let s = summarize(&t, d("2026-10-15"), 90);
        assert_eq!((s.buys.count, s.buys.shares, s.sells.count, s.sells.shares), (1, 1000.0, 1, 2399.0));
        assert_eq!((s.buys.insiders, s.sells.insiders), (1, 1));
        assert!((s.net_value() - (340_500.0 - 827_655.0)).abs() < 1e-6);
        // The window excludes older transactions.
        assert_eq!(summarize(&t, d("2026-10-15"), 3).buys.count, 0);
    }

    #[test]
    fn form4_url_drops_stylesheet_dir() {
        assert_eq!(
            form4_xml_url("https://www.sec.gov/Archives/edgar/data/320193/000114036126039785/xslF345X06/form4.xml"),
            "https://www.sec.gov/Archives/edgar/data/320193/000114036126039785/form4.xml"
        );
        assert_eq!(
            form4_xml_url("https://www.sec.gov/Archives/edgar/data/1/2/doc.xml"),
            "https://www.sec.gov/Archives/edgar/data/1/2/doc.xml"
        );
    }

    fn table(rows: &[(&str, &str, &str, u64, u64, &str)]) -> String {
        let mut s = String::from("<informationTable>");
        for (name, class, cusip, value, shares, put) in rows {
            s.push_str(&format!(
                "<infoTable><nameOfIssuer>{name}</nameOfIssuer><titleOfClass>{class}</titleOfClass><cusip>{cusip}</cusip><value>{value}</value><shrsOrPrnAmt><sshPrnamt>{shares}</sshPrnamt><sshPrnamtType>SH</sshPrnamtType></shrsOrPrnAmt>{put}</infoTable>"
            ));
        }
        s + "</informationTable>"
    }

    #[test]
    fn info_table_aggregates_lines_and_skips_options() {
        let xml = table(&[
            ("ALLY FINL INC", "COM", "02005N100", 577, 12, ""),
            ("ALLY FINL INC", "COM", "02005N100", 129, 3, ""),
            ("APPLE INC", "COM", "037833100", 5000, 20, ""),
            ("APPLE INC", "COM", "037833100", 9999, 99, "<putCall>Put</putCall>"),
        ]);
        let p = parse_info_table(&xml, false);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].cusip.as_str(), p[0].value, p[0].shares), ("037833100", 5000.0, 20.0));
        assert_eq!((p[1].value, p[1].shares), (706.0, 15.0));
        assert_eq!(parse_info_table(&xml, true)[0].value, 5_000_000.0);
    }

    #[test]
    fn info_table_reported_in_thousands_after_2023() {
        // $353 a share reported as 484744 (thousands) for 1,371,931 shares.
        let xml = table(&[
            ("ALPHABET INC", "CL C", "02079K107", 484_744, 1_371_931, ""),
            ("AMAZON COM INC", "COM", "023135106", 892_310, 4_000_000, ""),
        ]);
        let p = parse_info_table(&xml, false);
        assert_eq!(p[1].value, 484_744_000.0);
    }

    #[test]
    fn info_table_with_namespace_prefixes() {
        let xml = "<?xml version=\"1.0\"?><ns1:informationTable xmlns:ns1=\"x\"><ns1:infoTable><ns1:nameOfIssuer>ALPHABET INC</ns1:nameOfIssuer><ns1:titleOfClass>CAP STK CL A</ns1:titleOfClass><ns1:cusip>02079K305</ns1:cusip><ns1:value>366</ns1:value><ns1:shrsOrPrnAmt><ns1:sshPrnamt>10</ns1:sshPrnamt><ns1:sshPrnamtType>SH</ns1:sshPrnamtType></ns1:shrsOrPrnAmt></ns1:infoTable></ns1:informationTable>";
        let p = parse_info_table(xml, false);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].name.as_str(), p[0].shares, p[0].value), ("ALPHABET INC", 10.0, 366.0));
    }

    #[test]
    fn info_table_file_is_the_largest_non_cover_xml() {
        let json = r#"{"directory":{"item":[{"name":"0001-index.html","size":""},{"name":"56757.xml","size":"44724"},{"name":"primary_doc.xml","size":"5555"}]}}"#;
        assert_eq!(infotable_name(json).as_deref(), Some("56757.xml"));
        assert_eq!(infotable_name(r#"{"directory":{"item":[{"name":"primary_doc.xml","size":"5"}]}}"#), None);
    }

    fn pos(cusip: &str, name: &str, class: &str, shares: f64, value: f64) -> Position {
        Position { cusip: cusip.into(), name: name.into(), class: class.into(), shares, value }
    }

    fn filing(period: &str, positions: Vec<Position>) -> Filing13F {
        let total = positions.iter().map(|p| p.value).sum();
        Filing13F { accession: period.into(), filed: d(period), period: d(period), total, positions }
    }

    fn manager() -> Manager {
        Manager {
            cik: 1,
            name: "Test Capital".into(),
            latest: Some(filing(
                "2026-06-30",
                vec![
                    pos("A", "APPLE INC", "COM", 150.0, 600.0),
                    pos("B", "NEWCO INC", "COM", 10.0, 300.0),
                    pos("C", "TRIM CORP", "COM", 50.0, 90.0),
                    pos("D", "FLAT CO", "COM", 100.0, 10.0),
                ],
            )),
            prev: Some(filing(
                "2026-03-31",
                vec![
                    pos("A", "APPLE INC", "COM", 100.0, 400.0),
                    pos("C", "TRIM CORP", "COM", 100.0, 150.0),
                    pos("D", "FLAT CO", "COM", 100.0, 10.0),
                    pos("E", "GONE INC", "COM", 70.0, 700.0),
                ],
            )),
            updated: 0,
        }
    }

    #[test]
    fn quarter_over_quarter_changes() {
        let c = manager().top(10);
        let moves: Vec<_> = c.iter().map(|c| (c.position.cusip.as_str(), c.mv)).collect();
        assert_eq!(moves, [("A", Move::Added), ("B", Move::New), ("C", Move::Reduced), ("D", Move::Unchanged)]);
        assert_eq!(c[0].pct, Some(50.0));
        assert_eq!(c[2].pct, Some(-50.0));
        assert_eq!(manager().top(2).len(), 2);
        let sold = manager().sold_out(5);
        assert_eq!(sold.len(), 1);
        assert_eq!((sold[0].position.cusip.as_str(), sold[0].mv), ("E", Move::Sold));
    }

    const TICKERS: &str = r#"{
      "0":{"cik_str":320193,"ticker":"AAPL","title":"Apple Inc."},
      "1":{"cik_str":1652044,"ticker":"GOOGL","title":"Alphabet Inc."},
      "2":{"cik_str":1652044,"ticker":"GOOG","title":"Alphabet Inc."},
      "3":{"cik_str":70858,"ticker":"BAC","title":"BANK OF AMERICA CORP /DE/"},
      "4":{"cik_str":1067983,"ticker":"BRK-B","title":"BERKSHIRE HATHAWAY INC"},
      "5":{"cik_str":1067983,"ticker":"BRK-A","title":"BERKSHIRE HATHAWAY INC"},
      "6":{"cik_str":40729,"ticker":"ALLY","title":"Ally Financial Inc."},
      "7":{"cik_str":1,"ticker":"AAA","title":"Same Name Corp"},
      "8":{"cik_str":2,"ticker":"BBB","title":"Same Name Inc"}
    }"#;

    #[test]
    fn issuer_names_resolve_to_tickers() {
        let idx = TitleIndex::parse(TICKERS).unwrap();
        assert_eq!(idx.ticker("APPLE INC", "COM"), Some("AAPL"));
        assert_eq!(idx.ticker("ALLY FINL INC", "COM"), Some("ALLY"));
        assert_eq!(idx.ticker("BANK AMERICA CORP", "COM"), Some("BAC"));
        assert_eq!(idx.ticker("BERKSHIRE HATHAWAY INC DEL", "CL B NEW"), Some("BRK-B"));
        // Dual-class names without a class suffix on the ticker: class A is the first listed.
        assert_eq!(idx.ticker("ALPHABET INC", "CAP STK CL C"), Some("GOOG"));
        assert_eq!(idx.ticker("ALPHABET INC", "CAP STK CL A"), Some("GOOGL"));
        assert_eq!(idx.ticker("ALPHABET INC", "COM"), Some("GOOGL"));
        // A shared name is ambiguous.
        assert_eq!(idx.ticker("SAME NAME CORP", "COM"), None);
        assert_eq!(idx.ticker("UNKNOWN WIDGETS", "COM"), None);
    }

    #[test]
    fn reverse_lookup_among_managers() {
        let idx = TitleIndex::parse(TICKERS).unwrap();
        let h = holders_of(&[manager()], "aapl", &idx);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].mv, h[0].pct, h[0].rank, h[0].shares), (Move::Added, Some(50.0), 1, 150.0));
        assert!((h[0].weight - 60.0).abs() < 1e-9);
        assert!(holders_of(&[manager()], "ALLY", &idx).is_empty());
    }

    const SI: &str = "\"accountingYearMonthNumber\"|\"symbolCode\"|\"issueName\"|\"issuerServicesGroupExchangeCode\"|\"marketClassCode\"|\"currentShortPositionQuantity\"|\"previousShortPositionQuantity\"|\"stockSplitFlag\"|\"averageDailyVolumeQuantity\"|\"daysToCoverQuantity\"|\"revisionFlag\"|\"changePercent\"|\"changePreviousNumber\"|\"settlementDate\"\r\n\
\"20260930\"|\"AAPL\"|\"Apple Inc. Common Stock\"|\"R\"|\"NNM\"|\"120763539\"|\"128753092\"||\"39858260\"|\"3.03\"||\"-6.21\"|\"-7989553\"|\"2026-09-30\"\r\n\
\"20260930\"|\"BRKB\"|\"BERKSHIRE HATHAWAY Class B\"|\"A\"|\"NYSE\"|\"12370081\"|\"12529378\"||\"5322205\"|\"2.32\"||\"-1.27\"|\"-159297\"|\"2026-09-30\"\r\n";

    #[test]
    fn short_interest_rows() {
        let c = compact_short_interest(SI);
        let sym = symbol_variants("AAPL");
        let r = find_short_interest(&c, d("2026-09-30"), &sym).unwrap();
        assert_eq!((r.short, r.prev, r.avg_volume, r.days_to_cover), (120763539.0, 128753092.0, 39858260.0, 3.03));
        assert_eq!(r.change(), -7989553.0);
        assert!((r.change_pct().unwrap() + 6.205).abs() < 0.01);
        let b = find_short_interest(&c, d("2026-09-30"), &symbol_variants("BRK-B")).unwrap();
        assert_eq!(b.short, 12370081.0);
        assert!(find_short_interest(&c, d("2026-09-30"), &symbol_variants("MSFT")).is_none());
    }

    #[test]
    fn short_sale_volume_rows() {
        let txt = "Date|Symbol|ShortVolume|ShortExemptVolume|TotalVolume|Market\n20261015|AAPL|7818518.562700|40699|13563308.037655|B,Q,N\n20261015|BRK/B|645941.9|1355|1519142.8|B,Q,N\n";
        let c = compact_short_volume(txt);
        let a = find_short_volume(&c, d("2026-10-15"), &symbol_variants("AAPL")).unwrap();
        assert!((a.ratio().unwrap() - 57.64).abs() < 0.01);
        assert!(find_short_volume(&c, d("2026-10-15"), &symbol_variants("BRK-B")).is_some());
    }

    #[test]
    fn settlement_calendar() {
        let s = settlement_dates(d("2026-10-16"), 3, 6);
        // 2026-10-15 is too recent to be published; the last published settlement is Sep 30.
        assert_eq!(&s[..4], [d("2026-09-30"), d("2026-09-15"), d("2026-08-31"), d("2026-08-14")]);
        // 2026-08-15 is a Saturday: the mid-month date rolls back to Friday.
        assert!(s.iter().all(|x| x.weekday().number_from_monday() <= 5));
        let w = recent_weekdays(d("2026-10-16"), 3);
        assert_eq!(w, [d("2026-10-16"), d("2026-10-15"), d("2026-10-14")]);
        assert_eq!(recent_weekdays(d("2026-10-12"), 2), [d("2026-10-12"), d("2026-10-09")]);
    }
}
