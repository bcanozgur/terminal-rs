//! SEC EDGAR company descriptions (DES) and XBRL financial statements (FA) for US filers.
//!
//! Sources (all free, no key):
//! - `www.sec.gov/files/company_tickers.json`: ticker → CIK.
//! - `data.sec.gov/submissions/CIK##########.json`: name, SIC, exchange, fiscal year end,
//!   addresses and recent filings.
//! - `data.sec.gov/api/xbrl/companyfacts/CIK##########.json`: every XBRL fact the company
//!   has reported, from which annual and quarterly statements are rebuilt.
//!
//! SEC fair access: every request carries a User-Agent naming the user and a contact
//! address ([`UA_ENV`] or the Settings field), requests are throttled to under 10 per
//! second process-wide, and responses are cached on disk with a TTL.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::Deserialize;
use tokio::sync::Mutex;
use tr_core::AssetClass;

/// Environment variable holding the SEC User-Agent (e.g. `Jane Doe jane@example.com`).
pub const UA_ENV: &str = "TR_SEC_USER_AGENT";

const TICKERS_URL: &str = "https://www.sec.gov/files/company_tickers.json";
const TICKERS_TTL: Duration = Duration::from_secs(24 * 3600);
const SUBMISSIONS_TTL: Duration = Duration::from_secs(6 * 3600);
const FACTS_TTL: Duration = Duration::from_secs(12 * 3600);
/// Minimum spacing between SEC requests (8 per second, under the 10/s limit).
const MIN_GAP: Duration = Duration::from_millis(125);

// ---- ticker map ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TickerRow {
    cik_str: u64,
    ticker: String,
}

/// Parse `company_tickers.json` into ticker (upper case, `BRK-B` style) → CIK.
pub fn parse_tickers(json: &str) -> Result<HashMap<String, u64>> {
    let rows: HashMap<String, TickerRow> = serde_json::from_str(json).context("company_tickers.json")?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in rows.into_values() {
        // The file lists a company's primary ticker first; keep the first CIK per ticker.
        out.entry(r.ticker.to_ascii_uppercase()).or_insert(r.cik_str);
    }
    Ok(out)
}

/// The EDGAR ticker for a market key, or `None` when the security is not a US-listed
/// equity (indices, FX, crypto, rates, futures and foreign listings have no SEC filings).
pub fn edgar_ticker(symbol: &str) -> Option<String> {
    let sec = crate::security(symbol);
    if !matches!(sec.class, AssetClass::Equity | AssetClass::Etf) || crate::native_currency(symbol) != "USD" {
        return None;
    }
    let y = crate::yahoo::yahoo_symbol(&sec);
    // A 2+ letter suffix is a foreign exchange (`SAP.DE`); `BRK-B` style share classes stay.
    if y.rsplit_once('.').is_some_and(|(_, s)| s.len() >= 2) || y.starts_with('^') {
        return None;
    }
    Some(y.replace('.', "-").to_ascii_uppercase())
}

// ---- submissions (DES) --------------------------------------------------------------------

/// One filing from the submissions feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Filing {
    pub form: String,
    pub filed: NaiveDate,
    /// Period of report, when the form has one.
    pub report: Option<NaiveDate>,
    pub accession: String,
    pub description: String,
    /// 8-K item numbers (e.g. `2.02,9.01`).
    pub items: String,
    /// Primary document on sec.gov.
    pub url: String,
}

/// Company profile from the submissions feed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompanyProfile {
    pub cik: u64,
    pub name: String,
    pub tickers: Vec<String>,
    pub exchanges: Vec<String>,
    pub sic: String,
    pub sic_description: String,
    /// Filer category (e.g. `Large accelerated filer`).
    pub category: String,
    pub entity_type: String,
    /// Fiscal year end as `MMDD`.
    pub fiscal_year_end: String,
    pub state_of_incorporation: String,
    pub ein: String,
    pub phone: String,
    pub website: String,
    pub business_address: String,
    pub former_names: Vec<String>,
    /// Recent filings, newest first.
    pub filings: Vec<Filing>,
}

impl CompanyProfile {
    /// Fiscal year end as e.g. `Sep 26`.
    pub fn fiscal_year_end_label(&self) -> String {
        let fy = &self.fiscal_year_end;
        let (m, d) = (fy.get(..2).and_then(|m| m.parse::<u32>().ok()), fy.get(2..4));
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        match (m, d) {
            (Some(m @ 1..=12), Some(d)) => format!("{} {d}", MONTHS[m as usize - 1]),
            _ => fy.clone(),
        }
    }

    /// Filings of the given forms (exact match), newest first.
    pub fn filings_of<'a>(&'a self, forms: &'a [&str]) -> impl Iterator<Item = &'a Filing> + 'a {
        self.filings.iter().filter(move |f| forms.contains(&f.form.as_str()))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Address {
    street1: Option<String>,
    street2: Option<String>,
    city: Option<String>,
    #[serde(rename = "stateOrCountry")]
    state: Option<String>,
    #[serde(rename = "zipCode")]
    zip: Option<String>,
}

impl Address {
    fn line(&self) -> String {
        let city = [self.city.as_deref(), self.state.as_deref(), self.zip.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        [self.street1.as_deref(), self.street2.as_deref(), Some(city.as_str())]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Recent {
    accession_number: Vec<String>,
    filing_date: Vec<String>,
    report_date: Vec<String>,
    form: Vec<String>,
    items: Vec<String>,
    primary_document: Vec<String>,
    primary_doc_description: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Filings {
    recent: Recent,
}

#[derive(Deserialize)]
struct FormerName {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Submissions {
    cik: serde_json::Value,
    name: String,
    #[serde(default)]
    entity_type: Option<String>,
    #[serde(default)]
    sic: Option<String>,
    #[serde(default)]
    sic_description: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tickers: Vec<Option<String>>,
    #[serde(default)]
    exchanges: Vec<Option<String>>,
    #[serde(default)]
    ein: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    investor_website: Option<String>,
    #[serde(default)]
    fiscal_year_end: Option<String>,
    #[serde(default)]
    state_of_incorporation: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    addresses: HashMap<String, Address>,
    #[serde(default)]
    former_names: Vec<FormerName>,
    #[serde(default)]
    filings: Filings,
}

fn date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// Parse a `submissions/CIK##########.json` document.
pub fn parse_submissions(json: &str) -> Result<CompanyProfile> {
    let s: Submissions = serde_json::from_str(json).context("submissions JSON")?;
    let cik = match &s.cik {
        serde_json::Value::String(c) => c.parse().unwrap_or_default(),
        v => v.as_u64().unwrap_or_default(),
    };
    let r = &s.filings.recent;
    let at = |v: &Vec<String>, i: usize| v.get(i).cloned().unwrap_or_default();
    let filings = (0..r.form.len())
        .filter_map(|i| {
            let accession = at(&r.accession_number, i);
            let doc = at(&r.primary_document, i);
            let folder = accession.replace('-', "");
            let url = if doc.is_empty() {
                format!("https://www.sec.gov/Archives/edgar/data/{cik}/{folder}/")
            } else {
                format!("https://www.sec.gov/Archives/edgar/data/{cik}/{folder}/{doc}")
            };
            Some(Filing {
                form: at(&r.form, i),
                filed: date(&at(&r.filing_date, i))?,
                report: date(&at(&r.report_date, i)),
                accession,
                description: at(&r.primary_doc_description, i),
                items: at(&r.items, i),
                url,
            })
        })
        .collect();
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    let website = s.website.clone().filter(|w| !w.is_empty()).or_else(|| s.investor_website.clone());
    Ok(CompanyProfile {
        cik,
        name: s.name,
        tickers: s.tickers.into_iter().flatten().collect(),
        exchanges: s.exchanges.into_iter().flatten().collect(),
        sic: opt(&s.sic),
        sic_description: opt(&s.sic_description),
        category: opt(&s.category),
        entity_type: opt(&s.entity_type),
        fiscal_year_end: opt(&s.fiscal_year_end),
        state_of_incorporation: opt(&s.state_of_incorporation),
        ein: opt(&s.ein),
        phone: opt(&s.phone),
        website: website.unwrap_or_default(),
        business_address: s.addresses.get("business").map(Address::line).unwrap_or_default(),
        former_names: s.former_names.into_iter().map(|f| f.name).collect(),
        filings,
    })
}

// ---- company facts (FA) -------------------------------------------------------------------

/// Which statement a metric belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statement {
    Income,
    Balance,
    CashFlow,
    Ratios,
}

impl Statement {
    pub const ALL: [Statement; 4] = [Statement::Income, Statement::Balance, Statement::CashFlow, Statement::Ratios];

    pub fn label(self) -> &'static str {
        match self {
            Statement::Income => "Income",
            Statement::Balance => "Balance Sheet",
            Statement::CashFlow => "Cash Flow",
            Statement::Ratios => "Ratios",
        }
    }
}

/// How a metric's values are expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Usd,
    PerShare,
    /// A fraction shown as a percentage (0.25 = 25%).
    Pct,
    /// A plain multiple (e.g. debt/equity 1.4x).
    Times,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    Revenue,
    CostOfRevenue,
    GrossProfit,
    OperatingIncome,
    NetIncome,
    EpsDiluted,
    TotalAssets,
    TotalLiabilities,
    Equity,
    Cash,
    Debt,
    OperatingCashFlow,
    Capex,
    FreeCashFlow,
    GrossMargin,
    OperatingMargin,
    NetMargin,
    Roe,
    DebtToEquity,
    FcfMargin,
}

/// How a raw metric is reported in XBRL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A flow over a period (income, cash flow): quarters may need deriving from YTD.
    Flow,
    /// A per-share flow (not additive, so never derived).
    PerShare,
    /// A point-in-time balance.
    Instant,
}

impl Metric {
    pub const ALL: [Metric; 20] = [
        Metric::Revenue,
        Metric::CostOfRevenue,
        Metric::GrossProfit,
        Metric::OperatingIncome,
        Metric::NetIncome,
        Metric::EpsDiluted,
        Metric::TotalAssets,
        Metric::TotalLiabilities,
        Metric::Equity,
        Metric::Cash,
        Metric::Debt,
        Metric::OperatingCashFlow,
        Metric::Capex,
        Metric::FreeCashFlow,
        Metric::GrossMargin,
        Metric::OperatingMargin,
        Metric::NetMargin,
        Metric::Roe,
        Metric::DebtToEquity,
        Metric::FcfMargin,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Metric::Revenue => "Revenue",
            Metric::CostOfRevenue => "Cost of Revenue",
            Metric::GrossProfit => "Gross Profit",
            Metric::OperatingIncome => "Operating Income",
            Metric::NetIncome => "Net Income",
            Metric::EpsDiluted => "EPS (Diluted)",
            Metric::TotalAssets => "Total Assets",
            Metric::TotalLiabilities => "Total Liabilities",
            Metric::Equity => "Shareholders' Equity",
            Metric::Cash => "Cash & Equivalents",
            Metric::Debt => "Total Debt",
            Metric::OperatingCashFlow => "Operating Cash Flow",
            Metric::Capex => "Capital Expenditure",
            Metric::FreeCashFlow => "Free Cash Flow",
            Metric::GrossMargin => "Gross Margin",
            Metric::OperatingMargin => "Operating Margin",
            Metric::NetMargin => "Net Margin",
            Metric::Roe => "Return on Equity",
            Metric::DebtToEquity => "Debt / Equity",
            Metric::FcfMargin => "FCF Margin",
        }
    }

    pub fn statement(self) -> Statement {
        use Metric::*;
        match self {
            Revenue | CostOfRevenue | GrossProfit | OperatingIncome | NetIncome | EpsDiluted => Statement::Income,
            TotalAssets | TotalLiabilities | Equity | Cash | Debt => Statement::Balance,
            OperatingCashFlow | Capex | FreeCashFlow => Statement::CashFlow,
            GrossMargin | OperatingMargin | NetMargin | Roe | DebtToEquity | FcfMargin => Statement::Ratios,
        }
    }

    pub fn unit(self) -> Unit {
        use Metric::*;
        match self {
            EpsDiluted => Unit::PerShare,
            GrossMargin | OperatingMargin | NetMargin | Roe | FcfMargin => Unit::Pct,
            DebtToEquity => Unit::Times,
            _ => Unit::Usd,
        }
    }

    /// The metrics of one statement, in display order.
    pub fn of(statement: Statement) -> impl Iterator<Item = Metric> {
        Metric::ALL.into_iter().filter(move |m| m.statement() == statement)
    }

    /// XBRL `us-gaap` tags in priority order (earlier tags win where both report a period).
    fn tags(self) -> &'static [&'static str] {
        use Metric::*;
        match self {
            Revenue => &[
                "Revenues",
                "RevenueFromContractWithCustomerExcludingAssessedTax",
                "RevenueFromContractWithCustomerIncludingAssessedTax",
                "SalesRevenueNet",
                "SalesRevenueGoodsNet",
                "SalesRevenueServicesNet",
            ],
            CostOfRevenue => &[
                "CostOfGoodsAndServicesSold",
                "CostOfRevenue",
                "CostOfGoodsSold",
                "CostOfGoodsAndServiceExcludingDepreciationDepletionAndAmortization",
            ],
            GrossProfit => &["GrossProfit"],
            OperatingIncome => &["OperatingIncomeLoss"],
            NetIncome => &["NetIncomeLoss", "NetIncomeLossAvailableToCommonStockholdersBasic", "ProfitLoss"],
            EpsDiluted => &["EarningsPerShareDiluted", "EarningsPerShareBasicAndDiluted"],
            TotalAssets => &["Assets"],
            TotalLiabilities => &["Liabilities"],
            Equity => &["StockholdersEquity", "StockholdersEquityIncludingPortionAttributableToNoncontrollingInterest"],
            Cash => &[
                "CashAndCashEquivalentsAtCarryingValue",
                "CashCashEquivalentsRestrictedCashAndRestrictedCashEquivalents",
                "Cash",
            ],
            OperatingCashFlow => &[
                "NetCashProvidedByUsedInOperatingActivities",
                "NetCashProvidedByUsedInOperatingActivitiesContinuingOperations",
            ],
            Capex => &["PaymentsToAcquirePropertyPlantAndEquipment", "PaymentsToAcquireProductiveAssets"],
            _ => &[],
        }
    }

    fn kind(self) -> Kind {
        match self.statement() {
            Statement::Balance => Kind::Instant,
            _ if self == Metric::EpsDiluted => Kind::PerShare,
            _ => Kind::Flow,
        }
    }
}

/// One fiscal period (a year or a quarter) of statement values.
#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub end: NaiveDate,
    /// e.g. `FY2025` or `Q3 FY26`.
    pub label: String,
    pub values: BTreeMap<Metric, f64>,
}

impl Period {
    pub fn get(&self, m: Metric) -> Option<f64> {
        self.values.get(&m).copied()
    }
}

/// Annual and quarterly statements rebuilt from a company's XBRL facts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Financials {
    pub entity: String,
    /// Fiscal years, oldest first.
    pub annual: Vec<Period>,
    /// Fiscal quarters, oldest first (Q4 derived as the year less the first three quarters).
    pub quarterly: Vec<Period>,
    /// Shares outstanding from the latest cover page (all classes).
    pub shares_outstanding: Option<f64>,
    pub shares_as_of: Option<NaiveDate>,
    /// Where the share count came from (`cover page`, `balance sheet`, `diluted average`).
    pub shares_source: &'static str,
    /// `quarterly` holds year-to-date totals (3M, 6M, 9M, 12M) rather than single quarters.
    pub quarterly_ytd: bool,
}

/// Snapshot valuation from a live price.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Valuation {
    pub market_cap: Option<f64>,
    pub enterprise_value: Option<f64>,
    pub pe: Option<f64>,
    pub ps: Option<f64>,
    pub pb: Option<f64>,
    pub ttm_revenue: Option<f64>,
    pub ttm_net_income: Option<f64>,
    pub ttm_eps: Option<f64>,
}

/// The period one year before `periods[i]` (end 350–380 days earlier).
pub fn prior_year(periods: &[Period], i: usize) -> Option<&Period> {
    let end = periods.get(i)?.end;
    periods[..i].iter().rev().find(|p| (350..=380).contains(&(end - p.end).num_days()))
}

impl Financials {
    /// Trailing twelve months of a flow metric: the last four contiguous quarters when they
    /// are newer than the last fiscal year, otherwise the last fiscal year.
    pub fn ttm(&self, m: Metric) -> Option<f64> {
        let annual = self.annual.last();
        let q = &self.quarterly;
        if self.quarterly_ytd {
            // Latest year-to-date + last fiscal year − the same year-to-date a year earlier.
            if let (Some(a), Some(last)) = (annual, q.last())
                && last.end > a.end
                && let Some(before) = prior_year(q, q.len() - 1)
                && let (Some(x), Some(y), Some(z)) = (last.get(m), a.get(m), before.get(m))
            {
                return Some(x + y - z);
            }
            return annual.and_then(|a| a.get(m));
        }
        if q.len() >= 4 {
            let last4 = &q[q.len() - 4..];
            let contiguous = last4.windows(2).all(|w| (80..=100).contains(&(w[1].end - w[0].end).num_days()));
            let newer = annual.is_none_or(|a| last4[3].end > a.end);
            if contiguous && newer {
                let vals: Option<Vec<f64>> = last4.iter().map(|p| p.get(m)).collect();
                if let Some(v) = vals {
                    return Some(v.iter().sum());
                }
            }
        }
        annual.and_then(|a| a.get(m))
    }

    /// The most recent value of a balance-sheet metric.
    pub fn latest(&self, m: Metric) -> Option<f64> {
        let q = self.quarterly.iter().rev().find_map(|p| p.get(m).map(|v| (p.end, v)));
        let a = self.annual.iter().rev().find_map(|p| p.get(m).map(|v| (p.end, v)));
        match (q, a) {
            (Some(q), Some(a)) => Some(if q.0 >= a.0 { q.1 } else { a.1 }),
            (x, y) => x.or(y).map(|v| v.1),
        }
    }

    pub fn valuation(&self, price: Option<f64>) -> Valuation {
        let ttm_revenue = self.ttm(Metric::Revenue);
        let ttm_net_income = self.ttm(Metric::NetIncome);
        let ttm_eps = self.ttm(Metric::EpsDiluted);
        let market_cap = price.zip(self.shares_outstanding).map(|(p, s)| p * s).filter(|m| *m > 0.0);
        let pos = |x: Option<f64>| x.filter(|v| *v > 0.0);
        let enterprise_value =
            market_cap.map(|m| m + self.latest(Metric::Debt).unwrap_or(0.0) - self.latest(Metric::Cash).unwrap_or(0.0));
        Valuation {
            market_cap,
            enterprise_value,
            pe: market_cap.zip(pos(ttm_net_income)).map(|(m, n)| m / n),
            ps: market_cap.zip(pos(ttm_revenue)).map(|(m, r)| m / r),
            pb: market_cap.zip(pos(self.latest(Metric::Equity))).map(|(m, e)| m / e),
            ttm_revenue,
            ttm_net_income,
            ttm_eps,
        }
    }
}

#[derive(Deserialize)]
struct RawFact {
    #[serde(default)]
    start: Option<String>,
    end: String,
    val: f64,
    #[serde(default)]
    fy: Option<i32>,
    #[serde(default)]
    fp: Option<String>,
    #[serde(default)]
    filed: String,
    #[serde(default)]
    accn: String,
}

#[derive(Deserialize)]
struct Concept {
    #[serde(default)]
    units: HashMap<String, Vec<RawFact>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompanyFacts {
    #[serde(default)]
    entity_name: String,
    #[serde(default)]
    facts: HashMap<String, HashMap<String, Concept>>,
}

/// A deduplicated observation of one metric.
#[derive(Clone, Copy)]
struct Obs {
    start: Option<NaiveDate>,
    end: NaiveDate,
    val: f64,
    /// Tag priority (lower wins), then latest filing wins.
    prio: usize,
    filed: NaiveDate,
}

fn days(a: NaiveDate, b: NaiveDate) -> i64 {
    (b - a).num_days()
}

/// Observations per (start, end): the highest-priority tag, and among its facts the latest
/// filing (so restatements replace originals and duplicate frames collapse).
fn observations(gaap: &HashMap<String, Concept>, m: Metric) -> Vec<Obs> {
    let unit = if m.kind() == Kind::PerShare { "USD/shares" } else { "USD" };
    let mut best: BTreeMap<(Option<NaiveDate>, NaiveDate), Obs> = BTreeMap::new();
    for (prio, tag) in m.tags().iter().enumerate() {
        let Some(facts) = gaap.get(*tag).and_then(|c| c.units.get(unit)) else { continue };
        for f in facts {
            let (Some(end), Some(filed)) = (date(&f.end), date(&f.filed)) else { continue };
            let start = f.start.as_deref().and_then(date);
            if (m.kind() == Kind::Instant) != start.is_none() {
                continue;
            }
            let o = Obs { start, end, val: f.val, prio, filed };
            best.entry((start, end))
                .and_modify(|b| {
                    if (o.prio, std::cmp::Reverse(o.filed)) < (b.prio, std::cmp::Reverse(b.filed)) {
                        *b = o;
                    }
                })
                .or_insert(o);
        }
    }
    best.into_values().collect()
}

fn is_year(o: &Obs) -> bool {
    o.start.is_some_and(|s| (350..=380).contains(&days(s, o.end)))
}

fn is_quarter_len(d: i64) -> bool {
    (80..=100).contains(&d)
}

/// Annual values by period end.
fn annual_values(obs: &[Obs]) -> BTreeMap<NaiveDate, f64> {
    obs.iter().filter(|o| is_year(o)).map(|o| (o.end, o.val)).collect()
}

/// Quarterly values by period end: reported three-month figures, plus quarters derived as
/// the difference of two year-to-date figures sharing a start (e.g. 9M − 6M, FY − 9M) for
/// additive metrics.
fn quarterly_values(obs: &[Obs], additive: bool) -> BTreeMap<NaiveDate, f64> {
    let mut out: BTreeMap<NaiveDate, f64> = obs
        .iter()
        .filter(|o| o.start.is_some_and(|s| is_quarter_len(days(s, o.end))))
        .map(|o| (o.end, o.val))
        .collect();
    if !additive {
        return out;
    }
    let mut by_start: BTreeMap<NaiveDate, Vec<&Obs>> = BTreeMap::new();
    for o in obs {
        if let Some(s) = o.start {
            by_start.entry(s).or_default().push(o);
        }
    }
    for group in by_start.values_mut() {
        group.sort_by_key(|o| o.end);
        for (i, b) in group.iter().enumerate() {
            if out.contains_key(&b.end) {
                continue;
            }
            if let Some(a) = group[..i].iter().rev().find(|a| is_quarter_len(days(a.end, b.end))) {
                out.insert(b.end, b.val - a.val);
            }
        }
    }
    out
}

/// The value at `end`, or at the nearest date within a week (balance sheets are sometimes
/// dated a day or two off the income statement's period end).
fn near(map: &BTreeMap<NaiveDate, f64>, end: NaiveDate) -> Option<f64> {
    if let Some(v) = map.get(&end) {
        return Some(*v);
    }
    map.range(end - chrono::Days::new(7)..=end + chrono::Days::new(7))
        .min_by_key(|(d, _)| days(**d, end).abs())
        .map(|(_, v)| *v)
}

/// Fiscal (year, period) labels by period end, from each filing's own (latest) period.
fn fiscal_labels(gaap: &HashMap<String, Concept>) -> HashMap<NaiveDate, (i32, String)> {
    let mut by_accn: HashMap<&str, (NaiveDate, i32, &str)> = HashMap::new();
    for m in [Metric::Revenue, Metric::NetIncome, Metric::TotalAssets] {
        for tag in m.tags() {
            let Some(c) = gaap.get(*tag) else { continue };
            for f in c.units.values().flatten() {
                let (Some(end), Some(fy), Some(fp)) = (date(&f.end), f.fy, f.fp.as_deref()) else { continue };
                let e = by_accn.entry(&f.accn).or_insert((end, fy, fp));
                if end > e.0 {
                    *e = (end, fy, fp);
                }
            }
        }
    }
    by_accn.into_values().map(|(end, fy, fp)| (end, (fy, fp.to_string()))).collect()
}

/// Shares outstanding from the latest cover page, summed across share classes.
fn cover_shares(facts: &HashMap<String, HashMap<String, Concept>>) -> Option<(f64, NaiveDate)> {
    let list = facts.get("dei")?.get("EntityCommonStockSharesOutstanding")?.units.get("shares")?;
    let latest = list.iter().max_by(|a, b| (&a.filed, &a.end).cmp(&(&b.filed, &b.end)))?;
    let total: f64 = list.iter().filter(|f| f.accn == latest.accn && f.end == latest.end).map(|f| f.val).sum();
    Some((total, date(&latest.end)?))
}

/// The latest value of a `us-gaap` share count (instant or duration).
fn gaap_shares(gaap: &HashMap<String, Concept>, tag: &str) -> Option<(f64, NaiveDate)> {
    let list = gaap.get(tag)?.units.get("shares")?;
    let f =
        list.iter().filter(|f| date(&f.end).is_some()).max_by(|a, b| (&a.end, &a.filed).cmp(&(&b.end, &b.filed)))?;
    Some((f.val, date(&f.end)?))
}

/// Shares outstanding: the cover page when current, else (multi-class filers report the
/// cover count per class, which company facts omit) the balance sheet or diluted average.
fn shares_outstanding(
    facts: &HashMap<String, HashMap<String, Concept>>,
    gaap: &HashMap<String, Concept>,
    latest_period: Option<NaiveDate>,
) -> Option<(f64, NaiveDate, &'static str)> {
    let current = |d: NaiveDate| latest_period.is_none_or(|p| days(d, p) < 200);
    if let Some((v, d)) = cover_shares(facts).filter(|(v, d)| *v > 0.0 && current(*d)) {
        return Some((v, d, "cover page"));
    }
    [
        ("CommonStockSharesOutstanding", "balance sheet"),
        ("WeightedAverageNumberOfDilutedSharesOutstanding", "diluted average"),
    ]
    .into_iter()
    .filter_map(|(tag, src)| gaap_shares(gaap, tag).filter(|(v, d)| *v > 0.0 && current(*d)).map(|(v, d)| (v, d, src)))
    .max_by_key(|x| x.1)
}

fn ratio(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    let (a, b) = (a?, b?);
    (b.abs() > f64::EPSILON).then(|| a / b)
}

/// Fill derived lines and ratios. `years` is the period length in years (1 or 0.25), used
/// to annualise return on equity.
pub(crate) fn derive(v: &mut BTreeMap<Metric, f64>, years: f64) {
    use Metric::*;
    let g = |v: &BTreeMap<Metric, f64>, m| v.get(&m).copied();
    if !v.contains_key(&GrossProfit)
        && let (Some(r), Some(c)) = (g(v, Revenue), g(v, CostOfRevenue))
    {
        v.insert(GrossProfit, r - c);
    }
    if let (Some(o), Some(c)) = (g(v, OperatingCashFlow), g(v, Capex)) {
        v.insert(FreeCashFlow, o - c);
    }
    let rev = g(v, Revenue).filter(|r| *r > 0.0);
    let pairs = [
        (GrossMargin, g(v, GrossProfit), rev),
        (OperatingMargin, g(v, OperatingIncome), rev),
        (NetMargin, g(v, NetIncome), rev),
        (FcfMargin, g(v, FreeCashFlow), rev),
        (Roe, g(v, NetIncome).map(|n| n / years), g(v, Equity).filter(|e| *e > 0.0)),
        (DebtToEquity, g(v, Debt), g(v, Equity).filter(|e| *e > 0.0)),
    ];
    for (m, a, b) in pairs {
        if let Some(x) = ratio(a, b) {
            v.insert(m, x);
        }
    }
}

fn period_label(end: NaiveDate, labels: &HashMap<NaiveDate, (i32, String)>, annual: bool) -> String {
    let label =
        labels.get(&end).or_else(|| labels.iter().filter(|(d, _)| days(**d, end).abs() <= 7).map(|(_, l)| l).next());
    match (label, annual) {
        (Some((fy, _)), true) => format!("FY{fy}"),
        (Some((fy, fp)), false) if fp == "FY" => format!("Q4 FY{:02}", fy % 100),
        (Some((fy, fp)), false) => format!("{fp} FY{:02}", fy % 100),
        (None, true) => format!("FY{}", end.format("%Y")),
        (None, false) => end.format("%b %Y").to_string(),
    }
}

/// Parse a `companyfacts/CIK##########.json` document into annual and quarterly statements.
pub fn parse_companyfacts(json: &str) -> Result<Financials> {
    let cf: CompanyFacts = serde_json::from_str(json).context("companyfacts JSON")?;
    let empty = HashMap::new();
    let gaap = cf.facts.get("us-gaap").unwrap_or(&empty);
    if gaap.is_empty() {
        bail!("no US GAAP financial data (foreign filers report under IFRS)");
    }
    let labels = fiscal_labels(gaap);

    let mut annual: BTreeMap<Metric, BTreeMap<NaiveDate, f64>> = BTreeMap::new();
    let mut quarterly: BTreeMap<Metric, BTreeMap<NaiveDate, f64>> = BTreeMap::new();
    let mut instants: BTreeMap<Metric, BTreeMap<NaiveDate, f64>> = BTreeMap::new();
    let mut debt_parts: Vec<BTreeMap<NaiveDate, f64>> = Vec::new();
    for m in Metric::ALL.into_iter().filter(|m| !m.tags().is_empty()) {
        let obs = observations(gaap, m);
        match m.kind() {
            Kind::Instant => {
                instants.insert(m, obs.iter().map(|o| (o.end, o.val)).collect());
            }
            k => {
                annual.insert(m, annual_values(&obs));
                quarterly.insert(m, quarterly_values(&obs, k == Kind::Flow));
            }
        }
    }
    // Debt: long-term debt (or its current + noncurrent parts) plus short-term borrowings.
    for tags in [
        &["LongTermDebt"][..],
        &["LongTermDebtNoncurrent"],
        &["LongTermDebtCurrent"],
        &["CommercialPaper", "ShortTermBorrowings"],
    ] {
        let mut map = BTreeMap::new();
        for tag in tags.iter().rev() {
            if let Some(facts) = gaap.get(*tag).and_then(|c| c.units.get("USD")) {
                let mut latest: BTreeMap<NaiveDate, (NaiveDate, f64)> = BTreeMap::new();
                for f in facts.iter().filter(|f| f.start.is_none()) {
                    let (Some(end), Some(filed)) = (date(&f.end), date(&f.filed)) else { continue };
                    let e = latest.entry(end).or_insert((filed, f.val));
                    if filed >= e.0 {
                        *e = (filed, f.val);
                    }
                }
                map.extend(latest.into_iter().map(|(d, (_, v))| (d, v)));
            }
        }
        debt_parts.push(map);
    }
    let debt_at = |end: NaiveDate| -> Option<f64> {
        let lt = near(&debt_parts[0], end).or_else(|| {
            let (n, c) = (near(&debt_parts[1], end), near(&debt_parts[2], end));
            (n.is_some() || c.is_some()).then(|| n.unwrap_or(0.0) + c.unwrap_or(0.0))
        });
        let st = near(&debt_parts[3], end);
        (lt.is_some() || st.is_some()).then(|| lt.unwrap_or(0.0) + st.unwrap_or(0.0))
    };

    let key = [Metric::Revenue, Metric::NetIncome, Metric::OperatingIncome, Metric::OperatingCashFlow];
    let ends = |src: &BTreeMap<Metric, BTreeMap<NaiveDate, f64>>| -> BTreeSet<NaiveDate> {
        key.iter().filter_map(|m| src.get(m)).flat_map(|v| v.keys().copied()).collect()
    };
    let build = |src: &BTreeMap<Metric, BTreeMap<NaiveDate, f64>>, is_annual: bool| -> Vec<Period> {
        // Collapse period ends a few days apart (52/53-week calendars, odd tagging).
        let mut out: Vec<Period> = Vec::new();
        for end in ends(src) {
            if out.last().is_some_and(|p| days(p.end, end) < 20) {
                continue;
            }
            let mut values = BTreeMap::new();
            for (m, series) in src {
                if let Some(v) = series.get(&end) {
                    values.insert(*m, *v);
                }
            }
            for (m, series) in &instants {
                if let Some(v) = near(series, end) {
                    values.insert(*m, v);
                }
            }
            if let Some(d) = debt_at(end) {
                values.insert(Metric::Debt, d);
            }
            derive(&mut values, if is_annual { 1.0 } else { 0.25 });
            out.push(Period { end, label: period_label(end, &labels, is_annual), values });
        }
        out
    };
    let (annual, quarterly) = (build(&annual, true), build(&quarterly, false));
    let latest = annual.iter().chain(&quarterly).map(|p| p.end).max();
    let shares = shares_outstanding(&cf.facts, gaap, latest);
    Ok(Financials {
        entity: cf.entity_name,
        annual,
        quarterly,
        shares_outstanding: shares.map(|s| s.0),
        shares_as_of: shares.map(|s| s.1),
        shares_source: shares.map_or("", |s| s.2),
        quarterly_ytd: false,
    })
}

// ---- network + cache ----------------------------------------------------------------------

/// Everything DES and FA show for one company.
#[derive(Debug, Clone, PartialEq)]
pub struct Company {
    pub ticker: String,
    pub profile: CompanyProfile,
    /// Statements, or why they are unavailable (no XBRL data, IFRS filer…).
    pub financials: Result<Financials, String>,
}

/// The User-Agent to use: the Settings value when set, else [`UA_ENV`].
pub fn user_agent(setting: &str) -> Option<String> {
    let s = setting.trim();
    if !s.is_empty() {
        return Some(s.to_string());
    }
    std::env::var(UA_ENV).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// SEC's contact requirement: the User-Agent must carry an email address.
pub fn valid_user_agent(ua: &str) -> bool {
    ua.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | ','));
        w.split_once('@').is_some_and(|(a, d)| !a.is_empty() && d.contains('.'))
    })
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(40)).gzip(true).build().unwrap_or_default())
}

/// Process-wide SEC request throttle.
async fn throttle() {
    static NEXT: Mutex<Option<Instant>> = Mutex::const_new(None);
    let mut next = NEXT.lock().await;
    let now = Instant::now();
    if let Some(t) = *next
        && t > now
    {
        tokio::time::sleep(t - now).await;
    }
    *next = Some(Instant::now() + MIN_GAP);
}

async fn fetch(url: &str, ua: &str) -> Result<String> {
    throttle().await;
    let resp = client()
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/json")
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

/// A disk-cached GET: fresh files are served without a request; on a network error a stale
/// file is served rather than nothing.
async fn cached(dir: &Path, name: &str, ttl: Duration, url: &str, ua: &str) -> Result<String> {
    let path = dir.join(name);
    let age =
        std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if age.is_some_and(|a| a < ttl)
        && let Ok(s) = std::fs::read_to_string(&path)
    {
        return Ok(s);
    }
    match fetch(url, ua).await {
        Ok(body) => {
            let _ = std::fs::create_dir_all(dir);
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
            Ok(body)
        }
        Err(e) => match std::fs::read_to_string(&path) {
            Ok(s) if !e.to_string().contains("not found") => {
                tracing::warn!("EDGAR {url}: {e:#}; serving cached copy");
                Ok(s)
            }
            _ => Err(e),
        },
    }
}

async fn ticker_map(dir: &Path, ua: &str) -> Result<Arc<HashMap<String, u64>>> {
    type Map = Arc<HashMap<String, u64>>;
    static MAP: Mutex<Option<(Instant, Map)>> = Mutex::const_new(None);
    let mut g = MAP.lock().await;
    if let Some((at, m)) = g.as_ref()
        && at.elapsed() < TICKERS_TTL
    {
        return Ok(m.clone());
    }
    let json = cached(dir, "company_tickers.json", TICKERS_TTL, TICKERS_URL, ua).await?;
    let m = Arc::new(parse_tickers(&json)?);
    *g = Some((Instant::now(), m.clone()));
    Ok(m)
}

/// The CIK for an EDGAR ticker.
pub async fn cik(dir: &Path, ticker: &str, ua: &str) -> Result<u64> {
    let map = ticker_map(dir, ua).await?;
    map.get(&ticker.to_ascii_uppercase())
        .or_else(|| map.get(&ticker.replace(['.', '/'], "-").to_ascii_uppercase()))
        .copied()
        .ok_or_else(|| anyhow!("{ticker} is not an SEC registrant (no EDGAR filings)"))
}

/// The raw `submissions` JSON document for a ticker (throttled, cached).
pub async fn submissions_json(dir: &Path, ticker: &str, ua: &str) -> Result<String> {
    if !valid_user_agent(ua) {
        bail!("set an SEC User-Agent with a contact email (Settings › Panels, or {UA_ENV})");
    }
    let cik = cik(dir, ticker, ua).await?;
    let url = format!("https://data.sec.gov/submissions/CIK{cik:010}.json");
    cached(dir, &format!("submissions_{cik}.json"), SUBMISSIONS_TTL, &url, ua).await
}

/// The raw XBRL `companyfacts` JSON document for a ticker (throttled, cached).
pub async fn companyfacts_json(dir: &Path, ticker: &str, ua: &str) -> Result<String> {
    if !valid_user_agent(ua) {
        bail!("set an SEC User-Agent with a contact email (Settings › Panels, or {UA_ENV})");
    }
    let cik = cik(dir, ticker, ua).await?;
    let url = format!("https://data.sec.gov/api/xbrl/companyfacts/CIK{cik:010}.json");
    cached(dir, &format!("companyfacts_{cik}.json"), FACTS_TTL, &url, ua).await
}

/// The cache directory under the app's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("edgar")
}

/// Load the profile and financial statements for a market key.
pub async fn company(dir: &Path, symbol: &str, ua: &str) -> Result<Company> {
    let Some(ticker) = edgar_ticker(symbol) else {
        bail!("SEC EDGAR covers US-listed companies only; {} is not a US equity", crate::security(symbol).key());
    };
    if !valid_user_agent(ua) {
        bail!("set an SEC User-Agent with a contact email (Settings › Panels, or {UA_ENV})");
    }
    let cik = cik(dir, &ticker, ua).await?;
    let sub_url = format!("https://data.sec.gov/submissions/CIK{cik:010}.json");
    let facts_url = format!("https://data.sec.gov/api/xbrl/companyfacts/CIK{cik:010}.json");
    let sub = cached(dir, &format!("submissions_{cik}.json"), SUBMISSIONS_TTL, &sub_url, ua).await?;
    let profile = parse_submissions(&sub)?;
    let financials = match cached(dir, &format!("companyfacts_{cik}.json"), FACTS_TTL, &facts_url, ua).await {
        Ok(json) => parse_companyfacts(&json).map_err(|e| format!("{e:#}")),
        Err(e) if e.to_string().contains("not found") => Err("no XBRL financial statements on EDGAR".into()),
        Err(e) => Err(format!("{e:#}")),
    };
    Ok(Company { ticker, profile, financials })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}/{name}")).unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        date(s).unwrap()
    }

    #[test]
    fn tickers_map_to_ciks() {
        let m = parse_tickers(&fixture("sec_company_tickers.json")).unwrap();
        assert_eq!(m.get("AAPL"), Some(&320193));
        assert_eq!(m.get("BRK-B"), Some(&1067983));
        assert!(m.contains_key("GOOGL") && m.contains_key("GOOG"));
    }

    #[test]
    fn us_equities_only() {
        assert_eq!(edgar_ticker("AAPL US Equity").as_deref(), Some("AAPL"));
        assert_eq!(edgar_ticker("BRK.B").as_deref(), Some("BRK-B"));
        assert_eq!(edgar_ticker("BTCUSD Curncy"), None);
        assert_eq!(edgar_ticker("SPX Index"), None);
        assert_eq!(edgar_ticker("US10Y Govt"), None);
        assert_eq!(edgar_ticker("THYAO.IS"), None);
        assert_eq!(edgar_ticker("SAP.DE"), None);
    }

    #[test]
    fn user_agent_needs_contact() {
        assert!(valid_user_agent("Jane Doe jane@example.com"));
        assert!(valid_user_agent("terminal-rs (ops@example.org)"));
        assert!(!valid_user_agent("terminal-rs/0.1"));
        assert!(!valid_user_agent(""));
    }

    #[test]
    fn submissions_profile() {
        let p = parse_submissions(&fixture("sec_submissions_aapl.json")).unwrap();
        assert_eq!(p.cik, 320193);
        assert_eq!(p.name, "Apple Inc.");
        assert_eq!(p.sic, "3571");
        assert_eq!(p.sic_description, "Electronic Computers");
        assert!(p.tickers.contains(&"AAPL".to_string()));
        assert!(p.exchanges.iter().any(|e| e == "Nasdaq"));
        assert_eq!(p.fiscal_year_end_label(), "Sep 26");
        assert_eq!(p.state_of_incorporation, "CA");
        assert!(p.business_address.contains("CUPERTINO"), "{}", p.business_address);
        assert!(p.filings.windows(2).all(|w| w[0].filed >= w[1].filed));
        let k = p.filings_of(&["10-K"]).next().expect("a 10-K");
        assert!(k.url.starts_with("https://www.sec.gov/Archives/edgar/data/320193/"), "{}", k.url);
        assert!(k.url.ends_with(".htm"), "{}", k.url);
        assert!(p.filings_of(&["10-Q"]).count() >= 2);
        assert!(p.filings_of(&["8-K"]).any(|f| !f.items.is_empty()));
    }

    #[test]
    fn annual_statements() {
        let f = parse_companyfacts(&fixture("sec_companyfacts_aapl.json")).unwrap();
        assert_eq!(f.entity, "Apple Inc.");
        let fy25 = f.annual.iter().find(|p| p.end == d("2025-09-27")).expect("FY2025");
        assert_eq!(fy25.label, "FY2025");
        assert_eq!(fy25.get(Metric::NetIncome), Some(112_010_000_000.0));
        assert_eq!(fy25.get(Metric::TotalAssets), Some(359_241_000_000.0));
        assert_eq!(fy25.get(Metric::OperatingCashFlow), Some(111_482_000_000.0));
        let rev = fy25.get(Metric::Revenue).unwrap();
        assert!(rev > 400e9 && rev < 450e9, "{rev}");
        let gm = fy25.get(Metric::GrossMargin).unwrap();
        assert!(gm > 0.4 && gm < 0.5, "{gm}");
        let fcf = fy25.get(Metric::FreeCashFlow).unwrap();
        assert!((fcf - (fy25.get(Metric::OperatingCashFlow).unwrap() - fy25.get(Metric::Capex).unwrap())).abs() < 1.0);
        assert!(fy25.get(Metric::EpsDiluted).is_some_and(|e| e > 5.0 && e < 10.0));
        assert!(fy25.get(Metric::Debt).is_some_and(|x| x > 50e9));
        assert!(fy25.get(Metric::DebtToEquity).is_some());
        // One period per fiscal year, ascending, with year-over-year neighbours.
        assert!(f.annual.windows(2).all(|w| days(w[0].end, w[1].end) > 300));
        let i = f.annual.iter().position(|p| p.end == d("2025-09-27")).unwrap();
        assert_eq!(prior_year(&f.annual, i).map(|p| p.end), Some(d("2024-09-28")));
        assert_eq!(prior_year(&f.annual, i).unwrap().get(Metric::NetIncome), Some(93_736_000_000.0));
    }

    #[test]
    fn quarterly_statements_with_derived_q4() {
        let f = parse_companyfacts(&fixture("sec_companyfacts_aapl.json")).unwrap();
        let q = |end: &str| f.quarterly.iter().find(|p| p.end == d(end)).unwrap_or_else(|| panic!("{end}"));
        // Reported quarter.
        assert_eq!(q("2026-06-27").get(Metric::NetIncome), Some(29_789_000_000.0));
        assert_eq!(q("2026-06-27").label, "Q3 FY26");
        // Cash flow is only reported year-to-date: Q3 = 9M − 6M.
        assert_eq!(q("2026-06-27").get(Metric::OperatingCashFlow), Some(116_996_000_000.0 - 82_627_000_000.0));
        // Q4 = FY − 9M.
        let q4 = q("2025-09-27");
        assert_eq!(q4.label, "Q4 FY25");
        assert_eq!(q4.get(Metric::NetIncome), Some(112_010_000_000.0 - 84_544_000_000.0));
        assert_eq!(q4.get(Metric::TotalAssets), Some(359_241_000_000.0));
        // EPS isn't additive, so Q4 EPS is not invented.
        assert_eq!(q4.get(Metric::EpsDiluted), None);
        assert!(q("2026-06-27").get(Metric::EpsDiluted).is_some());
        assert!(f.quarterly.windows(2).all(|w| is_quarter_len(days(w[0].end, w[1].end))), "contiguous quarters");
        let i = f.quarterly.iter().position(|p| p.end == d("2026-06-27")).unwrap();
        assert_eq!(prior_year(&f.quarterly, i).map(|p| p.end), Some(d("2025-06-28")));
    }

    #[test]
    fn valuation_from_live_price() {
        let f = parse_companyfacts(&fixture("sec_companyfacts_aapl.json")).unwrap();
        assert_eq!(f.shares_outstanding, Some(14_594_180_000.0));
        assert_eq!(f.shares_as_of, Some(d("2026-07-17")));
        assert_eq!(f.shares_source, "cover page");
        // TTM = Q4 FY25 + Q1..Q3 FY26.
        let ttm = f.ttm(Metric::NetIncome).unwrap();
        assert_eq!(ttm, 112_010_000_000.0 - 84_544_000_000.0 + 101_464_000_000.0);
        let v = f.valuation(Some(250.0));
        let mcap = v.market_cap.unwrap();
        assert!((mcap - 250.0 * 14_594_180_000.0).abs() < 1.0);
        assert!((v.pe.unwrap() - mcap / ttm).abs() < 1e-9);
        assert!(v.ps.unwrap() > 1.0 && v.pb.unwrap() > 1.0);
        assert!(v.enterprise_value.unwrap() > mcap - 100e9);
        assert_eq!(f.valuation(None).market_cap, None);
    }

    #[test]
    fn duplicates_prefer_priority_then_latest_filing() {
        let json = r#"{"entityName":"X","facts":{"us-gaap":{
            "Revenues":{"units":{"USD":[
                {"start":"2023-01-01","end":"2023-12-31","val":100,"fy":2023,"fp":"FY","form":"10-K","filed":"2024-02-01","accn":"a"},
                {"start":"2023-01-01","end":"2023-12-31","val":110,"fy":2024,"fp":"FY","form":"10-K","filed":"2025-02-01","accn":"b"},
                {"start":"2024-01-01","end":"2024-12-31","val":120,"fy":2024,"fp":"FY","form":"10-K","filed":"2025-02-01","accn":"b"}]}},
            "RevenueFromContractWithCustomerExcludingAssessedTax":{"units":{"USD":[
                {"start":"2023-01-01","end":"2023-12-31","val":999,"fy":2023,"fp":"FY","form":"10-K","filed":"2024-02-01","accn":"a"},
                {"start":"2022-01-01","end":"2022-12-31","val":90,"fy":2022,"fp":"FY","form":"10-K","filed":"2023-02-01","accn":"c"}]}}
        }}}"#;
        let f = parse_companyfacts(json).unwrap();
        let vals: Vec<_> = f.annual.iter().map(|p| (p.label.as_str(), p.get(Metric::Revenue))).collect();
        assert_eq!(vals, vec![("FY2022", Some(90.0)), ("FY2023", Some(110.0)), ("FY2024", Some(120.0))]);
        assert!(parse_companyfacts(r#"{"entityName":"Y","facts":{"ifrs-full":{}}}"#).is_err());
        // A stale cover page (multi-class filers) falls back to the balance-sheet count.
        assert_eq!(f.shares_outstanding, None);
        let json = r#"{"entityName":"Z","facts":{
            "dei":{"EntityCommonStockSharesOutstanding":{"units":{"shares":[{"end":"2011-04-29","val":941481,"filed":"2011-05-01","accn":"o"}]}}},
            "us-gaap":{
            "Revenues":{"units":{"USD":[{"start":"2024-01-01","end":"2024-12-31","val":5,"fy":2024,"fp":"FY","filed":"2025-02-01","accn":"b"}]}},
            "CommonStockSharesOutstanding":{"units":{"shares":[{"end":"2024-12-31","val":1500000,"filed":"2025-02-01","accn":"b"}]}}}}}"#;
        let f = parse_companyfacts(json).unwrap();
        assert_eq!((f.shares_outstanding, f.shares_source), (Some(1_500_000.0), "balance sheet"));
    }
}
