//! Per-symbol statistics from Yahoo's `/v7/finance/quote` (needs the session crumb): market
//! cap, day volume, the three month average volume and the 52 week range. They feed the
//! watchlist's extra columns; every field is optional because Yahoo omits them for indices,
//! currencies and some ETFs.

use anyhow::{Result, anyhow, bail};

use crate::yahoo::YahooProvider;

const QUOTE: &str = "https://query2.finance.yahoo.com/v7/finance/quote";

/// Symbols per request.
const CHUNK: usize = 40;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuoteStats {
    pub symbol: String,
    pub market_cap: Option<f64>,
    pub volume: Option<f64>,
    /// Three month average daily volume.
    pub avg_volume_3m: Option<f64>,
    pub high_52w: Option<f64>,
    pub low_52w: Option<f64>,
}

pub fn parse_quote_stats(json: &str) -> Result<Vec<QuoteStats>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let rows = v["quoteResponse"]["result"].as_array().ok_or_else(|| anyhow!("unexpected quote payload"))?;
    let pos = |r: &serde_json::Value, k: &str| r[k].as_f64().filter(|x| x.is_finite() && *x > 0.0);
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(QuoteStats {
                symbol: r["symbol"].as_str()?.to_string(),
                market_cap: pos(r, "marketCap"),
                volume: pos(r, "regularMarketVolume"),
                avg_volume_3m: pos(r, "averageDailyVolume3Month"),
                high_52w: pos(r, "fiftyTwoWeekHigh"),
                low_52w: pos(r, "fiftyTwoWeekLow"),
            })
        })
        .collect())
}

impl YahooProvider {
    /// Statistics for Yahoo symbols, a few requests at most per call.
    pub async fn quote_stats(&self, symbols: &[String]) -> Result<Vec<QuoteStats>> {
        let fields = "marketCap,regularMarketVolume,averageDailyVolume3Month,fiftyTwoWeekHigh,fiftyTwoWeekLow";
        let mut out = Vec::new();
        for chunk in symbols.chunks(CHUNK) {
            let mut last_err = None;
            let mut body = None;
            for refresh in [false, true] {
                let crumb = self.crumb(refresh).await?;
                let q = [("symbols", chunk.join(",")), ("fields", fields.to_string()), ("crumb", crumb)];
                let resp = self.client.get(QUOTE).query(&q).send().await?;
                let status = resp.status();
                let text = resp.text().await?;
                if status.as_u16() == 401 || text.contains("Invalid Crumb") {
                    last_err = Some(anyhow!("yahoo quote: crumb rejected"));
                    continue;
                }
                if !status.is_success() {
                    bail!("yahoo quote: HTTP {status}");
                }
                body = Some(text);
                break;
            }
            let body = body.ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("yahoo quote failed")))?;
            out.extend(parse_quote_stats(&body)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stats_and_drops_missing_or_zero_fields() {
        let json = r#"{"quoteResponse":{"result":[
            {"symbol":"AAPL","marketCap":3.1e12,"regularMarketVolume":52000000,"averageDailyVolume3Month":61000000,
             "fiftyTwoWeekHigh":260.1,"fiftyTwoWeekLow":164.08},
            {"symbol":"^GSPC","regularMarketVolume":0},
            {"noSymbol":1}],"error":null}}"#;
        let s = parse_quote_stats(json).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].market_cap, Some(3.1e12));
        assert_eq!(s[0].avg_volume_3m, Some(61_000_000.0));
        assert_eq!((s[0].low_52w, s[0].high_52w), (Some(164.08), Some(260.1)));
        assert_eq!(s[1], QuoteStats { symbol: "^GSPC".into(), ..Default::default() });
        assert!(parse_quote_stats("{}").is_err());
    }
}
