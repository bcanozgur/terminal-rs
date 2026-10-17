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

#[tokio::test]
#[ignore = "network"]
async fn yahoo_extended_hours() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let y = YahooProvider::new().unwrap();
    // Empty during regular hours and the overnight gap after a few days off; never an error.
    let x = y.extended(&["AAPL".into(), "GOOGL".into(), "THYAO.IS".into()]).await.unwrap();
    println!("{x:?}");
    assert!(x.iter().all(|(s, e)| s != "THYAO.IS" && e.price > 0.0), "{x:?}");
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

/// ETF holdings from SEC N-PORT (VOO, QQQ) and Yahoo (SPY, no N-PORT).
/// Needs `TR_SEC_USER_AGENT`; prints what was found.
#[tokio::test]
#[ignore = "network"]
async fn etf_holdings() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ua = tr_data::edgar::user_agent("").expect("set TR_SEC_USER_AGENT to run this test");
    let dir = std::env::temp_dir().join(format!("tr-etf-live-{}", std::process::id()));
    for t in ["VOO", "QQQ", "SPY", "GLD"] {
        match tr_data::etf::load(&dir, t, &ua, false).await {
            Ok(f) => {
                println!(
                    "{t}: {} · {:?} as of {:?} · {} holdings ({} positions, {} with ticker) · {} sectors",
                    f.name,
                    f.source,
                    f.as_of,
                    f.holdings.len(),
                    f.positions,
                    f.resolved(),
                    f.sectors.len()
                );
                for h in f.holdings.iter().take(5) {
                    println!("   {:>6.2}%  {:<28} {:?}", h.weight, h.name, h.ticker);
                }
            }
            Err(e) => println!("{t}: {e:#}"),
        }
    }
    let voo = tr_data::etf::read_cached(&dir, "VOO").expect("VOO from N-PORT");
    assert_eq!(voo.source, tr_data::etf::Source::Nport);
    assert!(voo.holdings.len() > 400 && voo.as_of.is_some());
    assert!(voo.holdings.iter().any(|h| h.ticker.as_deref() == Some("AAPL")));
    let _ = std::fs::remove_dir_all(&dir);
}

mod earnings {
    use tr_data::earnings::{Nasdaq, Timing};

    #[tokio::test]
    #[ignore = "network"]
    async fn nasdaq_calendar_and_history() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let n = Nasdaq::new().unwrap();
        // Mid-quarter weekdays can be empty; scan the last two weeks for a busy one.
        let today = chrono::Utc::now().date_naive();
        let mut best = 0;
        for back in 1..15 {
            let d = today - chrono::Duration::days(back);
            if chrono::Datelike::weekday(&d).number_from_monday() > 5 {
                continue;
            }
            best = best.max(n.calendar(d).await.unwrap().len());
        }
        assert!(best > 0, "no earnings in the last two weeks");
        let r = n.reported("AAPL").await.unwrap();
        assert!(r.len() >= 3 && r[0].eps.is_some());
        let _ = Timing::Unknown;
    }
}

#[tokio::test]
#[ignore = "network"]
async fn trfunds_kap() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("tr-trfunds-live-{}", std::process::id()));
    let list = tr_data::trfunds::fund_list(&dir).await.unwrap();
    assert!(list.len() > 1000, "{} funds", list.len());
    assert!(list.iter().any(|f| f.code == "TI2"));
    let f = tr_data::trfunds::load(&dir, "TI2", false).await.unwrap();
    println!("{:?}\n{:?}", f.profile, f.latest().map(|s| (s.label(), s.report.positions.len(), s.report.net_assets)));
    let snap = f.latest().expect("a monthly report");
    assert!(snap.report.positions.len() > 10, "{:?}", f.note);
    assert!(f.profile.mgmt_fee.is_some());
    assert!(!f.changes().is_empty() || f.previous().is_none());
    // The prospectus of TZT says redemptions are paid the first business day (TEFAS: sell valör 1).
    let tzt = tr_data::trfunds::load(&dir, "TZT", false).await.unwrap();
    assert!(tzt.profile.valor.as_deref().is_some_and(|v| v.starts_with("Sell T+1")), "{:?}", tzt.profile.valor);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Indexes the curated list and prints how many funds' reports parse (slow: ~50 funds).
#[tokio::test]
#[ignore = "network, slow"]
async fn trfunds_curated_coverage() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let dir = std::env::temp_dir().join("tr-trfunds-coverage");
    let mut ok = 0;
    for code in tr_data::trfunds::CURATED {
        match tr_data::trfunds::load(&dir, code, false).await {
            Ok(f) => {
                let n = f.latest().map_or(0, |s| s.report.positions.len());
                println!(
                    "{code}: {} · {n} stocks · {} reports · {:?}",
                    f.latest().map_or("-".into(), |s| s.label()),
                    f.reports.len(),
                    f.note
                );
                ok += usize::from(n > 0);
            }
            Err(e) => println!("{code}: ERROR {e:#}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
    }
    println!("{ok}/{} funds with stock positions", tr_data::trfunds::CURATED.len());
}

/// Needs `TR_SEC_USER_AGENT`. Form 4s of an active large issuer, a manager's 13F and the reverse lookup.
#[tokio::test]
#[ignore = "network"]
async fn smartmoney_sec() {
    use tr_data::smartmoney as sm;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ua = tr_data::edgar::user_agent("").expect("set TR_SEC_USER_AGENT to run this test");
    let dir = std::env::temp_dir().join(format!("tr-smart-live-{}", std::process::id()));
    let ins = sm::load_insiders(&dir, "AAPL", &ua).await.unwrap();
    assert!(ins.read > 0 && !ins.txs.is_empty(), "AAPL Form 4s");
    assert!(ins.txs.iter().all(|t| t.shares > 0.0));
    let sum = sm::summarize(&ins.txs, chrono::Local::now().date_naive(), 90);
    println!("AAPL: {} filings ({} read), {} txs, 90d {sum:?}", ins.filings, ins.read, ins.txs.len());
    let berkshire = sm::load_manager(&dir, 1067983, &ua, false).await.unwrap();
    let latest = berkshire.latest.as_ref().unwrap();
    assert!(latest.positions.len() > 20 && latest.total > 1e11, "Berkshire 13F");
    assert!(berkshire.prev.is_some());
    assert!(berkshire.top(25).iter().any(|c| c.mv != sm::Move::Unchanged) || latest.positions.len() > 25);
    let idx = sm::title_index(&dir, &ua).await.unwrap();
    let tickers = idx.resolve(&latest.positions[..25]);
    for c in berkshire.top(12) {
        println!(
            "{:>6} {:<28} {:>14.0} {:?} {:?}",
            tickers.get(&c.position.cusip).map_or("-", |t| t.as_str()),
            c.position.name,
            c.position.value,
            c.mv,
            c.pct
        );
    }
    println!("resolved {}/25 tickers", tickers.len());
    for p in latest.positions[..25].iter().filter(|p| !tickers.contains_key(&p.cusip)) {
        println!("unresolved: {} | {}", p.name, p.class);
    }
    let held = sm::holders_of(std::slice::from_ref(&berkshire), "AAPL", &idx);
    assert_eq!(held.len(), 1, "Berkshire holds Apple");
}

/// FINRA files need no login or key.
#[tokio::test]
#[ignore = "network"]
async fn smartmoney_finra() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("tr-finra-live-{}", std::process::id()));
    let d = tr_data::smartmoney::load_short(&dir, "AAPL").await;
    assert!(d.interest_error.is_none() && d.interest.len() >= 3, "short interest: {:?}", d.interest_error);
    println!("{:?}\n{:?}", d.interest.first(), d.volume.first());
    assert!(d.interest[0].short > 1e6 && d.interest[0].days_to_cover > 0.0);
    assert!(d.volume_error.is_none() && d.volume.len() >= 5, "short volume: {:?}", d.volume_error);
    assert!(d.volume.iter().all(|v| v.ratio().is_some_and(|r| r > 0.0 && r <= 100.0)));
}
