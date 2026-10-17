//! US ETF contents (`ETF`): holdings, sector weights, reverse search and fund overlap.
//!
//! Sources (all free, no key):
//! - SEC **N-PORT-P** filings. `www.sec.gov/files/company_tickers_mf.json` maps an ETF ticker to
//!   its fund family (CIK) and series id; the EDGAR filing feed for the series names the newest
//!   `NPORT-P`, whose `primary_doc.xml` lists every position (name, CUSIP, value, `pctVal`).
//!   Filed quarterly with a 60 day lag, so the holdings are always a few months old: the report
//!   date is kept and shown. Same SEC User-Agent, throttle and disk cache as [`crate::edgar`].
//! - **Yahoo Finance** `quoteSummary` (`topHoldings`, the ordinary crumb session the options
//!   chain uses): the ten largest holdings with tickers and the sector weights. It is the only
//!   source for funds that file no N-PORT (grantor trusts like GLD, UITs like SPY and DIA) and
//!   the sector breakdown for all funds. N-PORT carries no sectors.
//!
//! Holding names are resolved to tickers best effort: Yahoo's own tickers for its top ten, then
//! an exact match of the normalised issuer name against SEC `company_tickers.json`. A name
//! that matches nothing, or two different companies, keeps no ticker.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::edgar;

const MF_URL: &str = "https://www.sec.gov/files/company_tickers_mf.json";
const MF_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// How long a cached fund is served without asking the SEC whether a newer N-PORT exists.
const NPORT_TTL: i64 = 3 * 24 * 3600;
const YAHOO_TTL: i64 = 24 * 3600;

/// A curated starting list: ticker, name, category.
pub const CURATED: &[(&str, &str, &str)] = &[
    ("SPY", "SPDR S&P 500 ETF Trust", "Broad market"),
    ("VOO", "Vanguard S&P 500 ETF", "Broad market"),
    ("IVV", "iShares Core S&P 500 ETF", "Broad market"),
    ("VTI", "Vanguard Total Stock Market ETF", "Broad market"),
    ("QQQ", "Invesco QQQ Trust", "Broad market"),
    ("IWM", "iShares Russell 2000 ETF", "Broad market"),
    ("DIA", "SPDR Dow Jones Industrial Average ETF", "Broad market"),
    ("VUG", "Vanguard Growth ETF", "Style"),
    ("VTV", "Vanguard Value ETF", "Style"),
    ("SCHD", "Schwab US Dividend Equity ETF", "Dividend"),
    ("VIG", "Vanguard Dividend Appreciation ETF", "Dividend"),
    ("VYM", "Vanguard High Dividend Yield ETF", "Dividend"),
    ("JEPI", "JPMorgan Equity Premium Income ETF", "Income"),
    ("XLK", "Technology Select Sector SPDR", "Sector"),
    ("XLF", "Financial Select Sector SPDR", "Sector"),
    ("XLE", "Energy Select Sector SPDR", "Sector"),
    ("XLV", "Health Care Select Sector SPDR", "Sector"),
    ("XLY", "Consumer Discretionary Select Sector SPDR", "Sector"),
    ("XLP", "Consumer Staples Select Sector SPDR", "Sector"),
    ("XLI", "Industrial Select Sector SPDR", "Sector"),
    ("XLU", "Utilities Select Sector SPDR", "Sector"),
    ("XLB", "Materials Select Sector SPDR", "Sector"),
    ("XLRE", "Real Estate Select Sector SPDR", "Sector"),
    ("XLC", "Communication Services Select Sector SPDR", "Sector"),
    ("VGT", "Vanguard Information Technology ETF", "Sector"),
    ("SMH", "VanEck Semiconductor ETF", "Sector"),
    ("VNQ", "Vanguard Real Estate ETF", "Sector"),
    ("ARKK", "ARK Innovation ETF", "Thematic"),
    ("VEA", "Vanguard FTSE Developed Markets ETF", "International"),
    ("VWO", "Vanguard FTSE Emerging Markets ETF", "International"),
    ("EFA", "iShares MSCI EAFE ETF", "International"),
    ("EEM", "iShares MSCI Emerging Markets ETF", "International"),
    ("AGG", "iShares Core US Aggregate Bond ETF", "Bonds"),
    ("BND", "Vanguard Total Bond Market ETF", "Bonds"),
    ("TLT", "iShares 20+ Year Treasury Bond ETF", "Bonds"),
    ("LQD", "iShares iBoxx $ Investment Grade Corporate Bond ETF", "Bonds"),
    ("HYG", "iShares iBoxx $ High Yield Corporate Bond ETF", "Bonds"),
    ("GLD", "SPDR Gold Shares", "Commodity"),
    ("SLV", "iShares Silver Trust", "Commodity"),
];

// ---- model --------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Full portfolio from an SEC N-PORT-P filing.
    Nport,
    /// Only the ten largest holdings, from Yahoo Finance.
    Yahoo,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Nport => "SEC N-PORT",
            Source::Yahoo => "Yahoo Finance (top 10)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holding {
    pub name: String,
    /// Resolved ticker (`BRK-B` style), when known.
    #[serde(default)]
    pub ticker: Option<String>,
    #[serde(default)]
    pub cusip: Option<String>,
    /// Percent of net assets.
    pub weight: f64,
    /// Position value in USD.
    #[serde(default)]
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sector {
    pub label: String,
    /// Percent of the equity portfolio.
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fund {
    pub ticker: String,
    pub name: String,
    pub source: Source,
    /// Reporting date of the holdings (N-PORT) or `None` when the source does not say (Yahoo).
    #[serde(default)]
    pub as_of: Option<NaiveDate>,
    /// When the SEC accepted the filing.
    #[serde(default)]
    pub filed: Option<NaiveDate>,
    #[serde(default)]
    pub accession: Option<String>,
    #[serde(default)]
    pub net_assets: Option<f64>,
    /// Largest first.
    pub holdings: Vec<Holding>,
    /// Number of positions in the filing (holdings may be trimmed of zero weights).
    #[serde(default)]
    pub positions: usize,
    #[serde(default)]
    pub sectors: Vec<Sector>,
    /// Unix seconds when the data was last fetched.
    #[serde(default)]
    pub updated: i64,
}

impl Fund {
    /// Only the largest holdings are known (Yahoo): reverse search and overlap are partial.
    pub fn partial(&self) -> bool {
        self.source == Source::Yahoo
    }

    pub fn resolved(&self) -> usize {
        self.holdings.iter().filter(|h| h.ticker.is_some()).count()
    }

    /// The ten largest holdings and the rest as one weight (percent).
    pub fn top_and_other(&self, n: usize) -> (&[Holding], f64) {
        let top = &self.holdings[..self.holdings.len().min(n)];
        let shown: f64 = top.iter().map(|h| h.weight).sum();
        (top, (100.0 - shown).max(0.0))
    }

    fn ttl(&self) -> i64 {
        if self.source == Source::Nport { NPORT_TTL } else { YAHOO_TTL }
    }

    pub fn stale(&self, now: i64) -> bool {
        now - self.updated > self.ttl()
    }
}

// ---- N-PORT parsing -----------------------------------------------------------------------

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.trim().to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Text of the first `<tag>…</tag>` in `s`.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{name}>"))? + start;
    Some(&s[start..end])
}

/// Value of the first `<tag attr="…"` in `s`.
fn attr<'a>(s: &'a str, name: &str, attr: &str) -> Option<&'a str> {
    let open = format!("<{name} {attr}=\"");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find('"')? + start;
    Some(&s[start..end])
}

fn num(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())
}

fn date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim().get(..10)?, "%Y-%m-%d").ok()
}

/// A parsed `primary_doc.xml`.
#[derive(Debug, Clone, PartialEq)]
pub struct NportDoc {
    pub series_id: String,
    pub series_name: String,
    pub registrant: String,
    pub period_end: Option<NaiveDate>,
    pub net_assets: Option<f64>,
    /// Long positions with a positive weight, in filing order.
    pub holdings: Vec<Holding>,
    pub positions: usize,
}

/// Parse an N-PORT-P `primary_doc.xml`.
pub fn parse_nport(xml: &str) -> Result<NportDoc> {
    let gen_info = tag(xml, "genInfo").context("not an N-PORT filing (no genInfo)")?;
    let series_name = unescape(tag(gen_info, "seriesName").unwrap_or_default());
    let fund_info = xml.split("<fundInfo>").nth(1).unwrap_or_default();
    let net_assets = num(tag(fund_info, "netAssets"));
    let mut holdings = Vec::new();
    let mut positions = 0;
    for chunk in xml.split("<invstOrSec>").skip(1) {
        let body = chunk.split("</invstOrSec>").next().unwrap_or(chunk);
        positions += 1;
        if tag(body, "payoffProfile").is_some_and(|p| p.eq_ignore_ascii_case("short")) {
            continue;
        }
        let Some(weight) = num(tag(body, "pctVal")).filter(|w| *w > 0.0) else { continue };
        let name = unescape(tag(body, "name").or_else(|| tag(body, "title")).unwrap_or_default());
        if name.is_empty() {
            continue;
        }
        let cusip = tag(body, "cusip")
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| !c.is_empty() && c != "N/A" && c.chars().any(|ch| ch != '0'));
        let ticker = attr(body, "ticker", "value").map(|t| t.trim().to_ascii_uppercase()).filter(|t| !t.is_empty());
        holdings.push(Holding { name, ticker, cusip, weight, value: num(tag(body, "valUSD")) });
    }
    Ok(NportDoc {
        series_id: tag(gen_info, "seriesId").unwrap_or_default().trim().to_string(),
        series_name,
        registrant: unescape(tag(gen_info, "regName").unwrap_or_default()),
        period_end: tag(gen_info, "repPdDate").and_then(date),
        net_assets,
        holdings,
        positions,
    })
}

/// One filing from the EDGAR atom feed of a series.
#[derive(Debug, Clone, PartialEq)]
pub struct NportFiling {
    pub accession: String,
    pub filed: NaiveDate,
    pub cik: u64,
}

impl NportFiling {
    pub fn url(&self) -> String {
        format!(
            "https://www.sec.gov/Archives/edgar/data/{}/{}/primary_doc.xml",
            self.cik,
            self.accession.replace('-', "")
        )
    }
}

/// The `NPORT-P` filings in a `browse-edgar … output=atom` feed.
pub fn parse_filings(atom: &str) -> Vec<NportFiling> {
    atom.split("<entry>")
        .skip(1)
        .filter_map(|e| {
            if !tag(e, "filing-type")?.trim().starts_with("NPORT-P") {
                return None;
            }
            let href = tag(e, "filing-href")?;
            let cik = href.split("/data/").nth(1)?.split('/').next()?.parse().ok()?;
            Some(NportFiling {
                accession: tag(e, "accession-number")?.trim().to_string(),
                filed: date(tag(e, "filing-date")?)?,
                cik,
            })
        })
        .collect()
}

/// ETF ticker → (fund family CIK, series id) from `company_tickers_mf.json`.
pub fn parse_mf(json: &str) -> Result<HashMap<String, (u64, String)>> {
    #[derive(Deserialize)]
    struct Mf {
        data: Vec<(u64, String, String, String)>,
    }
    let mf: Mf = serde_json::from_str(json).context("company_tickers_mf.json")?;
    let mut out = HashMap::with_capacity(mf.data.len());
    for (cik, series, _class, symbol) in mf.data {
        out.entry(symbol.to_ascii_uppercase()).or_insert((cik, series));
    }
    Ok(out)
}

// ---- Yahoo parsing ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct YahooFund {
    pub name: Option<String>,
    pub holdings: Vec<Holding>,
    pub sectors: Vec<Sector>,
}

fn sector_label(key: &str) -> String {
    match key {
        "realestate" => "Real Estate",
        "consumer_cyclical" => "Consumer Cyclical",
        "basic_materials" => "Basic Materials",
        "consumer_defensive" => "Consumer Defensive",
        "technology" => "Technology",
        "communication_services" => "Communication Services",
        "financial_services" => "Financial Services",
        "utilities" => "Utilities",
        "industrials" => "Industrials",
        "energy" => "Energy",
        "healthcare" => "Healthcare",
        other => return other.replace('_', " "),
    }
    .into()
}

/// Parse a `quoteSummary?modules=topHoldings,price` response.
pub fn parse_yahoo(json: &str) -> Result<YahooFund> {
    let v: serde_json::Value = serde_json::from_str(json).context("yahoo quoteSummary")?;
    let r = v.pointer("/quoteSummary/result/0").ok_or_else(|| anyhow!("yahoo returned no fund data"))?;
    let raw = |x: &serde_json::Value| x.get("raw").and_then(|r| r.as_f64());
    let mut out = YahooFund {
        name: r
            .pointer("/price/longName")
            .or_else(|| r.pointer("/price/shortName"))
            .and_then(|n| n.as_str())
            .map(str::to_string),
        ..Default::default()
    };
    if let Some(top) = r.get("topHoldings") {
        for h in top.get("holdings").and_then(|h| h.as_array()).into_iter().flatten() {
            let Some(w) = h.get("holdingPercent").and_then(raw).filter(|w| *w > 0.0) else { continue };
            let name = h.get("holdingName").and_then(|n| n.as_str()).unwrap_or_default().to_string();
            let ticker = h
                .get("symbol")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.replace('.', "-").to_ascii_uppercase());
            out.holdings.push(Holding { name, ticker, cusip: None, weight: w * 100.0, value: None });
        }
        for s in top.get("sectorWeightings").and_then(|s| s.as_array()).into_iter().flatten() {
            for (k, val) in s.as_object().into_iter().flatten() {
                if let Some(w) = raw(val).filter(|w| *w > 0.0) {
                    out.sectors.push(Sector { label: sector_label(k), weight: w * 100.0 });
                }
            }
        }
        out.sectors.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    }
    Ok(out)
}

// ---- name → ticker ------------------------------------------------------------------------

/// Issuer name reduced to what two sources agree on: lower case, no punctuation, no corporate
/// suffixes (`Inc`, `Corp`, `Holdings`…). `keep_class` keeps share-class words (`Class C`).
pub fn norm_name(s: &str, keep_class: bool) -> String {
    const DROP: [&str; 19] = [
        "inc",
        "incorporated",
        "corp",
        "corporation",
        "co",
        "cos",
        "company",
        "companies",
        "ltd",
        "limited",
        "plc",
        "the",
        "holdings",
        "holding",
        "group",
        "sa",
        "nv",
        "ag",
        "lp",
    ];
    // `Charles Schwab Corp/The`, `DANAHER CORP /DE/`, `COSTCO WHOLESALE CORP /NEW`.
    let s = match s.find('/') {
        Some(p) if s[p + 1..].trim().len() <= 5 => &s[..p],
        _ => s,
    };
    let cleaned: String = s
        .to_lowercase()
        .replace(['\'', '’', '.', ','], "")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut out: Vec<&str> = Vec::new();
    let mut words = cleaned.split_whitespace();
    while let Some(w) = words.next() {
        if matches!(w, "class" | "cl") {
            if keep_class {
                out.push("class");
                out.extend(words.next());
            } else {
                words.next();
            }
        } else if !DROP.contains(&w) {
            out.push(w);
        }
    }
    // `US BANCORP DE`: a trailing state of incorporation.
    if out.len() > 1 && matches!(out.last(), Some(&"de" | &"new")) {
        out.pop();
    }
    out.join(" ")
}

/// Normalised SEC registrant name → its tickers (primary listing first); names shared by two
/// companies are left out.
#[derive(Debug, Default)]
pub struct NameMap(HashMap<String, Vec<String>>);

impl NameMap {
    pub fn parse(json: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Row {
            cik_str: u64,
            ticker: String,
            title: String,
        }
        let rows: HashMap<String, Row> = serde_json::from_str(json).context("company_tickers.json")?;
        // The file is ordered by company rank with a company's primary ticker first.
        let mut rows: Vec<(usize, Row)> = rows.into_iter().map(|(k, r)| (k.parse().unwrap_or(usize::MAX), r)).collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.ticker.cmp(&b.1.ticker)));
        let mut seen: HashMap<String, (u64, Vec<String>)> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for (_, r) in rows {
            let key = norm_name(&r.title, false);
            if key.is_empty() {
                continue;
            }
            let ticker = r.ticker.to_ascii_uppercase();
            if !is_common_ticker(&ticker) {
                continue;
            }
            match seen.get_mut(&key) {
                Some((cik, _)) if *cik != r.cik_str => {
                    ambiguous.insert(key);
                }
                Some((_, t)) => t.push(ticker),
                None => {
                    seen.insert(key, (r.cik_str, vec![ticker]));
                }
            }
        }
        Ok(Self(seen.into_iter().filter(|(k, _)| !ambiguous.contains(k)).map(|(k, (_, t))| (k, t)).collect()))
    }

    /// The tickers of the company called `name` (several for dual-class issuers).
    pub fn lookup(&self, name: &str) -> &[String] {
        self.0.get(&norm_name(name, false)).map_or(&[], Vec::as_slice)
    }
}

/// Not a preferred share, warrant, right or unit (`WFC-PY`, `ACAC-WT`).
fn is_common_ticker(t: &str) -> bool {
    let Some((_, suffix)) = t.rsplit_once('-') else { return true };
    !(suffix.starts_with('P') && suffix.len() <= 2 || matches!(suffix, "WT" | "WS" | "W" | "R" | "RT" | "U" | "UN"))
}

/// The share class named in an issuer name (`Class B` → `B`).
fn class_letter(name: &str) -> Option<char> {
    let lower = name.to_lowercase();
    let mut words = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    while let Some(w) = words.next() {
        if matches!(w, "class" | "cl") {
            return words.next().and_then(|c| (c.len() == 1).then(|| c.chars().next())).flatten();
        }
    }
    None
}

/// Fill in missing tickers: Yahoo's top ten (which know share classes) first, then SEC names.
/// A dual-class issuer is resolved by the class in the name, or, when the fund holds every
/// class under one name, by weight order (the larger position takes the primary ticker).
fn resolve_tickers(holdings: &mut [Holding], yahoo: &[Holding], names: &NameMap) {
    let by_name: HashMap<String, &str> =
        yahoo.iter().filter_map(|y| Some((norm_name(&y.name, true), y.ticker.as_deref()?))).collect();
    let mut rank: HashMap<String, usize> = HashMap::new();
    let mut group: HashMap<String, usize> = HashMap::new();
    for h in holdings.iter().filter(|h| h.ticker.is_none() && class_letter(&h.name).is_none()) {
        *group.entry(norm_name(&h.name, false)).or_default() += 1;
    }
    // Holdings are sorted by weight, so the first of a group is its largest.
    for h in holdings.iter_mut().filter(|h| h.ticker.is_none()) {
        if let Some(t) = by_name.get(&norm_name(&h.name, true)) {
            h.ticker = Some(t.to_string());
            continue;
        }
        let tickers = names.lookup(&h.name);
        h.ticker = match tickers {
            [] => None,
            [one] => Some(one.clone()),
            many => match class_letter(&h.name) {
                Some(c) => {
                    let mut hit = many.iter().filter(|t| {
                        let t = t.to_ascii_lowercase();
                        t.ends_with(&format!("-{c}")) || t.ends_with(&format!(".{c}"))
                    });
                    match (hit.next(), hit.next()) {
                        (Some(t), None) => Some(t.clone()),
                        _ => None,
                    }
                }
                None => {
                    let key = norm_name(&h.name, false);
                    let n = rank.entry(key.clone()).or_default();
                    let pick = if group.get(&key).copied() == Some(many.len()) { many.get(*n) } else { many.first() };
                    *n += 1;
                    pick.cloned()
                }
            },
        };
    }
}

fn sort_desc(h: &mut [Holding]) {
    h.sort_by(|a, b| b.weight.total_cmp(&a.weight));
}

/// Combine an N-PORT filing and/or Yahoo data into a [`Fund`].
pub fn build_fund(
    ticker: &str,
    nport: Option<(NportDoc, NportFiling)>,
    yahoo: Option<YahooFund>,
    names: &NameMap,
    now: i64,
) -> Result<Fund> {
    let yahoo_holdings = yahoo.as_ref().map(|y| y.holdings.clone()).unwrap_or_default();
    let sectors = yahoo.as_ref().map(|y| y.sectors.clone()).unwrap_or_default();
    let yahoo_name = yahoo.as_ref().and_then(|y| y.name.clone());
    let ticker = ticker.to_ascii_uppercase();
    if let Some((doc, filing)) = nport.filter(|(d, _)| !d.holdings.is_empty()) {
        let mut holdings = doc.holdings;
        sort_desc(&mut holdings);
        resolve_tickers(&mut holdings, &yahoo_holdings, names);
        return Ok(Fund {
            name: yahoo_name.unwrap_or(if doc.series_name.is_empty() { doc.registrant } else { doc.series_name }),
            ticker,
            source: Source::Nport,
            as_of: doc.period_end,
            filed: Some(filing.filed),
            accession: Some(filing.accession),
            net_assets: doc.net_assets,
            holdings,
            positions: doc.positions,
            sectors,
            updated: now,
        });
    }
    let Some(y) = yahoo.filter(|y| !y.holdings.is_empty()) else {
        bail!("no holdings found for {ticker}: no SEC N-PORT filing and Yahoo lists none");
    };
    let mut holdings = y.holdings;
    sort_desc(&mut holdings);
    Ok(Fund {
        name: y.name.unwrap_or_else(|| ticker.clone()),
        ticker,
        source: Source::Yahoo,
        as_of: None,
        filed: None,
        accession: None,
        net_assets: None,
        positions: holdings.len(),
        holdings,
        sectors: y.sectors,
        updated: now,
    })
}

// ---- analysis -----------------------------------------------------------------------------

fn same_ticker(a: &str, b: &str) -> bool {
    let n = |s: &str| s.replace(['.', '/'], "-").to_ascii_uppercase();
    n(a) == n(b)
}

/// A fund holding a security.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub fund: String,
    pub name: String,
    pub weight: f64,
    pub value: Option<f64>,
    /// The fund lists only its ten largest holdings, so absence from it proves nothing.
    pub partial: bool,
}

/// Which of `funds` hold `symbol` (a ticker or part of an issuer name), by weight.
pub fn reverse_search(funds: &[Arc<Fund>], symbol: &str) -> Vec<Held> {
    let q = symbol.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Held> = funds
        .iter()
        .filter_map(|f| {
            let h = f.holdings.iter().find(|h| h.ticker.as_deref().is_some_and(|t| same_ticker(t, q)))?;
            Some(Held {
                fund: f.ticker.clone(),
                name: f.name.clone(),
                weight: h.weight,
                value: h.value,
                partial: f.partial(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.weight.total_cmp(&a.weight));
    out
}

/// A security held by both funds of an [`overlap`].
#[derive(Debug, Clone, PartialEq)]
pub struct Shared {
    pub name: String,
    pub ticker: Option<String>,
    pub a: f64,
    pub b: f64,
}

impl Shared {
    pub fn common(&self) -> f64 {
        self.a.min(self.b)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Overlap {
    /// Sum over shared holdings of the smaller weight, percent.
    pub percent: f64,
    pub shared: Vec<Shared>,
    /// Either fund lists only its top ten: the overlap is a lower bound.
    pub partial: bool,
}

/// Shared holdings of two funds; matched by CUSIP, else by ticker.
pub fn overlap(a: &Fund, b: &Fund) -> Overlap {
    let mut by_cusip: HashMap<&str, usize> = HashMap::new();
    let mut by_ticker: HashMap<String, usize> = HashMap::new();
    for (i, h) in b.holdings.iter().enumerate() {
        if let Some(c) = &h.cusip {
            by_cusip.entry(c).or_insert(i);
        }
        if let Some(t) = &h.ticker {
            by_ticker.entry(t.replace(['.', '/'], "-").to_ascii_uppercase()).or_insert(i);
        }
    }
    let mut used = HashSet::new();
    let mut shared = Vec::new();
    for h in &a.holdings {
        let hit = h
            .cusip
            .as_deref()
            .and_then(|c| by_cusip.get(c))
            .or_else(|| h.ticker.as_ref().and_then(|t| by_ticker.get(&t.replace(['.', '/'], "-").to_ascii_uppercase())))
            .copied();
        if let Some(i) = hit
            && used.insert(i)
        {
            shared.push(Shared {
                name: h.name.clone(),
                ticker: h.ticker.clone(),
                a: h.weight,
                b: b.holdings[i].weight,
            });
        }
    }
    shared.sort_by(|x, y| y.common().total_cmp(&x.common()));
    Overlap { percent: shared.iter().map(Shared::common).sum(), shared, partial: a.partial() || b.partial() }
}

// ---- fetching and cache -------------------------------------------------------------------

/// The cache directory under the app's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("etf")
}

fn fund_path(dir: &Path, ticker: &str) -> PathBuf {
    let safe: String = ticker.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    dir.join(format!("{}.json", safe.to_ascii_uppercase()))
}

pub fn read_cached(dir: &Path, ticker: &str) -> Option<Fund> {
    serde_json::from_str(&std::fs::read_to_string(fund_path(dir, ticker)).ok()?).ok()
}

/// Every fund parsed into the cache so far.
pub fn all_cached(dir: &Path) -> Vec<Fund> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Fund> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    out
}

fn write_cached(dir: &Path, fund: &Fund) {
    let _ = std::fs::create_dir_all(dir);
    let path = fund_path(dir, &fund.ticker);
    let tmp = path.with_extension("tmp");
    if let Ok(json) = serde_json::to_string(fund)
        && std::fs::write(&tmp, json).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

type MfMap = Arc<HashMap<String, (u64, String)>>;
type Names = Arc<NameMap>;

async fn mf_map(dir: &Path, ua: &str) -> Result<MfMap> {
    static MAP: Mutex<Option<(Instant, MfMap)>> = Mutex::const_new(None);
    let mut g = MAP.lock().await;
    if let Some((at, m)) = g.as_ref()
        && at.elapsed() < MF_TTL
    {
        return Ok(m.clone());
    }
    let json = edgar::cached(&dir.join("sec"), "company_tickers_mf.json", MF_TTL, MF_URL, ua).await?;
    let m = Arc::new(parse_mf(&json)?);
    *g = Some((Instant::now(), m.clone()));
    Ok(m)
}

async fn name_map(dir: &Path, ua: &str) -> Result<Names> {
    static MAP: Mutex<Option<(Instant, Names)>> = Mutex::const_new(None);
    let mut g = MAP.lock().await;
    if let Some((at, m)) = g.as_ref()
        && at.elapsed() < edgar::TICKERS_TTL
    {
        return Ok(m.clone());
    }
    let json =
        edgar::cached(&dir.join("sec"), "company_tickers.json", edgar::TICKERS_TTL, edgar::TICKERS_URL, ua).await?;
    let m = Arc::new(NameMap::parse(&json)?);
    *g = Some((Instant::now(), m.clone()));
    Ok(m)
}

fn yahoo() -> Option<&'static crate::yahoo::YahooProvider> {
    static Y: OnceLock<Option<crate::yahoo::YahooProvider>> = OnceLock::new();
    Y.get_or_init(|| crate::yahoo::YahooProvider::new().ok()).as_ref()
}

async fn yahoo_fund(ticker: &str) -> Result<YahooFund> {
    let y = yahoo().ok_or_else(|| anyhow!("Yahoo client unavailable"))?;
    parse_yahoo(&y.quote_summary(ticker, "topHoldings,price").await?)
}

/// Load a fund: the disk cache while fresh, else N-PORT from the SEC (and Yahoo for sectors,
/// or alone for funds without an N-PORT), falling back to a stale copy when the network fails.
/// `force` skips the freshness check (the filing itself is only downloaded when it is new).
pub async fn load(dir: &Path, ticker: &str, ua: &str, force: bool) -> Result<Fund> {
    let ticker = ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() || !ticker.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        bail!("{ticker:?} is not an ETF ticker");
    }
    let now = chrono::Utc::now().timestamp();
    let cached = read_cached(dir, &ticker);
    if let Some(f) = cached.as_ref().filter(|f| !force && !f.stale(now)) {
        return Ok(f.clone());
    }
    match fetch_fund(dir, &ticker, ua, cached.as_ref(), now).await {
        Ok(f) => {
            write_cached(dir, &f);
            Ok(f)
        }
        Err(e) => match cached {
            Some(f) => {
                tracing::warn!("ETF {ticker}: {e:#}; serving cached copy");
                Ok(f)
            }
            None => Err(e),
        },
    }
}

async fn fetch_fund(dir: &Path, ticker: &str, ua: &str, cached: Option<&Fund>, now: i64) -> Result<Fund> {
    if !edgar::valid_user_agent(ua) {
        bail!("set an SEC User-Agent with a contact email (Settings › Panels, or {})", edgar::UA_ENV);
    }
    let sec_ticker = ticker.replace('.', "-");
    let series = mf_map(dir, ua).await?.get(&sec_ticker).cloned();
    let yahoo = yahoo_fund(ticker).await.map_err(|e| tracing::debug!("ETF {ticker} yahoo: {e:#}")).ok();
    let mut nport = None;
    if let Some((_, series_id)) = series {
        let url = format!(
            "https://www.sec.gov/cgi-bin/browse-edgar?action=getcompany&CIK={series_id}&type=NPORT&owner=include&count=10&output=atom"
        );
        let atom = edgar::fetch(&url, ua).await?;
        let newest =
            parse_filings(&atom).into_iter().max_by(|a, b| a.filed.cmp(&b.filed).then(a.accession.cmp(&b.accession)));
        if let Some(filing) = newest {
            // Same filing as the cached copy: only Yahoo's sectors may have changed.
            if let Some(c) =
                cached.filter(|c| c.source == Source::Nport && c.accession.as_deref() == Some(&filing.accession))
            {
                let mut f = c.clone();
                if let Some(y) = &yahoo
                    && !y.sectors.is_empty()
                {
                    f.sectors = y.sectors.clone();
                }
                f.updated = now;
                return Ok(f);
            }
            let xml = edgar::fetch(&filing.url(), ua).await?;
            nport = Some((parse_nport(&xml)?, filing));
        }
    }
    let names = match name_map(dir, ua).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("ETF {ticker}: SEC company names unavailable: {e:#}");
            Arc::default()
        }
    };
    build_fund(ticker, nport, yahoo, &names, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edgarSubmission xmlns="http://www.sec.gov/edgar/nport">
  <headerData><submissionType>NPORT-P</submissionType></headerData>
  <formData>
    <genInfo>
      <regName>EXAMPLE INDEX FUNDS</regName>
      <seriesName>EXAMPLE 500 INDEX FUND</seriesName>
      <seriesId>S000000001</seriesId>
      <repPdDate>2026-06-30</repPdDate>
    </genInfo>
    <fundInfo><totAssets>1010.0</totAssets><netAssets>1000.0</netAssets></fundInfo>
    <invstOrSecs>
      <invstOrSec>
        <name>Apple Inc</name><title>APPLE INC</title><cusip>037833100</cusip>
        <balance>10</balance><valUSD>70.0</valUSD><pctVal>7.0</pctVal><payoffProfile>Long</payoffProfile>
      </invstOrSec>
      <invstOrSec>
        <name>Alphabet Inc Class C</name><cusip>02079K107</cusip>
        <valUSD>30.0</valUSD><pctVal>3.0</pctVal><payoffProfile>Long</payoffProfile>
      </invstOrSec>
      <invstOrSec>
        <name>Johnson &amp; Johnson</name><cusip>478160104</cusip>
        <identifiers><ticker value="JNJ"/></identifiers>
        <valUSD>20.0</valUSD><pctVal>2.0</pctVal><payoffProfile>Long</payoffProfile>
      </invstOrSec>
      <invstOrSec>
        <name>Short Thing</name><cusip>111111111</cusip><pctVal>-1.0</pctVal><payoffProfile>Short</payoffProfile>
      </invstOrSec>
      <invstOrSec>
        <name>Cash Sweep</name><cusip>000000000</cusip><pctVal>0.5</pctVal><payoffProfile>Long</payoffProfile>
      </invstOrSec>
    </invstOrSecs>
  </formData>
</edgarSubmission>"#;

    const ATOM: &str = r#"<feed><entry>
<category term="NPORT-P"/><content>
<accession-number>0000036405-26-000473</accession-number>
<filing-date>2026-08-28</filing-date>
<filing-href>https://www.sec.gov/Archives/edgar/data/36405/000003640526000473/0000036405-26-000473-index.htm</filing-href>
<filing-type>NPORT-P</filing-type></content></entry>
<entry><content><accession-number>0000036405-26-000100</accession-number>
<filing-date>2026-05-29</filing-date>
<filing-href>https://www.sec.gov/Archives/edgar/data/36405/000003640526000100/x-index.htm</filing-href>
<filing-type>NPORT-EX</filing-type></content></entry></feed>"#;

    const YAHOO: &str = r#"{"quoteSummary":{"result":[{"price":{"longName":"Example 500 ETF"},
"topHoldings":{"holdings":[
 {"symbol":"AAPL","holdingName":"Apple Inc","holdingPercent":{"raw":0.07}},
 {"symbol":"GOOG","holdingName":"Alphabet Inc Class C","holdingPercent":{"raw":0.03}}],
 "sectorWeightings":[{"technology":{"raw":0.6}},{"healthcare":{"raw":0.4}},{"energy":{"raw":0.0}}]}}]}}"#;

    const TICKERS: &str = r#"{"0":{"cik_str":320193,"ticker":"AAPL","title":"Apple Inc."},
"1":{"cik_str":1652044,"ticker":"GOOGL","title":"Alphabet Inc."},
"2":{"cik_str":1652044,"ticker":"GOOG","title":"Alphabet Inc."},
"8":{"cik_str":72971,"ticker":"WFC","title":"WELLS FARGO & COMPANY/MN"},
"9":{"cik_str":72971,"ticker":"WFC-PY","title":"WELLS FARGO & COMPANY/MN"},
"6":{"cik_str":1067983,"ticker":"BRK-B","title":"BERKSHIRE HATHAWAY INC"},
"7":{"cik_str":1067983,"ticker":"BRK-A","title":"BERKSHIRE HATHAWAY INC"},
"3":{"cik_str":200406,"ticker":"JNJ","title":"JOHNSON & JOHNSON"},
"4":{"cik_str":1,"ticker":"AAA","title":"Acme Corp"},
"5":{"cik_str":2,"ticker":"AAB","title":"Acme Inc"}}"#;

    fn fund(ticker: &str, src: Source, h: &[(&str, Option<&str>, Option<&str>, f64)]) -> Fund {
        Fund {
            ticker: ticker.into(),
            name: ticker.into(),
            source: src,
            as_of: None,
            filed: None,
            accession: None,
            net_assets: None,
            holdings: h
                .iter()
                .map(|(n, t, c, w)| Holding {
                    name: n.to_string(),
                    ticker: t.map(str::to_string),
                    cusip: c.map(str::to_string),
                    weight: *w,
                    value: None,
                })
                .collect(),
            positions: h.len(),
            sectors: vec![],
            updated: 0,
        }
    }

    #[test]
    fn nport_holdings_are_parsed() {
        let d = parse_nport(NPORT).unwrap();
        assert_eq!(d.series_name, "EXAMPLE 500 INDEX FUND");
        assert_eq!(d.series_id, "S000000001");
        assert_eq!(d.period_end, NaiveDate::from_ymd_opt(2026, 6, 30));
        assert_eq!(d.net_assets, Some(1000.0));
        // The short position is dropped; the zero-CUSIP cash line keeps no CUSIP.
        assert_eq!(d.positions, 5);
        assert_eq!(d.holdings.len(), 4);
        assert_eq!(d.holdings[0].cusip.as_deref(), Some("037833100"));
        assert_eq!(d.holdings[0].value, Some(70.0));
        assert_eq!(d.holdings[2].name, "Johnson & Johnson");
        assert_eq!(d.holdings[2].ticker.as_deref(), Some("JNJ"));
        assert_eq!(d.holdings[3].cusip, None);
        assert!(parse_nport("<x/>").is_err());
    }

    #[test]
    fn filing_feed_keeps_nport_p_only() {
        let f = parse_filings(ATOM);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].cik, 36405);
        assert_eq!(f[0].url(), "https://www.sec.gov/Archives/edgar/data/36405/000003640526000473/primary_doc.xml");
    }

    #[test]
    fn mf_map_gives_series() {
        let json = r#"{"fields":["cik","seriesId","classId","symbol"],"data":[[36405,"S000002839","C1","VOO"],[36405,"S000002839","C2","VFIAX"]]}"#;
        let m = parse_mf(json).unwrap();
        assert_eq!(m["VOO"], (36405, "S000002839".to_string()));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn yahoo_top_holdings_and_sectors() {
        let y = parse_yahoo(YAHOO).unwrap();
        assert_eq!(y.name.as_deref(), Some("Example 500 ETF"));
        assert_eq!(y.holdings.len(), 2);
        assert!((y.holdings[0].weight - 7.0).abs() < 1e-9);
        let labels: Vec<_> = y.sectors.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Technology", "Healthcare"]);
        assert!(parse_yahoo(r#"{"quoteSummary":{"result":null}}"#).is_err());
    }

    #[test]
    fn names_normalise_and_resolve() {
        assert_eq!(norm_name("Apple Inc.", false), norm_name("APPLE INC", false));
        assert_eq!(norm_name("Alphabet Inc Class C", false), "alphabet");
        assert_eq!(norm_name("Alphabet Inc Class C", true), "alphabet class c");
        assert_eq!(norm_name("Lowe's Cos Inc", false), "lowes");
        let m = NameMap::parse(TICKERS).unwrap();
        assert_eq!(m.lookup("Apple Inc"), ["AAPL"]);
        // Preferred shares and `/MN` suffixes do not get in the way.
        assert_eq!(m.lookup("Wells Fargo & Co"), ["WFC"]);
        assert_eq!(norm_name("Charles Schwab Corp/The", false), "charles schwab");
        assert_eq!(norm_name("US BANCORP DE", false), norm_name("US Bancorp", false));
        assert_eq!(m.lookup("Johnson & Johnson"), ["JNJ"]);
        // Two companies called Acme: no guess.
        assert!(m.lookup("Acme").is_empty());
        // Same company with two share classes: both, primary first.
        assert_eq!(m.lookup("Alphabet Inc Class A"), ["GOOGL", "GOOG"]);
        assert_eq!(class_letter("Berkshire Hathaway Inc Class B"), Some('b'));
        assert_eq!(class_letter("Apple Inc"), None);
    }

    #[test]
    fn fund_combines_nport_yahoo_and_names() {
        let doc = parse_nport(NPORT).unwrap();
        let filing = parse_filings(ATOM).remove(0);
        let names = NameMap::parse(TICKERS).unwrap();
        let y = parse_yahoo(YAHOO).unwrap();
        let f = build_fund("example", Some((doc, filing)), Some(y), &names, 5).unwrap();
        assert_eq!(f.ticker, "EXAMPLE");
        assert_eq!(f.name, "Example 500 ETF");
        assert_eq!(f.source, Source::Nport);
        assert_eq!(f.as_of, NaiveDate::from_ymd_opt(2026, 6, 30));
        assert_eq!(f.holdings[0].ticker.as_deref(), Some("AAPL"));
        // Yahoo knows the share class; the SEC name table could only guess.
        assert_eq!(f.holdings[1].ticker.as_deref(), Some("GOOG"));
        assert_eq!(f.holdings[2].ticker.as_deref(), Some("JNJ"));
        assert_eq!(f.sectors.len(), 2);
        let (top, other) = f.top_and_other(2);
        assert_eq!(top.len(), 2);
        assert!((other - 90.0).abs() < 1e-9);
        assert!(!f.partial());
    }

    #[test]
    fn dual_class_issuers_are_resolved_by_class_or_weight() {
        let names = NameMap::parse(TICKERS).unwrap();
        let h = |n: &str, w: f64| Holding { name: n.into(), ticker: None, cusip: None, weight: w, value: None };
        // Named class.
        let mut a = vec![h("Berkshire Hathaway Inc Class B", 1.0)];
        resolve_tickers(&mut a, &[], &names);
        assert_eq!(a[0].ticker.as_deref(), Some("BRK-B"));
        // Both classes under one name: the larger takes the primary ticker.
        let mut b = vec![h("Alphabet Inc", 3.0), h("Alphabet Inc", 2.0)];
        resolve_tickers(&mut b, &[], &names);
        assert_eq!(b[0].ticker.as_deref(), Some("GOOGL"));
        assert_eq!(b[1].ticker.as_deref(), Some("GOOG"));
        // One unnamed position of a dual-class issuer: the primary listing.
        let mut c = vec![h("Alphabet Inc", 3.0)];
        resolve_tickers(&mut c, &[], &names);
        assert_eq!(c[0].ticker.as_deref(), Some("GOOGL"));
    }

    #[test]
    fn yahoo_only_fund_is_partial() {
        let y = parse_yahoo(YAHOO).unwrap();
        let f = build_fund("SPY", None, Some(y), &NameMap::default(), 5).unwrap();
        assert!(f.partial());
        assert_eq!(f.as_of, None);
        assert!(build_fund("SPY", None, None, &NameMap::default(), 5).is_err());
    }

    #[test]
    fn reverse_search_orders_by_weight() {
        let a = Arc::new(fund("AAA", Source::Nport, &[("Apple Inc", Some("AAPL"), None, 5.0)]));
        let b = Arc::new(fund("BBB", Source::Yahoo, &[("Apple Inc", Some("AAPL"), None, 9.0)]));
        let c = Arc::new(fund("CCC", Source::Nport, &[("Microsoft", Some("MSFT"), None, 4.0)]));
        let hits = reverse_search(&[a, b, c], "aapl");
        assert_eq!(hits.iter().map(|h| h.fund.as_str()).collect::<Vec<_>>(), ["BBB", "AAA"]);
        assert!(hits[0].partial && !hits[1].partial);
        assert!(reverse_search(&[], "AAPL").is_empty());
    }

    #[test]
    fn overlap_sums_the_smaller_weights() {
        let a = fund(
            "AAA",
            Source::Nport,
            &[
                ("Apple", Some("AAPL"), Some("C1"), 6.0),
                ("Microsoft", Some("MSFT"), Some("C2"), 4.0),
                ("Only A", None, Some("C3"), 1.0),
            ],
        );
        // B matches Apple by CUSIP only and Microsoft by ticker only.
        let b = fund(
            "BBB",
            Source::Nport,
            &[
                ("APPLE INC", None, Some("C1"), 8.0),
                ("Microsoft", Some("MSFT"), None, 2.0),
                ("Only B", None, Some("C9"), 3.0),
            ],
        );
        let o = overlap(&a, &b);
        assert_eq!(o.shared.len(), 2);
        assert!((o.percent - 8.0).abs() < 1e-9);
        assert_eq!(o.shared[0].name, "Apple");
        assert!(!o.partial);
        let y = fund("YYY", Source::Yahoo, &[("Apple", Some("AAPL"), None, 7.0)]);
        let o = overlap(&a, &y);
        assert!(o.partial);
        assert!((o.percent - 6.0).abs() < 1e-9);
    }

    #[test]
    fn disk_cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("tr-etf-test-{}", std::process::id()));
        let f = fund("VOO", Source::Nport, &[("Apple", Some("AAPL"), Some("C1"), 6.0)]);
        write_cached(&dir, &f);
        assert_eq!(read_cached(&dir, "voo"), Some(f.clone()));
        assert_eq!(all_cached(&dir), vec![f]);
        assert_eq!(read_cached(&dir, "../etc"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
