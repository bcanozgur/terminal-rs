//! Network tests against Yahoo; run with `cargo test -p tr-data -- --ignored`.
use tr_core::Timeframe;
use tr_data::MarketData;
use tr_data::yahoo::YahooProvider;

#[tokio::test]
#[ignore = "network"]
async fn yahoo_history_and_paging() {
    let y = YahooProvider::new().unwrap();
    for tf in [Timeframe::M5, Timeframe::H4, Timeframe::D1, Timeframe::W1, Timeframe::MN1] {
        let recent = y.history("AAPL", tf, None, 200).await.unwrap();
        assert!(recent.len() >= 100, "{tf}: {}", recent.len());
        assert!(recent.windows(2).all(|w| w[0].ts < w[1].ts), "{tf}");
        let older = y.history("AAPL", tf, Some(recent[0].ts), 200).await.unwrap();
        assert!(older.last().unwrap().ts < recent[0].ts, "{tf}");
    }
    let q = y.quotes(&["AAPL".into(), "MSFT".into()]).await.unwrap();
    assert_eq!(q.len(), 2);
}

mod markets {
    use std::time::Duration;
    use tr_core::Timeframe;
    use tr_data::MarketData;
    use tr_data::markets::Markets;

    fn m() -> Markets {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Markets::new(None).unwrap()
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn every_asset_class_has_history() {
        let m = m();
        for (key, tf) in [
            ("BTCUSD Curncy", Timeframe::H1),
            ("ETHUSD Curncy", Timeframe::D1),
            ("EURUSD Curncy", Timeframe::D1),
            ("US10Y Govt", Timeframe::D1),
            ("US10Y Govt", Timeframe::M5),
            ("US2Y Govt", Timeframe::W1),
            ("CL1 Comdty", Timeframe::D1),
            ("GC1 Comdty", Timeframe::H1),
            ("SPX Index", Timeframe::D1),
            ("NKY Index", Timeframe::D1),
            ("SPY US Equity", Timeframe::D1),
            ("AAPL US Equity", Timeframe::M15),
        ] {
            let bars = m.history(key, tf, None, 1_500).await.unwrap_or_else(|e| panic!("{key} {tf}: {e:#}"));
            assert!(bars.len() > 50, "{key} {tf}: {}", bars.len());
            assert!(bars.windows(2).all(|w| w[0].ts < w[1].ts), "{key} {tf}");
            let older = m.history(key, tf, Some(bars[0].ts), 200).await;
            if let Ok(o) = older {
                assert!(o.last().is_none_or(|b| b.ts < bars[0].ts), "{key} {tf} paging");
            }
        }
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn mixed_quotes_curve_search_and_live() {
        let m = m();
        let keys: Vec<String> =
            ["BTCUSD Curncy", "EURUSD Curncy", "US10Y Govt", "CL1 Comdty", "SPX Index", "AAPL US Equity"]
                .map(String::from)
                .to_vec();
        let q = m.quotes(&keys).await.unwrap();
        for k in &keys {
            let x = q.iter().find(|x| &x.symbol == k).unwrap_or_else(|| panic!("no quote for {k}"));
            assert!(x.last > 0.0 && x.prev_close > 0.0, "{k}");
        }
        let c = m.yield_curves().await.unwrap();
        assert!(c.current.points.len() >= 10 && c.year_ago.is_some());
        let hits = m.search("bitcoin").await.unwrap();
        assert!(hits.iter().any(|h| h.key == "BTCUSD Curncy"), "{hits:?}");
        let mut feed = m.live("BTCUSD Curncy", Timeframe::M1).unwrap();
        let bar = tokio::time::timeout(Duration::from_secs(20), feed.recv()).await.expect("live tick").unwrap();
        assert!(bar.close > 1000.0);
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn news_and_order_books() {
        let m = m();
        for key in [Some("AAPL US Equity"), Some("BTCUSD Curncy"), None] {
            let n = m.news(key).await.unwrap();
            assert!(!n.is_empty(), "news for {key:?}");
        }
        let b = m.order_book("BTCUSD Curncy", 20).await.unwrap();
        assert_eq!(b.bids.len(), 20);
        assert!(b.bids[0].0 < b.asks[0].0 && !b.simulated);
        assert!(m.order_book("AAPL US Equity", 20).await.is_err());
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn option_chains() {
        let m = m();
        let c = m.options("AAPL US Equity", None).await.unwrap();
        assert!(!c.simulated && c.spot > 1.0 && c.expirations.len() > 5);
        assert!(!c.calls.is_empty() && !c.puts.is_empty());
        let later = m.options("AAPL US Equity", Some(c.expirations[3])).await.unwrap();
        assert_eq!(later.expiration, c.expirations[3]);
        let spx = m.options("SPX Index", None).await.unwrap();
        assert!(spx.calls.len() > 20, "index options");
        assert!(m.options("BTCUSD Curncy", None).await.is_err());
    }
}

/// Needs `TR_SEC_USER_AGENT` (name and contact email), per SEC fair-access rules.
#[tokio::test]
#[ignore = "network"]
async fn edgar_profile_and_financials() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ua = tr_data::edgar::user_agent("").expect("set TR_SEC_USER_AGENT to run this test");
    let dir = std::env::temp_dir().join(format!("tr-edgar-live-{}", std::process::id()));
    let c = tr_data::edgar::company(&dir, "AAPL US Equity", &ua).await.unwrap();
    assert_eq!(c.profile.cik, 320193);
    assert!(c.profile.filings_of(&["10-K"]).next().is_some());
    let f = c.financials.unwrap();
    assert!(f.annual.len() >= 10 && f.quarterly.len() >= 10);
    assert!(f.annual.last().unwrap().get(tr_data::edgar::Metric::Revenue).is_some_and(|r| r > 100e9));
    assert!(f.shares_outstanding.is_some_and(|s| s > 1e9));
    // Served from the disk cache the second time.
    let again = tr_data::edgar::company(&dir, "AAPL", &ua).await.unwrap();
    assert_eq!(again.profile.name, c.profile.name);
    assert!(tr_data::edgar::company(&dir, "BTCUSD Curncy", &ua).await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
