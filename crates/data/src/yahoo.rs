//! Yahoo Finance public chart history (`/v8/finance/chart`). No key required.
//!
//! Yahoo has no 4h interval, so 4h is resampled from 1h. Intraday depth is limited by
//! Yahoo: 1m ≈ 7 days per request (30 days total), 5–30m ≈ 60 days, 1h ≈ 730 days.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tr_core::{AssetClass, Bar, Security, Timeframe, catalog};

use crate::{Extended, ExtendedQuote, MarketData, Quote, QuoteSource, SearchHit, Session, TradingSession};

const BASE: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

pub struct YahooProvider {
    client: reqwest::Client,
    /// Session crumb for endpoints that require one (options); the cookie lives in the
    /// client's cookie store.
    crumb: tokio::sync::Mutex<Option<String>>,
}

impl YahooProvider {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            // Yahoo rejects requests without a browser-like user agent.
            .user_agent("Mozilla/5.0 (compatible; terminal-rs)")
            .timeout(Duration::from_secs(20))
            .cookie_store(true)
            .build()?;
        Ok(Self { client, crumb: tokio::sync::Mutex::new(None) })
    }

    /// Poll Yahoo for the forming bar of `symbol` every few seconds (it has no push feed).
    /// Only the bar-sized intervals Yahoo serves natively; W/M and 4h are resampled, so skip.
    ///
    /// Yahoo's newest intraday row is stamped with the last trade time, not the bar open, so
    /// rows are snapped onto the grid of the older (complete) rows: the pane sees one bar per
    /// interval that updates in place and a new one only when the interval rolls over.
    pub fn live(&self, symbol: String, tf: Timeframe) -> Option<crate::LiveFeed> {
        if matches!(tf, Timeframe::W1 | Timeframe::MN1 | Timeframe::H4) {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let me = Self { client: self.client.clone(), crumb: tokio::sync::Mutex::new(None) };
        tokio::spawn(async move {
            let (iv, _) = interval(tf);
            let step = tf.seconds();
            while !tx.is_closed() {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let now = chrono::Utc::now().timestamp();
                let back = if tf == Timeframe::D1 { 5 * 86_400 } else { step * 4 + 3_600 };
                let chart = match me.chart(&symbol, iv, now - back, now + 60).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!("yahoo live {symbol}: {e:#}");
                        continue;
                    }
                };
                let Some(bar) = forming_bar(&chart, tf, now) else { continue };
                if tx.send(bar).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }

    async fn chart(&self, symbol: &str, interval: &str, period1: i64, period2: i64) -> Result<Chart> {
        let url = format!("{BASE}/{}", urlencode(symbol));
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("interval", interval.to_string()),
                ("period1", period1.to_string()),
                ("period2", period2.to_string()),
                ("includePrePost", "false".into()),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("yahoo {symbol}: HTTP {}", resp.status());
        }
        parse_chart(&resp.text().await?)
    }
}

/// The current bar of `chart`, aligned to the interval grid (see [`YahooProvider::live`]).
fn forming_bar(chart: &Chart, tf: Timeframe, now: i64) -> Option<Bar> {
    if tf == Timeframe::D1 {
        return chart.bars.last().copied();
    }
    let step = tf.seconds();
    let anchor = chart.bars.first()?.ts;
    let bucket = |ts: i64| anchor + (ts - anchor).div_euclid(step) * step;
    let last_bucket = bucket(chart.bars.last()?.ts);
    let mut cur: Option<Bar> = None;
    for b in chart.bars.iter().filter(|b| bucket(b.ts) == last_bucket) {
        cur = Some(match cur {
            None => Bar::new(last_bucket, b.open, b.high, b.low, b.close, b.volume),
            Some(c) => {
                Bar::new(last_bucket, c.open, c.high.max(b.high), c.low.min(b.low), b.close, c.volume + b.volume)
            }
        });
    }
    let mut bar = cur?;
    // While this interval is still running, pin the close to the latest trade.
    if now < last_bucket + step
        && let Some(p) = chart.last_price
    {
        bar.close = p;
        bar.high = bar.high.max(p);
        bar.low = bar.low.min(p);
    }
    Some(bar)
}

/// Yahoo stamps the forming intraday row with the last trade time. Snap the last few rows
/// onto the interval grid of the rows before them, merging rows that share an interval, so
/// live updates replace the forming bar instead of adding a second one.
fn snap_tail(bars: &mut Vec<Bar>, step: i64) {
    const TAIL: usize = 5;
    if bars.len() <= TAIL {
        return;
    }
    let anchor = bars[bars.len() - TAIL - 1].ts;
    let tail = bars.split_off(bars.len() - TAIL);
    for b in tail {
        let ts = anchor + (b.ts - anchor).div_euclid(step) * step;
        match bars.last_mut() {
            Some(last) if last.ts == ts => {
                last.high = last.high.max(b.high);
                last.low = last.low.min(b.low);
                last.close = b.close;
                last.volume += b.volume;
            }
            _ => bars.push(Bar::new(ts, b.open, b.high, b.low, b.close, b.volume)),
        }
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'^' | b'=' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Map our timeframe to a Yahoo interval and the maximum lookback Yahoo serves for it.
fn interval(tf: Timeframe) -> (&'static str, Option<i64>) {
    const DAY: i64 = 86_400;
    match tf {
        Timeframe::M1 => ("1m", Some(7 * DAY)),
        Timeframe::M5 => ("5m", Some(59 * DAY)),
        Timeframe::M15 => ("15m", Some(59 * DAY)),
        Timeframe::M30 => ("30m", Some(59 * DAY)),
        Timeframe::H1 | Timeframe::H4 => ("60m", Some(729 * DAY)),
        Timeframe::D1 => ("1d", None),
        Timeframe::W1 => ("1wk", None),
        Timeframe::MN1 => ("1mo", None),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub bars: Vec<Bar>,
    pub last_price: Option<f64>,
    pub prev_close: Option<f64>,
}

#[derive(Deserialize)]
struct Envelope {
    chart: ChartBody,
}
#[derive(Deserialize)]
struct ChartBody {
    result: Option<Vec<ChartResult>>,
    error: Option<serde_json::Value>,
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
    regular_market_price: Option<f64>,
    chart_previous_close: Option<f64>,
    previous_close: Option<f64>,
}
#[derive(Deserialize)]
struct Indicators {
    quote: Vec<QuoteArrays>,
}
#[derive(Deserialize, Default)]
struct QuoteArrays {
    #[serde(default)]
    open: Vec<Option<f64>>,
    #[serde(default)]
    high: Vec<Option<f64>>,
    #[serde(default)]
    low: Vec<Option<f64>>,
    #[serde(default)]
    close: Vec<Option<f64>>,
    #[serde(default)]
    volume: Vec<Option<f64>>,
}

/// Parse a chart response. Rows with any missing OHLC value (Yahoo emits nulls for
/// halted or partial periods) are skipped.
pub fn parse_chart(json: &str) -> Result<Chart> {
    let env: Envelope = serde_json::from_str(json).context("parsing Yahoo chart JSON")?;
    if let Some(err) = env.chart.error.filter(|e| !e.is_null()) {
        return Err(anyhow!("yahoo error: {err}"));
    }
    let r = env.chart.result.and_then(|mut v| v.pop()).ok_or_else(|| anyhow!("empty Yahoo result"))?;
    let q = r.indicators.quote.into_iter().next().unwrap_or_default();
    let mut bars = Vec::with_capacity(r.timestamp.len());
    for (i, &ts) in r.timestamp.iter().enumerate() {
        let get = |v: &Vec<Option<f64>>| v.get(i).copied().flatten();
        if let (Some(o), Some(h), Some(l), Some(c)) = (get(&q.open), get(&q.high), get(&q.low), get(&q.close)) {
            bars.push(Bar::new(ts, o, h, l, c, get(&q.volume).unwrap_or(0.0)));
        }
    }
    Ok(Chart {
        bars,
        last_price: r.meta.regular_market_price,
        prev_close: r.meta.previous_close.or(r.meta.chart_previous_close),
    })
}

#[async_trait]
impl MarketData for YahooProvider {
    fn name(&self) -> &str {
        "yahoo"
    }

    async fn history(&self, symbol: &str, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let now = chrono::Utc::now().timestamp();
        let period2 = end.unwrap_or(now + 60);
        let (iv, max_back) = interval(tf);
        let base_tf = if tf == Timeframe::H4 { Timeframe::H1 } else { tf };
        let factor = if tf == Timeframe::H4 { 4 } else { 1 };
        // Over-ask to cover weekends/holidays and, intraday, the ~6.5h equity session
        // (crypto and FX simply return more than needed and get clipped).
        let stretch = if tf.is_intraday() { 6 } else { 2 };
        let want = limit as i64 * factor * base_tf.seconds() * stretch + 7 * 86_400;
        let mut period1 = period2 - want;
        if let Some(max) = max_back {
            period1 = period1.max(now - max);
        }
        if period1 >= period2 {
            return Ok(Vec::new());
        }
        let mut bars = self.chart(symbol, iv, period1, period2).await?.bars;
        if tf.is_intraday() {
            snap_tail(&mut bars, base_tf.seconds());
        }
        // Yahoo's weekly/monthly timestamps are session opens; snap to our calendar buckets.
        if matches!(tf, Timeframe::W1 | Timeframe::MN1 | Timeframe::H4) {
            bars = tf.resample(&bars);
        }
        Ok(crate::clip(bars, end, limit))
    }

    /// Batched via the spark endpoint (20 symbols per request).
    async fn quotes(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let mut out = Vec::with_capacity(symbols.len());
        for chunk in symbols.chunks(20) {
            out.extend(self.spark(chunk).await?);
        }
        Ok(out)
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        let resp = self
            .client
            .get(SEARCH)
            .query(&[("q", query), ("quotesCount", "12"), ("newsCount", "0"), ("listsCount", "0")])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("yahoo search: HTTP {}", resp.status());
        }
        parse_search(&resp.text().await?)
    }
}

const OPTIONS: &str = "https://query2.finance.yahoo.com/v7/finance/options";

impl YahooProvider {
    /// A session crumb: visiting fc.yahoo.com sets the consent cookie, then getcrumb
    /// returns the token bound to it. Cached until a request is rejected.
    async fn crumb(&self, refresh: bool) -> Result<String> {
        let mut guard = self.crumb.lock().await;
        if let Some(c) = guard.as_ref().filter(|_| !refresh) {
            return Ok(c.clone());
        }
        // fc.yahoo.com answers 404 but sets the cookie; only transport errors matter.
        self.client.get("https://fc.yahoo.com").send().await.context("yahoo session")?;
        let resp = self.client.get("https://query2.finance.yahoo.com/v1/test/getcrumb").send().await?;
        let crumb = resp.text().await?.trim().to_string();
        if crumb.is_empty() || crumb.contains('{') || crumb.len() > 64 {
            bail!("yahoo did not issue a crumb");
        }
        *guard = Some(crumb.clone());
        Ok(crumb)
    }

    /// Option chain for a Yahoo symbol and expiration (`None` = nearest).
    pub async fn options(&self, symbol: &str, expiration: Option<i64>) -> Result<crate::OptionChain> {
        let mut last_err = None;
        for refresh in [false, true] {
            let crumb = self.crumb(refresh).await?;
            let mut q = vec![("crumb", crumb)];
            if let Some(e) = expiration {
                q.push(("date", e.to_string()));
            }
            let resp = self.client.get(format!("{OPTIONS}/{}", urlencode(symbol))).query(&q).send().await?;
            let status = resp.status();
            let body = resp.text().await?;
            if status.as_u16() == 401 || body.contains("Invalid Crumb") {
                last_err = Some(anyhow!("yahoo options: crumb rejected"));
                continue;
            }
            if !status.is_success() {
                bail!("yahoo options: HTTP {status}");
            }
            return parse_options(&body);
        }
        Err(last_err.unwrap_or_else(|| anyhow!("yahoo options failed")))
    }

    /// Headlines related to a Yahoo symbol or free-text query.
    pub async fn news(&self, query: &str) -> Result<Vec<crate::NewsItem>> {
        let resp = self
            .client
            .get(SEARCH)
            .query(&[("q", query), ("quotesCount", "0"), ("newsCount", "25"), ("listsCount", "0")])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("yahoo news: HTTP {}", resp.status());
        }
        parse_news(&resp.text().await?)
    }
}

/// Parse `/v7/finance/options/{symbol}`.
pub fn parse_options(json: &str) -> Result<crate::OptionChain> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo options")?;
    let r = &v["optionChain"]["result"][0];
    if r.is_null() {
        let e = v["optionChain"]["error"]["description"].as_str().unwrap_or("no option chain");
        bail!("yahoo options: {e}");
    }
    let q = &r["quote"];
    let spot = q["regularMarketPrice"].as_f64().ok_or_else(|| anyhow!("option chain has no underlying price"))?;
    let num = |c: &serde_json::Value, k: &str| c[k].as_f64().filter(|x| x.is_finite());
    let contracts = |side: &serde_json::Value| -> Vec<crate::OptionContract> {
        let mut out: Vec<crate::OptionContract> = side
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(crate::OptionContract {
                            contract: c["contractSymbol"].as_str()?.to_string(),
                            strike: c["strike"].as_f64()?,
                            last: num(c, "lastPrice"),
                            change: num(c, "change"),
                            bid: num(c, "bid"),
                            ask: num(c, "ask"),
                            volume: c["volume"].as_u64(),
                            open_interest: c["openInterest"].as_u64(),
                            // Yahoo reports ~0 as a placeholder for illiquid contracts.
                            iv: num(c, "impliedVolatility").filter(|x| *x >= 0.005),
                            in_the_money: c["inTheMoney"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.strike.total_cmp(&b.strike));
        out
    };
    let o = &r["options"][0];
    let mut expirations: Vec<i64> =
        r["expirationDates"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
    expirations.sort_unstable();
    Ok(crate::OptionChain {
        underlying: r["underlyingSymbol"].as_str().unwrap_or_default().to_string(),
        spot,
        dividend_yield: q["trailingAnnualDividendYield"].as_f64().unwrap_or(0.0).clamp(0.0, 0.2),
        expiration: o["expirationDate"].as_i64().or(expirations.first().copied()).unwrap_or_default(),
        expirations,
        calls: contracts(&o["calls"]),
        puts: contracts(&o["puts"]),
        ts: q["regularMarketTime"].as_i64().unwrap_or_default(),
        source: "Yahoo".into(),
        simulated: false,
    })
}

/// Parse the `news` array of `/v1/finance/search`, newest first.
pub fn parse_news(json: &str) -> Result<Vec<crate::NewsItem>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo news")?;
    let mut out: Vec<crate::NewsItem> = v["news"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| {
                    Some(crate::NewsItem {
                        title: n["title"].as_str()?.trim().to_string(),
                        publisher: n["publisher"].as_str().unwrap_or("").to_string(),
                        link: n["link"].as_str()?.to_string(),
                        ts: n["providerPublishTime"].as_i64().unwrap_or_default(),
                        tickers: n["relatedTickers"]
                            .as_array()
                            .map(|t| t.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|a| std::cmp::Reverse(a.ts));
    Ok(out)
}

const SPARK: &str = "https://query1.finance.yahoo.com/v8/finance/spark";
const SEARCH: &str = "https://query2.finance.yahoo.com/v1/finance/search";

impl YahooProvider {
    /// Last price, previous close and the last few daily closes for each Yahoo symbol.
    pub async fn spark(&self, symbols: &[String]) -> Result<Vec<Quote>> {
        let resp = self
            .client
            .get(SPARK)
            .query(&[("symbols", symbols.join(",")), ("range", "10d".into()), ("interval", "1d".into())])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("yahoo spark: HTTP {}", resp.status());
        }
        parse_spark(&resp.text().await?)
    }
}

impl YahooProvider {
    /// Pre/post-market price from today's 1-minute chart including extended hours.
    pub async fn extended_chart(&self, symbol: &str) -> Result<Option<ExtendedQuote>> {
        let url = format!("{BASE}/{}", urlencode(symbol));
        let resp = self
            .client
            .get(&url)
            .query(&[("interval", "1m"), ("range", "1d"), ("includePrePost", "true")])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("yahoo {symbol} extended hours: HTTP {}", resp.status());
        }
        parse_extended_chart(&resp.text().await?, chrono::Utc::now().timestamp())
    }
}

/// Classify a chart with `includePrePost=true` at `now`: during the regular session there is
/// no extended move; otherwise the newest trade before the open is measured from the previous
/// close, and the newest after the close from the regular-session price.
pub fn parse_extended_chart(json: &str, now: i64) -> Result<Option<ExtendedQuote>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo chart")?;
    let r = &v["chart"]["result"][0];
    let meta = &r["meta"];
    let period = |k: &str| {
        let p = &meta["currentTradingPeriod"][k];
        Some((p["start"].as_i64()?, p["end"].as_i64()?))
    };
    let (Some(regular), Some(price)) = (period("regular"), meta["regularMarketPrice"].as_f64()) else {
        return Ok(None);
    };
    if (regular.0..regular.1).contains(&now) {
        return Ok(Some(ExtendedQuote { session: TradingSession::Regular, price, reference: price, ts: now }));
    }
    let prev = meta["previousClose"].as_f64().or_else(|| meta["chartPreviousClose"].as_f64());
    let ts = r["timestamp"].as_array().cloned().unwrap_or_default();
    let closes = r["indicators"]["quote"][0]["close"].as_array().cloned().unwrap_or_default();
    let last = ts.iter().zip(&closes).rev().find_map(|(t, c)| Some((t.as_i64()?, c.as_f64()?)));
    Ok(Some(match last {
        Some((t, p)) if t >= regular.1 => {
            ExtendedQuote { session: TradingSession::Post, price: p, reference: price, ts: t }
        }
        Some((t, p)) if t < regular.0 => match prev {
            Some(prev) => ExtendedQuote { session: TradingSession::Pre, price: p, reference: prev, ts: t },
            None => return Ok(None),
        },
        // No extended-hours trade yet.
        _ => ExtendedQuote { session: TradingSession::Regular, price, reference: price, ts: now },
    }))
}

/// Parse `/v8/finance/spark`: `{SYMBOL: {timestamp: [...], close: [...], chartPreviousClose}}`.
pub fn parse_spark(json: &str) -> Result<Vec<Quote>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo spark")?;
    let obj = v.as_object().ok_or_else(|| anyhow!("unexpected spark payload"))?;
    let mut out = Vec::new();
    for (sym, d) in obj {
        let closes: Vec<f64> =
            d["close"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
        let Some(&last) = closes.last() else { continue };
        let prev =
            if closes.len() >= 2 { closes[closes.len() - 2] } else { d["chartPreviousClose"].as_f64().unwrap_or(last) };
        let ts = d["timestamp"].as_array().and_then(|a| a.last()).and_then(|t| t.as_i64()).unwrap_or_default();
        out.push(Quote {
            symbol: sym.clone(),
            last,
            prev_close: prev,
            ts,
            spark: closes,
            extended: None,
            source: QuoteSource::Yahoo,
            traded: None,
            delay_min: None,
        });
    }
    Ok(out)
}

const QUOTE: &str = "https://query2.finance.yahoo.com/v7/finance/quote";

impl YahooProvider {
    /// The latest pre-market or after-hours trade for each Yahoo symbol that has one (US
    /// listings only; the spark endpoint carries regular-session prices only).
    pub async fn extended(&self, symbols: &[String]) -> Result<Vec<(String, Extended)>> {
        Ok(self.extended_and_times(symbols).await?.0)
    }

    /// Extended-hours trades plus each symbol's last regular-session trade time, from the
    /// same request (the spark endpoint only dates its daily bars).
    pub async fn extended_and_times(&self, symbols: &[String]) -> Result<(Vec<(String, Extended)>, Vec<QuoteMeta>)> {
        let (mut ext, mut times) = (Vec::new(), Vec::new());
        for chunk in symbols.chunks(50) {
            let (e, t) = self.extended_chunk(chunk).await?;
            ext.extend(e);
            times.extend(t);
        }
        Ok((ext, times))
    }

    async fn extended_chunk(&self, symbols: &[String]) -> Result<(Vec<(String, Extended)>, Vec<QuoteMeta>)> {
        let fields = "marketState,regularMarketPrice,regularMarketTime,exchangeDataDelayedBy,preMarketPrice,preMarketChange,\
                      preMarketChangePercent,preMarketTime,postMarketPrice,postMarketChange,\
                      postMarketChangePercent,postMarketTime";
        let mut last_err = None;
        for refresh in [false, true] {
            let crumb = self.crumb(refresh).await?;
            let q = [("symbols", symbols.join(",")), ("fields", fields.to_string()), ("crumb", crumb)];
            let resp = self.client.get(QUOTE).query(&q).send().await?;
            let status = resp.status();
            let body = resp.text().await?;
            if status.as_u16() == 401 || body.contains("Invalid Crumb") {
                last_err = Some(anyhow!("yahoo quote: crumb rejected"));
                continue;
            }
            if !status.is_success() {
                bail!("yahoo quote: HTTP {status}");
            }
            return Ok((parse_extended(&body, chrono::Utc::now().timestamp())?, parse_quote_meta(&body)?));
        }
        Err(last_err.unwrap_or_else(|| anyhow!("yahoo quote failed")))
    }
}

/// Per-symbol facts from `/v7/finance/quote`: last regular-session trade time and the
/// exchange's feed delay in minutes (`exchangeDataDelayedBy`).
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteMeta {
    pub symbol: String,
    pub traded: Option<i64>,
    pub delay_min: Option<u32>,
}

pub fn parse_quote_meta(json: &str) -> Result<Vec<QuoteMeta>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo quote")?;
    let rows = v["quoteResponse"]["result"].as_array().ok_or_else(|| anyhow!("unexpected quote payload"))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let traded = r["regularMarketTime"].as_i64().filter(|t| *t > 0);
            let delay_min = r["exchangeDataDelayedBy"].as_u64().and_then(|d| u32::try_from(d).ok());
            Some(QuoteMeta { symbol: r["symbol"].as_str()?.to_string(), traded, delay_min })
        })
        .collect())
}

/// Parse `/v7/finance/quote` into extended-hours trades. Outside regular hours the newer of
/// the pre- and post-market trades wins, provided it came after the regular session's last
/// trade and within the last four days (so a stale price never outlives a long weekend).
pub fn parse_extended(json: &str, now: i64) -> Result<Vec<(String, Extended)>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo quote")?;
    let rows = v["quoteResponse"]["result"].as_array().ok_or_else(|| anyhow!("unexpected quote payload"))?;
    let mut out = Vec::new();
    for r in rows {
        let Some(sym) = r["symbol"].as_str() else { continue };
        if r["marketState"].as_str() == Some("REGULAR") {
            continue;
        }
        let regular = r["regularMarketPrice"].as_f64().filter(|x| *x > 0.0);
        let regular_ts = r["regularMarketTime"].as_i64().unwrap_or_default();
        let leg = |session: Session, prefix: &str| {
            let price = r[format!("{prefix}Price")].as_f64().filter(|x| x.is_finite() && *x > 0.0)?;
            let ts = r[format!("{prefix}Time")].as_i64()?;
            if ts <= regular_ts || now - ts > 4 * 86_400 {
                return None;
            }
            let change = r[format!("{prefix}Change")].as_f64().or_else(|| Some(price - regular?))?;
            let change_pct = r[format!("{prefix}ChangePercent")]
                .as_f64()
                .or_else(|| regular.map(|b| change / b * 100.0))
                .unwrap_or_default();
            Some(Extended { session, price, change, change_pct, ts })
        };
        let best = match (leg(Session::Pre, "preMarket"), leg(Session::Post, "postMarket")) {
            (Some(a), Some(b)) => Some(if a.ts >= b.ts { a } else { b }),
            (a, b) => a.or(b),
        };
        if let Some(e) = best {
            out.push((sym.to_string(), e));
        }
    }
    Ok(out)
}

/// Map a Yahoo symbol and quote type to a market key.
pub fn security_for_yahoo(symbol: &str, quote_type: &str) -> Option<Security> {
    if let Some(l) = catalog::CATALOG.iter().find(|l| l.yahoo == Some(symbol)) {
        return Some(l.security());
    }
    let class_share = |s: &str| match s.split_once('-') {
        Some((a, b)) if b.len() == 1 && a.chars().all(|c| c.is_ascii_uppercase()) => format!("{a}.{b}"),
        _ => s.to_string(),
    };
    Some(match quote_type {
        "EQUITY" => Security::new(class_share(symbol), AssetClass::Equity),
        "ETF" => Security::new(class_share(symbol), AssetClass::Etf),
        "INDEX" => Security::new(symbol, AssetClass::Index),
        "CURRENCY" => Security::new(symbol.trim_end_matches("=X"), AssetClass::Fx),
        "CRYPTOCURRENCY" => Security::new(symbol.replace('-', ""), AssetClass::Crypto),
        "FUTURE" => Security::new(symbol, AssetClass::Cmdty),
        _ => return None,
    })
}

/// Yahoo symbol for any security (catalog first, then class conventions).
pub fn yahoo_symbol(sec: &Security) -> String {
    if let Some(y) = catalog::by_security(sec).and_then(|l| l.yahoo) {
        return y.to_string();
    }
    match sec.class {
        AssetClass::Equity | AssetClass::Etf => {
            let t = sec.ticker.to_ascii_uppercase();
            // Borsa Istanbul indices are typed bare (`XU100`) but Yahoo lists them as `XU100.IS`.
            if matches!(t.as_str(), "XU100" | "XU030") {
                return format!("{t}.IS");
            }
            // `THYAO.IS`, `SAP.DE`: a 2+ letter suffix is an exchange, not a share class (`BRK.B`).
            match t.rsplit_once('.') {
                Some((_, suffix)) if suffix.len() >= 2 => t,
                _ => sec.ticker.replace('.', "-"),
            }
        }
        AssetClass::Index if sec.ticker.starts_with('^') => sec.ticker.clone(),
        AssetClass::Index => format!("^{}", sec.ticker),
        AssetClass::Fx => format!("{}=X", sec.ticker),
        AssetClass::Crypto => match sec.pair() {
            Some((b, q)) => format!("{b}-{}", if q == "USDT" { "USD" } else { q }),
            None => sec.ticker.clone(),
        },
        AssetClass::Govt | AssetClass::Cmdty => sec.ticker.clone(),
    }
}

/// The currency a Yahoo symbol is quoted in, judged from its exchange suffix or index.
/// Unknown symbols are assumed USD.
pub fn currency_of(yahoo_symbol: &str) -> &'static str {
    match yahoo_symbol {
        "^GDAXI" | "^FCHI" | "^STOXX50E" | "^IBEX" | "FTSEMIB.MI" => return "EUR",
        "^FTSE" => return "GBP",
        "^N225" => return "JPY",
        "^HSI" => return "HKD",
        "^BVSP" => return "BRL",
        "^GSPTSE" => return "CAD",
        "^AXJO" => return "AUD",
        "^KS11" => return "KRW",
        "^BSESN" => return "INR",
        "^SSMI" => return "CHF",
        "^MXX" => return "MXN",
        _ => {}
    }
    match yahoo_symbol.rsplit_once('.').map(|(_, x)| x) {
        Some("IS") => "TRY",
        Some("DE" | "PA" | "MI" | "AS" | "MC" | "BR" | "HE" | "VI" | "LS") => "EUR",
        Some("T") => "JPY",
        Some("HK") => "HKD",
        Some("TO") => "CAD",
        Some("SW") => "CHF",
        Some("AX") => "AUD",
        Some("SA") => "BRL",
        Some("NS" | "BO") => "INR",
        Some("KS") => "KRW",
        _ => "USD",
    }
}

/// Parse `/v1/finance/search` into market-key hits.
pub fn parse_search(json: &str) -> Result<Vec<SearchHit>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing Yahoo search")?;
    Ok(v["quotes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|q| {
                    let sec = security_for_yahoo(q["symbol"].as_str()?, q["quoteType"].as_str()?)?;
                    let name = q["shortname"].as_str().or(q["longname"].as_str()).unwrap_or("").trim().to_string();
                    Some(SearchHit {
                        key: sec.key(),
                        name,
                        class: sec.class,
                        exchange: q["exchDisp"].as_str().or(q["exchange"].as_str()).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_hours_sessions() {
        let chart = |ts: &str, closes: &str| {
            format!(
                r#"{{"chart":{{"result":[{{"meta":{{"regularMarketPrice":200.0,"previousClose":190.0,
                "currentTradingPeriod":{{"pre":{{"start":50,"end":100}},"regular":{{"start":100,"end":200}},
                "post":{{"start":200,"end":300}}}}}},"timestamp":[{ts}],
                "indicators":{{"quote":[{{"close":[{closes}]}}]}}}}],"error":null}}}}"#
            )
        };
        let q = |json: &str, now| parse_extended_chart(json, now).unwrap().unwrap();
        // Regular session: no move.
        let r = q(&chart("60,150", "195.0,200.0"), 150);
        assert_eq!((r.session, r.move_pct()), (TradingSession::Regular, None));
        // Pre-market against yesterday's close.
        let pre = q(&chart("60,70", "191.0,199.5"), 80);
        assert_eq!((pre.session, pre.price, pre.reference), (TradingSession::Pre, 199.5, 190.0));
        assert!((pre.move_pct().unwrap() - 5.0).abs() < 1e-9);
        // After hours against today's close; null rows are skipped.
        let post = q(&chart("150,210,220", "200.0,190.0,null"), 250);
        assert_eq!((post.session, post.price, post.reference), (TradingSession::Post, 190.0, 200.0));
        assert!((post.move_pct().unwrap() + 5.0).abs() < 1e-9);
        // After the close with no extended trade yet.
        assert_eq!(q(&chart("150", "200.0"), 205).session, TradingSession::Regular);
        assert!(parse_extended_chart(r#"{"chart":{"result":null,"error":{}}}"#, 0).unwrap().is_none());
    }

    #[test]
    fn parses_fixture() {
        let json = include_str!("../tests/fixtures/yahoo_aapl_1d.json");
        let c = parse_chart(json).unwrap();
        assert_eq!(c.bars.len(), 7);
        assert!(c.bars.windows(2).all(|w| w[0].ts < w[1].ts));
        assert!(c.bars.iter().all(|b| b.high >= b.low && b.volume > 0.0));
        assert_eq!(c.last_price, Some(336.64));
    }

    #[test]
    fn extended_hours_pick_the_newest_leg_after_the_close() {
        let json = r#"{"quoteResponse":{"result":[
            {"symbol":"GOOGL","marketState":"PREPRE","regularMarketPrice":349.22,"regularMarketTime":1000,
             "postMarketPrice":347.9954,"postMarketChange":-1.2246,"postMarketChangePercent":-0.3507,"postMarketTime":1500},
            {"symbol":"AAPL","marketState":"PRE","regularMarketPrice":342.0,"regularMarketTime":1000,
             "postMarketPrice":340.67,"postMarketTime":1500,"preMarketPrice":345.0,"preMarketTime":2000},
            {"symbol":"MSFT","marketState":"REGULAR","regularMarketPrice":500.0,"regularMarketTime":3000,
             "postMarketPrice":499.0,"postMarketTime":1500},
            {"symbol":"OLD","marketState":"CLOSED","regularMarketPrice":10.0,"regularMarketTime":1000,
             "postMarketPrice":11.0,"postMarketTime":900},
            {"symbol":"THYAO.IS","marketState":"REGULAR","regularMarketPrice":276.75,"regularMarketTime":1000}
        ]}}"#;
        let x = parse_extended(json, 3000).unwrap();
        assert_eq!(x.len(), 2, "regular hours and pre-close trades show nothing: {x:?}");
        let (s, g) = &x[0];
        assert_eq!((s.as_str(), g.session, g.price, g.ts), ("GOOGL", Session::Post, 347.9954, 1500));
        assert!((g.change_pct + 0.3507).abs() < 1e-9);
        let (s, a) = &x[1];
        assert_eq!((s.as_str(), a.session, a.price), ("AAPL", Session::Pre, 345.0));
        assert!((a.change - 3.0).abs() < 1e-9 && (a.change_pct - 3.0 / 342.0 * 100.0).abs() < 1e-9, "derived");
        assert!(parse_extended(json, 1500 + 5 * 86_400).unwrap().is_empty(), "stale after a few days");
    }

    #[test]
    fn skips_null_rows_and_reports_errors() {
        let json = r#"{"chart":{"result":[{"meta":{},"timestamp":[1,2],"indicators":{"quote":[{"open":[1.0,null],"high":[2.0,2.0],"low":[0.5,0.5],"close":[1.5,1.5],"volume":[10,10]}]}}],"error":null}}"#;
        assert_eq!(parse_chart(json).unwrap().bars.len(), 1);
        let err = r#"{"chart":{"result":null,"error":{"code":"Not Found","description":"No data found"}}}"#;
        assert!(parse_chart(err).is_err());
    }

    #[test]
    fn options_fixture() {
        let c = parse_options(include_str!("../tests/fixtures/yahoo_options.json")).unwrap();
        assert_eq!(c.underlying, "AAPL");
        assert!((c.spot - 340.155).abs() < 1e-9);
        assert_eq!(c.expirations.len(), 24);
        assert!(c.expirations.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(c.expiration, c.expirations[0]);
        assert_eq!((c.calls.len(), c.puts.len()), (8, 8));
        assert!(c.calls.windows(2).all(|w| w[0].strike < w[1].strike));
        let k = c.calls.iter().find(|k| k.strike < c.spot).unwrap();
        assert!(k.in_the_money && k.iv.is_some() && k.bid.is_some() && k.open_interest.is_some());
        assert!(c.calls.iter().all(|k| k.contract.starts_with("AAPL")));
        assert!(!c.simulated && c.dividend_yield > 0.0);
        let err = r#"{"optionChain":{"result":[],"error":{"description":"No data found"}}}"#;
        assert!(parse_options(err).is_err());
    }

    #[test]
    fn news_fixture() {
        let n = parse_news(include_str!("../tests/fixtures/yahoo_news.json")).unwrap();
        assert!(!n.is_empty());
        assert!(n.windows(2).all(|w| w[0].ts >= w[1].ts));
        assert!(n.iter().all(|x| !x.title.is_empty() && x.link.starts_with("https://")));
    }

    #[test]
    fn spark_and_search_fixtures() {
        let q = parse_spark(include_str!("../tests/fixtures/yahoo_spark.json")).unwrap();
        assert_eq!(q.len(), 4);
        let spx = q.iter().find(|x| x.symbol == "^GSPC").unwrap();
        assert_eq!(spx.last, 7811.54);
        assert_eq!(spx.prev_close, 7765.36);
        assert_eq!(spx.spark.len(), 5);
        let hits = parse_search(include_str!("../tests/fixtures/yahoo_search.json")).unwrap();
        assert_eq!(hits[0].key, "NVDA US Equity");
        assert!(hits.iter().any(|h| h.class == AssetClass::Etf));
        assert!(hits.iter().any(|h| h.class == AssetClass::Cmdty));
    }

    #[test]
    fn trade_times_and_delays_come_from_the_quote_payload() {
        let json = r#"{"quoteResponse":{"result":[{"symbol":"AAPL","regularMarketTime":1791000000,"exchangeDataDelayedBy":15},
            {"symbol":"EURUSD=X","regularMarketTime":0,"exchangeDataDelayedBy":0},{"symbol":"X"}]}}"#;
        let m = |symbol: &str, traded, delay_min| QuoteMeta { symbol: symbol.into(), traded, delay_min };
        assert_eq!(
            parse_quote_meta(json).unwrap(),
            vec![m("AAPL", Some(1_791_000_000), Some(15)), m("EURUSD=X", None, Some(0)), m("X", None, None)]
        );
    }

    #[test]
    fn symbol_mapping() {
        let s = |t: &str, c| yahoo_symbol(&Security::new(t, c));
        assert_eq!(s("SPX", AssetClass::Index), "^GSPC");
        assert_eq!(s("CL1", AssetClass::Cmdty), "CL=F");
        assert_eq!(s("AUDNZD", AssetClass::Fx), "AUDNZD=X");
        assert_eq!(s("BRK.B", AssetClass::Equity), "BRK-B");
        assert_eq!(s("ZZZ", AssetClass::Equity), "ZZZ");
        assert_eq!(security_for_yahoo("EURUSD=X", "CURRENCY").unwrap().key(), "EURUSD Curncy");
        assert_eq!(security_for_yahoo("GC=F", "FUTURE").unwrap().key(), "GC1 Comdty");
        assert_eq!(security_for_yahoo("DOGE-USD", "CRYPTOCURRENCY").unwrap().key(), "DOGEUSD Curncy");
        assert_eq!(security_for_yahoo("BF-B", "EQUITY").unwrap().ticker, "BF.B");
    }

    #[test]
    fn encodes_index_symbols() {
        assert_eq!(urlencode("^GSPC"), "^GSPC");
        assert_eq!(urlencode("BRK B"), "BRK%20B");
    }
}

#[cfg(test)]
mod symbol_tests {
    use super::*;

    #[test]
    fn borsa_istanbul_symbols() {
        let y = |t: &str| yahoo_symbol(&Security::equity(t));
        assert_eq!(y("XU100"), "XU100.IS");
        assert_eq!(y("xu030"), "XU030.IS");
        assert_eq!(y("THYAO.IS"), "THYAO.IS");
        assert_eq!(y("BRK.B"), "BRK-B");
        assert_eq!(currency_of("THYAO.IS"), "TRY");
        assert_eq!(currency_of("XU100.IS"), "TRY");
        assert_eq!(currency_of("AAPL"), "USD");
        assert_eq!(currency_of("^GDAXI"), "EUR");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    fn bar(ts: i64, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar::new(ts, o, h, l, c, 10.0)
    }

    #[test]
    fn history_tail_is_snapped_to_the_grid() {
        let t0 = 1_700_000_000 - 1_700_000_000 % 3600 + 1800;
        let mut bars: Vec<Bar> = (0..8).map(|i| bar(t0 + i * 3600, 1.0, 2.0, 0.5, 1.5)).collect();
        bars.push(bar(t0 + 7 * 3600 + 900, 1.5, 3.0, 1.4, 2.5));
        snap_tail(&mut bars, 3600);
        assert_eq!(bars.len(), 8);
        let l = bars.last().unwrap();
        assert_eq!((l.ts, l.high, l.close), (t0 + 7 * 3600, 3.0, 2.5));
    }

    #[test]
    fn off_grid_tail_joins_the_forming_bar() {
        // Hourly grid at :30; the newest row is stamped at the last trade time.
        let t0 = 1_700_000_000 - 1_700_000_000 % 3600 + 1800;
        let chart = Chart {
            bars: vec![
                bar(t0, 100.0, 101.0, 99.0, 100.5),
                bar(t0 + 3600, 100.5, 102.0, 100.0, 101.0),
                bar(t0 + 3600 + 600, 101.0, 101.2, 100.9, 101.1),
            ],
            last_price: Some(101.5),
            prev_close: None,
        };
        let b = forming_bar(&chart, Timeframe::H1, t0 + 3600 + 700).unwrap();
        assert_eq!(b.ts, t0 + 3600);
        assert_eq!((b.open, b.high, b.low, b.close), (100.5, 102.0, 100.0, 101.5));
    }
}
