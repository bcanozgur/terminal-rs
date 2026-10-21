//! Turkish mutual funds (`TRF`): the fund list, a fund card, asset and stock allocation, monthly
//! buy/sell changes and reverse search (which funds hold a BIST stock).
//!
//! Source: **KAP** (kap.org.tr, the Public Disclosure Platform), the pages and JSON endpoints its
//! own website calls, all readable without a login:
//! - `/tr/YatirimFonlari/YF`: every investment fund (code, name, founder, umbrella fund, id).
//! - `/tr/fon-bilgileri/genel/<slug>`: the fund's general information form (management fee,
//!   fund manager, inception).
//! - `/tr/api/disclosure/filter/FILTERYFBF/<fund id>/<subject>/<days>`: the fund's disclosures of
//!   one kind; the subject used here is the monthly "Portföy Dağılım Raporu" (portfolio
//!   distribution report).
//! - `/tr/api/notification/attachment-detail/<index>` and `/tr/api/file/download/<id>`: the
//!   report itself, a PDF (every portfolio manager lays it out in its own variant of the SPK
//!   template). Its text is rebuilt from glyph positions and parsed by [`parse_report`].
//!
//! Not available from KAP's structured data, hence `None`/"n/a" in the card: the settlement
//! day (valör; it appears nowhere in the fund's disclosures except free text in the prospectus).
//! Report layouts that [`parse_report`] does not recognise yield funds without stock positions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

const BASE: &str = "https://www.kap.org.tr";
const LIST_TTL: i64 = 7 * 24 * 3600;
/// Reports are monthly (filed in the first days of the month): a fund is re-checked daily.
const FUND_TTL: i64 = 24 * 3600;
const PROFILE_TTL: i64 = 14 * 24 * 3600;
const REPORT_TITLE: &str = "Portföy Dağılım Raporu";
/// How far back the disclosure search looks (KAP's maximum), so a fund that files late or
/// stopped reporting still shows its last reports.
const LOOKBACK_DAYS: u32 = 365;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// Positions under this weight (percent of fund value) are left out of lists and charts.
pub const MIN_WEIGHT: f64 = 0.1;

/// Popular funds indexed by "Index curated list" (mostly equity-heavy funds, so that reverse
/// search by stock has something to find).
pub const CURATED: &[&str] = &[
    "TI2", "TTE", "TIE", "TAU", "TI3", "IDH", "KPH", "TIL", "IHT", "AK3", "AKU", "ADP", "ALC", "ICZ", "KVT", "GHS",
    "GAE", "GKG", "GRT", "GTM", "TYH", "ICF", "YZH", "YHS", "YEF", "YHZ", "YAS", "TZD", "ZLH", "ZJL", "HVS", "HBU",
    "FYD", "SLG", "DAH", "DZE", "IIH", "ACC", "GAF", "OYH", "OSS", "IPB", "KPC", "DPT", "PHE", "NNF", "MAC", "HKH",
    "BUY", "VAY",
];

// ---- model --------------------------------------------------------------------------------

/// One row of KAP's fund list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundInfo {
    pub code: String,
    pub name: String,
    /// KAP's internal fund id (needed to list its disclosures).
    pub oid: String,
    /// Founder (portfolio management company).
    pub manager: String,
    /// Fund type, from the umbrella fund's name (`Hisse Senedi`, `Para Piyasası`…).
    pub kind: String,
    /// The fund's page name on kap.org.tr (empty: derived from code and name).
    #[serde(default)]
    pub slug: String,
}

impl FundInfo {
    /// The fund's page name on kap.org.tr: lower-case, ASCII, dashes.
    pub fn page_slug(&self) -> String {
        if self.slug.is_empty() { slugify(&format!("{} {}", self.code, self.name)) } else { self.slug.clone() }
    }
}

/// An allocation line of the monthly report ("Hisse Senedi", 88.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alloc {
    pub label: String,
    /// Percent.
    pub weight: f64,
}

/// A stock position (all lots of one ticker added up).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// BIST ticker without the `.E` / `.IS` suffix.
    pub ticker: String,
    /// Percent of the fund's total value (negative for short or hedged lots netted in).
    pub weight: f64,
    /// Nominal number of shares, when the report says.
    #[serde(default)]
    pub shares: Option<f64>,
}

/// What a [`Report::mix`] measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MixKind {
    /// The report's average over the month.
    #[default]
    Average,
    /// Share of the fund value at month end.
    MonthEnd,
}

/// What a monthly "Portföy Dağılım Raporu" says.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Report {
    /// Fund total value / net assets, TL.
    #[serde(default)]
    pub net_assets: Option<f64>,
    #[serde(default)]
    pub units: Option<f64>,
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// Price change over the month, year to date and 12 months, percent.
    #[serde(default)]
    pub monthly: Option<f64>,
    #[serde(default)]
    pub ytd: Option<f64>,
    #[serde(default)]
    pub yearly: Option<f64>,
    /// Asset allocation, largest first: the month-end split from the report's group totals when
    /// it has them, else the report's "average monthly portfolio mix" (see `mix_kind`).
    #[serde(default)]
    pub mix: Vec<Alloc>,
    #[serde(default)]
    pub mix_kind: MixKind,
    /// The report prints no market values, so the weights are nominal × unit cost over the fund
    /// value (an estimate).
    #[serde(default)]
    pub approx_weights: bool,
    /// Stocks held at the end of the month, largest weight first.
    #[serde(default)]
    pub positions: Vec<Position>,
}

impl Report {
    /// Positions of at least [`MIN_WEIGHT`] percent.
    pub fn significant(&self) -> impl Iterator<Item = &Position> {
        self.positions.iter().filter(|p| p.weight >= MIN_WEIGHT)
    }

    /// The `n` largest positions and the rest of the stock weight as one figure (percent).
    pub fn top_and_other(&self, n: usize) -> (Vec<&Position>, f64) {
        let top: Vec<&Position> = self.significant().take(n).collect();
        let all: f64 = self.positions.iter().map(|p| p.weight).sum();
        let shown: f64 = top.iter().map(|p| p.weight).sum();
        (top, (all - shown).max(0.0))
    }

    pub fn stock_weight(&self) -> f64 {
        self.positions.iter().map(|p| p.weight).sum()
    }

    pub fn weight_of(&self, ticker: &str) -> Option<&Position> {
        let t = clean_ticker(ticker);
        self.positions.iter().find(|p| p.ticker == t)
    }
}

/// A report with the period it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub year: i32,
    /// The month, or the week for a fund that reports weekly (see `weekly`).
    pub month: u32,
    /// KAP's period (`donem`) is a week number: some serbest funds file every week.
    #[serde(default)]
    pub weekly: bool,
    /// KAP disclosure number.
    pub index: u64,
    /// Publication date as KAP prints it (`05.10.2026 13:00:53`).
    pub published: String,
    pub report: Report,
}

impl Snapshot {
    pub fn label(&self) -> String {
        // Copies saved before `weekly` existed still show week numbers above 12.
        if self.weekly || self.month > 12 {
            return format!("Week {} {}", self.month, self.year);
        }
        NaiveDate::from_ymd_opt(self.year, self.month.clamp(1, 12), 1)
            .map_or_else(|| format!("{}-{:02}", self.year, self.month), |d| d.format("%b %Y").to_string())
    }

    fn key(&self) -> (i32, u32) {
        (self.year, self.month)
    }
}

/// The fund's general information form.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Profile {
    pub isin: Option<String>,
    /// Applied management fee, percent per year.
    pub mgmt_fee: Option<f64>,
    pub entry_fee: Option<f64>,
    pub exit_fee: Option<f64>,
    pub perf_fee: Option<f64>,
    pub fund_manager: Option<String>,
    pub inception: Option<NaiveDate>,
    /// Umbrella fund type (`HİSSE SENEDİ ŞEMSİYE FONU`).
    pub umbrella: Option<String>,
    /// Settlement day. KAP's structured fund data does not carry it.
    pub valor: Option<String>,
}

/// Everything the detail view shows about one fund.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundData {
    pub info: FundInfo,
    pub profile: Profile,
    /// Newest first, at most two.
    pub reports: Vec<Snapshot>,
    /// Why no (or only one) report could be read, when that is the case.
    #[serde(default)]
    pub note: Option<String>,
    /// Unix seconds when the data was last fetched.
    pub updated: i64,
    #[serde(default)]
    pub profile_updated: i64,
}

impl FundData {
    pub fn latest(&self) -> Option<&Snapshot> {
        self.reports.first()
    }

    pub fn previous(&self) -> Option<&Snapshot> {
        self.reports.get(1)
    }

    pub fn stale(&self, now: i64) -> bool {
        now - self.updated > FUND_TTL
    }

    /// Changes of the stock positions from the previous report to the latest.
    pub fn changes(&self) -> Vec<Change> {
        match (self.previous(), self.latest()) {
            (Some(p), Some(l)) => changes(&p.report, &l.report),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    New,
    Added,
    Reduced,
    Exited,
}

impl Move {
    pub fn label(self) -> &'static str {
        match self {
            Move::New => "New",
            Move::Added => "Added",
            Move::Reduced => "Reduced",
            Move::Exited => "Exited",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub ticker: String,
    pub kind: Move,
    /// Percent of fund value before and after.
    pub before: f64,
    pub after: f64,
    pub shares_before: Option<f64>,
    pub shares_after: Option<f64>,
}

impl Change {
    pub fn delta_weight(&self) -> f64 {
        self.after - self.before
    }

    /// Change of the share count, percent, when both reports state it.
    pub fn delta_shares_pct(&self) -> Option<f64> {
        match (self.shares_before, self.shares_after) {
            (Some(b), Some(a)) if b > 0.0 => Some((a / b - 1.0) * 100.0),
            _ => None,
        }
    }
}

/// Buys and sells between two monthly reports: by share count when both state it (so a price
/// move alone is not a trade), else by weight. Positions under [`MIN_WEIGHT`] on both sides and
/// unchanged ones are left out; sorted by the size of the weight change.
pub fn changes(prev: &Report, cur: &Report) -> Vec<Change> {
    let before: HashMap<&str, &Position> = prev.positions.iter().map(|p| (p.ticker.as_str(), p)).collect();
    let after: HashMap<&str, &Position> = cur.positions.iter().map(|p| (p.ticker.as_str(), p)).collect();
    let mut tickers: Vec<&str> = before.keys().chain(after.keys()).copied().collect();
    tickers.sort_unstable();
    tickers.dedup();
    let mut out = Vec::new();
    for t in tickers {
        let (b, a) = (before.get(t), after.get(t));
        let (wb, wa) = (b.map_or(0.0, |p| p.weight), a.map_or(0.0, |p| p.weight));
        if wb < MIN_WEIGHT && wa < MIN_WEIGHT {
            continue;
        }
        let (sb, sa) = (b.and_then(|p| p.shares), a.and_then(|p| p.shares));
        let kind = if wb < MIN_WEIGHT {
            Move::New
        } else if wa < MIN_WEIGHT {
            Move::Exited
        } else {
            match (sb, sa) {
                (Some(x), Some(y)) if x > 0.0 && y > 0.0 => {
                    let r = y / x;
                    if r > 1.02 {
                        Move::Added
                    } else if r < 0.98 {
                        Move::Reduced
                    } else {
                        continue;
                    }
                }
                _ if wa - wb > 0.1 => Move::Added,
                _ if wb - wa > 0.1 => Move::Reduced,
                _ => continue,
            }
        };
        out.push(Change { ticker: t.to_string(), kind, before: wb, after: wa, shares_before: sb, shares_after: sa });
    }
    out.sort_by(|x, y| y.delta_weight().abs().total_cmp(&x.delta_weight().abs()));
    out
}

/// A fund holding a stock.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub code: String,
    pub name: String,
    pub weight: f64,
    /// Weight in the previous report, when the fund has one.
    pub previous: Option<f64>,
    pub period: String,
    /// The weight is an estimate (see [`Report::approx_weights`]).
    pub approx: bool,
}

/// Which of `funds` hold `ticker` (as of their latest report), largest weight first.
pub fn reverse_search(funds: &[Arc<FundData>], ticker: &str) -> Vec<Held> {
    let t = clean_ticker(ticker);
    if t.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Held> = funds
        .iter()
        .filter_map(|f| {
            let snap = f.latest()?;
            let w = snap.report.weight_of(&t).filter(|p| p.weight >= MIN_WEIGHT)?.weight;
            Some(Held {
                code: f.info.code.clone(),
                name: f.info.name.clone(),
                weight: w,
                previous: f.previous().and_then(|p| p.report.weight_of(&t)).map(|p| p.weight),
                period: snap.label(),
                approx: snap.report.approx_weights,
            })
        })
        .collect();
    out.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    out
}

/// A ticker as typed: `thyao`, `THYAO.IS`, `THYAO IS Equity`, `THYAO.E` → `THYAO`.
pub fn clean_ticker(s: &str) -> String {
    let w = s.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
    let w = w.strip_suffix(".IS").or_else(|| w.strip_suffix(".E")).unwrap_or(&w);
    w.to_string()
}

// ---- text helpers -------------------------------------------------------------------------

/// Turkish-aware upper case (`i` → `İ`, `ı` → `I`).
pub fn tr_upper(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            'i' => vec!['İ'],
            'ı' => vec!['I'],
            c => c.to_uppercase().collect(),
        })
        .collect()
}

fn tr_lower(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            'İ' => vec!['i'],
            'I' => vec!['ı'],
            c => c.to_lowercase().collect(),
        })
        .collect()
}

/// `HİSSE SENEDİ` → `Hisse Senedi`.
fn tr_title(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let lower = tr_lower(w);
            let mut cs = lower.chars();
            match cs.next() {
                Some(c) => tr_upper(&c.to_string()) + cs.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        let c = match c {
            'ı' | 'İ' | 'I' | 'i' => 'i',
            'ğ' | 'Ğ' => 'g',
            'ü' | 'Ü' => 'u',
            'ş' | 'Ş' => 's',
            'ö' | 'Ö' => 'o',
            'ç' | 'Ç' => 'c',
            c => c.to_ascii_lowercase(),
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

// ---- fund list ----------------------------------------------------------------------------

/// Value of `"key":"…"` in the first object after `from`, with JSON escapes decoded.
fn json_str(chunk: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = chunk.find(&pat)? + pat.len();
    let rest = &chunk[start..];
    let mut end = 0;
    let mut esc = false;
    for (i, c) in rest.char_indices() {
        if esc {
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            end = i;
            break;
        }
    }
    serde_json::from_str::<String>(&format!("\"{}\"", &rest[..end])).ok().map(|s| s.trim().to_string())
}

/// The funds of `/tr/YatirimFonlari/YF` (the Next.js payload of the page lists every fund).
pub fn parse_list(html: &str) -> Result<Vec<FundInfo>> {
    // The payload sits in a JS string, so its quotes are escaped.
    let s = html.replace("\\\"", "\"");
    let mut out: Vec<FundInfo> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for chunk in s.split("{\"umbFundName\":\"").skip(1) {
        let chunk = chunk.split("\"nonInactiveCount\"").next().unwrap_or(chunk);
        let umb = json_str(&format!("\"umbFundName\":\"{chunk}"), "umbFundName").unwrap_or_default();
        let (Some(code), Some(oid), Some(name)) =
            (json_str(chunk, "fundCode"), json_str(chunk, "fundOid"), json_str(chunk, "fundName"))
        else {
            continue;
        };
        if code.is_empty() || !seen.insert(code.clone()) {
            continue;
        }
        let manager = json_str(chunk, "fundMemberTitle").unwrap_or_default();
        let kind = fund_kind(&umb, &manager);
        out.push(FundInfo { code, name, oid, manager, kind, slug: String::new() });
    }
    if out.is_empty() {
        bail!("no funds found in the KAP fund list page");
    }
    // The page's own links name each fund's page exactly.
    let mut slugs: HashMap<String, &str> = HashMap::new();
    for part in s.split("/tr/fon-bilgileri/ozet/").skip(1) {
        let slug = part.split(['"', '\\', '?', '#']).next().unwrap_or_default();
        if let Some(code) = slug.split('-').next() {
            slugs.entry(code.to_ascii_uppercase()).or_insert(slug);
        }
    }
    for f in &mut out {
        if let Some(sl) = slugs.get(&f.code) {
            f.slug = sl.to_string();
        }
    }
    out.sort_by(|a, b| a.code.cmp(&b.code));
    Ok(out)
}

/// `A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş. BORÇLANMA ARAÇLARI ŞEMSİYE FONU` → `Borçlanma Araçları`.
fn fund_kind(umbrella: &str, manager: &str) -> String {
    let mut s = umbrella.trim();
    if let Some(r) = s.strip_prefix(manager.trim()) {
        s = r.trim();
    }
    for suffix in ["ŞEMSİYE FONU", "ŞEMSİYE FON"] {
        if let Some(r) = s.strip_suffix(suffix) {
            s = r.trim();
        }
    }
    if s.is_empty() { "–".into() } else { tr_title(s) }
}

// ---- profile (general information form) ---------------------------------------------------

/// The `value` of the form item `key` in the page's payload.
fn item_value(s: &str, key: &str) -> Option<serde_json::Value> {
    let pat = format!("\"itemKey\":\"{key}\",\"value\":");
    let start = s.find(&pat)? + pat.len();
    serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>().next()?.ok()
}

fn pct_str(v: Option<&serde_json::Value>) -> Option<f64> {
    let s = v?.as_str()?.trim().replace(',', ".");
    s.parse().ok().filter(|x: &f64| x.is_finite())
}

fn kap_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..8)?, "%Y%m%d").ok()
}

/// Parse `/tr/fon-bilgileri/genel/<slug>`.
pub fn parse_profile(html: &str) -> Result<Profile> {
    let s = html.replace("\\\"", "\"");
    if !s.contains("\"itemKey\":\"kpy81_") {
        bail!("not a KAP fund information page");
    }
    let text = |key: &str| {
        item_value(&s, key).and_then(|v| v.as_str().map(|t| t.trim().to_string())).filter(|t| !t.is_empty())
    };
    let first = |key: &str| item_value(&s, key).and_then(|v| v.as_array().and_then(|a| a.first().cloned()));
    let fees = first("kpy81_acc2_fon_ucret_kom_info");
    let fee = |k: &str| pct_str(fees.as_ref().and_then(|f| f.get(k)));
    Ok(Profile {
        isin: text("kpy81_acc1_ISIN"),
        mgmt_fee: fee("uygulananYonetimUcretiOranYillikYuzde")
            .or_else(|| fee("ictuzukteYerAlanYonetimUcretiOraniYillikYuzde")),
        entry_fee: fee("girisKomisyonu"),
        exit_fee: fee("cikisKomisyonu"),
        perf_fee: fee("performansUcretiOrani"),
        fund_manager: first("kpy81_acc3_fon_mudur")
            .and_then(|m| m.get("adSoyad").and_then(|n| n.as_str()).map(|n| n.trim().to_string()))
            .filter(|n| !n.is_empty()),
        inception: first("kpy81_acc1_halka_arz2")
            .and_then(|h| h.get("halkaArzTarihleri").and_then(|d| d.as_str()).and_then(kap_date)),
        umbrella: text("kpy81_acc1_fon_sem_tur").map(|t| tr_title(&t)),
        valor: None,
    })
}

// ---- report list --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ReportRef {
    pub index: u64,
    pub year: i32,
    pub month: u32,
    pub published: String,
    pub weekly: bool,
}

/// The reports in a `FILTERYFBF` response, newest month first, one per month (the last filed
/// wins, so a correction replaces the original).
pub fn parse_report_refs(json: &str) -> Result<Vec<ReportRef>> {
    let v: serde_json::Value = serde_json::from_str(json).context("KAP disclosure list")?;
    let mut refs: Vec<ReportRef> = v
        .as_array()
        .ok_or_else(|| anyhow!("KAP disclosure list is not an array"))?
        .iter()
        .filter_map(|d| {
            let b = d.get("disclosureBasic")?;
            if b.get("title").and_then(|t| t.as_str()).is_some_and(|t| t.trim() != REPORT_TITLE) {
                return None;
            }
            Some(ReportRef {
                index: b.get("disclosureIndex")?.as_u64()?,
                year: b.get("year")?.as_i64()? as i32,
                month: b.get("donem")?.as_u64()? as u32,
                published: b.get("publishDate").and_then(|p| p.as_str()).unwrap_or_default().to_string(),
                weekly: false,
            })
        })
        .collect();
    refs.sort_by_key(|r| std::cmp::Reverse((r.year, r.month, r.index)));
    refs.dedup_by_key(|r| (r.year, r.month));
    // A weekly filer: a period past 12, or the two newest reports published under 20 days apart.
    let day = |r: &ReportRef| NaiveDate::parse_from_str(r.published.get(..10)?, "%d.%m.%Y").ok();
    let close = match (refs.first().and_then(day), refs.get(1).and_then(day)) {
        (Some(a), Some(b)) => (a - b).num_days().abs() < 20,
        _ => false,
    };
    let weekly = close || refs.iter().any(|r| r.month > 12);
    for r in &mut refs {
        r.weekly = weekly;
    }
    Ok(refs)
}

/// Disclosure numbers of the fund's prospectuses (`İzahname`, then amendments), newest first.
pub fn parse_prospectus_refs(json: &str) -> Vec<u64> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let mut found: Vec<(bool, u64)> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let b = d.get("disclosureBasic")?;
            let title = b.get("title")?.as_str()?.trim();
            let index = b.get("disclosureIndex")?.as_u64().unwrap_or(0);
            title.starts_with("İzahname").then_some((title != "İzahname", index))
        })
        .collect();
    // The prospectus proper before its amendments, newest first within each.
    found.sort_by_key(|(amendment, index)| (*amendment, std::cmp::Reverse(*index)));
    found.into_iter().map(|(_, i)| i).collect()
}

/// The settlement of redemptions from a prospectus's "Satım Bedellerinin Ödenme Esasları"
/// section: `Sell T+1 (T+2 if ordered after 13:30)`. `None` when the text has no such section.
pub fn parse_valor(text: &str) -> Option<String> {
    let chars: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let up: Vec<char> = chars
        .iter()
        .map(|c| match c {
            'i' => 'İ',
            'ı' => 'I',
            c => c.to_uppercase().next().unwrap_or(*c),
        })
        .collect();
    let needle: Vec<char> = "SATIM BEDELLERİNİN ÖDENME".chars().collect();
    let at = up.windows(needle.len()).position(|w| w == needle.as_slice())?;
    let mut window: String = chars[at..(at + 900).min(chars.len())].iter().collect();
    if let Some(cut) = window.find("6.5") {
        window.truncate(cut);
    }
    let words: Vec<String> = window.split_whitespace().map(tr_lower).collect();
    let mut days: Vec<u32> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let n = match w.trim_matches(|c: char| !c.is_alphanumeric()) {
            "birinci" | "1" => 1,
            "ikinci" | "2" => 2,
            "üçüncü" | "3" => 3,
            "dördüncü" | "4" => 4,
            "beşinci" | "5" => 5,
            _ => continue,
        };
        if words.get(i + 1).is_some_and(|n| n.starts_with("işlem") || n.starts_with("iş")) {
            days.push(n);
        }
    }
    let first = *days.first()?;
    let cutoff = words.iter().position(|w| w == "saat").and_then(|i| words.get(i + 1)).and_then(|t| {
        let t: String = t.chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ':')).collect();
        let (h, m) = t.split_once(['.', ':'])?;
        (h.len() <= 2 && m.len() == 2).then(|| format!("{h}:{m}"))
    });
    Some(match (days.iter().find(|d| **d != first), cutoff) {
        (Some(later), Some(c)) => format!("Sell T+{first} (T+{later} if ordered after {c})"),
        (Some(later), None) => format!("Sell T+{first} (T+{later} when ordered late)"),
        (None, _) => format!("Sell T+{first}"),
    })
}

/// The attachment id of a disclosure from `attachment-detail`.
pub fn parse_attachment(json: &str) -> Result<String> {
    parse_attachments(json)?.into_iter().next().ok_or_else(|| anyhow!("the disclosure has no PDF attachment"))
}

/// Every PDF attachment's file id, in KAP's order. Some managers split a report over several
/// files (the portfolio table in one, the buys and sells in another).
pub fn parse_attachments(json: &str) -> Result<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json).context("KAP attachment list")?;
    Ok(v.pointer("/0/attachments")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter(|x| x.get("fileExtension").and_then(|e| e.as_str()).is_none_or(|e| e.eq_ignore_ascii_case("pdf")))
        .filter_map(|x| x.get("objId").and_then(|o| o.as_str()).map(str::to_string))
        .collect())
}

// ---- PDF text -----------------------------------------------------------------------------

struct Glyph {
    page: u32,
    /// Position in the content stream.
    seq: usize,
    x: f64,
    end: f64,
    y: f64,
    size: f64,
    text: String,
}

#[derive(Default)]
struct Collector {
    page: u32,
    height: f64,
    glyphs: Vec<Glyph>,
}

impl pdf_extract::OutputDev for Collector {
    fn begin_page(
        &mut self,
        page: u32,
        media_box: &pdf_extract::MediaBox,
        _: Option<(f64, f64, f64, f64)>,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        self.page = page;
        self.height = media_box.ury - media_box.lly;
        Ok(())
    }

    fn end_page(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        text: &str,
    ) -> std::result::Result<(), pdf_extract::OutputError> {
        let size = font_size * (trm.m11 * trm.m22 - trm.m12 * trm.m21).abs().sqrt();
        let x = trm.m31;
        self.glyphs.push(Glyph {
            page: self.page,
            seq: self.glyphs.len(),
            x,
            end: x + width * size,
            y: self.height - trm.m32,
            size,
            text: text.to_string(),
        });
        Ok(())
    }

    fn begin_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_word(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> std::result::Result<(), pdf_extract::OutputError> {
        Ok(())
    }
}

/// The text of a PDF as lines: glyphs on one baseline joined left to right, a double space
/// between cells. (Plain text extraction scrambles table cells; positions keep rows intact.)
pub fn pdf_lines(pdf: &[u8]) -> Result<Vec<String>> {
    let doc = pdf_extract::Document::load_mem(pdf).map_err(|e| anyhow!("unreadable PDF: {e}"))?;
    let mut c = Collector::default();
    pdf_extract::output_doc(&doc, &mut c).map_err(|e| anyhow!("PDF text extraction failed: {e}"))?;
    let mut g = c.glyphs;
    g.sort_by(|a, b| a.page.cmp(&b.page).then(a.y.total_cmp(&b.y)));
    let mut lines = Vec::new();
    let mut i = 0;
    while i < g.len() {
        let tol = 0.4 * g[i].size.max(1.0);
        let mut j = i + 1;
        while j < g.len() && g[j].page == g[i].page && (g[j].y - g[i].y).abs() < tol {
            j += 1;
        }
        // Runs of glyphs drawn left to right, in stream order. Where a font's widths are off, a
        // cell's glyphs reach past the next cell's start and sorting single glyphs by position
        // interleaves the two (`2,298,400.10.033%` for `2,298,400.00` and `1.33%`); whole runs
        // sorted by where they start keep each cell intact.
        let mut row: Vec<&Glyph> = g[i..j].iter().collect();
        row.sort_by_key(|gl| gl.seq);
        let mut runs: Vec<Vec<&Glyph>> = Vec::new();
        for gl in row {
            match runs.last_mut() {
                Some(run)
                    if run.last().is_some_and(|p| gl.x >= p.x - 0.1 * gl.size && gl.x <= p.end + 1.5 * gl.size) =>
                {
                    run.push(gl)
                }
                _ => runs.push(vec![gl]),
            }
        }
        runs.sort_by(|a, b| a[0].x.total_cmp(&b[0].x));
        let mut s = String::new();
        let mut end = f64::MIN;
        for run in runs {
            for (k, gl) in run.into_iter().enumerate() {
                if end > f64::MIN && gl.x > end + 0.15 * gl.size {
                    s.push(' ');
                    if gl.x > end + 1.5 * gl.size {
                        s.push(' ');
                    }
                } else if k == 0 && end > f64::MIN {
                    // A run starting inside the previous one: a separate cell.
                    s.push_str("  ");
                }
                s.push_str(&gl.text);
                end = gl.end;
            }
        }
        lines.push(fix_turkish(s));
        i = j;
    }
    if lines.iter().all(|l| l.trim().is_empty()) {
        bail!("the report PDF has no text layer (a scanned image), so it cannot be read");
    }
    Ok(lines)
}

/// Some managers' fonts map the Turkish letters to their neighbours in Latin Extended-A
/// (`BĠRĠNCĠ`, `KuruluĢ`, `A.ġ`); those code points do not occur in Turkish text.
fn fix_turkish(s: String) -> String {
    if !s.contains(['Ġ', 'ġ', 'Ģ']) {
        return s;
    }
    s.chars()
        .map(|c| match c {
            'Ġ' => 'İ',
            'Ģ' => 'ş',
            'ġ' => 'Ş',
            c => c,
        })
        .collect()
}

/// KAP serves attachments as a serialized Java byte array around the PDF: cut to `%PDF-`.
pub fn strip_wrapper(bytes: &[u8]) -> &[u8] {
    match bytes.windows(5).take(256).position(|w| w == b"%PDF-") {
        Some(i) => &bytes[i..],
        None => bytes,
    }
}

// ---- report parsing -----------------------------------------------------------------------

fn is_date(t: &str) -> bool {
    let b = t.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    match b.len() {
        8 => matches!(b[2], b'/' | b'.') && matches!(b[5], b'/' | b'.') && digits(0..2) && digits(3..5) && digits(6..8),
        10 => {
            matches!(b[2], b'/' | b'.') && matches!(b[5], b'/' | b'.') && digits(0..2) && digits(3..5) && digits(6..10)
        }
        _ => false,
    }
}

fn is_num(t: &str) -> bool {
    !is_date(t)
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '+' | '%'))
}

/// Number in either notation: `1,234.50`, `1.234,50`. A lone separator is a decimal point
/// unless it repeats; `comma_decimal` settles `1.234`-style ambiguity for the whole report.
fn parse_num(t: &str, comma_decimal: bool) -> Option<f64> {
    let t = t.trim().trim_end_matches('%');
    if !is_num(t) {
        return None;
    }
    let (commas, dots) = (t.matches(',').count(), t.matches('.').count());
    let s = if comma_decimal {
        t.replace('.', "").replace(',', ".")
    } else if commas > 0 && dots == 0 && commas == 1 && t.rsplit(',').next().is_some_and(|f| f.len() != 3) {
        t.replace(',', ".")
    } else {
        t.replace(',', "")
    };
    s.parse().ok().filter(|v: &f64| v.is_finite())
}

/// Whether a number string uses a decimal comma (`1.234,56`) rather than a decimal point.
fn decimal_comma(t: &str) -> bool {
    t.rfind(['.', ',']).is_some_and(|i| t.as_bytes()[i] == b',')
}

/// Drop a leading list label (`A.`, `B -`, `B.1.`, `III-`, `a-)`) from a line; a word such
/// as `HİSSE` (no separator after it) is left alone.
fn strip_label(s: &str) -> &str {
    let s = s.trim_start();
    let n = s.chars().take_while(|c| c.is_alphanumeric()).count();
    if n == 0 || n > 3 {
        return s;
    }
    let after = s[s.chars().take(n).map(char::len_utf8).sum::<usize>()..].trim_start();
    const SEP: [char; 4] = ['.', '-', ')', ' '];
    if !after.starts_with(['.', '-', ')']) {
        return s;
    }
    let mut r = after.trim_start_matches(SEP);
    loop {
        let t = r.trim_start_matches(|c: char| c.is_ascii_digit());
        if t.len() != r.len() && t.starts_with(['.', ')', '-']) {
            r = t.trim_start_matches(SEP);
        } else {
            return r;
        }
    }
}

fn is_equity_heading(up: &str) -> bool {
    let h = strip_label(up).trim();
    h.starts_with("HİSSE SENETLERİ") || h == "PAY" || h.starts_with("PAY ")
}

fn is_other_heading(up: &str) -> bool {
    if up.chars().any(|c| c.is_ascii_digit()) || up.len() > 60 {
        return false;
    }
    let h = strip_label(up).trim();
    // A single word must be the whole line and a phrase its start: a company called
    // `Altın Yunus …` or `Altınay …` must not end the stock section.
    const WORDS: [&str; 6] = ["REPO", "MEVDUAT", "TÜREV", "DİĞER", "SERTİFİKALAR", "SERTIFIKALAR"];
    const PHRASES: [&str; 22] = [
        "TERS REPO",
        "BORÇLANMA ARAÇ",
        "ÖZEL SEKTÖR",
        "KAMU BORÇ",
        "KAMU KİRA",
        "KİRA SERTİFİKA",
        "VARLIĞA DAYALI",
        "ALTIN VE",
        "KIYMETLİ MADEN",
        "YABANCI KİRA",
        "YABANCI BORÇ",
        "YABANCI TAHVİL",
        "YATIRIM FONU",
        "BORSA YATIRIM",
        "KATILIM HESAB",
        "GAYRİMENKUL SERTİFİKA",
        "KATILMA PAYI",
        "FON PORTFÖY DEĞERİ",
        "FİNANSMAN BONOSU",
        "TÜREV ARAÇ",
        "DİĞER VARLIK",
        "VADELİ İŞLEM",
    ];
    let h = h.trim_end_matches([':', ' ']);
    WORDS.contains(&h)
        || PHRASES
            .iter()
            .any(|p| h.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with([' ', '.', 'L', 'İ', 'I'])))
}

fn is_isin(t: &str) -> bool {
    t.len() == 12 && t.starts_with("TR") && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn is_ticker(t: &str) -> bool {
    let t = t.strip_suffix(".E").unwrap_or(t);
    (2..=6).contains(&t.len())
        && t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && !matches!(t, "GRUP" | "ANA" | "TOPLAM" | "FON" | "NET" | "TL" | "USD" | "EUR" | "MENKUL" | "KIYMET")
}

/// Text after the last `:` of a `label : value` line, else the line itself.
fn value_part(line: &str) -> &str {
    line.rsplit_once(':').map_or(line, |(_, v)| v)
}

fn last_num(line: &str, comma_decimal: bool) -> Option<f64> {
    value_part(line).split_whitespace().rev().find_map(|t| parse_num(t, comma_decimal))
}

/// One stock row of the portfolio table: ticker, shares and weight (and whether the weight is
/// only estimated). Layouts seen on KAP: `ASELS TL … TRAASELS91H2 1,946,160.80 264.70 25/09/26
/// … 31.13 29.77 29.02` (last column: percent of fund value), the same with `AEFES.E` and
/// `0.07% 0.06%`, `1 AKBNK.E AKBANK TAS 200.000,000 13.400.000,00 2,176842 …` (value in TL,
/// divided by the fund total), `AKBNK TRAAKBNK91N6 Akbank 1,634,550.35 109,514,873.32 4.47`
/// and `AKBNK TL 9,600.00 72.773797 24/09/26` (market value and weight columns missing from
/// the PDF: nominal × unit cost, divided by the fund total).
fn stock_row(line: &str, comma_decimal: bool, total: Option<f64>) -> Option<(Position, bool)> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let off = usize::from(toks.len() > 1 && toks[0].chars().all(|c| c.is_ascii_digit()));
    let ticker = *toks.get(off)?;
    if !is_ticker(ticker) {
        return None;
    }
    let rest = &toks[off + 1..];
    let nums: Vec<&str> = rest.iter().copied().filter(|t| is_num(t)).collect();
    let dated = rest.iter().any(|t| is_date(t));
    let isin_first = rest.first().is_some_and(|t| is_isin(t));
    let shares =
        nums.iter().filter(|t| !t.ends_with('%')).filter_map(|t| parse_num(t, comma_decimal)).find(|v| *v != 0.0);
    let position =
        |weight: f64| Position { ticker: clean_ticker(ticker), weight, shares: shares.filter(|s| s.is_finite()) };
    if dated && nums.len() == 2 {
        let (nominal, cost) = (parse_num(nums[0], comma_decimal)?, parse_num(nums[1], comma_decimal)?);
        return Some((position(nominal * cost / total? * 100.0), true));
    }
    if nums.len() < 3 {
        return None;
    }
    // `TICKER name nominal value %` (Ata Portföy): taken when the percent agrees with value / total
    // (weights are of the portfolio value, a little below the fund total).
    let three = nums.len() == 3 && nums[2].ends_with('%') && {
        match (parse_num(nums[1], comma_decimal), parse_num(nums[2], comma_decimal), total) {
            (Some(v), Some(pct), Some(t)) if t > 0.0 => (v / t * 100.0 - pct).abs() <= 0.05 + 0.05 * pct.abs(),
            _ => false,
        }
    };
    // Dated lot rows and `TICKER ISIN name nominal value %` rows end with the percent column.
    if !(dated || isin_first || three) && nums.len() < 4 {
        return None;
    }
    let weight = if dated || isin_first || three || total.is_none() {
        parse_num(nums.last()?, comma_decimal)?
    } else {
        parse_num(nums.get(1)?, comma_decimal)? / total? * 100.0
    };
    Some((position(weight), false))
}

/// Parse the text lines of a "Portföy Dağılım Raporu" PDF (see [`pdf_lines`]).
pub fn parse_report(lines: &[String]) -> Result<Report> {
    let up: Vec<String> = lines.iter().map(|l| tr_upper(l.trim())).collect();
    // A section heading carries a list label (`III-`, `3 -`); a value line does not.
    let heading = |u: &str, title: &str| strip_label(u) != u.trim_start() && strip_label(u).starts_with(title);
    let titled = up.iter().position(|l| heading(l, "FON PORTFÖY DEĞERİ"));
    // Some layouts lose that title: the table then begins at the first stock heading.
    let untitled = up.iter().position(|l| l.starts_with("A.PAY") || l.trim() == "HİSSE SENETLERİ");
    let start = [titled, untitled]
        .into_iter()
        .flatten()
        .min()
        .ok_or_else(|| anyhow!("not a portfolio distribution report (no portfolio table)"))?;
    let end = (start + 1..up.len())
        .find(|&i| heading(&up[i], "FON TOPLAM DEĞERİ") || heading(&up[i], "TOPLAM DEĞERİ TABLOSU"))
        .unwrap_or(up.len());

    // Fund total value (section IV), which also tells the number notation.
    let total_tok = (end.min(lines.len())..lines.len()).find_map(|i| {
        strip_label(&up[i])
            .starts_with("FON TOPLAM DEĞERİ")
            // Past the list label, which reads as a number (`4- FON TOPLAM DEĞERİ TABLOSU`).
            .then(|| {
                value_part(strip_label(lines[i].trim())).split_whitespace().find(|t| is_num(t)).map(str::to_string)
            })
            .flatten()
    });
    let header = |pred: &dyn Fn(&str) -> bool| (0..start).find(|&i| pred(&up[i])).map(|i| lines[i].as_str());
    let header_total_tok = header(&|u| u.contains("TOPLAM DEĞER") && !u.contains("TABLOSU"))
        .and_then(|l| value_part(l).split_whitespace().rev().find(|t| is_num(t)).map(str::to_string));
    let comma_decimal = total_tok.as_deref().or(header_total_tok.as_deref()).is_some_and(decimal_comma);
    let net_assets = total_tok
        .as_deref()
        .or(header_total_tok.as_deref())
        .and_then(|t| parse_num(t, comma_decimal))
        .filter(|v| *v > 0.0);

    let num_in = |pred: &dyn Fn(&str) -> bool| header(pred).and_then(|l| last_num(l, comma_decimal));
    let mut report = Report {
        net_assets,
        units: num_in(&|u| u.contains("PAYI SAYISI") || u.contains("PAY SAYISI")),
        unit_price: num_in(&|u| {
            (u.contains("AY SONU") || u.contains("DÖNEM SONU")) && (u.contains("FİYAT") || u.contains("KATILMA"))
        }),
        monthly: num_in(&|u| u.contains("AYLIK") && u.contains("ARTIŞ")),
        ytd: num_in(&|u| u.contains("YILBAŞINA") || u.contains("YIL BAŞINA")),
        yearly: num_in(&|u| u.contains("YILLIK") && u.contains("ARTIŞ")),
        ..Default::default()
    };

    // Average monthly mix: the lines under the "AYLIK ORTALAMA PORTFÖYDEKİ …" heading.
    if let Some(h) = (0..start).find(|&i| up[i].contains("ORTALAMA PORTFÖYDEKİ")) {
        for i in h + 1..start {
            let line = lines[i].trim();
            let u = &up[i];
            // Top-level heading of the next block (`G-)`, `G.`, `G -`).
            let mut cs = u.chars();
            if cs.next().is_some_and(|c| c.is_alphabetic()) && cs.as_str().trim_start().starts_with(['-', '.', ')']) {
                let c = line.chars().next().unwrap_or(' ');
                if c.is_uppercase() {
                    break;
                }
            }
            let Some(w) = last_num(line, comma_decimal).filter(|w| *w > 0.0) else { continue };
            let label = value_part_label(line);
            if !label.is_empty() {
                report.mix.push(Alloc { label, weight: w });
            }
        }
        report.mix.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    }

    // Stock rows of the portfolio table.
    let mut equity = false;
    let mut by_ticker: HashMap<String, Position> = HashMap::new();
    // Month-end split: the main group totals (`GRUP TOPLAMI`, indented ones are subgroups).
    let mut group = String::from("Other");
    let mut totals: Vec<Alloc> = Vec::new();
    // `A.PAY` can be printed over the ticker cell of the first stock row; the ISIN names it.
    let isin_ticker: HashMap<&str, &str> = (start..end)
        .filter_map(|i| {
            let mut toks = lines[i].split_whitespace();
            let t = toks.next().filter(|t| t.ends_with(".E") && is_ticker(t))?;
            Some((toks.find(|t| is_isin(t))?, t))
        })
        .collect();
    for i in start..end {
        let mut text = lines[i].trim().to_string();
        if let Some(rest) = text.strip_prefix("A.PAY").filter(|r| !r.trim().is_empty()) {
            equity = true;
            group = "Pay".into();
            let ticker = rest.split_whitespace().find(|t| is_isin(t)).and_then(|i| isin_ticker.get(i));
            text = format!("{} {}", ticker.unwrap_or(&"?"), rest.trim());
        }
        let raw = lines[i].as_str();
        if raw.starts_with("GRUP TOPLAM") || raw.starts_with("Ana Grup Toplam") || raw.starts_with("TOPLAM ") {
            let nums: Vec<f64> = raw.split_whitespace().filter_map(|t| parse_num(t, comma_decimal)).collect();
            if let (true, Some(pct)) = (nums.len() >= 2, nums.last().copied().filter(|p| (-100.0..=150.0).contains(p)))
            {
                match totals.iter_mut().find(|a| a.label == group) {
                    // A main total is the sum of the subtotals above it.
                    Some(a) => a.weight = a.weight.max(pct),
                    None => totals.push(Alloc { label: group.clone(), weight: pct }),
                }
            }
        } else if let Some((p, approx)) = stock_row(&text, comma_decimal, report.net_assets).filter(|_| equity) {
            report.approx_weights |= approx;
            let e =
                by_ticker.entry(p.ticker.clone()).or_insert(Position { ticker: p.ticker, weight: 0.0, shares: None });
            e.weight += p.weight;
            e.shares = match (e.shares, p.shares) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
        } else if is_equity_heading(&up[i]) {
            equity = true;
            group = heading_label(&up[i]);
        } else if is_other_heading(&up[i]) {
            equity = false;
            group = heading_label(&up[i]);
        }
    }
    totals.retain(|a| a.weight.abs() >= 0.05);
    let covered: f64 = totals.iter().map(|a| a.weight).sum();
    if !totals.is_empty() && (50.0..=200.0).contains(&covered) {
        totals.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        report.mix = totals;
        report.mix_kind = MixKind::MonthEnd;
    }
    report.positions = by_ticker.into_values().filter(|p| p.weight.abs() > 1e-9).collect();
    report.positions.sort_by(|a, b| b.weight.total_cmp(&a.weight).then(a.ticker.cmp(&b.ticker)));
    Ok(report)
}

/// `A) HİSSE SENETLERİ  :` → `Hisse Senetleri`.
fn heading_label(up: &str) -> String {
    tr_title(strip_label(up.trim()).trim_end_matches([':', ' ']).trim())
}

/// `a-)Hisse Senedi  :  88,52` → `Hisse Senedi`; `PAY  90.65%` → `Pay`.
fn value_part_label(line: &str) -> String {
    let label = match line.rsplit_once(':') {
        Some((l, _)) => l,
        None => {
            let cut = line.find(|c: char| c.is_ascii_digit() || c == '-').unwrap_or(line.len());
            &line[..cut]
        }
    };
    let label = label.trim();
    let label = {
        let mut cs = label.char_indices();
        match (cs.next(), label.find(")")) {
            (Some((_, c)), Some(p)) if c.is_alphabetic() && p <= 4 => &label[p + 1..],
            _ => label,
        }
    };
    let label = label.trim().trim_matches(|c: char| c == '-' || c == ':' || c == '.').trim();
    let label = if label.chars().all(|c| !c.is_lowercase()) { tr_title(label) } else { label.to_string() };
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- fetching and cache -------------------------------------------------------------------

/// The cache directory under the app's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("trfunds")
}

fn fund_path(dir: &Path, code: &str) -> PathBuf {
    let safe: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    dir.join(format!("{}.json", safe.to_ascii_uppercase()))
}

pub fn read_cached(dir: &Path, code: &str) -> Option<FundData> {
    serde_json::from_str(&std::fs::read_to_string(fund_path(dir, code)).ok()?).ok()
}

/// Every fund fetched into the cache so far.
pub fn all_cached(dir: &Path) -> Vec<FundData> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<FundData> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json") && e.file_name() != "list.json")
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a.info.code.cmp(&b.info.code));
    out
}

fn write_atomic(path: &Path, json: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[derive(Serialize, Deserialize)]
struct ListFile {
    updated: i64,
    funds: Vec<FundInfo>,
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    // A fresh connection per request: KAP closes idle ones without notice, which showed up as
    // "connection closed before message completed" on the next report download.
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(0)
            // Some report downloads die mid-body when compressed (identity works every time).
            .no_gzip()
            .build()
            .unwrap_or_default()
    })
}

async fn get_once(url: &str) -> Result<Vec<u8>> {
    let resp = client()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::REFERER, "https://www.kap.org.tr/tr")
        .header(reqwest::header::ACCEPT_LANGUAGE, "tr-TR,tr;q=0.9,en;q=0.8")
        .send()
        .await
        .with_context(|| format!("KAP request failed: {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("KAP answered {status} for {url}");
    }
    Ok(resp.bytes().await.context("reading the KAP response")?.to_vec())
}

/// Process-wide pause between KAP requests; it answers a burst with 429 or a closed connection.
async fn throttle() {
    static NEXT: tokio::sync::Mutex<Option<std::time::Instant>> = tokio::sync::Mutex::const_new(None);
    let mut next = NEXT.lock().await;
    let now = std::time::Instant::now();
    if let Some(t) = next.filter(|t| *t > now) {
        tokio::time::sleep(t - now).await;
    }
    *next = Some(std::time::Instant::now() + Duration::from_millis(800));
}

/// GET with backoff (5 s … 60 s) when KAP throttles (HTTP 429/503, or it closes the connection);
/// any other HTTP error status is final.
async fn get(url: &str) -> Result<Vec<u8>> {
    const BACKOFF: [u64; 4] = [5, 15, 30, 60];
    let mut attempt = 0;
    loop {
        throttle().await;
        match get_once(url).await {
            Err(e)
                if attempt < BACKOFF.len() && {
                    let m = format!("{e:#}");
                    !m.contains("KAP answered") || m.contains("429") || m.contains("503")
                } =>
            {
                tokio::time::sleep(Duration::from_secs(BACKOFF[attempt])).await;
                attempt += 1;
            }
            r => return r,
        }
    }
}

async fn get_text(url: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&get(url).await?).into_owned())
}

/// A JSON endpoint's body. KAP now and then answers with a type description instead of data;
/// that is retried like a dropped connection.
async fn get_json(url: &str) -> Result<String> {
    let mut attempt = 0;
    loop {
        let body = get_text(url).await?;
        if serde_json::from_str::<serde_json::Value>(&body).is_ok() {
            return Ok(body);
        }
        attempt += 1;
        if attempt > 4 {
            bail!("KAP returned no JSON for {url}");
        }
        tokio::time::sleep(Duration::from_secs(3 * attempt)).await;
    }
}

/// The fund list: the disk copy while it is under a week old, else KAP (falling back to an
/// older copy when the network fails).
pub async fn fund_list(dir: &Path) -> Result<Vec<FundInfo>> {
    let path = dir.join("list.json");
    let now = chrono::Utc::now().timestamp();
    let cached: Option<ListFile> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|l: &ListFile| !l.funds.is_empty());
    if let Some(l) = cached.as_ref().filter(|l| now - l.updated < LIST_TTL) {
        return Ok(l.funds.clone());
    }
    let fetched = async {
        let html = get_text(&format!("{BASE}/tr/YatirimFonlari/YF")).await?;
        tokio::task::spawn_blocking(move || parse_list(&html)).await.context("fund list task")?
    }
    .await;
    match fetched {
        Ok(funds) => {
            if let Ok(json) = serde_json::to_string(&ListFile { updated: now, funds: funds.clone() }) {
                write_atomic(&path, &json);
            }
            Ok(funds)
        }
        Err(e) => match cached {
            Some(l) => {
                tracing::warn!("TRF fund list: {e:#}; serving the cached copy");
                Ok(l.funds)
            }
            None => Err(e),
        },
    }
}

/// Cached fund list without touching the network.
pub fn read_fund_list(dir: &Path) -> Option<Vec<FundInfo>> {
    let l: ListFile = serde_json::from_str(&std::fs::read_to_string(dir.join("list.json")).ok()?).ok()?;
    Some(l.funds)
}

/// The first PDF attached to a disclosure.
async fn download_attachment(index: u64) -> Result<Vec<u8>> {
    let detail = get_json(&format!("{BASE}/tr/api/notification/attachment-detail/{index}")).await?;
    let obj = parse_attachment(&detail)?;
    get(&format!("{BASE}/tr/api/file/download/{obj}")).await
}

async fn fetch_snapshot(r: &ReportRef) -> Result<Snapshot> {
    let detail = get_json(&format!("{BASE}/tr/api/notification/attachment-detail/{}", r.index)).await?;
    let ids = parse_attachments(&detail)?;
    if ids.is_empty() {
        bail!("the disclosure has no PDF attachment");
    }
    let mut files = Vec::new();
    for id in ids.iter().take(4) {
        files.push(get(&format!("{BASE}/tr/api/file/download/{id}")).await?);
    }
    let report = tokio::task::spawn_blocking(move || {
        // All files' lines together; an unreadable file is skipped while another one reads.
        let mut lines = Vec::new();
        let mut err = None;
        for bytes in &files {
            let pdf = strip_wrapper(bytes);
            match std::panic::catch_unwind(|| pdf_lines(pdf)).map_err(|_| anyhow!("PDF reader crashed")) {
                Ok(Ok(l)) => lines.extend(l),
                Ok(Err(e)) | Err(e) => err = err.or(Some(e)),
            }
        }
        match err {
            Some(e) if lines.is_empty() => Err(e),
            _ => parse_report(&lines),
        }
    })
    .await
    .context("report parse task")??;
    Ok(Snapshot {
        year: r.year,
        month: r.month,
        weekly: r.weekly,
        index: r.index,
        published: r.published.clone(),
        report,
    })
}

/// Load a fund: the disk copy while fresh, else KAP (the profile, the two newest monthly reports;
/// reports already on disk are not downloaded again). A stale copy is served when the network
/// fails. `force` skips the freshness check.
pub async fn load(dir: &Path, code: &str, force: bool) -> Result<FundData> {
    let code = code.trim().to_ascii_uppercase();
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("{code:?} is not a fund code");
    }
    let now = chrono::Utc::now().timestamp();
    let cached = read_cached(dir, &code);
    if let Some(f) = cached.as_ref().filter(|f| !force && !f.stale(now)) {
        return Ok(f.clone());
    }
    match fetch_fund(dir, &code, cached.as_ref(), now).await {
        Ok(f) => {
            if let Ok(json) = serde_json::to_string(&f) {
                write_atomic(&fund_path(dir, &code), &json);
            }
            Ok(f)
        }
        Err(e) => match cached {
            Some(f) => {
                tracing::warn!("TRF {code}: {e:#}; serving the cached copy");
                Ok(f)
            }
            None => Err(e),
        },
    }
}

/// Settlement of the fund's redemptions, from its newest prospectus (a PDF on KAP).
async fn fetch_valor(refs: &[u64]) -> Option<String> {
    for index in refs.iter().take(2) {
        let found = async {
            let bytes = download_attachment(*index).await?;
            tokio::task::spawn_blocking(move || {
                let lines = std::panic::catch_unwind(|| pdf_lines(strip_wrapper(&bytes)))
                    .map_err(|_| anyhow!("PDF reader crashed"))??;
                Ok::<_, anyhow::Error>(parse_valor(&lines.join(" ")))
            })
            .await
            .context("prospectus task")?
        }
        .await;
        match found {
            Ok(Some(v)) => return Some(v),
            Ok(None) => {}
            Err(e) => tracing::debug!("TRF prospectus {index}: {e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    None
}

async fn fetch_fund(dir: &Path, code: &str, cached: Option<&FundData>, now: i64) -> Result<FundData> {
    let info = match cached {
        Some(c) => c.info.clone(),
        None => fund_list(dir)
            .await?
            .into_iter()
            .find(|f| f.code == code)
            .ok_or_else(|| anyhow!("{code} is not in KAP's investment fund list"))?,
    };
    let list =
        get_json(&format!("{BASE}/tr/api/disclosure/filter/FILTERYFBF/{}/ALL/{LOOKBACK_DAYS}", info.oid)).await?;
    let refs = parse_report_refs(&list)?;
    let (profile, profile_updated) = match cached
        .filter(|c| now - c.profile_updated < PROFILE_TTL && c.profile_updated > 0)
    {
        Some(c) => (c.profile.clone(), c.profile_updated),
        None => {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let html = get_text(&format!("{BASE}/tr/fon-bilgileri/genel/{}", info.page_slug())).await?;
            let mut p = tokio::task::spawn_blocking(move || parse_profile(&html)).await.context("profile task")??;
            p.valor = match fetch_valor(&parse_prospectus_refs(&list)).await {
                Some(v) => Some(v),
                None => cached.and_then(|c| c.profile.valor.clone()),
            };
            (p, now)
        }
    };
    let mut reports: Vec<Snapshot> = Vec::new();
    let mut note = None;
    for r in refs.iter().take(2) {
        if let Some(s) = cached.and_then(|c| c.reports.iter().find(|s| s.index == r.index)) {
            reports.push(s.clone());
            continue;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        match fetch_snapshot(r).await {
            Ok(s) => reports.push(s),
            Err(e) => {
                tracing::warn!("TRF {code} report {}: {e:#}", r.index);
                note = Some(format!("{}-{:02} report could not be read: {e:#}", r.year, r.month));
            }
        }
    }
    reports.sort_by_key(|s| std::cmp::Reverse(s.key()));
    if refs.is_empty() {
        note = Some(format!("KAP lists no portfolio distribution report for {code} in the last {LOOKBACK_DAYS} days."));
    } else if reports.iter().all(|s| s.report.positions.is_empty()) && note.is_none() {
        note = Some(
            "The report layout of this fund's manager is not recognised: no stock positions could be read.".into(),
        );
    }
    Ok(FundData { info, profile, reports, note, updated: now, profile_updated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    /// Variant 1 (İş, TEB, Ak, İstanbul…): decimal point or comma, one row per lot.
    const STANDARD: &str = "\
TTE-İŞ PORTFÖY BIST TEKNOLOJİ ENDEKSİ HİSSE SENEDİ FONU
I-FONU TANITICI BİLGİLER
D-)Toplam Değer/Net Varlık Değeri  :  2,256,779,760.50
E-) Katılma Payı Sayısı  :  2,023,067,154.000
II-FONUN PERFORMANSINA İLİŞKİN BİLGİLER
A-)Ay Sonu Pay Fiyatı (TL)  :  1.115524
C-)Aylık Pay Fiyatı Artış Oranı (TL)  :  -33.81
D-)Yılbaşına Göre Fiyat Artış Oranı  :  11.03
E-)Yıllık Pay Fiyatı Artış Oranı  :  6.33
F-)Aylık Ortalama Portföydeki Menkul Kıymetler Yüzdesi
a-)Hisse Senedi  :  92.76
b-)Hazine Bonosu  :  0.00
m-)Diğer  :  1.55
G-)Aylık Ortalama Portföy Devir Hızı (TL)
a-)Hisse Senedi  :  0.24
III-FON PORTFÖY DEĞERİ TABLOSU
MENKUL KIYMET  İHRAÇCI KURUM  ISIN KODU  NOMİNAL DEĞER  BİRİM ALIŞ FİYATI  TOPLAM DEĞER  GRUP (%)
HİSSE SENETLERİ
Hisse Türk
ASELSAN ELEKTRONİK
ASELS  TL  TRAASELS91H2  1,946,160.80  264.707206  25/09/26  8010051113300207990060  336.500000  654,883,107.90  31.13  29.77  29.02
ASELS  TL  TRAASELS91H2  -625,000.00  264.707206  30/09/26  8010051113531810470060  336.500000  -210,312,500.00  -10.00  -9.56  -9.32
ALTINAY SAVUNMA
ALTNY  TL  TREALTA00014  5,885,177.63  18.308172  25/09/26  8010051113299058290060  12.840000  75,565,680.72  3.59  3.44  3.35
ARENA  TL  TRAARENA91U1  822,446.50  33.340436  25/09/26  8010051113299203410060  15.0  1,000.00  0.00  0.00  0.04
SMART  TL  TRESMRT00018  481,517.00  32.435094  25/09/26  80100511225012035273200060  15.400000  7,415,361.80  0.35  0.34  0.33
GRUP TOPLAMI  115,336,598.04  2,103,986,559.00
T.REPO
TRT110429T18  TL  HAZİNE  01/10/26  TRT110429T18  36.94  72,000,000.00  36.940000  30/09/26  49,797,470.00  820381660259451528  72,072,867.95  36.940000  72,0
DİĞER
Y.Fonu Türk
IOO  TL  TRYISPO00969  522,267.00  3.890757  04/08/26  4.792443  2,500,000.00  0.11  0.11  0.11
IV-FON TOPLAM DEĞERİ TABLOSU
FON PORTFÖY DEĞERİ  2,199,694,018.44  97.47 %
FON TOPLAM DEĞERİ  2,256,779,760.50  100,00 %
";

    #[test]
    fn standard_layout() {
        let r = parse_report(&lines(STANDARD)).unwrap();
        assert_eq!(r.net_assets, Some(2_256_779_760.5));
        assert_eq!(r.units, Some(2_023_067_154.0));
        assert_eq!(r.unit_price, Some(1.115524));
        assert_eq!((r.monthly, r.ytd, r.yearly), (Some(-33.81), Some(11.03), Some(6.33)));
        assert_eq!(
            r.mix,
            vec![Alloc { label: "Hisse Senedi".into(), weight: 92.76 }, Alloc { label: "Diğer".into(), weight: 1.55 }]
        );
        // Lots of one ticker are netted; fund shares and repo rows are not stocks.
        let t: Vec<(&str, f64)> = r.positions.iter().map(|p| (p.ticker.as_str(), p.weight)).collect();
        assert_eq!(t.len(), 4, "{t:?}");
        assert_eq!(r.weight_of("ALTNY").unwrap().weight, 3.35, "a company name starting with ALTIN is not a heading");
        let asels = r.weight_of("ASELS").unwrap();
        assert!((asels.weight - 19.70).abs() < 1e-9);
        assert_eq!(asels.shares, Some(1_946_160.8 - 625_000.0));
        assert_eq!(r.weight_of("SMART").unwrap().weight, 0.33);
        assert!(r.weight_of("IOO").is_none() && r.weight_of("TRT110429T18").is_none());
        assert_eq!(r.top_and_other(2).0.len(), 2);
    }

    /// Variant 2 (Garanti): `.E` tickers, percent signs, dotted dates.
    const GARANTI: &str = "\
YATIRIM FONLARI PORTFÖY DAĞILIM RAPORU
D. TOPLAM DEĞER: (TL)  2,660,056,681.11
E. AYLIK ORTALAMA PORTFÖYDEKİ
MENKUL KIYMETLER YÜZDESİ
TÜREV TEMİNAT  2.37%
TERS REPO  6.82%
PAY  90.65%
F. AYLIK ORTALAMA TEDAVÜL ORANI:  2.53%
III-FON PORTFÖY DEĞERİ TABLOSU
A.PAY
AEFES.E  ANADOLU EFES  TRAAEFES91A9  0.00%  0  44,393.00  20.26 03.02.2026  0.00  18.10  803,513.30  0.03%  0.03%
AEFES.E  ANADOLU EFES  TRAAEFES91A9  0.00%  0  300,000.00  20.21 05.06.2026  0.00  18.10  5,430,000.00  0.22%  0.20%
YKBNK.E  YAPI KREDİ BANKASI  TRAYKBNK91N6  0.00%  0  49,119.00  34.96 08.06.2026  0.00%  0  0.00  34.04  1,672,010.76 0.07%  0.06%
Ana Grup Toplamı  64,818,709.36  2,462,156,527.96 100.04%  90.65%
B.BORÇLANMA ARAÇLARI
B.1.ÖZEL SEKTÖR BORÇLANMA ARAÇLARI
XYZAB  TL  TRBXYZ  1.00  2.00  3.00  4.00
IV- FON TOPLAM DEĞERİ TABLOSU
FON TOPLAM DEĞERİ:  2,660,056,681.11
";

    #[test]
    fn garanti_layout() {
        let r = parse_report(&lines(GARANTI)).unwrap();
        assert_eq!(r.net_assets, Some(2_660_056_681.11));
        let mix: Vec<&str> = r.mix.iter().map(|a| a.label.as_str()).collect();
        // The report prints month-end group totals, which win over the monthly average.
        assert_eq!(mix, ["Pay"]);
        assert_eq!(r.mix_kind, MixKind::MonthEnd);
        assert_eq!(r.mix[0].weight, 90.65);
        assert_eq!(r.positions.len(), 2, "{:?}", r.positions);
        assert!((r.weight_of("AEFES").unwrap().weight - 0.23).abs() < 1e-9);
        assert_eq!(r.weight_of("AEFES").unwrap().shares, Some(344_393.0));
        assert_eq!(r.weight_of("YKBNK.E").unwrap().weight, 0.06);
        assert!(r.weight_of("XYZAB").is_none(), "debt section is not equity");
    }

    /// Variant 3 (Ziraat): decimal comma, indexed rows, TL value column.
    const ZIRAAT: &str = "\
E - FON TOPLAM DEĞERİ  :  602.593.726,110
2 - FONUN PERFORMANSINA İLİŞKİN BİLGİLER
A - AY SONU PAY FİYATI  :  2,340501
F - AYLIK ORTALAMA PORTFÖYDEKİ MENKUL KIYMETLER YÜZDESİ
İMKB'de İşlem Gören Hisse Senetleri  :  94,568958
Ters Repo  :  4,663027
G - AYLIK ORTALAMA TEDAVÜL ORANI  :  0,264875
3 - FON PORTFÖY DEĞERİ TABLOSU
A - HİSSE SENETLERİ  İhraçcı  Nominal  Rayiç Değer  Oran (%)  Birim Alış Fiyatı
1. Banka İştirak ve Hissedarları
1 AKBNK.E  AKBANK TAS  200.000,000  13.400.000,00  2,176842  66,350
2 AKSEN.E  AKSA ENERJİ ÜRETİM A  150.000,000  10.935.000,00  1,776401  80,104
B - BORÇLANMA ARAÇLARI
3 XXXXX  FOO  1,00  2,00  3,00  4,00
4 - TOPLAM DEĞERİ TABLOSU
FON TOPLAM DEĞERİ  602.593.726,11  100,000000
";

    #[test]
    fn ziraat_layout() {
        let r = parse_report(&lines(ZIRAAT)).unwrap();
        assert_eq!(r.net_assets, Some(602_593_726.11));
        assert_eq!(r.unit_price, Some(2.340501));
        assert_eq!(r.mix.len(), 2);
        assert_eq!(r.mix[0].label, "İMKB'de İşlem Gören Hisse Senetleri");
        assert_eq!(r.positions.len(), 2, "{:?}", r.positions);
        let w = r.weight_of("AKBNK").unwrap();
        assert!((w.weight - 13_400_000.0 / 602_593_726.11 * 100.0).abs() < 1e-9);
        assert_eq!(w.shares, Some(200_000.0));
    }

    /// Variant 4 (Yapı Kredi): `TICKER ISIN name nominal value %`, split headings.
    const YAPI_KREDI: &str = "\
Ç. TOPLAM DEĞER/NET  :  2,429,017,049.68
D. KATILMA PAYI SAYISI  :  99,561,594.00
A. DÖNEM SONU KATILMA  :  24.397129
III- FON PORTFÖY DEĞERİ  bold
:
TABLOSU
Isin Kodu  Vade  İhraçcı  Nominal Değeri  Rayiç Değeri  %
A) HİSSE SENETLERİ  :
AKBNK  TRAAKBNK91N6  Akbank  1,634,550.35  109,514,873.32  4.47
ALARK  TRAALARK91Q0  Alarko Holding  0.62  61.16  0.00
TOPLAM  92,353,302.90  92,353,302.90  3.77
AF) SERTIFIKALAR  :
XXXXX  TRAXXXXX91Q0  Foo  1.00  2.00  3.00
IV- FON TOPLAM DEĞERİ  bold
:
TABLOSU
FON TOPLAM DEĞERİ  2,429,017,049.68  100.00
";

    #[test]
    fn yapi_kredi_layout() {
        let r = parse_report(&lines(YAPI_KREDI)).unwrap();
        assert_eq!(r.net_assets, Some(2_429_017_049.68));
        assert_eq!(r.units, Some(99_561_594.0));
        assert_eq!(r.unit_price, Some(24.397129));
        let t: Vec<(&str, f64)> = r.positions.iter().map(|p| (p.ticker.as_str(), p.weight)).collect();
        assert_eq!(t, [("AKBNK", 4.47)], "{t:?}");
    }

    #[test]
    fn stock_heading_glued_to_first_row() {
        let r = parse_report(&lines(
            "D. TOPLAM DEĞER: (TL)  1,000.00\nA.PAY  ANADOLU EFES  TRAAEFES91A9  0.00%  0  44.00  20.26 03.02.2026  0.00%  0  0.00  17.68  784.87 5.03%  4.03%\nAEFES.E  ANADOLU EFES  TRAAEFES91A9  0.00%  0  44.00  20.26 04.02.2026  0.00%  0  0.00  17.68  784.87 1.03%  1.00%\nB.BORÇLANMA ARAÇLARI\nIV- FON TOPLAM DEĞERİ TABLOSU\nFON TOPLAM DEĞERİ:  1,000.00",
        ))
        .unwrap();
        assert_eq!(r.positions.len(), 1);
        assert!((r.positions[0].weight - 5.03).abs() < 1e-9, "{:?}", r.positions);
    }

    #[test]
    fn redemption_settlement_from_a_prospectus() {
        let ti2 = "6.3. Katılma Payı Satım Esasları … 6.4. Satım Bedellerinin Ödenme Esasları Katılma payı bedelleri; iade talimatının, BIST Pay Piyasası’nın açık olduğu günlerde saat 13.30’a kadar verilmesi halinde, talimatın verilmesini takip eden ikinci işlem gününde, iade talimatının BIST Pay Piyasası’nın açık olduğu günlerde saat 13.30’dan sonra verilmesi halinde ise, talimatın verilmesini takip eden üçüncü işlem gününde yatırımcılara ödenir. 6.5. Giriş Çıkış Komisyonları: iki işlem günü";
        assert_eq!(parse_valor(ti2).as_deref(), Some("Sell T+2 (T+3 if ordered after 13:30)"));
        let tzt = "6.4.Satım Bedellerinin Ödenme Esasları Katılma payı bedelleri; iade talimatının saat 13:30’a kadar verilmesi halinde, talimatın verilmesini takip eden birinci işlem gününde, saat 13:30’dan sonra verilmesi halinde ise, talimatın verilmesini takip eden ikinci işlem gününde yatırımcılara ödenir.";
        assert_eq!(parse_valor(tzt).as_deref(), Some("Sell T+1 (T+2 if ordered after 13:30)"));
        assert_eq!(parse_valor("no such section"), None);
    }

    /// Variant 5 (ICBC/TEB): the PDF lost its value and weight columns.
    #[test]
    fn rows_without_market_values_are_estimated() {
        let r = parse_report(&lines(
            "D-)Toplam Değer/Net Varlık Değeri  :\n17,055,596.28\nIII-FON PORTFÖY DEĞERİ TABLOSU\nHİSSE SENETLERİ\nHisse Türk\nAKBANK   TRAAKBNK91  8010051\nAKBNK  TL  9,600.00  72.773797 24/09/26\nASELS  TL  3,430.00 368.133714 29/09/26\nGRUP TOPLAMI  291,576.42\nIV-FON TOPLAM DEĞERİ TABLOSU\nFON TOPLAM DEĞERİ  17,055,596.28  100,00 %",
        ))
        .unwrap();
        assert!(r.approx_weights);
        assert_eq!(r.mix_kind, MixKind::Average);
        let a = r.weight_of("AKBNK").unwrap();
        assert!((a.weight - 9600.0 * 72.773797 / 17_055_596.28 * 100.0).abs() < 1e-9);
        assert_eq!(a.shares, Some(9600.0));
        assert_eq!(r.positions.len(), 2);
    }

    #[test]
    fn not_a_report() {
        assert!(parse_report(&lines("hello\nworld")).is_err());
    }

    #[test]
    fn numbers_in_both_notations() {
        assert_eq!(parse_num("1,946,160.80", false), Some(1_946_160.8));
        assert_eq!(parse_num("1.946.160,80", true), Some(1_946_160.8));
        assert_eq!(parse_num("-0,16", false), Some(-0.16));
        assert_eq!(parse_num("12,02", false), Some(12.02));
        assert_eq!(parse_num("0.07%", false), Some(0.07));
        assert_eq!(parse_num("25/09/26", false), None);
        assert_eq!(parse_num("03.02.2026", false), None);
        assert!(decimal_comma("7.170.889.433,26") && !decimal_comma("2,256,779,760.50"));
    }

    #[test]
    fn buys_and_sells_between_months() {
        let p = |t: &str, w: f64, s: Option<f64>| Position { ticker: t.into(), weight: w, shares: s };
        let prev = Report {
            positions: vec![
                p("AAA", 5.0, Some(100.0)),
                p("BBB", 4.0, Some(100.0)),
                p("CCC", 3.0, Some(100.0)),
                p("DDD", 2.5, Some(50.0)),
                p("EEE", 0.01, None),
            ],
            ..Default::default()
        };
        let cur = Report {
            positions: vec![
                p("AAA", 6.5, Some(130.0)),
                p("BBB", 2.0, Some(40.0)),
                p("CCC", 3.5, Some(100.5)),
                p("FFF", 1.0, Some(10.0)),
                p("EEE", 0.02, None),
            ],
            ..Default::default()
        };
        let c = changes(&prev, &cur);
        let kinds: Vec<(&str, Move)> = c.iter().map(|x| (x.ticker.as_str(), x.kind)).collect();
        // CCC: price-driven weight move only (shares +0.5%), EEE: tiny on both sides.
        assert_eq!(kinds, [("DDD", Move::Exited), ("BBB", Move::Reduced), ("AAA", Move::Added), ("FFF", Move::New)]);
        assert!((c[2].delta_shares_pct().unwrap() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn reverse_search_uses_the_latest_report() {
        let snap = |m: u32, w: f64| Snapshot {
            year: 2026,
            month: m,
            weekly: false,
            index: 1,
            published: String::new(),
            report: Report {
                positions: vec![Position { ticker: "THYAO".into(), weight: w, shares: None }],
                ..Default::default()
            },
        };
        let fund = |code: &str, reports: Vec<Snapshot>| {
            Arc::new(FundData {
                info: FundInfo {
                    code: code.into(),
                    name: code.into(),
                    oid: String::new(),
                    manager: String::new(),
                    kind: String::new(),
                    slug: String::new(),
                },
                profile: Profile::default(),
                reports,
                note: None,
                updated: 0,
                profile_updated: 0,
            })
        };
        let funds =
            vec![fund("AAA", vec![snap(9, 3.0), snap(8, 2.0)]), fund("BBB", vec![snap(9, 7.0)]), fund("CCC", vec![])];
        let hits = reverse_search(&funds, "thyao.is");
        assert_eq!(hits.iter().map(|h| (h.code.as_str(), h.weight)).collect::<Vec<_>>(), [("BBB", 7.0), ("AAA", 3.0)]);
        assert_eq!(hits[1].previous, Some(2.0));
        assert_eq!(hits[0].period, "Sep 2026");
        assert!(reverse_search(&funds, "XXXXX").is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(clean_ticker("THYAO IS Equity"), "THYAO");
        assert_eq!(clean_ticker("akbnk.e"), "AKBNK");
        assert_eq!(tr_upper("Hisse Senedi İşlem ığ"), "HİSSE SENEDİ İŞLEM IĞ");
        assert_eq!(tr_title("HİSSE SENEDİ"), "Hisse Senedi");
        assert_eq!(slugify("TTE İŞ PORTFÖY BIST (TL) FONU"), "tte-is-portfoy-bist-tl-fonu");
        assert_eq!(
            fund_kind(
                "A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş. BORÇLANMA ARAÇLARI ŞEMSİYE FONU",
                "A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş."
            ),
            "Borçlanma Araçları"
        );
        assert_eq!(strip_wrapper(b"\xac\xed\x00\x05ur\x00%PDF-1.7 rest"), b"%PDF-1.7 rest");
    }

    #[test]
    fn fund_list_page() {
        // The page embeds the list in a JS string, so quotes arrive escaped.
        let html = r#"<script>self.__next_f.push([1,"{\"A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş. BORÇLANMA ARAÇLARI ŞEMSİYE FONU\":[{\"umbFundName\":\"A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş. BORÇLANMA ARAÇLARI ŞEMSİYE FONU\",\"umbMemberOid\":\"8aca\",\"umbMemberTitle\":\"A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş.\",\"fundOid\":\"4028328c998ee9d50199c2d60a6f6fef\",\"fundId\":\"5758\",\"fundCode\":\"BON\",\"fundName\":\" A1 CAPİTAL PORTFÖY BORÇLANMA ARAÇLARI FONU\",\"fundType\":\"YF\",\"fundMemberOid\":\"8aca\",\"fundMemberTitle\":\"A1 CAPİTAL PORTFÖY YÖNETİMİ A.Ş.\",\"fundCompanyCode\":\"6069\",\"nonInactiveCount\":2},{\"umbFundName\":\"İŞ PORTFÖY YÖNETİMİ A.Ş. HİSSE SENEDİ ŞEMSİYE FONU\",\"umbMemberOid\":\"x\",\"umbMemberTitle\":\"İŞ PORTFÖY YÖNETİMİ A.Ş.\",\"fundOid\":\"33E5\",\"fundId\":\"1\",\"fundCode\":\"TTE\",\"fundName\":\"İŞ PORTFÖY BIST TEKNOLOJİ FONU\",\"fundType\":\"YF\",\"fundMemberTitle\":\"İŞ PORTFÖY YÖNETİMİ A.Ş.\",\"nonInactiveCount\":2}]}"])</script><a href="/tr/fon-bilgileri/ozet/bon-a1-capital-portfoy-borclanma-araclari-fonu">BON</a>"#;
        let funds = parse_list(html).unwrap();
        assert_eq!(funds.len(), 2);
        assert_eq!(funds[0].code, "BON");
        assert_eq!(funds[0].name, "A1 CAPİTAL PORTFÖY BORÇLANMA ARAÇLARI FONU");
        assert_eq!(funds[0].oid, "4028328c998ee9d50199c2d60a6f6fef");
        assert_eq!(funds[0].kind, "Borçlanma Araçları");
        assert_eq!(funds[1].kind, "Hisse Senedi");
        assert_eq!(funds[0].page_slug(), "bon-a1-capital-portfoy-borclanma-araclari-fonu");
        assert_eq!(funds[1].page_slug(), "tte-is-portfoy-bist-teknoloji-fonu");
        assert!(parse_list("<html></html>").is_err());
    }

    #[test]
    fn general_information_form() {
        let html = r#"{\"itemKey\":\"kpy81_acc1_ISIN\",\"value\":\"TRMTTEWWWWW0\",\"x\":1},{\"itemKey\":\"kpy81_acc1_fon_sem_tur\",\"value\":\"HİSSE SENEDİ ŞEMSİYE FONU\"},{\"itemKey\":\"kpy81_acc2_fon_ucret_kom_info\",\"value\":[{\"uygulananYonetimUcretiOranYillikYuzde\":\"1,90\",\"girisKomisyonu\":\"0\",\"cikisKomisyonu\":\"0,5\",\"performansUcretiOrani\":\"0\"}],\"disclosureIndex\":null},{\"itemKey\":\"kpy81_acc3_fon_mudur\",\"value\":[{\"adSoyad\":\"Engin Döğenci \"}]},{\"itemKey\":\"kpy81_acc1_halka_arz2\",\"value\":[{\"halkaArzTarihleri\":\"20010207000000\"}]}"#;
        let p = parse_profile(html).unwrap();
        assert_eq!(p.isin.as_deref(), Some("TRMTTEWWWWW0"));
        assert_eq!((p.mgmt_fee, p.entry_fee, p.exit_fee, p.perf_fee), (Some(1.9), Some(0.0), Some(0.5), Some(0.0)));
        assert_eq!(p.fund_manager.as_deref(), Some("Engin Döğenci"));
        assert_eq!(p.inception, NaiveDate::from_ymd_opt(2001, 2, 7));
        assert_eq!(p.umbrella.as_deref(), Some("Hisse Senedi Şemsiye Fonu"));
        assert_eq!(p.valor, None);
        assert!(parse_profile("<html></html>").is_err());
    }

    #[test]
    fn report_list_and_attachment() {
        let json = r#"[
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":30,"year":2026,"donem":9,"publishDate":"05.10.2026 13:00:53"}},
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":31,"year":2026,"donem":9,"publishDate":"06.10.2026 09:00:00"}},
          {"disclosureBasic":{"title":"İzahname","disclosureIndex":99,"year":2026,"donem":0}},
          {"disclosureBasic":{"title":"İzahname (Değişiklik)","disclosureIndex":120,"year":2026,"donem":0}},
          {"disclosureBasic":{"title":"İzahname","disclosureIndex":50,"year":2025,"donem":0}},
          {"disclosureBasic":{"title":"Fon Gider Bilgileri","disclosureIndex":98,"year":2026,"donem":9}},
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":20,"year":2026,"donem":8,"publishDate":"03.09.2026 12:55:08"}},
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":10,"year":2026,"donem":7,"publishDate":"03.08.2026 10:00:00"}}]"#;
        let refs = parse_report_refs(json).unwrap();
        assert_eq!(refs.iter().map(|r| (r.month, r.index)).collect::<Vec<_>>(), [(9, 31), (8, 20), (7, 10)]);
        assert!(refs.iter().all(|r| !r.weekly));
        assert_eq!(parse_prospectus_refs(json), [99, 50, 120]);
        // A serbest fund filing every week: KAP's period is the week number.
        let weekly = r#"[
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":2,"year":2026,"donem":5,"publishDate":"10.02.2026 16:00:00"}},
          {"disclosureBasic":{"title":"Portföy Dağılım Raporu","disclosureIndex":1,"year":2026,"donem":4,"publishDate":"03.02.2026 16:00:00"}}]"#;
        let refs = parse_report_refs(weekly).unwrap();
        assert!(refs.iter().all(|r| r.weekly));
        let s = Snapshot {
            year: 2026,
            month: 40,
            weekly: false,
            index: 1,
            published: String::new(),
            report: Report::default(),
        };
        assert_eq!(s.label(), "Week 40 2026");
        assert_eq!(Snapshot { month: 9, ..s.clone() }.label(), "Sep 2026");
        assert_eq!(Snapshot { month: 5, weekly: true, ..s }.label(), "Week 5 2026");
        let att =
            r#"[{"disclosure":{},"attachments":[{"objId":"abc123","fileName":"TTE.pdf","fileExtension":"pdf"}]}]"#;
        assert_eq!(parse_attachment(att).unwrap(), "abc123");
        let two = r#"[{"attachments":[{"objId":"a","fileExtension":"pdf"},{"objId":"x","fileExtension":"xlsx"},{"objId":"b","fileExtension":"PDF"}]}]"#;
        assert_eq!(parse_attachments(two).unwrap(), ["a", "b"]);
        assert_eq!(fix_turkish("III-FON PORTFÖY DEĞERĠ, KuruluĢ, A.ġ".into()), "III-FON PORTFÖY DEĞERİ, Kuruluş, A.Ş");
        assert!(parse_attachment("[]").is_err());
    }
}
