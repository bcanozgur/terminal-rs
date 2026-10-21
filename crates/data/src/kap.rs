//! KAP disclosures (`KAP`): the public disclosure feed of Borsa Istanbul companies.
//!
//! Source: **KAP** (kap.org.tr), the JSON endpoints its own website calls, all readable without
//! a login. They were found in the site's Next.js bundles (`/_next/static/chunks/*.js` holds a
//! table of the `api/...` routes and the fetch code around them):
//! - `POST /tr/api/disclosure/list/main`, body `{fromDate, toDate, disclosureTypes, memberTypes,
//!   mkkMemberOid}` (dates `DD.MM.YYYY`, at most about two months apart): the disclosures of a
//!   period, newest first. `memberTypes: ["IGS"]` is the Borsa-listed companies,
//!   `disclosureTypes` of `ODA` (material events, also corporate actions), `FR` (financial
//!   reports) and `DG` (other) leave out the exchange's own circuit-breaker bulletins, which are
//!   most of the volume. With `mkkMemberOid` set it lists one company's disclosures (including
//!   the notices of others that name it, e.g. MKK trading bans).
//! - `GET /tr/api/member/filter/<text>`: companies matching a ticker or name (gives the
//!   `mkkMemberOid` that the list needs).
//! - `GET /tr/api/notification/attachment-detail/<index>`: one disclosure's form as HTML
//!   (Turkish and English side by side; the English half is hidden by CSS) and its attachment
//!   names. Reduced to text by [`html_to_text`].
//!
//! Requests follow `trfunds`: a browser User-Agent, a process-wide pause between requests and a
//! backoff when KAP answers 429/503 or drops the connection.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{NaiveDateTime, Utc};
use serde::Deserialize;

const BASE: &str = "https://www.kap.org.tr";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
/// KAP times are Istanbul local time, UTC+3 all year.
const TR_OFFSET: i64 = 3 * 3600;
/// The longest period KAP's list accepts for a company (it answers HTTP 500 beyond ~2 months).
pub const COMPANY_DAYS: u32 = 60;
/// A disclosure form larger than this is not read in the pane (financial reports are 2 MB+).
const MAX_DETAIL: usize = 1_500_000;

/// Where the disclosure is on kap.org.tr.
pub fn link(index: u64) -> String {
    format!("{BASE}/tr/Bildirim/{index}")
}

// ---- model --------------------------------------------------------------------------------

/// A coarse kind of disclosure, derived from KAP's class and title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// Özel Durum Açıklaması: material event.
    Material,
    /// Finansal Rapor: financial statements, activity reports.
    Financial,
    /// Kar payı (dividend) distribution.
    Dividend,
    /// Sermaye artırımı/azaltımı, bedelsiz, kayıtlı sermaye, birleşme.
    Capital,
    /// Pay geri alım: share buyback.
    Buyback,
    /// Pay alım satım: insider and major-holder trades.
    Insider,
    /// Genel kurul.
    Meeting,
    /// Borçlanma, ihraç and credit ratings.
    Debt,
    Other,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Material,
        Category::Financial,
        Category::Dividend,
        Category::Capital,
        Category::Buyback,
        Category::Insider,
        Category::Meeting,
        Category::Debt,
        Category::Other,
    ];

    /// Turkish label, as KAP users know them.
    pub fn label(self) -> &'static str {
        match self {
            Category::Material => "Özel Durum",
            Category::Financial => "Finansal Rapor",
            Category::Dividend => "Temettü",
            Category::Capital => "Sermaye",
            Category::Buyback => "Geri Alım",
            Category::Insider => "Pay Alım/Satım",
            Category::Meeting => "Genel Kurul",
            Category::Debt => "Borçlanma",
            Category::Other => "Diğer",
        }
    }

    /// Stable key for saved layouts and commands.
    pub fn key(self) -> &'static str {
        match self {
            Category::Material => "material",
            Category::Financial => "financial",
            Category::Dividend => "dividend",
            Category::Capital => "capital",
            Category::Buyback => "buyback",
            Category::Insider => "insider",
            Category::Meeting => "meeting",
            Category::Debt => "debt",
            Category::Other => "other",
        }
    }

    pub fn from_word(w: &str) -> Option<Category> {
        let w = fold(w);
        Some(match w.as_str() {
            "material" | "oda" | "ozel" | "ozeldurum" | "event" | "events" => Category::Material,
            "financial" | "fr" | "rapor" | "finansal" | "results" => Category::Financial,
            "dividend" | "dividends" | "div" | "temettu" | "karpayi" => Category::Dividend,
            "capital" | "sermaye" | "bedelsiz" | "split" => Category::Capital,
            "buyback" | "geri" | "gerialim" | "repurchase" => Category::Buyback,
            "insider" | "insiders" | "alimsatim" => Category::Insider,
            "meeting" | "agm" | "genelkurul" => Category::Meeting,
            "debt" | "borc" | "borclanma" | "rating" | "bond" => Category::Debt,
            "other" | "diger" => Category::Other,
            _ => return None,
        })
    }

    /// Classify a disclosure from KAP's class (`ODA`, `FR`, `DG`, `DUY`), title and summary.
    pub fn classify(class: &str, title: &str, summary: &str) -> Category {
        let t = fold(title);
        let s = fold(summary);
        let any = |hay: &str, words: &[&str]| words.iter().any(|w| hay.contains(w));
        if t.contains("kar payi") || t.contains("temettu") {
            return Category::Dividend;
        }
        if t.contains("geri alin") {
            return Category::Buyback;
        }
        if any(&t, &["sermaye artirimi", "sermaye azaltimi", "bedelsiz", "kayitli sermaye", "birlesme", "bolunme"]) {
            return Category::Capital;
        }
        if t.contains("genel kurul") {
            return Category::Meeting;
        }
        if t.contains("pay alim satim") {
            return Category::Insider;
        }
        if class.eq_ignore_ascii_case("FR") {
            return Category::Financial;
        }
        if any(&t, &["finansal rapor", "faaliyet raporu", "sorumluluk beyani"]) {
            return Category::Financial;
        }
        if any(
            &t,
            &[
                "borclanma",
                "sermaye piyasasi araci",
                "ihrac",
                "derecelendirme",
                "varant",
                "kira sertifika",
                "tahvil",
                "bono",
            ],
        ) {
            return Category::Debt;
        }
        // A generic material-event form whose subject is one of the above.
        if class.eq_ignore_ascii_case("ODA") || t.contains("ozel durum") {
            if any(&s, &["kar payi", "temettu"]) {
                return Category::Dividend;
            }
            if any(&s, &["geri alim", "geri alin"]) {
                return Category::Buyback;
            }
            if any(&s, &["bedelsiz", "sermaye artirim", "bedelli"]) {
                return Category::Capital;
            }
            return Category::Material;
        }
        Category::Other
    }
}

/// Lower-case ASCII fold of Turkish text (`İ`, `ı` → `i`, `ş` → `s`, ...): for matching only.
pub fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'İ' | 'I' | 'ı' | 'i' => 'i',
            'ş' | 'Ş' => 's',
            'ğ' | 'Ğ' => 'g',
            'ü' | 'Ü' => 'u',
            'ö' | 'Ö' => 'o',
            'ç' | 'Ç' => 'c',
            'â' | 'Â' => 'a',
            'î' | 'Î' => 'i',
            'û' | 'Û' => 'u',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// One disclosure of KAP's list.
#[derive(Debug, Clone, PartialEq)]
pub struct Disclosure {
    /// KAP's running number; the link is `/tr/Bildirim/<index>`.
    pub index: u64,
    /// Unix seconds.
    pub time: i64,
    /// Tickers of the company that filed it (a company may have several, e.g. `GARAN, TGB`).
    pub codes: Vec<String>,
    /// Other tickers the disclosure names (a rating agency's note on the rated company, an MKK
    /// notice on the stocks it covers).
    pub related: Vec<String>,
    /// The filer's name.
    pub company: String,
    /// KAP's id of the filer.
    pub member_oid: String,
    /// `ODA`, `FR`, `DG` or `DUY`.
    pub class: String,
    /// KAP's form title (e.g. "Özel Durum Açıklaması (Genel)").
    pub title: String,
    pub summary: String,
    pub category: Category,
    pub attachments: u32,
    /// Filed after the deadline.
    pub late: bool,
    /// A correction of an earlier disclosure.
    pub amended: bool,
}

impl Disclosure {
    pub fn link(&self) -> String {
        link(self.index)
    }

    /// Whether the disclosure is from, or names, the company with ticker `code`.
    pub fn mentions(&self, code: &str) -> bool {
        self.codes.iter().chain(&self.related).any(|c| c.eq_ignore_ascii_case(code))
    }

    /// Whether any of `set` (upper-case tickers) is the filer or named.
    pub fn mentions_any(&self, set: &std::collections::HashSet<String>) -> bool {
        self.codes.iter().chain(&self.related).any(|c| set.contains(c))
    }

    /// The filer's primary ticker, else the first named one; empty when none.
    pub fn code(&self) -> &str {
        self.codes.first().or(self.related.first()).map_or("", String::as_str)
    }

    /// Tickers shown in the list: the filer's, else the ones it names.
    pub fn code_label(&self) -> String {
        let v = if self.codes.is_empty() { &self.related } else { &self.codes };
        v.join(", ")
    }
}

/// A disclosure's form, reduced to text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detail {
    pub text: String,
    /// File names of the attachments (PDFs, mostly).
    pub attachments: Vec<String>,
}

// ---- parsing ------------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawItem {
    #[serde(rename = "disclosureBasic")]
    basic: RawBasic,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBasic {
    title: Option<String>,
    mkk_member_oid: Option<String>,
    company_title: Option<String>,
    stock_code: Option<String>,
    related_stocks: Option<String>,
    disclosure_class: Option<String>,
    publish_date: Option<String>,
    disclosure_index: Option<u64>,
    summary: Option<String>,
    attachment_count: Option<u32>,
    is_late: Option<bool>,
    is_changed: Option<String>,
}

fn split_codes(s: Option<String>) -> Vec<String> {
    s.iter()
        .flat_map(|s| s.split(','))
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
        .collect()
}

/// `22.10.2026 00:03:37` (Istanbul time) as unix seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let t = NaiveDateTime::parse_from_str(s.trim(), "%d.%m.%Y %H:%M:%S").ok()?;
    Some(t.and_utc().timestamp() - TR_OFFSET)
}

/// KAP's list response as disclosures, newest first. Rows without an index or time are skipped;
/// an error object (`{"success":false,"errorMessage":...}`) is an error.
pub fn parse_list(json: &str) -> Result<Vec<Disclosure>> {
    let v: serde_json::Value = serde_json::from_str(json).context("KAP disclosure list: not JSON")?;
    if let Some(e) = v.get("errorMessage").and_then(|e| e.as_str()) {
        bail!("KAP: {e}");
    }
    let items: Vec<RawItem> = serde_json::from_value(v).context("KAP disclosure list: unexpected shape")?;
    let mut out: Vec<Disclosure> = items
        .into_iter()
        .filter_map(|i| {
            let b = i.basic;
            let index = b.disclosure_index?;
            let time = parse_time(b.publish_date.as_deref()?)?;
            let class = b.disclosure_class.unwrap_or_default();
            let title = b.title.unwrap_or_default();
            let summary = b.summary.map(|s| s.trim().to_string()).unwrap_or_default();
            Some(Disclosure {
                index,
                time,
                codes: split_codes(b.stock_code),
                related: split_codes(b.related_stocks),
                company: b.company_title.unwrap_or_default(),
                member_oid: b.mkk_member_oid.unwrap_or_default(),
                category: Category::classify(&class, &title, &summary),
                class,
                title,
                summary,
                attachments: b.attachment_count.unwrap_or(0),
                late: b.is_late.unwrap_or(false),
                amended: b.is_changed.is_some_and(|c| !c.is_empty()),
            })
        })
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.index));
    out.dedup_by_key(|d| d.index);
    Ok(out)
}

/// `member/filter` response: the ids of the matching companies, best match first.
pub fn parse_members(json: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct M {
        mkk_member_oid: String,
    }
    let v: Vec<M> = serde_json::from_str(json).context("KAP company search: unexpected response")?;
    Ok(v.into_iter().map(|m| m.mkk_member_oid).collect())
}

/// `attachment-detail` response: the form as text and the attachment file names.
pub fn parse_detail(json: &str) -> Result<Detail> {
    let v: serde_json::Value = serde_json::from_str(json).context("KAP disclosure: not JSON")?;
    if let Some(e) = v.get("errorMessage").and_then(|e| e.as_str()) {
        bail!("KAP: {e}");
    }
    let first = v.get(0).ok_or_else(|| anyhow!("KAP returned no disclosure"))?;
    let bodies = first.get("disclosureBody").and_then(|b| b.as_array()).into_iter().flatten();
    let mut text = String::new();
    for b in bodies.filter_map(|b| b.as_str()) {
        let t = html_to_text(b);
        if !t.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&t);
        }
    }
    let attachments = first
        .get("attachments")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("fileName").or_else(|| a.get("objId")).and_then(|n| n.as_str()))
        .map(str::to_string)
        .collect();
    Ok(Detail { text, attachments })
}

/// Reduce a KAP form (XBRL-style tables, Turkish and English side by side) to readable text:
/// hidden parts (the English half, technical field names, templates) are dropped, table cells
/// and rows become line breaks.
pub fn html_to_text(html: &str) -> String {
    const CELL: char = '\u{1}';
    let mut out = String::new();
    // (tag, hidden) for the open elements.
    let mut stack: Vec<(String, bool)> = Vec::new();
    let b = html.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            // Comments and tags; quoted attribute values may contain '>'.
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map_or(b.len(), |e| i + e + 3);
                continue;
            }
            let mut j = i + 1;
            let mut quote = 0u8;
            while j < b.len() {
                match (quote, b[j]) {
                    (0, b'>') => break,
                    (0, q @ (b'"' | b'\'')) => quote = q,
                    (q, c) if q != 0 && c == q => quote = 0,
                    _ => {}
                }
                j += 1;
            }
            let tag = &html[i + 1..j.min(b.len())];
            i = j + 1;
            let closing = tag.starts_with('/');
            let body = tag.trim_start_matches('/').trim_end_matches('/');
            let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
            let name = body[..name_end].to_ascii_lowercase();
            if name.starts_with('!') || name.is_empty() {
                continue;
            }
            let hidden_now = stack.last().is_some_and(|s| s.1);
            let void = matches!(name.as_str(), "br" | "hr" | "img" | "input" | "meta" | "link" | "col") || tag.ends_with('/');
            if closing {
                if let Some(pos) = stack.iter().rposition(|s| s.0 == name) {
                    let hidden_el = stack[pos].1;
                    stack.truncate(pos);
                    if !hidden_el {
                        match name.as_str() {
                            "td" | "th" => out.push(CELL),
                            "tr" | "p" | "li" | "table" | "h1" | "h2" | "h3" | "h4" => out.push('\n'),
                            _ => {}
                        }
                    }
                }
                continue;
            }
            if name == "br" && !hidden_now {
                out.push('\n');
            }
            if void {
                continue;
            }
            let attrs = body[name_end..].to_ascii_lowercase();
            let hidden = hidden_now
                || attrs.contains("display: none")
                || attrs.contains("display:none")
                || attr_classes(&attrs).any(|c| c == "taxonomy-field-name" || c == "taxonomy-field-documentation");
            stack.push((name, hidden));
        } else {
            let end = html[i..].find('<').map_or(b.len(), |e| i + e);
            if !stack.last().is_some_and(|s| s.1) {
                decode_entities(&html[i..end], &mut out);
            }
            i = end;
        }
    }
    // Cells of one row join with " · "; empty cells and rows go, runs of blanks collapse.
    let mut lines: Vec<String> = Vec::new();
    for row in out.split('\n') {
        let cells: Vec<String> = row
            .split(CELL)
            .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|c| !c.is_empty() && !matches!(c.as_str(), "[]" | "Türkçe" | "İngilizce"))
            .collect();
        if cells.is_empty() {
            continue;
        }
        let line = cells.join(" · ");
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn attr_classes(attrs: &str) -> impl Iterator<Item = &str> {
    let start = attrs.find("class=").map(|p| p + 6);
    let rest = start.map_or("", |s| &attrs[s..]);
    let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
    let end = rest.find(['"', '\'']).unwrap_or(rest.len());
    rest[..end].split_whitespace()
}

fn decode_entities(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let decoded = rest.find(';').filter(|&e| e <= 10).and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let n = ent.strip_prefix('#')?;
                    let code = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

// ---- requests -----------------------------------------------------------------------------

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    // A fresh connection per request: KAP closes idle ones without notice.
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(0)
            .build()
            .unwrap_or_default()
    })
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

async fn send_once(path: &str, body: Option<&str>) -> Result<String> {
    let url = format!("{BASE}{path}");
    let req = match body {
        Some(b) => client().post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").body(b.to_string()),
        None => client().get(&url),
    };
    let resp = req
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::REFERER, "https://www.kap.org.tr/tr")
        .header(reqwest::header::ACCEPT_LANGUAGE, "tr-TR,tr;q=0.9,en;q=0.8")
        .send()
        .await
        .with_context(|| format!("KAP request failed: {path}"))?;
    let status = resp.status();
    let text = resp.text().await.context("reading the KAP response")?;
    if !status.is_success() {
        // The list answers an error object with HTTP 500 (e.g. a period that is too long).
        let why = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("errorMessage").and_then(|e| e.as_str().map(str::to_string)))
            .unwrap_or_default();
        bail!("KAP answered {status} for {path} {why}");
    }
    Ok(text)
}

/// GET or POST with backoff (5 s … 60 s) when KAP throttles (HTTP 429/503, or it closes the
/// connection); any other HTTP error status is final.
async fn request(path: &str, body: Option<&str>) -> Result<String> {
    const BACKOFF: [u64; 4] = [5, 15, 30, 60];
    let mut attempt = 0;
    loop {
        throttle().await;
        match send_once(path, body).await {
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

/// `DD.MM.YYYY` of today in Istanbul, `back` days ago.
fn istanbul_date(back: u32) -> String {
    let t = Utc::now() + chrono::Duration::seconds(TR_OFFSET) - chrono::Duration::days(i64::from(back));
    t.format("%d.%m.%Y").to_string()
}

fn list_body(from: &str, to: &str, oid: Option<&str>) -> String {
    serde_json::json!({
        "fromDate": from,
        "toDate": to,
        "disclosureTypes": ["ODA", "FR", "DG"],
        "memberTypes": ["IGS"],
        "mkkMemberOid": oid,
    })
    .to_string()
}

/// The latest disclosures of listed companies: today and the `back` days before it, newest
/// first. One day is about 100 KB of JSON; three cover a weekend.
pub async fn latest(back: u32) -> Result<Vec<Disclosure>> {
    let body = list_body(&istanbul_date(back), &istanbul_date(0), None);
    parse_list(&request("/tr/api/disclosure/list/main", Some(&body)).await?)
}

/// KAP's company ids for a ticker, cached for the process.
async fn member_oids(code: &str) -> Result<Vec<String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = code.to_ascii_uppercase();
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Ok(v);
    }
    let oids = parse_members(&request(&format!("/tr/api/member/filter/{key}"), None).await?)?;
    if !oids.is_empty()
        && let Ok(mut c) = cache.lock()
    {
        c.insert(key, oids.clone());
    }
    Ok(oids)
}

/// One company's disclosures of the last `days` (at most [`COMPANY_DAYS`]), newest first,
/// including other filers' notices that name it.
pub async fn company(code: &str, days: u32) -> Result<Vec<Disclosure>> {
    let code = code.trim().to_ascii_uppercase();
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("not a ticker: {code:?}");
    }
    let oids = member_oids(&code).await?;
    if oids.is_empty() {
        bail!("KAP has no company matching {code}");
    }
    let (from, to) = (istanbul_date(days.min(COMPANY_DAYS)), istanbul_date(0));
    let mut all: Vec<Disclosure> = Vec::new();
    for oid in oids.iter().take(3) {
        let body = list_body(&from, &to, Some(oid));
        let rows = parse_list(&request("/tr/api/disclosure/list/main", Some(&body)).await?)?;
        all.extend(rows.into_iter().filter(|d| d.mentions(&code) || &d.member_oid == oid));
    }
    all.sort_by_key(|d| std::cmp::Reverse(d.index));
    all.dedup_by_key(|d| d.index);
    Ok(all)
}

/// A disclosure's form as text. Financial reports (megabytes of tables) are not read.
pub async fn detail(index: u64) -> Result<Detail> {
    let json = request(&format!("/tr/api/notification/attachment-detail/{index}"), None).await?;
    if json.len() > MAX_DETAIL {
        bail!("this disclosure is too large to show here ({} MB); open it on KAP", json.len() / 1_000_000);
    }
    parse_detail(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../tests/fixtures/kap_list.json");

    fn by_index(v: &[Disclosure], i: u64) -> &Disclosure {
        v.iter().find(|d| d.index == i).unwrap_or_else(|| panic!("no disclosure {i}"))
    }

    #[test]
    fn parses_the_list() {
        let v = parse_list(LIST).unwrap();
        assert_eq!(v.len(), 35);
        assert!(v.windows(2).all(|w| w[0].index > w[1].index), "newest first");
        let d = by_index(&v, 1685533);
        assert_eq!(d.codes, ["MEYSU"]);
        assert_eq!(d.company, "MEYSU GIDA SANAYİ VE TİCARET A.Ş.");
        assert_eq!(d.title, "Özel Durum Açıklaması (Genel)");
        assert_eq!(d.summary, "Tazminat davası hk.");
        assert_eq!(d.class, "ODA");
        assert_eq!(d.category, Category::Material);
        assert_eq!(d.link(), "https://www.kap.org.tr/tr/Bildirim/1685533");
        // 22.10.2026 00:03:37 Istanbul is 21.10.2026 21:03:37 UTC.
        assert_eq!(d.time, 1_792_616_617);
    }

    #[test]
    fn publish_time_is_istanbul_local() {
        assert_eq!(parse_time("01.01.1970 03:00:00"), Some(0));
        assert_eq!(parse_time("22.10.2026 00:03:37"), Some(1_792_616_617));
        assert_eq!(parse_time("garbage"), None);
    }

    #[test]
    fn several_codes_and_related_stocks() {
        let v = parse_list(LIST).unwrap();
        let multi = by_index(&v, 1685381);
        assert_eq!(multi.codes, ["GARAN", "TGB"]);
        assert_eq!(multi.code(), "GARAN");
        assert_eq!(multi.code_label(), "GARAN, TGB");
        assert!(multi.mentions("garan") && multi.mentions("TGB") && !multi.mentions("THYAO"));
        // A rating agency's note names the rated company.
        let rating = by_index(&v, 1685513);
        assert_eq!(rating.codes, ["TRATE"]);
        assert_eq!(rating.related, ["TARFN"]);
        assert!(rating.mentions("TARFN"));
        // Insider-trade notices without a filer ticker are listed under the stock they name.
        let insider = by_index(&v, 1684760);
        assert!(insider.codes.is_empty());
        assert_eq!(insider.code_label(), "BALSU");
        assert_eq!(insider.category, Category::Insider);
    }

    #[test]
    fn categories() {
        let v = parse_list(LIST).unwrap();
        assert_eq!(by_index(&v, 1685525).category, Category::Financial);
        assert_eq!(by_index(&v, 1685523).category, Category::Buyback);
        assert_eq!(by_index(&v, 1685136).category, Category::Capital);
        assert_eq!(by_index(&v, 1685239).category, Category::Meeting);
        assert_eq!(by_index(&v, 1685140).category, Category::Debt);
        assert_eq!(by_index(&v, 1685381).category, Category::Other);
        assert!(v.iter().any(|d| d.category == Category::Dividend), "the dividend notice");
        let div = v.iter().find(|d| d.title.starts_with("Kar Payı")).unwrap();
        assert_eq!(div.category, Category::Dividend);
        let buyback = v.iter().find(|d| d.summary.contains("Geri Alım Programı")).unwrap();
        assert_eq!(buyback.category, Category::Buyback);
    }

    #[test]
    fn classify_by_words() {
        let c = Category::classify;
        assert_eq!(c("ODA", "Özel Durum Açıklaması (Genel)", "Kâr Payı Dağıtım Kararı"), Category::Dividend);
        assert_eq!(c("ODA", "Özel Durum Açıklaması (Genel)", "Bedelsiz sermaye artırımı"), Category::Capital);
        assert_eq!(c("ODA", "Özel Durum Açıklaması (Genel)", "Yeni sipariş"), Category::Material);
        assert_eq!(c("ODA", "KAR PAYI DAĞITIM İŞLEMLERİNE İLİŞKİN BİLDİRİM", ""), Category::Dividend);
        assert_eq!(c("DG", "Finansal Takvim", ""), Category::Other);
        assert_eq!(c("FR", "Sorumluluk Beyanı", ""), Category::Financial);
    }

    #[test]
    fn category_words() {
        assert_eq!(Category::from_word("TEMETTÜ"), Some(Category::Dividend));
        assert_eq!(Category::from_word("buyback"), Some(Category::Buyback));
        assert_eq!(Category::from_word("fr"), Some(Category::Financial));
        assert_eq!(Category::from_word("zzz"), None);
        for c in Category::ALL {
            assert_eq!(Category::from_word(c.key()), Some(c), "{c:?}");
        }
    }

    #[test]
    fn company_feed_includes_notices_naming_it() {
        let v = parse_list(include_str!("../tests/fixtures/kap_company_thyao.json")).unwrap();
        assert_eq!(v.len(), 9);
        assert_eq!(v.iter().filter(|d| d.codes == ["THYAO"]).count(), 3);
        assert!(v.iter().all(|d| d.mentions("THYAO") || d.codes.is_empty()));
        let ban = &v[0];
        assert_eq!(ban.title, "SPK İşlem Yasağı Nedeniyle Pay Duyurusu");
        assert!(ban.codes.is_empty() && ban.mentions("THYAO") && ban.related.len() > 10);
    }

    #[test]
    fn error_objects_are_errors() {
        let e = parse_list(r#"{"success":false,"errorMessage":"HTTP 400 - "}"#).unwrap_err();
        assert!(format!("{e}").contains("HTTP 400"), "{e}");
        assert!(parse_list("<html>").is_err());
        assert_eq!(parse_list("[]").unwrap().len(), 0);
    }

    #[test]
    fn members() {
        let m = parse_members(include_str!("../tests/fixtures/kap_member_thyao.json")).unwrap();
        assert_eq!(m, ["4028e4a140f2ed720140f376bebb01a7"]);
        assert!(parse_members("[]").unwrap().is_empty());
    }

    #[test]
    fn detail_text_keeps_turkish_and_drops_hidden_parts() {
        let d = parse_detail(include_str!("../tests/fixtures/kap_detail_oda.json")).unwrap();
        assert!(d.attachments.is_empty());
        assert!(d.text.contains("Özel Durum Açıklaması (Genel)"), "{}", d.text);
        assert!(d.text.contains("Yapılan Açıklama Güncelleme mi?"), "{}", d.text);
        assert!(d.text.contains("Tazminat") || d.text.contains("tazminat"), "{}", d.text);
        // The English half is display:none, the field names are technical.
        assert!(!d.text.contains("Update Notification Flag"), "{}", d.text);
        assert!(!d.text.contains("oda_UpdateAnnouncementFlag"), "{}", d.text);
        assert!(!d.text.contains("CONSOLIDATION_METHOD"), "{}", d.text);
        assert!(!d.text.contains('<') && !d.text.contains("&nbsp;"), "{}", d.text);
    }

    #[test]
    fn detail_lists_attachments() {
        let d = parse_detail(include_str!("../tests/fixtures/kap_detail_attachment.json")).unwrap();
        assert_eq!(d.attachments, ["Tarfin Tarım Not Güncelleme 21 Ekim 2026.pdf"]);
        assert!(!d.text.is_empty());
    }

    #[test]
    fn html_reduction() {
        let h = "<table><tr><td><div class=\"x\">Konu</div></td><td>Pay&nbsp;&amp;&#350;irket</td>\
                 <td style=\"display: none;\">hidden</td></tr><tr><td></td></tr>\
                 <tr><td class=\"taxonomy-field-name\">sys_Field|</td><td>Satır<br>iki</td></tr></table>\
                 <p>Son &lt;satır&gt;</p><!-- c -->";
        assert_eq!(html_to_text(h), "Konu · Pay &Şirket\nSatır\niki\nSon <satır>");
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn folding() {
        assert_eq!(fold("PAYLARIN GERİ ALINMASI"), "paylarin geri alinmasi");
        assert_eq!(fold("Kâr Payı Şirketi"), "kar payi sirketi");
    }

    #[test]
    fn request_bodies() {
        let b = list_body("21.10.2026", "22.10.2026", None);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["fromDate"], "21.10.2026");
        assert!(v["mkkMemberOid"].is_null());
        assert_eq!(v["memberTypes"][0], "IGS");
        assert_eq!(v["disclosureTypes"].as_array().unwrap().len(), 3);
        assert_eq!(istanbul_date(0).len(), 10);
    }
}
