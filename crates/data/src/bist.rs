//! Borsa Istanbul company descriptions (DES) and financial statements (FA).
//!
//! Sources (free, no key, plain GET with a browser User-Agent):
//! - İş Yatırım `Common/Data.aspx/MaliTablo`: full statements per quarter-end (TRY or USD),
//!   up to four periods per call. Industrial companies use the `XI_29` template, banks
//!   `UFRS` (consolidated) or `UFRS_K` (solo). Income-statement and cash-flow values are
//!   cumulative year-to-date, so quarters are de-cumulated here.
//! - İş Yatırım company card (HTML): trade name, business, address, paid-in capital, free
//!   float, foreign ownership and the shareholder structure.
//!
//! KAP's company pages load their data client-side, so they are not parsed. Requests are
//! throttled to about one per second process-wide, cached on disk, and a 403/429 backs the
//! host off for ten minutes instead of retrying (no attempt is made to get around limits).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use tokio::sync::Mutex;
use tr_core::AssetClass;

use crate::edgar::{Financials, Metric, Period};

const MALITABLO_URL: &str = "https://www.isyatirim.com.tr/_layouts/15/IsYatirim.Website/Common/Data.aspx/MaliTablo";
const CARD_URL: &str = "https://www.isyatirim.com.tr/tr-tr/analiz/hisse/Sayfalar/sirket-karti.aspx";
const STATEMENT_TTL: Duration = Duration::from_secs(12 * 3600);
const CARD_TTL: Duration = Duration::from_secs(3 * 3600);
/// Minimum spacing between requests to İş Yatırım.
const MIN_GAP: Duration = Duration::from_millis(1100);
/// How long to stay away after a 403/429.
const BACKOFF: Duration = Duration::from_secs(600);
/// Quarter-ends requested (six years: six calls of four periods).
const QUARTERS: usize = 24;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// Statement templates to try in order: industrials, then banks (consolidated, solo).
const GROUPS: [&str; 3] = ["XI_29", "UFRS", "UFRS_K"];

/// Reporting currency of the statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Currency {
    #[default]
    Try,
    Usd,
}

impl Currency {
    pub fn code(self) -> &'static str {
        match self {
            Currency::Try => "TRY",
            Currency::Usd => "USD",
        }
    }
}

/// The Borsa Istanbul code for a market key (`THYAO.IS` → `THYAO`), or `None` when the
/// security is not a TRY-quoted BIST equity.
pub fn bist_ticker(symbol: &str) -> Option<String> {
    let sec = crate::security(symbol);
    if !matches!(sec.class, AssetClass::Equity | AssetClass::Etf) || crate::native_currency(symbol) != "TRY" {
        return None;
    }
    let y = crate::yahoo::yahoo_symbol(&sec);
    let code = y.strip_suffix(".IS")?;
    (!code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric())).then(|| code.to_ascii_uppercase())
}

// ---- statements ---------------------------------------------------------------------------

/// One quarter-end of raw `MaliTablo` values by item code (cumulative year-to-date for the
/// income statement and cash flow).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub year: i32,
    pub month: u32,
    pub vals: HashMap<String, f64>,
}

impl Snapshot {
    fn end(&self) -> Option<NaiveDate> {
        let (y, m) = if self.month == 12 { (self.year + 1, 1) } else { (self.year, self.month + 1) };
        NaiveDate::from_ymd_opt(y, m, 1)?.pred_opt()
    }

    fn get(&self, codes: &[&str]) -> Option<f64> {
        codes.iter().find_map(|c| self.vals.get(*c).copied())
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    ok: bool,
    #[serde(default, rename = "errorDescription")]
    error: Option<String>,
    #[serde(default)]
    value: Option<Vec<RawRow>>,
}

#[derive(Deserialize)]
struct RawRow {
    #[serde(rename = "itemCode")]
    code: String,
    value1: Option<String>,
    value2: Option<String>,
    value3: Option<String>,
    value4: Option<String>,
}

/// Parse one `MaliTablo` reply into a snapshot per requested period. A period the company
/// has not reported (all values null) is omitted; an empty reply (wrong template) yields no
/// snapshots.
pub fn parse_malitablo(json: &str, periods: &[(i32, u32)]) -> Result<Vec<Snapshot>> {
    let r: Reply = serde_json::from_str(json).context("MaliTablo JSON")?;
    if !r.ok {
        bail!("İş Yatırım: {}", r.error.unwrap_or_else(|| "request failed".into()));
    }
    let rows = r.value.unwrap_or_default();
    let mut out = Vec::new();
    for (i, &(year, month)) in periods.iter().enumerate().take(4) {
        let mut vals = HashMap::new();
        for row in &rows {
            let raw = match i {
                0 => &row.value1,
                1 => &row.value2,
                2 => &row.value3,
                _ => &row.value4,
            };
            if let Some(v) = raw.as_deref().and_then(|s| s.trim().parse::<f64>().ok()) {
                vals.insert(row.code.clone(), v);
            }
        }
        if !vals.is_empty() {
            out.push(Snapshot { year, month, vals });
        }
    }
    Ok(out)
}

/// Statement lines read from a snapshot, by template.
struct Lines {
    flows: BTreeMap<Metric, f64>,
    instants: BTreeMap<Metric, f64>,
    /// Paid-in capital in TRY/USD; BIST shares have a nominal value of 1 TL, so this is
    /// the share count scaled by the exchange rate.
    capital: Option<f64>,
}

fn lines(s: &Snapshot, bank: bool) -> Lines {
    use Metric::*;
    let mut flows = BTreeMap::new();
    let mut instants = BTreeMap::new();
    let put = |map: &mut BTreeMap<Metric, f64>, m: Metric, v: Option<f64>| {
        if let Some(v) = v {
            map.insert(m, v);
        }
    };
    let sum = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    let assets = s.get(&["1BL", "1Z"]);
    let total_equity = s.get(&["2N"]).filter(|v| *v != 0.0);
    if bank {
        // Banks: income = total operating income; "gross profit" = net interest income.
        put(&mut flows, Revenue, s.get(&["3CE"]));
        put(&mut flows, CostOfRevenue, s.get(&["3B"]));
        put(&mut flows, GrossProfit, s.get(&["3C"]));
        put(&mut flows, OperatingIncome, s.get(&["3CH"]));
        put(&mut flows, NetIncome, s.get(&["3ZA", "3Z"]));
        put(&mut instants, Cash, s.get(&["1A"]));
        let debt = sum(sum(s.get(&["2C"]), s.get(&["2E"])), s.get(&["2NBA"]));
        put(&mut instants, Debt, debt);
    } else {
        put(&mut flows, Revenue, s.get(&["3C"]));
        put(&mut flows, CostOfRevenue, s.get(&["3CA"]).map(f64::abs));
        put(&mut flows, GrossProfit, s.get(&["3D"]));
        put(&mut flows, OperatingIncome, s.get(&["3DF"]));
        put(&mut flows, NetIncome, s.get(&["3Z"]));
        put(&mut flows, OperatingCashFlow, s.get(&["4C"]));
        put(&mut flows, Capex, s.get(&["4CAI"]).map(f64::abs));
        put(&mut instants, Cash, sum(s.get(&["1AA"]), s.get(&["1AB"])));
        put(&mut instants, Debt, sum(s.get(&["2AA"]), s.get(&["2BA"])));
    }
    let equity = if bank { s.get(&["2O"]) } else { s.get(&["2O"]).or(total_equity) };
    put(&mut instants, TotalAssets, assets);
    put(&mut instants, Equity, equity);
    if let (Some(a), Some(e)) = (assets, if bank { equity } else { total_equity.or(equity) }) {
        instants.insert(TotalLiabilities, a - e);
    }
    Lines { flows, instants, capital: s.get(&["2OA"]).filter(|c| *c > 0.0) }
}

fn period(s: &Snapshot, bank: bool, prev_ytd: Option<&Snapshot>, annual: bool) -> Option<Period> {
    let end = s.end()?;
    let cur = lines(s, bank);
    let mut values = cur.instants;
    // Quarterly flows are the year-to-date figure less the previous quarter's; the first
    // quarter of a year is its own year-to-date.
    let prev = prev_ytd.map(|p| lines(p, bank).flows);
    for (m, v) in cur.flows {
        let q = if annual || s.month == 3 { Some(v) } else { prev.as_ref().and_then(|p| p.get(&m)).map(|p| v - p) };
        if let Some(q) = q {
            values.insert(m, q);
        }
    }
    if let (Some(ni), Some(cap)) = (values.get(&Metric::NetIncome).copied(), cur.capital) {
        values.insert(Metric::EpsDiluted, ni / cap);
    }
    crate::edgar::derive(&mut values, if annual { 1.0 } else { 0.25 });
    let label = if annual { format!("FY{}", s.year) } else { format!("Q{} FY{:02}", s.month / 3, s.year % 100) };
    Some(Period { end, label, values })
}

/// Annual and quarterly statements from quarter-end snapshots (any order).
pub fn build_financials(snaps: &[Snapshot], bank: bool, entity: &str) -> Financials {
    let mut sorted: Vec<&Snapshot> = snaps.iter().filter(|s| s.month % 3 == 0 && (3..=12).contains(&s.month)).collect();
    sorted.sort_by_key(|s| (s.year, s.month));
    let find = |y: i32, m: u32| sorted.iter().copied().find(|s| s.year == y && s.month == m);
    let annual = sorted.iter().filter(|s| s.month == 12).filter_map(|s| period(s, bank, None, true)).collect();
    let quarterly = sorted
        .iter()
        .filter_map(|s| {
            let prev = if s.month > 3 { find(s.year, s.month - 3) } else { None };
            period(s, bank, prev, false)
        })
        .collect();
    let capital = sorted.iter().rev().find_map(|s| lines(s, bank).capital);
    Financials {
        entity: entity.into(),
        annual,
        quarterly,
        shares_outstanding: capital,
        shares_as_of: sorted.last().and_then(|s| s.end()),
        shares_source: "paid-in capital at 1 TL nominal",
    }
}

// ---- company card -------------------------------------------------------------------------

/// Profile from the İş Yatırım company card.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BistProfile {
    pub code: String,
    pub name: String,
    pub founded: String,
    pub activity: String,
    pub phone: String,
    pub fax: String,
    pub address: String,
    /// Paid-in capital, million TRY (latest reported).
    pub paid_in_capital_mn: Option<f64>,
    /// Free float, percent.
    pub free_float_pct: Option<f64>,
    /// Foreign ownership, percent.
    pub foreign_pct: Option<f64>,
    /// İş Yatırım's own market cap and net debt, million TRY.
    pub market_cap_mn: Option<f64>,
    pub net_debt_mn: Option<f64>,
    /// Shareholders and their percentage, largest first.
    pub shareholders: Vec<(String, f64)>,
    /// TRY per USD on the page header.
    pub usd_try: Option<f64>,
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out.replace("&amp;", "&").replace("&nbsp;", " ").replace("&#39;", "'").replace("&quot;", "\"");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text of the cell following the cell labelled `label` (`<th>`/`<td>` pairs).
fn after_label(html: &str, label: &str) -> Option<String> {
    let at = html.find(label)?;
    let rest = &html[at + label.len()..];
    let td = rest.find("<td")?;
    let body = &rest[td..];
    let open_end = body.find('>')?;
    let close = body.find("</td>")?;
    (close > open_end).then(|| strip_tags(&body[open_end + 1..close]))
}

/// `1.018.517,0` → `1018517.0`; `23,79 %` → `23.79`; units after the number are ignored.
pub fn parse_tr_number(s: &str) -> Option<f64> {
    let num: String = s.trim().chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-')).collect();
    num.replace('.', "").replace(',', ".").parse().ok()
}

fn owners(html: &str) -> Vec<(String, f64)> {
    let Some(at) = html.find("var OrtaklikYapisidata") else { return Vec::new() };
    let line = html[at..].lines().next().unwrap_or("");
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(n) = rest.find("name:") {
        rest = &rest[n + 5..];
        let q = rest.trim_start();
        let Some(quote) = q.chars().next().filter(|c| matches!(c, '\'' | '"')) else { continue };
        let q = &q[1..];
        let Some(end) = q.find(quote) else { break };
        let name = q[..end].to_string();
        let Some(y) = q[end..].find("y:") else { break };
        let num: String = q[end + y + 2..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-'))
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            out.push((name, v));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

fn header_usd_try(html: &str) -> Option<f64> {
    let at = html.find("$/₺")?;
    let rest = &html[at..];
    let d = rest.find("data-chartdata=\"")?;
    let data = &rest[d + 16..];
    let data = &data[..data.find('"')?];
    let last = data.rsplit(['[', ',']).find(|p| !p.trim_matches([']', ' ']).is_empty())?;
    last.trim_matches([']', ' ']).parse().ok().filter(|v: &f64| *v > 0.0)
}

/// Parse an `sirket-karti.aspx` page.
pub fn parse_card(html: &str, code: &str) -> Result<BistProfile> {
    let name = after_label(html, "<th>Ünvanı</th>").ok_or_else(|| anyhow!("{code}: no company card on İş Yatırım"))?;
    let text = |label: &str| after_label(html, label).unwrap_or_default();
    let num = |label: &str| after_label(html, label).and_then(|s| parse_tr_number(&s));
    Ok(BistProfile {
        code: code.into(),
        name,
        founded: text("<th>Kuruluş</th>"),
        activity: text("<th>Faal Alanı</th>"),
        phone: text("<th>Telefon</th>"),
        fax: text("<th>Faks</th>"),
        address: text("<th>Adres</th>"),
        paid_in_capital_mn: num("<td>Ödenmiş Sermaye</td>"),
        free_float_pct: num("<th>Halka Açıklık Oranı (%)</th>"),
        foreign_pct: num("<th>Yabancı Oranı (%)</th>"),
        market_cap_mn: num("<th>Piyasa Değeri</th>"),
        net_debt_mn: num("<th>Net Borç</th>"),
        shareholders: owners(html),
        usd_try: header_usd_try(html),
    })
}

// ---- network + cache ----------------------------------------------------------------------

/// Everything DES and FA show for one BIST company.
#[derive(Debug, Clone, PartialEq)]
pub struct BistCompany {
    pub ticker: String,
    pub currency: Currency,
    /// Statement template that answered (`XI_29`, `UFRS`, `UFRS_K`).
    pub group: String,
    pub bank: bool,
    pub profile: Result<BistProfile, String>,
    pub financials: Result<Financials, String>,
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(30)).gzip(true).build().unwrap_or_default())
}

struct Gate {
    next: Option<Instant>,
    blocked_until: Option<Instant>,
}

static GATE: Mutex<Gate> = Mutex::const_new(Gate { next: None, blocked_until: None });

/// Process-wide throttle and 403/429 back-off. Fails fast while backing off.
async fn throttle() -> Result<()> {
    let mut g = GATE.lock().await;
    let now = Instant::now();
    if let Some(t) = g.blocked_until
        && t > now
    {
        bail!("İş Yatırım asked us to slow down; paused for {} more minutes", (t - now).as_secs().div_ceil(60));
    }
    if let Some(t) = g.next
        && t > now
    {
        tokio::time::sleep(t - now).await;
    }
    g.next = Some(Instant::now() + MIN_GAP);
    Ok(())
}

async fn fetch(url: &str) -> Result<String> {
    throttle().await?;
    let resp = client()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT_LANGUAGE, "tr-TR,tr;q=0.9,en;q=0.8")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        GATE.lock().await.blocked_until = Some(Instant::now() + BACKOFF);
        bail!("İş Yatırım refused the request ({status}); not retrying for ten minutes");
    }
    if !status.is_success() {
        bail!("İş Yatırım returned {status}");
    }
    Ok(resp.text().await?)
}

/// A disk-cached GET; a stale copy is served when the network fails.
async fn cached(dir: &Path, name: &str, ttl: Duration, url: &str) -> Result<String> {
    let path = dir.join(name);
    let age =
        std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a < ttl)
        && let Ok(s) = std::fs::read_to_string(&path)
    {
        return Ok(s);
    }
    match fetch(url).await {
        Ok(body) => {
            let _ = std::fs::create_dir_all(dir);
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
            Ok(body)
        }
        Err(e) => match std::fs::read_to_string(&path) {
            Ok(s) => {
                tracing::warn!("İş Yatırım {url}: {e:#}; serving cached copy");
                Ok(s)
            }
            Err(_) => Err(e),
        },
    }
}

/// The cache directory under the app's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bist")
}

/// The last `n` quarter-ends on or before `today`, newest first.
pub fn quarter_ends(today: NaiveDate, n: usize) -> Vec<(i32, u32)> {
    let mut y = today.year();
    let mut m = today.month() / 3 * 3;
    if m == 0 {
        y -= 1;
        m = 12;
    }
    (0..n)
        .map(|_| {
            let p = (y, m);
            if m == 3 {
                y -= 1;
                m = 12;
            } else {
                m -= 3;
            }
            p
        })
        .collect()
}

fn malitablo_url(code: &str, ccy: Currency, group: &str, periods: &[(i32, u32)]) -> String {
    let mut url = format!("{MALITABLO_URL}?companyCode={code}&exchange={}&financialGroup={group}", ccy.code());
    for (i, (y, m)) in periods.iter().enumerate() {
        url.push_str(&format!("&year{0}={y}&period{0}={m}", i + 1));
    }
    url
}

/// All snapshots for a company in one template and currency.
async fn statements(dir: &Path, code: &str, ccy: Currency, group: &str, today: NaiveDate) -> Result<Vec<Snapshot>> {
    let mut out = Vec::new();
    for chunk in quarter_ends(today, QUARTERS).chunks(4) {
        let tag: String = chunk.iter().map(|(y, m)| format!("{y}{m:02}")).collect::<Vec<_>>().join("_");
        let name = format!("malitablo_{code}_{}_{group}_{tag}.json", ccy.code());
        let json = cached(dir, &name, STATEMENT_TTL, &malitablo_url(code, ccy, group, chunk)).await?;
        let snaps = parse_malitablo(&json, chunk)?;
        out.extend(snaps);
    }
    Ok(out)
}

/// Load the profile and statements for a market key in the given currency.
pub async fn company(dir: &Path, symbol: &str, ccy: Currency) -> Result<BistCompany> {
    let Some(code) = bist_ticker(symbol) else {
        bail!("not a Borsa Istanbul equity: {}", crate::security(symbol).key());
    };
    let today = chrono::Local::now().date_naive();
    let card = cached(dir, &format!("card_{code}.html"), CARD_TTL, &format!("{CARD_URL}?hisse={code}"))
        .await
        .and_then(|h| parse_card(&h, &code))
        .map_err(|e| format!("{e:#}"));
    let name = card.as_ref().map(|p| p.name.clone()).unwrap_or_else(|_| code.clone());

    let mut group = GROUPS[0].to_string();
    let mut financials = Err("no statements published on İş Yatırım".to_string());
    let mut last_err = None;
    for g in GROUPS {
        match statements(dir, &code, ccy, g, today).await {
            Ok(snaps) if snaps.is_empty() => continue,
            Ok(snaps) => {
                group = g.to_string();
                financials = Ok(build_financials(&snaps, g != "XI_29", &name));
                break;
            }
            Err(e) => {
                last_err = Some(format!("{e:#}"));
                break;
            }
        }
    }
    if let Some(e) = last_err {
        financials = Err(e);
    }
    Ok(BistCompany { ticker: code, currency: ccy, bank: group != "XI_29", group, profile: card, financials })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}/{name}")).unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// The periods of the recorded request (`year1..4`, `period1..4`).
    const PERIODS: [(i32, u32); 4] = [(2025, 6), (2025, 3), (2024, 12), (2024, 9)];

    #[test]
    fn bist_tickers() {
        assert_eq!(bist_ticker("THYAO.IS").as_deref(), Some("THYAO"));
        assert_eq!(bist_ticker("AKBNK.IS").as_deref(), Some("AKBNK"));
        assert_eq!(bist_ticker("AAPL US Equity"), None);
        assert_eq!(bist_ticker("SAP.DE"), None);
        assert_eq!(bist_ticker("BTCUSD Curncy"), None);
        assert_eq!(bist_ticker("SPX Index"), None);
        // EDGAR keeps ignoring BIST listings.
        assert_eq!(crate::edgar::edgar_ticker("THYAO.IS"), None);
    }

    #[test]
    fn turkish_numbers() {
        assert_eq!(parse_tr_number("1.018.517,0"), Some(1_018_517.0));
        assert_eq!(parse_tr_number("23,79"), Some(23.79));
        assert_eq!(parse_tr_number("381.915,0 mnTL"), Some(381_915.0));
        assert_eq!(parse_tr_number("-4,5"), Some(-4.5));
        assert_eq!(parse_tr_number("n/a"), None);
    }

    #[test]
    fn quarter_end_list() {
        assert_eq!(quarter_ends(d("2026-10-16"), 5), vec![(2026, 9), (2026, 6), (2026, 3), (2025, 12), (2025, 9)]);
        assert_eq!(quarter_ends(d("2026-02-10"), 2), vec![(2025, 12), (2025, 9)]);
        let url = malitablo_url("THYAO", Currency::Usd, "XI_29", &PERIODS);
        assert!(url.ends_with("companyCode=THYAO&exchange=USD&financialGroup=XI_29&year1=2025&period1=6&year2=2025&period2=3&year3=2024&period3=12&year4=2024&period4=9"), "{url}");
    }

    #[test]
    fn industrial_statements_decumulate() {
        let snaps = parse_malitablo(&fixture("isyatirim_malitablo_thyao.json"), &PERIODS).unwrap();
        assert_eq!(snaps.len(), 4);
        let f = build_financials(&snaps, false, "Türk Hava Yolları");
        // Annual: FY2024 as reported.
        let fy24 = f.annual.iter().find(|p| p.end == d("2024-12-31")).expect("FY2024");
        assert_eq!(fy24.label, "FY2024");
        assert_eq!(fy24.get(Metric::Revenue), Some(975_710_686_451.0));
        assert_eq!(fy24.get(Metric::NetIncome), Some(148_403_104_528.0));
        // Quarterly: Q1 is its own YTD, Q2 = H1 − Q1, Q4 = FY − 9M.
        let q = |end: &str| f.quarterly.iter().find(|p| p.end == d(end)).unwrap_or_else(|| panic!("{end}"));
        assert_eq!(q("2025-03-31").label, "Q1 FY25");
        assert_eq!(q("2025-03-31").get(Metric::Revenue), Some(176_712_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::Revenue), Some(408_036_000_000.0 - 176_712_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::NetIncome), Some(25_013_000_000.0 + 1_818_000_000.0));
        assert_eq!(
            q("2024-12-31").get(Metric::Revenue),
            Some(975_710_686_451.0 - 551_928_000_000.0),
            "Q4 derived from FY − 9M"
        );
        // The Q3 2024 quarter has no Q2 snapshot to subtract, so its flows are absent.
        assert_eq!(q("2024-09-30").get(Metric::Revenue), None);
        // Balance sheet is point-in-time.
        assert_eq!(q("2025-06-30").get(Metric::TotalAssets), Some(1_652_589_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::Equity), Some(752_073_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::TotalLiabilities), Some(1_652_589_000_000.0 - 752_120_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::Debt), Some(138_324_000_000.0 + 446_268_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::Cash), Some(98_791_000_000.0 + 150_560_000_000.0));
        // Cash flow: de-cumulated OCF and positive capex; FCF and margins derived.
        assert_eq!(q("2025-06-30").get(Metric::OperatingCashFlow), Some(105_659_000_000.0 - 34_510_000_000.0));
        assert_eq!(q("2025-06-30").get(Metric::Capex), Some(35_408_000_000.0 - 10_998_000_000.0));
        assert!(q("2025-06-30").get(Metric::FreeCashFlow).is_some());
        assert!(q("2025-06-30").get(Metric::GrossMargin).is_some_and(|m| m > 0.0 && m < 0.5));
        // EPS from net income over the 1 TL-nominal share count (1.38 bn shares).
        let eps = q("2025-06-30").get(Metric::EpsDiluted).unwrap();
        assert!((eps - 26_831_000_000.0 / 1_380_000_000.0).abs() < 1e-9, "{eps}");
        assert_eq!(f.shares_outstanding, Some(1_380_000_000.0));
    }

    #[test]
    fn bank_statements_use_the_bank_template() {
        let snaps = parse_malitablo(&fixture("isyatirim_malitablo_akbnk.json"), &PERIODS).unwrap();
        let f = build_financials(&snaps, true, "Akbank");
        let q = |end: &str| f.quarterly.iter().find(|p| p.end == d(end)).unwrap_or_else(|| panic!("{end}"));
        let h1 = q("2025-06-30");
        assert_eq!(h1.get(Metric::TotalAssets), Some(2_818_347_573_000.0));
        assert_eq!(h1.get(Metric::Equity), Some(258_900_893_000.0));
        assert_eq!(h1.get(Metric::NetIncome), Some(24_851_000_000.0 - 13_727_079_000.0));
        assert_eq!(h1.get(Metric::Revenue), Some(92_661_000_000.0 - 49_211_210_000.0));
        assert_eq!(h1.get(Metric::GrossProfit), Some(36_444_000_000.0 - 17_874_867_000.0), "net interest income");
        assert_eq!(h1.get(Metric::Cash), Some(429_829_704_000.0));
        assert_eq!(h1.get(Metric::Debt), Some(128_386_386_000.0 + 131_797_701_000.0 + 76_805_130_000.0));
        assert_eq!(h1.get(Metric::OperatingCashFlow), None, "banks have no cash-flow lines here");
        assert!(h1.get(Metric::Roe).is_some_and(|r| r > 0.0));
        assert_eq!(f.annual.len(), 1);
    }

    #[test]
    fn empty_and_error_replies() {
        // A bank asked for the industrial template returns an empty list: no snapshots.
        assert!(parse_malitablo(&fixture("isyatirim_malitablo_empty.json"), &PERIODS).unwrap().is_empty());
        let err = parse_malitablo(r#"{"ok":false,"errorDescription":"boom","value":null}"#, &PERIODS).unwrap_err();
        assert!(err.to_string().contains("boom"));
        // Unreported periods (null values) are dropped.
        let json = r#"{"ok":true,"value":[{"itemCode":"1BL","value1":null,"value2":"5","value3":null,"value4":null}]}"#;
        let s = parse_malitablo(json, &PERIODS).unwrap();
        assert_eq!((s.len(), s[0].year, s[0].month), (1, 2025, 3));
    }

    #[test]
    fn company_card() {
        let p = parse_card(&fixture("isyatirim_card_thyao.html"), "THYAO").unwrap();
        assert_eq!(p.name, "Türk Hava Yolları");
        assert_eq!(p.founded, "29.02.1956");
        assert!(p.activity.contains("bayrak taşıyıcısı"), "{}", p.activity);
        assert_eq!(p.phone, "(0212)4636363");
        assert!(p.address.contains("BAKIRKÖY"), "{}", p.address);
        assert_eq!(p.paid_in_capital_mn, Some(1_380.0));
        assert_eq!(p.free_float_pct, Some(49.1));
        assert_eq!(p.foreign_pct, Some(23.79));
        assert_eq!(p.market_cap_mn, Some(381_915.0));
        assert_eq!(p.net_debt_mn, Some(620_383.0));
        assert_eq!(p.shareholders, vec![("Diğer".to_string(), 50.88), ("Türkiye Varlık Fonu".to_string(), 49.12)]);
        assert_eq!(p.usd_try, Some(49.52748));
        assert!(parse_card("<html>no card</html>", "XXXX").is_err());
    }

    /// Live check against İş Yatırım (network): `cargo test -p tr-data -- --ignored bist_live`.
    #[tokio::test]
    #[ignore = "network"]
    async fn bist_live() {
        let dir = std::env::temp_dir().join("tr-bist-live");
        let thy = company(&dir, "THYAO.IS", Currency::Try).await.unwrap();
        let f = thy.financials.as_ref().unwrap();
        assert!(f.annual.len() >= 4 && f.quarterly.len() >= 12);
        assert!(f.quarterly.last().unwrap().get(Metric::Revenue).is_some());
        assert!(thy.profile.as_ref().unwrap().name.contains("Hava"));
        let usd = company(&dir, "THYAO.IS", Currency::Usd).await.unwrap();
        assert!(usd.financials.unwrap().annual.last().unwrap().get(Metric::TotalAssets).unwrap() < 1e12);
        let bank = company(&dir, "AKBNK.IS", Currency::Try).await.unwrap();
        assert!(bank.bank && bank.financials.unwrap().annual.last().unwrap().get(Metric::NetIncome).is_some());
    }
}
