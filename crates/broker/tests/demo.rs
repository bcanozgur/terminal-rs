//! The paper-trading demo account: fills, resting orders, buying power, persistence.

use chrono::{DateTime, Duration, TimeZone, Utc};
use tr_broker::demo::{self, ACCOUNT_ID, DemoBook, Event, STARTING_CASH};
use tr_broker::trading::{Amount, OrderRequest, OrderType, Side, TimeInForce};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 15, 0, 0).unwrap()
}

fn order(side: Side, symbol: &str, units: f64) -> OrderRequest {
    OrderRequest {
        account_id: ACCOUNT_ID.into(),
        symbol: symbol.into(),
        side,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Day,
        amount: Amount::Units(units),
        limit: None,
        stop: None,
    }
}

fn limit(side: Side, symbol: &str, units: f64, px: f64) -> OrderRequest {
    OrderRequest { order_type: OrderType::Limit, limit: Some(px), ..order(side, symbol, units) }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn starts_with_virtual_cash() {
    let b = DemoBook::new(t0());
    assert_eq!((b.cash, b.buying_power(), b.equity()), (STARTING_CASH, STARTING_CASH, STARTING_CASH));
    let pf = b.portfolio();
    assert!(pf.demo && !pf.simulated);
    assert_eq!(pf.accounts.len(), 1);
    assert!(pf.accounts[0].is_paper);
    assert_eq!(pf.accounts[0].id, ACCOUNT_ID);
}

#[test]
fn market_orders_fill_at_the_quote() {
    let mut b = DemoBook::new(t0());
    let o = b.submit(&order(Side::Buy, "aapl", 10.0), Some(200.0), t0()).unwrap();
    assert_eq!((o.status.as_str(), o.execution_price, o.filled), ("EXECUTED", Some(200.0), Some(10.0)));
    assert_eq!(o.symbol, "AAPL");
    assert!(close(b.cash, STARTING_CASH - 2000.0));
    assert_eq!(b.held("AAPL"), 10.0);

    // Average cost on adding, realised P&L on selling.
    b.submit(&order(Side::Buy, "AAPL", 10.0), Some(220.0), t0()).unwrap();
    let p = b.position("AAPL").unwrap();
    assert!(close(p.cost_basis.unwrap(), 210.0));
    b.submit(&order(Side::Sell, "AAPL", 5.0), Some(230.0), t0()).unwrap();
    assert!(close(b.realized, 100.0));
    assert_eq!(b.held("AAPL"), 15.0);
    assert!(close(b.cash, STARTING_CASH - 2000.0 - 2200.0 + 1150.0));

    // Marking to market moves equity, not cash.
    let cash = b.cash;
    b.on_price("AAPL", 250.0, t0());
    assert!(close(b.equity(), cash + 15.0 * 250.0));
    assert_eq!(b.activities.len(), 3, "every fill is recorded");
    assert!(close(b.activities[0].amount.unwrap(), 1150.0));
}

#[test]
fn notional_orders_buy_fractional_shares() {
    let mut b = DemoBook::new(t0());
    let req = OrderRequest { amount: Amount::Notional(500.0), ..order(Side::Buy, "NVDA", 0.0) };
    let o = b.submit(&req, Some(400.0), t0()).unwrap();
    assert_eq!(o.filled, Some(1.25));
    assert!(close(b.cash, STARTING_CASH - 500.0));
}

#[test]
fn refuses_what_a_cash_account_cannot_do() {
    let mut b = DemoBook::new(t0());
    let e = b.submit(&order(Side::Buy, "BRK.A", 1.0), Some(700_000.0), t0()).unwrap_err();
    assert!(e.contains("Not enough buying power"), "{e}");
    let e = b.submit(&order(Side::Sell, "TSLA", 1.0), Some(250.0), t0()).unwrap_err();
    assert!(e.contains("can't sell short"), "{e}");
    b.submit(&order(Side::Buy, "TSLA", 2.0), Some(250.0), t0()).unwrap();
    let e = b.submit(&order(Side::Sell, "TSLA", 3.0), Some(250.0), t0()).unwrap_err();
    assert!(e.contains("up to 2 TSLA"), "{e}");
    let e = b.submit(&order(Side::Buy, "MSFT", 1.0), None, t0()).unwrap_err();
    assert!(e.contains("Waiting for a price"), "market orders need a quote: {e}");
    let other = OrderRequest { account_id: "snaptrade-uuid".into(), ..order(Side::Buy, "MSFT", 1.0) };
    assert!(b.submit(&other, Some(400.0), t0()).is_err(), "only the demo account");
    assert!(b.orders.iter().all(|o| o.status == "EXECUTED"), "refused orders leave no trace");
}

#[test]
fn limit_orders_rest_until_the_price_crosses() {
    let mut b = DemoBook::new(t0());
    let o = b.submit(&limit(Side::Buy, "AAPL", 10.0, 190.0), Some(200.0), t0()).unwrap();
    assert!(o.is_open());
    assert!(close(b.buying_power(), STARTING_CASH - 1900.0), "a resting buy sets cash aside");
    assert_eq!(b.cash, STARTING_CASH);
    assert_eq!(b.watched(), vec!["AAPL".to_string()]);

    assert!(b.on_price("AAPL", 195.0, t0()).is_empty(), "not there yet");
    let ev = b.on_price("AAPL", 189.5, t0());
    assert_eq!(ev.len(), 1);
    let Event::Filled(f) = &ev[0] else { panic!("{ev:?}") };
    assert_eq!(f.execution_price, Some(189.5), "fills at the crossing quote, at or better than the limit");
    assert!(ev[0].message().starts_with("Bought 10 AAPL at $189.50"));
    assert!(close(b.cash, STARTING_CASH - 1895.0));
    assert!(close(b.buying_power(), b.cash), "the reservation is released");

    // A sell limit above the market waits for a rise and holds the shares back.
    let s = b.submit(&limit(Side::Sell, "AAPL", 10.0, 210.0), Some(190.0), t0()).unwrap();
    assert!(s.is_open());
    assert_eq!(b.available("AAPL"), 0.0);
    assert!(b.submit(&order(Side::Sell, "AAPL", 1.0), Some(190.0), t0()).is_err(), "shares are spoken for");
    b.on_price("AAPL", 211.0, t0());
    assert_eq!(b.held("AAPL"), 0.0);
    assert!(b.position("AAPL").is_none());
}

#[test]
fn marketable_limits_fill_immediately() {
    let mut b = DemoBook::new(t0());
    let o = b.submit(&limit(Side::Buy, "MSFT", 2.0, 420.0), Some(410.0), t0()).unwrap();
    assert_eq!((o.status.as_str(), o.execution_price), ("EXECUTED", Some(410.0)));
}

#[test]
fn stops_trigger_then_fill() {
    let mut b = DemoBook::new(t0());
    b.submit(&order(Side::Buy, "TSLA", 10.0), Some(250.0), t0()).unwrap();
    let stop = OrderRequest { order_type: OrderType::Stop, stop: Some(240.0), ..order(Side::Sell, "TSLA", 10.0) };
    assert!(b.submit(&stop, Some(250.0), t0()).unwrap().is_open());
    assert!(b.on_price("TSLA", 245.0, t0()).is_empty());
    let ev = b.on_price("TSLA", 238.0, t0());
    assert!(matches!(&ev[..], [Event::Filled(o)] if o.execution_price == Some(238.0)));

    // Stop-limit: the stop arms the order, then the limit decides the fill.
    b.submit(&order(Side::Buy, "TSLA", 10.0), Some(238.0), t0()).unwrap();
    let sl = OrderRequest {
        order_type: OrderType::StopLimit,
        stop: Some(230.0),
        limit: Some(228.0),
        ..order(Side::Sell, "TSLA", 10.0)
    };
    b.submit(&sl, Some(238.0), t0()).unwrap();
    assert!(b.on_price("TSLA", 225.0, t0()).is_empty(), "triggered, but below the sell limit");
    assert_eq!(b.orders[0].status, "TRIGGERED");
    assert!(b.orders[0].is_open());
    let ev = b.on_price("TSLA", 229.0, t0());
    assert!(matches!(&ev[..], [Event::Filled(o)] if o.execution_price == Some(229.0)));
}

#[test]
fn resting_buys_are_rejected_if_cash_ran_out() {
    let mut b = DemoBook::new(t0());
    b.reset(1_000.0, t0());
    b.submit(&limit(Side::Buy, "AAPL", 5.0, 150.0), Some(200.0), t0()).unwrap();
    // A later order cannot spend the reserved cash.
    let e = b.submit(&order(Side::Buy, "F", 30.0), Some(10.0), t0()).unwrap_err();
    assert!(e.contains("buying power"), "{e}");
    assert!(b.submit(&order(Side::Buy, "F", 25.0), Some(10.0), t0()).is_ok());
    let ev = b.on_price("AAPL", 149.0, t0());
    assert!(matches!(&ev[..], [Event::Filled(_)]), "its own reservation pays for it: {ev:?}");
}

#[test]
fn cancel_ioc_and_day_expiry() {
    let mut b = DemoBook::new(t0());
    let o = b.submit(&limit(Side::Buy, "AAPL", 1.0, 100.0), Some(200.0), t0()).unwrap();
    b.cancel(&o.id, t0()).unwrap();
    assert_eq!(b.orders[0].status, "CANCELED");
    assert!(b.cancel(&o.id, t0()).is_err(), "already cancelled");
    assert_eq!(b.buying_power(), STARTING_CASH);

    let ioc = OrderRequest { time_in_force: TimeInForce::Ioc, ..limit(Side::Buy, "AAPL", 1.0, 100.0) };
    assert_eq!(b.submit(&ioc, Some(200.0), t0()).unwrap().status, "CANCELED", "IOC never rests");

    let day = b.submit(&limit(Side::Buy, "AAPL", 1.0, 100.0), Some(200.0), t0()).unwrap();
    let gtc = OrderRequest { time_in_force: TimeInForce::Gtc, ..limit(Side::Buy, "AAPL", 1.0, 100.0) };
    let gtc = b.submit(&gtc, Some(200.0), t0()).unwrap();
    assert!(b.expire(t0() + Duration::hours(4)).is_empty(), "before the close");
    let ev = b.expire(t0() + Duration::hours(5));
    assert!(matches!(&ev[..], [Event::Expired(o)] if o.id == day.id));
    assert!(b.orders.iter().find(|o| o.id == gtc.id).unwrap().is_open(), "GTC orders stay");
    assert_eq!(
        demo::day_expiry(Utc.with_ymd_and_hms(2026, 10, 12, 21, 0, 0).unwrap()).to_rfc3339(),
        "2026-10-13T20:00:00+00:00"
    );
}

#[test]
fn close_position_sells_everything_available() {
    let mut b = DemoBook::new(t0());
    b.submit(&order(Side::Buy, "AMD", 12.5), Some(100.0), t0()).unwrap();
    let req = b.close_request("amd").unwrap();
    assert_eq!((req.side, req.amount), (Side::Sell, Amount::Units(12.5)));
    b.submit(&req, Some(101.0), t0()).unwrap();
    assert!(b.position("AMD").is_none());
    assert!(b.close_request("AMD").is_none());
    assert!(close(b.cash, STARTING_CASH + 12.5));
}

#[test]
fn persists_and_resets() {
    let dir = std::env::temp_dir().join(format!("tr-demo-test-{}", std::process::id()));
    let path = dir.join("demo.json");
    let _ = std::fs::remove_dir_all(&dir);
    let fresh = DemoBook::load(&path, t0()).unwrap();
    assert_eq!(fresh.cash, STARTING_CASH, "a missing file is a new account");

    let mut b = DemoBook::new(t0());
    b.submit(&order(Side::Buy, "AAPL", 3.0), Some(200.0), t0()).unwrap();
    b.submit(&limit(Side::Buy, "AAPL", 1.0, 150.0), Some(200.0), t0()).unwrap();
    b.save(&path).unwrap();
    let mut back = DemoBook::load(&path, t0()).unwrap();
    assert_eq!(back, b);
    // Ids keep counting after a reload.
    let next = back.submit(&order(Side::Buy, "AAPL", 1.0), Some(200.0), t0()).unwrap();
    assert!(b.orders.iter().all(|o| o.id != next.id));

    back.reset(50_000.0, t0());
    assert_eq!((back.cash, back.starting_cash), (50_000.0, 50_000.0));
    assert!(back.positions.is_empty() && back.orders.is_empty() && back.activities.is_empty());

    std::fs::write(&path, b"{not json").unwrap();
    assert!(DemoBook::load(&path, t0()).is_err(), "a damaged file is reported, not silently replaced");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_is_bounded_but_keeps_open_orders() {
    let mut b = DemoBook::new(t0());
    let open = b.submit(&limit(Side::Buy, "AAPL", 1.0, 1.0), Some(200.0), t0()).unwrap();
    for _ in 0..(demo::HISTORY + 20) {
        b.submit(&order(Side::Buy, "F", 1.0), Some(1.0), t0()).unwrap();
    }
    assert_eq!(b.orders.len(), demo::HISTORY);
    assert!(b.orders.iter().any(|o| o.id == open.id));
    assert!(b.activities.len() <= demo::HISTORY);
}

#[test]
fn a_full_history_never_loses_the_order_just_placed() {
    let mut b = DemoBook::new(t0());
    for i in 0..demo::HISTORY {
        b.submit(&limit(Side::Buy, "F", 1.0, 0.01 + i as f64 * 1e-4), Some(1.0), t0()).unwrap();
    }
    assert!(b.orders.iter().all(|o| o.is_open()));
    let o = b.submit(&order(Side::Buy, "F", 1.0), Some(1.0), t0()).unwrap();
    assert_eq!(o.status, "EXECUTED", "the fill is reported even when trimming drops it from history");
    assert_eq!(b.held("F"), 1.0);
}

#[test]
fn late_quotes_never_fill_expired_day_orders() {
    let mut b = DemoBook::new(t0());
    b.submit(&limit(Side::Buy, "AAPL", 10.0, 190.0), Some(200.0), t0()).unwrap();
    let ev = b.on_price("AAPL", 185.0, t0() + Duration::days(1));
    assert!(matches!(&ev[..], [Event::Expired(_)]), "{ev:?}");
    assert_eq!((b.cash, b.held("AAPL")), (STARTING_CASH, 0.0));
}

#[test]
fn dollar_amounts_never_cost_more_than_stated() {
    let mut b = DemoBook::new(t0());
    let all_in = OrderRequest { amount: Amount::Notional(STARTING_CASH), ..order(Side::Buy, "F", 0.0) };
    b.submit(&all_in, Some(7.0), t0()).unwrap();
    assert!(b.cash >= 0.0 && b.cash < 0.01, "{}", b.cash);
}
