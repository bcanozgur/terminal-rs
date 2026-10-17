//! Network tests against FRED/ALFRED for the economic calendar; run with
//! `cargo test -p tr-data --test eco_live -- --ignored --nocapture`. Set `FRED_API_KEY` to
//! also exercise the official API.
use chrono::NaiveDate;
use tr_data::eco::{DateSource, Eco, KeyStore, RowState, event, monday};

fn eco() -> Eco {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Eco::new(None).unwrap()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[tokio::test]
#[ignore = "network"]
async fn week_with_release_values() {
    let e = eco();
    let key = KeyStore::from_env();
    let today = chrono::Utc::now().date_naive();
    // The previous week is fully released.
    let prev = monday(today) - chrono::Days::new(7);
    let w = e.week(key.as_deref(), prev, now()).await;
    println!("{:?} notes={:?}", w.source, w.notes);
    for r in &w.rows {
        println!("{} {:?} {:<8} {:?} {:?} {:?} {:?}", r.date, r.et, r.code, r.state, r.period, r.actual, r.prior);
    }
    if w.source == DateSource::Unavailable {
        assert!(!w.latest.is_empty(), "latest values are keyless");
    } else {
        assert!(w.rows.iter().any(|r| r.code == "CLAIMS"), "claims are weekly");
        assert!(w.rows.iter().filter(|r| r.code != "FOMC").all(|r| r.state == RowState::Released), "{:?}", w.rows);
    }
}

#[tokio::test]
#[ignore = "network"]
async fn release_day_vintage_and_history() {
    let e = eco();
    let cpi = event("CPI").unwrap();
    let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
    let day = e.observations(cpi.series, d(2026, 1, 1), Some(d(2026, 9, 11)), now()).await.unwrap();
    assert_eq!(day.last().unwrap().0, d(2026, 8, 1), "the Sep 11 vintage ends with August");
    let s = e.series(cpi, now()).await.unwrap();
    assert!(s.values.len() > 100);
}
