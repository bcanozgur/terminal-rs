//! Built-in security master: well-known instruments across asset classes with their
//! data-source symbols, overview-panel grouping and a plausible level for sample data.
//! Anything not listed still resolves by heuristics (see [`crate::security`]).

use std::sync::LazyLock;

use crate::security::{AssetClass, Security, YellowKey};

/// Sections of the world markets overview (WEI-style) panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Americas,
    Emea,
    AsiaPac,
    Futures,
    Rates,
    Fx,
    Crypto,
    Commodities,
    Etfs,
    Stocks,
}

impl Group {
    pub const OVERVIEW: [Group; 8] = [
        Group::Americas,
        Group::Emea,
        Group::AsiaPac,
        Group::Futures,
        Group::Rates,
        Group::Fx,
        Group::Crypto,
        Group::Commodities,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::Americas => "AMERICAS",
            Group::Emea => "EMEA",
            Group::AsiaPac => "ASIA/PACIFIC",
            Group::Futures => "EQUITY FUTURES",
            Group::Rates => "US TREASURIES",
            Group::Fx => "CURRENCIES",
            Group::Crypto => "CRYPTO",
            Group::Commodities => "COMMODITIES",
            Group::Etfs => "ETFS",
            Group::Stocks => "STOCKS",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub ticker: &'static str,
    pub class: AssetClass,
    pub name: &'static str,
    pub group: Group,
    /// Yahoo Finance chart symbol.
    pub yahoo: Option<&'static str>,
    /// Binance spot symbol (crypto).
    pub binance: Option<&'static str>,
    /// Coinbase Exchange product id (crypto).
    pub coinbase: Option<&'static str>,
    /// Column label in Treasury.gov's daily par yield curve CSV.
    pub treasury: Option<&'static str>,
    /// FRED series id (fallback for Treasury yields).
    pub fred: Option<&'static str>,
    /// Tenor in years (Treasuries), for the yield curve.
    pub tenor_years: Option<f64>,
    /// Typical level used to anchor synthetic sample data.
    pub sample_level: f64,
}

impl Listing {
    pub fn security(&self) -> Security {
        Security::new(self.ticker, self.class)
    }

    pub fn key(&self) -> String {
        self.security().key()
    }
}

fn base(ticker: &'static str, class: AssetClass, name: &'static str, group: Group, level: f64) -> Listing {
    Listing {
        ticker,
        class,
        name,
        group,
        yahoo: None,
        binance: None,
        coinbase: None,
        treasury: None,
        fred: None,
        tenor_years: None,
        sample_level: level,
    }
}

fn yahoo(
    ticker: &'static str,
    class: AssetClass,
    name: &'static str,
    group: Group,
    y: &'static str,
    level: f64,
) -> Listing {
    Listing { yahoo: Some(y), ..base(ticker, class, name, group, level) }
}

fn crypto(
    ticker: &'static str,
    name: &'static str,
    binance: &'static str,
    coinbase: &'static str,
    level: f64,
) -> Listing {
    Listing {
        binance: Some(binance),
        coinbase: Some(coinbase),
        yahoo: None,
        ..base(ticker, AssetClass::Crypto, name, Group::Crypto, level)
    }
}

fn fx(ticker: &'static str, name: &'static str, level: f64) -> Listing {
    // Yahoo quotes pairs as `EURUSD=X`; leaked lifetimes are fine for a static table.
    let y: &'static str = Box::leak(format!("{ticker}=X").into_boxed_str());
    yahoo(ticker, AssetClass::Fx, name, Group::Fx, y, level)
}

#[allow(clippy::too_many_arguments)]
fn tsy(
    ticker: &'static str,
    name: &'static str,
    col: &'static str,
    fred: &'static str,
    years: f64,
    y: Option<&'static str>,
    level: f64,
) -> Listing {
    Listing {
        treasury: Some(col),
        fred: Some(fred),
        tenor_years: Some(years),
        yahoo: y,
        ..base(ticker, AssetClass::Govt, name, Group::Rates, level)
    }
}

pub static CATALOG: LazyLock<Vec<Listing>> = LazyLock::new(|| {
    use AssetClass::*;
    use Group::*;
    vec![
        // Indices
        yahoo("SPX", Index, "S&P 500 Index", Americas, "^GSPC", 6000.0),
        yahoo("INDU", Index, "Dow Jones Industrial Average", Americas, "^DJI", 44000.0),
        yahoo("CCMP", Index, "NASDAQ Composite Index", Americas, "^IXIC", 19000.0),
        yahoo("NDX", Index, "NASDAQ-100 Index", Americas, "^NDX", 21000.0),
        yahoo("RTY", Index, "Russell 2000 Index", Americas, "^RUT", 2300.0),
        yahoo("VIX", Index, "Cboe Volatility Index", Americas, "^VIX", 16.0),
        yahoo("SPTSX", Index, "S&P/TSX Composite", Americas, "^GSPTSE", 25000.0),
        yahoo("IBOV", Index, "Brazil Ibovespa", Americas, "^BVSP", 130000.0),
        yahoo("MEXBOL", Index, "Mexico IPC", Americas, "^MXX", 55000.0),
        yahoo("UKX", Index, "FTSE 100 Index", Emea, "^FTSE", 8500.0),
        yahoo("DAX", Index, "DAX Index", Emea, "^GDAXI", 20000.0),
        yahoo("CAC", Index, "CAC 40 Index", Emea, "^FCHI", 7800.0),
        yahoo("SX5E", Index, "Euro Stoxx 50", Emea, "^STOXX50E", 5000.0),
        yahoo("SMI", Index, "Swiss Market Index", Emea, "^SSMI", 12000.0),
        yahoo("IBEX", Index, "IBEX 35", Emea, "^IBEX", 12000.0),
        yahoo("FTSEMIB", Index, "FTSE MIB", Emea, "FTSEMIB.MI", 35000.0),
        yahoo("NKY", Index, "Nikkei 225", AsiaPac, "^N225", 40000.0),
        yahoo("HSI", Index, "Hang Seng Index", AsiaPac, "^HSI", 20000.0),
        yahoo("SHCOMP", Index, "Shanghai Composite", AsiaPac, "000001.SS", 3300.0),
        yahoo("KOSPI", Index, "KOSPI Index", AsiaPac, "^KS11", 2600.0),
        yahoo("AS51", Index, "S&P/ASX 200", AsiaPac, "^AXJO", 8200.0),
        yahoo("SENSEX", Index, "BSE Sensex", AsiaPac, "^BSESN", 80000.0),
        yahoo("TWSE", Index, "Taiwan Weighted Index", AsiaPac, "^TWII", 22000.0),
        yahoo("DXY", Index, "US Dollar Index", Group::Fx, "DX-Y.NYB", 104.0),
        // Equity index futures (generic first contract)
        yahoo("ES1", Cmdty, "E-mini S&P 500 Future", Futures, "ES=F", 6000.0),
        yahoo("NQ1", Cmdty, "E-mini NASDAQ-100 Future", Futures, "NQ=F", 21000.0),
        yahoo("DM1", Cmdty, "E-mini Dow Future", Futures, "YM=F", 44000.0),
        yahoo("RTY1", Cmdty, "E-mini Russell 2000 Future", Futures, "RTY=F", 2300.0),
        yahoo("TY1", Cmdty, "10-Year T-Note Future", Futures, "ZN=F", 110.0),
        yahoo("US1", Cmdty, "US Treasury Bond Future", Futures, "ZB=F", 115.0),
        // Commodities
        yahoo("CL1", Cmdty, "WTI Crude Oil Future", Commodities, "CL=F", 75.0),
        yahoo("CO1", Cmdty, "Brent Crude Oil Future", Commodities, "BZ=F", 79.0),
        yahoo("NG1", Cmdty, "Henry Hub Natural Gas Future", Commodities, "NG=F", 3.0),
        yahoo("XB1", Cmdty, "RBOB Gasoline Future", Commodities, "RB=F", 2.2),
        yahoo("HO1", Cmdty, "NY Harbor ULSD Future", Commodities, "HO=F", 2.4),
        yahoo("GC1", Cmdty, "Gold Future", Commodities, "GC=F", 2400.0),
        yahoo("SI1", Cmdty, "Silver Future", Commodities, "SI=F", 30.0),
        yahoo("HG1", Cmdty, "Copper Future", Commodities, "HG=F", 4.5),
        yahoo("PL1", Cmdty, "Platinum Future", Commodities, "PL=F", 1000.0),
        yahoo("C1", Cmdty, "Corn Future", Commodities, "ZC=F", 450.0),
        yahoo("W1", Cmdty, "Wheat Future", Commodities, "ZW=F", 550.0),
        yahoo("S1", Cmdty, "Soybean Future", Commodities, "ZS=F", 1050.0),
        yahoo("KC1", Cmdty, "Coffee Future", Commodities, "KC=F", 230.0),
        yahoo("CT1", Cmdty, "Cotton Future", Commodities, "CT=F", 70.0),
        // US Treasury yields (Treasury.gov par curve, FRED fallback, Yahoo intraday where available)
        tsy("US1M", "US Treasury 1 Month", "1 Mo", "DGS1MO", 1.0 / 12.0, None, 4.2),
        tsy("US3M", "US Treasury 3 Month", "3 Mo", "DGS3MO", 0.25, Some("^IRX"), 4.2),
        tsy("US6M", "US Treasury 6 Month", "6 Mo", "DGS6MO", 0.5, None, 4.2),
        tsy("US1Y", "US Treasury 1 Year", "1 Yr", "DGS1", 1.0, None, 4.1),
        tsy("US2Y", "US Treasury 2 Year", "2 Yr", "DGS2", 2.0, None, 4.0),
        tsy("US3Y", "US Treasury 3 Year", "3 Yr", "DGS3", 3.0, None, 4.0),
        tsy("US5Y", "US Treasury 5 Year", "5 Yr", "DGS5", 5.0, Some("^FVX"), 4.0),
        tsy("US7Y", "US Treasury 7 Year", "7 Yr", "DGS7", 7.0, None, 4.1),
        tsy("US10Y", "US Treasury 10 Year", "10 Yr", "DGS10", 10.0, Some("^TNX"), 4.2),
        tsy("US20Y", "US Treasury 20 Year", "20 Yr", "DGS20", 20.0, None, 4.5),
        tsy("US30Y", "US Treasury 30 Year", "30 Yr", "DGS30", 30.0, Some("^TYX"), 4.4),
        // FX
        fx("EURUSD", "Euro / US Dollar", 1.09),
        fx("USDJPY", "US Dollar / Japanese Yen", 150.0),
        fx("GBPUSD", "British Pound / US Dollar", 1.28),
        fx("USDCHF", "US Dollar / Swiss Franc", 0.88),
        fx("AUDUSD", "Australian Dollar / US Dollar", 0.66),
        fx("USDCAD", "US Dollar / Canadian Dollar", 1.36),
        fx("NZDUSD", "New Zealand Dollar / US Dollar", 0.60),
        fx("USDCNH", "US Dollar / Offshore Yuan", 7.2),
        fx("USDMXN", "US Dollar / Mexican Peso", 18.5),
        fx("EURGBP", "Euro / British Pound", 0.85),
        fx("EURJPY", "Euro / Japanese Yen", 162.0),
        fx("USDINR", "US Dollar / Indian Rupee", 84.0),
        fx("USDTRY", "US Dollar / Turkish Lira", 42.0),
        // Crypto
        crypto("BTCUSD", "Bitcoin / US Dollar", "BTCUSDT", "BTC-USD", 65000.0),
        crypto("ETHUSD", "Ethereum / US Dollar", "ETHUSDT", "ETH-USD", 3000.0),
        crypto("SOLUSD", "Solana / US Dollar", "SOLUSDT", "SOL-USD", 150.0),
        crypto("XRPUSD", "XRP / US Dollar", "XRPUSDT", "XRP-USD", 0.6),
        crypto("BNBUSD", "BNB / US Dollar", "BNBUSDT", "BNB-USD", 550.0),
        crypto("DOGEUSD", "Dogecoin / US Dollar", "DOGEUSDT", "DOGE-USD", 0.12),
        crypto("ADAUSD", "Cardano / US Dollar", "ADAUSDT", "ADA-USD", 0.45),
        crypto("AVAXUSD", "Avalanche / US Dollar", "AVAXUSDT", "AVAX-USD", 30.0),
        crypto("LINKUSD", "Chainlink / US Dollar", "LINKUSDT", "LINK-USD", 14.0),
        crypto("LTCUSD", "Litecoin / US Dollar", "LTCUSDT", "LTC-USD", 75.0),
        crypto("DOTUSD", "Polkadot / US Dollar", "DOTUSDT", "DOT-USD", 6.0),
        // ETFs
        yahoo("SPY", Etf, "SPDR S&P 500 ETF Trust", Etfs, "SPY", 600.0),
        yahoo("QQQ", Etf, "Invesco QQQ Trust", Etfs, "QQQ", 520.0),
        yahoo("IWM", Etf, "iShares Russell 2000 ETF", Etfs, "IWM", 220.0),
        yahoo("DIA", Etf, "SPDR Dow Jones Industrial Average ETF", Etfs, "DIA", 440.0),
        yahoo("VTI", Etf, "Vanguard Total Stock Market ETF", Etfs, "VTI", 290.0),
        yahoo("EFA", Etf, "iShares MSCI EAFE ETF", Etfs, "EFA", 80.0),
        yahoo("EEM", Etf, "iShares MSCI Emerging Markets ETF", Etfs, "EEM", 45.0),
        yahoo("TMF", Etf, "Direxion Daily 20+ Year Treasury Bull 3X Shares", Etfs, "TMF", 28.0),
        yahoo("TLT", Etf, "iShares 20+ Year Treasury Bond ETF", Etfs, "TLT", 92.0),
        yahoo("IEF", Etf, "iShares 7-10 Year Treasury Bond ETF", Etfs, "IEF", 95.0),
        yahoo("HYG", Etf, "iShares iBoxx High Yield Corporate Bond ETF", Etfs, "HYG", 79.0),
        yahoo("LQD", Etf, "iShares iBoxx Investment Grade Corporate Bond ETF", Etfs, "LQD", 110.0),
        yahoo("GLD", Etf, "SPDR Gold Shares", Etfs, "GLD", 220.0),
        yahoo("SLV", Etf, "iShares Silver Trust", Etfs, "SLV", 27.0),
        yahoo("USO", Etf, "United States Oil Fund", Etfs, "USO", 75.0),
        yahoo("XLF", Etf, "Financial Select Sector SPDR", Etfs, "XLF", 45.0),
        yahoo("XLE", Etf, "Energy Select Sector SPDR", Etfs, "XLE", 90.0),
        yahoo("XLK", Etf, "Technology Select Sector SPDR", Etfs, "XLK", 230.0),
        yahoo("SMH", Etf, "VanEck Semiconductor ETF", Etfs, "SMH", 250.0),
        yahoo("ARKK", Etf, "ARK Innovation ETF", Etfs, "ARKK", 50.0),
        yahoo("IBIT", Etf, "iShares Bitcoin Trust", Etfs, "IBIT", 38.0),
        // Large-cap stocks (any other ticker resolves as a US equity)
        yahoo("AAPL", Equity, "Apple Inc", Stocks, "AAPL", 230.0),
        yahoo("MSFT", Equity, "Microsoft Corp", Stocks, "MSFT", 420.0),
        yahoo("NVDA", Equity, "NVIDIA Corp", Stocks, "NVDA", 130.0),
        yahoo("AMZN", Equity, "Amazon.com Inc", Stocks, "AMZN", 190.0),
        yahoo("GOOGL", Equity, "Alphabet Inc Class A", Stocks, "GOOGL", 170.0),
        yahoo("META", Equity, "Meta Platforms Inc", Stocks, "META", 560.0),
        yahoo("TSLA", Equity, "Tesla Inc", Stocks, "TSLA", 250.0),
        yahoo("BRK.B", Equity, "Berkshire Hathaway Inc Class B", Stocks, "BRK-B", 450.0),
        yahoo("JPM", Equity, "JPMorgan Chase & Co", Stocks, "JPM", 210.0),
        yahoo("V", Equity, "Visa Inc", Stocks, "V", 280.0),
        yahoo("UNH", Equity, "UnitedHealth Group Inc", Stocks, "UNH", 560.0),
        yahoo("XOM", Equity, "Exxon Mobil Corp", Stocks, "XOM", 115.0),
        yahoo("LLY", Equity, "Eli Lilly & Co", Stocks, "LLY", 850.0),
        yahoo("AVGO", Equity, "Broadcom Inc", Stocks, "AVGO", 170.0),
        yahoo("AMD", Equity, "Advanced Micro Devices", Stocks, "AMD", 160.0),
        yahoo("NFLX", Equity, "Netflix Inc", Stocks, "NFLX", 700.0),
        yahoo("COIN", Equity, "Coinbase Global Inc", Stocks, "COIN", 200.0),
        yahoo("MSTR", Equity, "MicroStrategy Inc", Stocks, "MSTR", 300.0),
        // Borsa Istanbul: indices and BIST 30 stocks, quoted in TRY on Yahoo (`.IS`)
        yahoo("XU100", Index, "BIST 100 Index (XU100)", Emea, "XU100.IS", 12000.0),
        yahoo("XU030", Index, "BIST 30 Index (XU030, XU30)", Emea, "XU030.IS", 15000.0),
        yahoo("AKBNK", Equity, "Akbank (BIST)", Stocks, "AKBNK.IS", 67.0),
        yahoo("ALARK", Equity, "Alarko Holding (BIST)", Stocks, "ALARK.IS", 109.0),
        yahoo("ARCLK", Equity, "Arcelik (BIST)", Stocks, "ARCLK.IS", 76.0),
        yahoo("ASELS", Equity, "Aselsan (BIST)", Stocks, "ASELS.IS", 349.0),
        yahoo("ASTOR", Equity, "Astor Enerji (BIST)", Stocks, "ASTOR.IS", 187.0),
        yahoo("BIMAS", Equity, "BIM Birlesik Magazalar (BIST)", Stocks, "BIMAS.IS", 400.0),
        yahoo("BRSAN", Equity, "Borusan Mannesmann (BIST)", Stocks, "BRSAN.IS", 658.0),
        yahoo("EKGYO", Equity, "Emlak Konut GYO (BIST)", Stocks, "EKGYO.IS", 20.0),
        yahoo("ENKAI", Equity, "Enka Insaat (BIST)", Stocks, "ENKAI.IS", 78.0),
        yahoo("EREGL", Equity, "Eregli Demir ve Celik (BIST)", Stocks, "EREGL.IS", 37.0),
        yahoo("FROTO", Equity, "Ford Otosan (BIST)", Stocks, "FROTO.IS", 71.0),
        yahoo("GARAN", Equity, "Garanti BBVA (BIST)", Stocks, "GARAN.IS", 132.0),
        yahoo("GUBRF", Equity, "Gubre Fabrikalari (BIST)", Stocks, "GUBRF.IS", 393.0),
        yahoo("HALKB", Equity, "Halkbank (BIST)", Stocks, "HALKB.IS", 43.0),
        yahoo("ISCTR", Equity, "Is Bankasi (BIST)", Stocks, "ISCTR.IS", 13.0),
        yahoo("KCHOL", Equity, "Koc Holding (BIST)", Stocks, "KCHOL.IS", 210.0),
        yahoo("KRDMD", Equity, "Kardemir (BIST)", Stocks, "KRDMD.IS", 42.0),
        yahoo("MGROS", Equity, "Migros (BIST)", Stocks, "MGROS.IS", 470.0),
        yahoo("OYAKC", Equity, "Oyak Cimento (BIST)", Stocks, "OYAKC.IS", 20.0),
        yahoo("PETKM", Equity, "Petkim (BIST)", Stocks, "PETKM.IS", 20.0),
        yahoo("PGSUS", Equity, "Pegasus Hava Tasimaciligi (BIST)", Stocks, "PGSUS.IS", 129.0),
        yahoo("SAHOL", Equity, "Sabanci Holding (BIST)", Stocks, "SAHOL.IS", 84.0),
        yahoo("SASA", Equity, "Sasa Polyester (BIST)", Stocks, "SASA.IS", 2.0),
        yahoo("SISE", Equity, "Sisecam (BIST)", Stocks, "SISE.IS", 37.0),
        yahoo("TAVHL", Equity, "TAV Havalimanlari (BIST)", Stocks, "TAVHL.IS", 270.0),
        yahoo("TCELL", Equity, "Turkcell (BIST)", Stocks, "TCELL.IS", 98.0),
        yahoo("THYAO", Equity, "Turk Hava Yollari (BIST)", Stocks, "THYAO.IS", 276.0),
        yahoo("TOASO", Equity, "Tofas (BIST)", Stocks, "TOASO.IS", 246.0),
        yahoo("TTKOM", Equity, "Turk Telekom (BIST)", Stocks, "TTKOM.IS", 52.0),
        yahoo("TUPRS", Equity, "Tupras (BIST)", Stocks, "TUPRS.IS", 381.0),
        yahoo("VAKBN", Equity, "Vakifbank (BIST)", Stocks, "VAKBN.IS", 34.0),
        yahoo("YKBNK", Equity, "Yapi Kredi (BIST)", Stocks, "YKBNK.IS", 36.0),
    ]
});

/// Find a listing by ticker, optionally restricted to a yellow key.
pub fn lookup(ticker: &str, key: Option<YellowKey>) -> Option<&'static Listing> {
    let t = ticker.trim().to_ascii_uppercase();
    CATALOG.iter().find(|l| l.ticker == t && key.is_none_or(|k| l.class.yellow_key() == k))
}

pub fn by_security(s: &Security) -> Option<&'static Listing> {
    CATALOG.iter().find(|l| l.ticker == s.ticker && l.class == s.class)
}

pub fn group(g: Group) -> impl Iterator<Item = &'static Listing> {
    CATALOG.iter().filter(move |l| l.group == g)
}

/// Treasury tenors in curve order.
pub fn treasury_curve() -> Vec<&'static Listing> {
    let mut v: Vec<&Listing> = CATALOG.iter().filter(|l| l.tenor_years.is_some()).collect();
    v.sort_by(|a, b| a.tenor_years.partial_cmp(&b.tenor_years).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Ranked search over tickers and names. An optional yellow key in the query
/// (`gold cmdty`, `usd <crncy>`) filters by class.
pub fn search(query: &str, limit: usize) -> Vec<&'static Listing> {
    let mut key = None;
    let words: Vec<String> = query
        .split_whitespace()
        .filter(|w| match YellowKey::parse(w) {
            Some(k) => {
                key = Some(k);
                false
            }
            None => true,
        })
        .map(|w| w.to_ascii_uppercase().replace('/', ""))
        .collect();
    if words.is_empty() && key.is_none() {
        return Vec::new();
    }
    let mut scored: Vec<(i32, &Listing)> = CATALOG
        .iter()
        .filter(|l| key.is_none_or(|k| l.class.yellow_key() == k))
        .filter_map(|l| {
            let name = l.name.to_ascii_uppercase();
            let mut score = 0;
            for (i, w) in words.iter().enumerate() {
                let s = if l.ticker == w.as_str() {
                    1000
                } else if l.ticker.starts_with(w.as_str()) {
                    600 - (l.ticker.len() as i32 - w.len() as i32) * 10
                } else if name.split(|c: char| !c.is_alphanumeric()).any(|n| n.starts_with(w.as_str())) {
                    300
                } else if name.contains(w.as_str()) {
                    150
                } else {
                    return None;
                };
                score += if i == 0 { s } else { s / 2 };
            }
            Some((score, l))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.ticker.cmp(b.1.ticker)));
    scored.into_iter().take(limit).map(|(_, l)| l).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_keys_and_provider_symbols() {
        let mut keys: Vec<String> = CATALOG.iter().map(|l| l.key()).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate market keys");
        for l in CATALOG.iter() {
            let has_source = l.yahoo.is_some() || l.binance.is_some() || l.treasury.is_some();
            assert!(has_source, "{} has no data source", l.ticker);
            assert!(l.sample_level > 0.0);
        }
        assert_eq!(lookup("EURUSD", None).unwrap().yahoo, Some("EURUSD=X"));
    }

    #[test]
    fn search_ranks_ticker_then_name() {
        let r = search("btc", 5);
        assert_eq!(r[0].ticker, "BTCUSD");
        let r = search("gold", 5);
        assert!(r.iter().any(|l| l.ticker == "GC1") && r.iter().any(|l| l.ticker == "GLD"));
        let r = search("gold cmdty", 5);
        assert!(r.iter().all(|l| l.class == AssetClass::Cmdty));
        let r = search("10 year", 5);
        assert!(r.iter().any(|l| l.ticker == "US10Y"));
        let r = search("EUR <CRNCY>", 10);
        assert!(r.iter().all(|l| l.class.yellow_key() == YellowKey::Curncy) && !r.is_empty());
        assert!(search("", 5).is_empty());
    }

    #[test]
    fn curve_is_ordered() {
        let c = treasury_curve();
        assert_eq!(c.first().unwrap().ticker, "US1M");
        assert_eq!(c.last().unwrap().ticker, "US30Y");
        assert_eq!(c.len(), 11);
    }

    #[test]
    fn overview_groups_are_populated() {
        for g in Group::OVERVIEW {
            assert!(group(g).count() >= 4, "{g:?}");
        }
    }

    #[test]
    fn borsa_istanbul_search() {
        let top = |q: &str| search(q, 5).first().map(|l| l.ticker);
        assert_eq!(top("xu100"), Some("XU100"));
        assert_eq!(top("xu030"), Some("XU030"));
        assert_eq!(top("xu30"), Some("XU030"));
        assert_eq!(top("thy"), Some("THYAO"));
        assert_eq!(top("garan"), Some("GARAN"));
        assert_eq!(lookup("THYAO", None).and_then(|l| l.yahoo), Some("THYAO.IS"));
    }
}
