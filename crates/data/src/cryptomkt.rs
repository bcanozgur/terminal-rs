//! Crypto market overview (`CRYP`), all from free, keyless endpoints:
//!
//! - CoinGecko `/global` (market cap, volume, dominance) and `/coins/categories` (stablecoins),
//!   cached for five minutes to stay inside the free rate limit.
//! - alternative.me Fear & Greed index (30 days).
//! - Binance USDⓈ-M futures (`fapi.binance.com`): mark price, funding, open interest and the
//!   long/short account ratio. The host is geo-blocked in some regions, so every section of the
//!   [`Overview`] carries its own error and the others keep working.
//! - 24h top movers among Binance USDT spot pairs.
//! - Turkey: BtcTurk USDT/TRY and BTC/TRY against the Yahoo USDTRY quote.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::MarketData;
use crate::yahoo::YahooProvider;

const COINGECKO: &str = "https://api.coingecko.com/api/v3";
const FNG: &str = "https://api.alternative.me/fng/?limit=30";
const FAPI: &str = "https://fapi.binance.com";
const SPOT: [&str; 2] = ["https://data-api.binance.vision", "https://api.binance.com"];
const BTCTURK: &str = "https://api.btcturk.com/api/v2/ticker";

/// CoinGecko responses are reused for this long.
const GECKO_TTL: Duration = Duration::from_secs(300);
/// Coins in the derivatives table (Binance perpetual `<COIN>USDT`).
pub const DERIV_COINS: [&str; 6] = ["BTC", "ETH", "SOL", "XRP", "BNB", "DOGE"];
/// Minimum 24h quote volume (USDT) for a spot pair to count as a mover.
pub const MIN_MOVER_VOLUME: f64 = 5_000_000.0;
/// Rows per mover list.
pub const MOVERS_TOP: usize = 10;

const STABLES: [&str; 16] = [
    "USDC", "FDUSD", "TUSD", "USDP", "DAI", "BUSD", "USDE", "USD1", "EUR", "AEUR", "EURI", "XUSD", "RLUSD", "PYUSD",
    "USDS", "UST",
];

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

// ---- global ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Global {
    pub market_cap: f64,
    pub market_cap_change_24h: Option<f64>,
    pub volume_24h: f64,
    pub volume_change_24h: Option<f64>,
    pub btc_dominance: f64,
    pub eth_dominance: f64,
    /// Stablecoin category market cap and its 24h change in percent (optional).
    pub stable_cap: Option<f64>,
    pub stable_change_24h: Option<f64>,
    pub active_coins: Option<u64>,
    /// Unix seconds, CoinGecko's own timestamp.
    pub updated: i64,
}

/// Parse CoinGecko `/global`.
pub fn parse_global(json: &str) -> Result<Global> {
    let v: Value = serde_json::from_str(json).context("parsing CoinGecko global")?;
    let d = v.get("data").ok_or_else(|| anyhow!("CoinGecko global: no data ({})", v["status"]["error_message"]))?;
    Ok(Global {
        market_cap: num(&d["total_market_cap"]["usd"]).ok_or_else(|| anyhow!("CoinGecko global: no market cap"))?,
        market_cap_change_24h: num(&d["market_cap_change_percentage_24h_usd"]),
        volume_24h: num(&d["total_volume"]["usd"]).unwrap_or(0.0),
        volume_change_24h: num(&d["volume_change_percentage_24h_usd"]),
        btc_dominance: num(&d["market_cap_percentage"]["btc"]).unwrap_or(0.0),
        eth_dominance: num(&d["market_cap_percentage"]["eth"]).unwrap_or(0.0),
        stable_cap: None,
        stable_change_24h: None,
        active_coins: d["active_cryptocurrencies"].as_u64(),
        updated: d["updated_at"].as_i64().unwrap_or(0),
    })
}

/// Parse CoinGecko `/coins/categories`: the stablecoins row as `(market cap, 24h change %)`.
pub fn parse_stablecoins(json: &str) -> Result<(f64, Option<f64>)> {
    let rows: Vec<Value> = serde_json::from_str(json).context("parsing CoinGecko categories")?;
    let r = rows
        .iter()
        .find(|r| r["id"] == "stablecoins")
        .ok_or_else(|| anyhow!("CoinGecko categories: no stablecoins row"))?;
    Ok((num(&r["market_cap"]).ok_or_else(|| anyhow!("stablecoins: no market cap"))?, num(&r["market_cap_change_24h"])))
}

// ---- fear & greed ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FearGreed {
    pub value: u32,
    pub label: String,
    /// Unix seconds of today's reading.
    pub ts: i64,
    /// Daily values, oldest first, ending with today's.
    pub history: Vec<f64>,
}

/// Parse alternative.me `/fng/` (newest first).
pub fn parse_fng(json: &str) -> Result<FearGreed> {
    let v: Value = serde_json::from_str(json).context("parsing Fear & Greed")?;
    if let Some(e) = v["metadata"]["error"].as_str() {
        bail!("Fear & Greed: {e}");
    }
    let rows = v["data"].as_array().ok_or_else(|| anyhow!("Fear & Greed: no data"))?;
    let first = rows.first().ok_or_else(|| anyhow!("Fear & Greed: empty"))?;
    let history: Vec<f64> = rows.iter().rev().filter_map(|r| num(&r["value"])).collect();
    Ok(FearGreed {
        value: num(&first["value"]).ok_or_else(|| anyhow!("Fear & Greed: bad value"))? as u32,
        label: first["value_classification"].as_str().unwrap_or("").to_string(),
        ts: num(&first["timestamp"]).unwrap_or(0.0) as i64,
        history,
    })
}

// ---- derivatives ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Deriv {
    /// `BTC`
    pub coin: String,
    pub mark: f64,
    pub index: f64,
    /// Last funding rate as a fraction (0.0001 = 0.01%) per 8 hours.
    pub funding: f64,
    /// Unix milliseconds.
    pub next_funding_ms: i64,
    /// Open interest in coins and in USD, when the endpoints answered.
    pub oi_coins: Option<f64>,
    pub oi_usd: Option<f64>,
    /// Open interest change over 24h in percent.
    pub oi_change_24h: Option<f64>,
    /// Global long/short account ratio and the long share in percent.
    pub ls_ratio: Option<f64>,
    pub long_pct: Option<f64>,
}

impl Deriv {
    /// Funding annualised in percent, assuming three payments a day.
    pub fn funding_apr(&self) -> f64 {
        self.funding * 3.0 * 365.0 * 100.0
    }
}

/// Parse `/fapi/v1/premiumIndex` (one object or the whole array) for the wanted coins.
pub fn parse_premium(json: &str, coins: &[&str]) -> Result<Vec<Deriv>> {
    let v: Value = serde_json::from_str(json).context("parsing Binance premiumIndex")?;
    if let Some(msg) = v.get("msg").and_then(Value::as_str) {
        bail!("Binance futures: {msg}");
    }
    let rows: Vec<Value> = match v {
        Value::Array(a) => a,
        o @ Value::Object(_) => vec![o],
        _ => bail!("Binance premiumIndex: unexpected response"),
    };
    let mut out = Vec::new();
    for coin in coins {
        let sym = format!("{coin}USDT");
        if let Some(r) = rows.iter().find(|r| r["symbol"] == sym.as_str())
            && let Some(mark) = num(&r["markPrice"])
        {
            out.push(Deriv {
                coin: coin.to_string(),
                mark,
                index: num(&r["indexPrice"]).unwrap_or(mark),
                funding: num(&r["lastFundingRate"]).unwrap_or(0.0),
                next_funding_ms: r["nextFundingTime"].as_i64().unwrap_or(0),
                ..Default::default()
            });
        }
    }
    if out.is_empty() {
        bail!("Binance premiumIndex: none of the coins found");
    }
    Ok(out)
}

/// Parse `/fapi/v1/openInterest`: open interest in coins.
pub fn parse_open_interest(json: &str) -> Result<f64> {
    let v: Value = serde_json::from_str(json).context("parsing Binance openInterest")?;
    num(&v["openInterest"]).ok_or_else(|| anyhow!("Binance openInterest: {}", v["msg"]))
}

/// Parse `/futures/data/openInterestHist` (oldest first): `(latest value in USD, change since
/// the first row in percent)`.
pub fn parse_oi_hist(json: &str) -> Result<(f64, Option<f64>)> {
    let rows: Vec<Value> = serde_json::from_str(json).context("parsing Binance openInterestHist")?;
    let vals: Vec<f64> = rows.iter().filter_map(|r| num(&r["sumOpenInterestValue"])).collect();
    let last = *vals.last().ok_or_else(|| anyhow!("Binance openInterestHist: empty"))?;
    let change = vals.first().filter(|f| **f > 0.0 && vals.len() > 1).map(|f| (last / f - 1.0) * 100.0);
    Ok((last, change))
}

/// Parse `/futures/data/globalLongShortAccountRatio`: `(ratio, long share in percent)`.
pub fn parse_long_short(json: &str) -> Result<(f64, f64)> {
    let rows: Vec<Value> = serde_json::from_str(json).context("parsing Binance long/short ratio")?;
    let r = rows.last().ok_or_else(|| anyhow!("Binance long/short ratio: empty"))?;
    Ok((
        num(&r["longShortRatio"]).ok_or_else(|| anyhow!("no ratio"))?,
        num(&r["longAccount"]).ok_or_else(|| anyhow!("no long share"))? * 100.0,
    ))
}

// ---- movers ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Mover {
    /// `SOL`
    pub base: String,
    pub price: f64,
    pub change_pct: f64,
    /// 24h volume in USDT.
    pub quote_volume: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Movers {
    pub gainers: Vec<Mover>,
    pub losers: Vec<Mover>,
}

/// Parse `/api/v3/ticker/24hr` into the top gainers and losers among USDT pairs: leveraged
/// tokens (`BTCUP`, `ETHDOWN`) and stablecoin pairs are dropped, and so is anything under
/// [`MIN_MOVER_VOLUME`] of 24h volume.
pub fn parse_movers(json: &str) -> Result<Movers> {
    let rows: Vec<Value> = serde_json::from_str(json).context("parsing Binance 24hr ticker")?;
    let bases: std::collections::HashSet<&str> =
        rows.iter().filter_map(|r| r["symbol"].as_str()?.strip_suffix("USDT")).collect();
    let mut all: Vec<Mover> = rows
        .iter()
        .filter_map(|r| {
            let base = r["symbol"].as_str()?.strip_suffix("USDT")?;
            if base.is_empty() || STABLES.contains(&base) {
                return None;
            }
            // Leveraged tokens are `<COIN>UP/DOWN/BULL/BEAR`; JUP and the like have no `J` pair.
            let leveraged = ["UP", "DOWN", "BULL", "BEAR"]
                .iter()
                .any(|s| base.strip_suffix(s).is_some_and(|b| !b.is_empty() && bases.contains(b)));
            if leveraged {
                return None;
            }
            let quote_volume = num(&r["quoteVolume"])?;
            if quote_volume < MIN_MOVER_VOLUME {
                return None;
            }
            Some(Mover {
                base: base.to_string(),
                price: num(&r["lastPrice"])?,
                change_pct: num(&r["priceChangePercent"])?,
                quote_volume,
            })
        })
        .collect();
    all.sort_by(|a, b| b.change_pct.total_cmp(&a.change_pct));
    let gainers: Vec<Mover> = all.iter().filter(|m| m.change_pct > 0.0).take(MOVERS_TOP).cloned().collect();
    let losers: Vec<Mover> = all.iter().rev().filter(|m| m.change_pct < 0.0).take(MOVERS_TOP).cloned().collect();
    Ok(Movers { gainers, losers })
}

// ---- turkey ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Turkey {
    /// BtcTurk USDT/TRY last and the Yahoo USDTRY quote.
    pub usdt_try: f64,
    pub usdtry_fx: Option<f64>,
    /// `usdt_try / usdtry_fx - 1` in percent.
    pub usdt_premium: Option<f64>,
    /// BtcTurk BTC/TRY, Binance BTCUSDT and the premium of BTC/TRY over BTCUSDT x USDTRY.
    pub btc_try: Option<f64>,
    pub btc_usd: Option<f64>,
    pub btc_premium: Option<f64>,
}

/// Parse BtcTurk `/api/v2/ticker?pairSymbol=...`: the last price.
pub fn parse_btcturk(json: &str) -> Result<f64> {
    let v: Value = serde_json::from_str(json).context("parsing BtcTurk ticker")?;
    if v["success"] == false {
        bail!("BtcTurk: {}", v["message"].as_str().unwrap_or("error"));
    }
    num(&v["data"][0]["last"]).ok_or_else(|| anyhow!("BtcTurk: no last price"))
}

/// Combine the quotes into the premiums. Pure, so it is unit tested.
pub fn turkey_premiums(usdt_try: f64, fx: Option<f64>, btc_try: Option<f64>, btc_usd: Option<f64>) -> Turkey {
    let pct = |a: f64, b: f64| (b > 0.0).then(|| (a / b - 1.0) * 100.0);
    Turkey {
        usdt_try,
        usdtry_fx: fx,
        usdt_premium: fx.and_then(|f| pct(usdt_try, f)),
        btc_try,
        btc_usd,
        btc_premium: match (btc_try, btc_usd, fx) {
            (Some(t), Some(u), Some(f)) => pct(t, u * f),
            _ => None,
        },
    }
}

// ---- fetching -------------------------------------------------------------------------------

/// One refresh. Each section fails on its own.
#[derive(Debug, Clone)]
pub struct Overview {
    pub global: Result<Global, String>,
    pub fear_greed: Result<FearGreed, String>,
    pub derivs: Result<Vec<Deriv>, String>,
    pub movers: Result<Movers, String>,
    pub turkey: Result<Turkey, String>,
    /// Unix seconds.
    pub fetched: i64,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("terminal-rs/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?)
}

async fn get(c: &reqwest::Client, url: &str, query: &[(&str, &str)]) -> Result<String> {
    let resp = c.get(url).query(query).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        // Binance explains geo blocks in the body; keep a short snippet.
        let snippet: String = body.chars().take(120).collect();
        bail!("{} HTTP {status} {snippet}", url.split('?').next().unwrap_or(url));
    }
    Ok(body)
}

/// Spot REST with the public mirror first, then the main API.
async fn get_spot(c: &reqwest::Client, path: &str, query: &[(&str, &str)]) -> Result<String> {
    let mut last = anyhow!("no Binance host");
    for host in SPOT {
        match get(c, &format!("{host}{path}"), query).await {
            Ok(b) => return Ok(b),
            Err(e) => last = e,
        }
    }
    Err(last)
}

static GECKO_CACHE: Mutex<Option<(Instant, Global)>> = Mutex::new(None);

async fn fetch_global(c: &reqwest::Client) -> Result<Global> {
    if let Some((at, g)) = GECKO_CACHE.lock().ok().and_then(|g| g.clone())
        && at.elapsed() < GECKO_TTL
    {
        return Ok(g);
    }
    let fresh: Result<Global> = async {
        let mut g = parse_global(&get(c, &format!("{COINGECKO}/global"), &[]).await?)?;
        // Optional: a failure here (rate limit, big payload) just leaves the stablecoin cell empty.
        if let Ok(b) = get(c, &format!("{COINGECKO}/coins/categories"), &[]).await
            && let Ok((cap, ch)) = parse_stablecoins(&b)
        {
            g.stable_cap = Some(cap);
            g.stable_change_24h = ch;
        }
        Ok(g)
    }
    .await;
    match fresh {
        Ok(g) => {
            if let Ok(mut c) = GECKO_CACHE.lock() {
                *c = Some((Instant::now(), g.clone()));
            }
            Ok(g)
        }
        // Rate limited: a stale value beats an empty card.
        Err(e) => match GECKO_CACHE.lock().ok().and_then(|g| g.clone()) {
            Some((_, g)) => Ok(g),
            None => Err(e),
        },
    }
}

async fn fetch_fng(c: &reqwest::Client) -> Result<FearGreed> {
    parse_fng(&get(c, FNG, &[]).await?)
}

async fn fetch_deriv_extras(c: &reqwest::Client, d: &mut Deriv) {
    let sym = format!("{}USDT", d.coin);
    let q = [("symbol", sym.as_str())];
    let hq = [("symbol", sym.as_str()), ("period", "1h"), ("limit", "25")];
    let lq = [("symbol", sym.as_str()), ("period", "1h"), ("limit", "1")];
    let (u1, u2, u3) = (
        format!("{FAPI}/fapi/v1/openInterest"),
        format!("{FAPI}/futures/data/openInterestHist"),
        format!("{FAPI}/futures/data/globalLongShortAccountRatio"),
    );
    let (oi, hist, ls) = tokio::join!(get(c, &u1, &q), get(c, &u2, &hq), get(c, &u3, &lq));
    d.oi_coins = oi.ok().and_then(|b| parse_open_interest(&b).ok());
    if let Some((usd, ch)) = hist.ok().and_then(|b| parse_oi_hist(&b).ok()) {
        d.oi_usd = Some(usd);
        d.oi_change_24h = ch;
    }
    if d.oi_usd.is_none() {
        d.oi_usd = d.oi_coins.map(|c| c * d.mark);
    }
    if let Some((r, l)) = ls.ok().and_then(|b| parse_long_short(&b).ok()) {
        d.ls_ratio = Some(r);
        d.long_pct = Some(l);
    }
}

async fn fetch_derivs(c: &reqwest::Client) -> Result<Vec<Deriv>> {
    let mut v = parse_premium(&get(c, &format!("{FAPI}/fapi/v1/premiumIndex"), &[]).await?, &DERIV_COINS)?;
    futures::future::join_all(v.iter_mut().map(|d| fetch_deriv_extras(c, d))).await;
    Ok(v)
}

async fn fetch_movers(c: &reqwest::Client) -> Result<Movers> {
    parse_movers(&get_spot(c, "/api/v3/ticker/24hr", &[]).await?)
}

async fn fetch_turkey(c: &reqwest::Client) -> Result<Turkey> {
    let one = |pair: &'static str| async move { parse_btcturk(&get(c, BTCTURK, &[("pairSymbol", pair)]).await?) };
    let fx = async {
        let q = YahooProvider::new()?.quotes(&["USDTRY=X".to_string()]).await?;
        q.first().map(|q| q.last).filter(|p| *p > 0.0).ok_or_else(|| anyhow!("no USDTRY quote"))
    };
    let btc_usd = async {
        let b = get_spot(c, "/api/v3/ticker/price", &[("symbol", "BTCUSDT")]).await?;
        let v: Value = serde_json::from_str(&b)?;
        num(&v["price"]).ok_or_else(|| anyhow!("no BTCUSDT price"))
    };
    let (usdt, btc_try, fx, btc_usd) = tokio::join!(one("USDTTRY"), one("BTCTRY"), fx, btc_usd);
    Ok(turkey_premiums(usdt?, fx.ok(), btc_try.ok(), btc_usd.ok()))
}

/// Fetch every section concurrently.
pub async fn fetch() -> Result<Overview> {
    let c = client()?;
    let e = |e: anyhow::Error| format!("{e:#}");
    let (global, fear_greed, derivs, movers, turkey) =
        tokio::join!(fetch_global(&c), fetch_fng(&c), fetch_derivs(&c), fetch_movers(&c), fetch_turkey(&c));
    Ok(Overview {
        global: global.map_err(e),
        fear_greed: fear_greed.map_err(e),
        derivs: derivs.map_err(e),
        movers: movers.map_err(e),
        turkey: turkey.map_err(e),
        fetched: chrono::Utc::now().timestamp(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_and_stablecoins() {
        let g = parse_global(include_str!("../tests/fixtures/cg_global.json")).unwrap();
        assert!((g.market_cap - 2.849e12).abs() < 1e9);
        assert!((g.btc_dominance - 59.67).abs() < 0.01);
        assert!(g.market_cap_change_24h.unwrap() < 0.0);
        assert_eq!(g.active_coins, Some(22417));
        let (cap, ch) = parse_stablecoins(include_str!("../tests/fixtures/cg_categories.json")).unwrap();
        assert!((cap - 2.983e11).abs() < 1e9 && ch.unwrap() > 0.0);
        assert!(parse_stablecoins("[]").is_err());
        // CoinGecko's rate-limit body.
        assert!(parse_global(r#"{"status":{"error_code":429,"error_message":"rate limit"}}"#).is_err());
    }

    #[test]
    fn fear_greed_is_oldest_first() {
        let f = parse_fng(include_str!("../tests/fixtures/fng.json")).unwrap();
        assert_eq!((f.value, f.label.as_str()), (63, "Greed"));
        assert_eq!(f.history, vec![20.0, 55.0, 63.0]);
        assert!(parse_fng(r#"{"data":[],"metadata":{"error":null}}"#).is_err());
        assert!(parse_fng(r#"{"data":[],"metadata":{"error":"limit"}}"#).is_err());
    }

    #[test]
    fn premium_index_picks_perps_only() {
        let d = parse_premium(include_str!("../tests/fixtures/bn_premium.json"), &["BTC", "ETH", "SOL"]).unwrap();
        assert_eq!(d.len(), 2, "SOL missing, the dated future is ignored");
        assert!((d[0].funding - 0.00001206).abs() < 1e-12);
        assert!((d[0].funding_apr() - 1.32057).abs() < 1e-3);
        assert!(d[1].funding < 0.0);
        assert_eq!(d[0].next_funding_ms, 1792627200000);
        // Geo block / error body.
        let e = parse_premium(r#"{"code":0,"msg":"Service unavailable from a restricted location"}"#, &["BTC"]);
        assert!(format!("{}", e.unwrap_err()).contains("restricted"));
    }

    #[test]
    fn open_interest_and_ratio() {
        assert!((parse_open_interest(include_str!("../tests/fixtures/bn_oi.json")).unwrap() - 98281.199).abs() < 1e-6);
        let (usd, ch) = parse_oi_hist(include_str!("../tests/fixtures/bn_oihist.json")).unwrap();
        assert!((usd - 8_311_537_620.32).abs() < 1.0);
        assert!((ch.unwrap() - 0.178).abs() < 0.01);
        let (r, l) = parse_long_short(include_str!("../tests/fixtures/bn_ls.json")).unwrap();
        assert!((r - 1.0296).abs() < 1e-9 && (l - 50.73).abs() < 1e-9);
        assert!(parse_oi_hist("[]").is_err());
    }

    #[test]
    fn movers_filter_and_rank() {
        let m = parse_movers(include_str!("../tests/fixtures/bn_24hr.json")).unwrap();
        let g: Vec<&str> = m.gainers.iter().map(|x| x.base.as_str()).collect();
        let l: Vec<&str> = m.losers.iter().map(|x| x.base.as_str()).collect();
        // BTCUP/BTCDOWN (leveraged), USDC (stable), THIN (no volume) and ETHBTC (not USDT) drop out.
        assert_eq!(g, ["JUP", "SOL"]);
        assert_eq!(l, ["WIF", "BTC"]);
    }

    #[test]
    fn btcturk_and_premiums() {
        let last = parse_btcturk(include_str!("../tests/fixtures/btcturk.json")).unwrap();
        assert!((last - 49.358).abs() < 1e-9);
        assert!(parse_btcturk(r#"{"data":[],"success":false,"message":"bad pair"}"#).is_err());
        let t = turkey_premiums(49.5, Some(49.0), Some(4_300_000.0), Some(86_000.0));
        assert!((t.usdt_premium.unwrap() - 1.0204).abs() < 1e-3);
        // 86000 * 49 = 4_214_000 -> +2.04%
        assert!((t.btc_premium.unwrap() - 2.04).abs() < 0.01);
        let none = turkey_premiums(49.5, None, Some(1.0), Some(1.0));
        assert!(none.usdt_premium.is_none() && none.btc_premium.is_none());
    }
}
