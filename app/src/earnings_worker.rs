//! Background side of the earnings calendar (ERN / EVTS / EE): requests, responses and the
//! handler the worker dispatches to. Kept out of `worker.rs`, which only carries one
//! `Request::Earnings` / `Response::Earnings` variant and a one-line match arm.

use std::sync::{Mutex, OnceLock};

use chrono::{NaiveDate, Utc};
use tr_core::Timeframe;
use tr_data::Provider;
use tr_data::earnings::{
    CalendarRow, EarningsStore, EdgarSource, Nasdaq, NoEdgar, TickerEarnings, calendar_day, price_reaction,
    ticker_history, week_days,
};

use crate::worker::{PaneId, data_dir};

pub enum EarnReq {
    /// One week of the calendar (Monday given), fetched day by day.
    Week { pane: PaneId, monday: NaiveDate },
    /// A ticker's history; `pane` is the earnings pane or the chart that wants markers.
    Ticker { pane: PaneId, symbol: String },
    /// Which of `symbols` (tickers) report today.
    Today { symbols: Vec<String> },
}

pub enum EarnResp {
    Day { pane: PaneId, monday: NaiveDate, day: NaiveDate, rows: Result<Vec<CalendarRow>, String> },
    Ticker { pane: PaneId, symbol: String, data: Result<Box<TickerEarnings>, String> },
    Today(Vec<CalendarRow>),
}

struct Engine {
    store: Mutex<EarningsStore>,
    nasdaq: Nasdaq,
    edgar: Box<dyn EdgarSource>,
}

fn engine() -> Result<&'static Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let dir = data_dir();
            let _ = std::fs::create_dir_all(&dir);
            let store = EarningsStore::open(dir.join("earnings.sqlite3"))
                .or_else(|e| {
                    tracing::warn!("earnings cache unavailable ({e:#}); using memory");
                    EarningsStore::open_in_memory()
                })
                .map_err(|e| format!("{e:#}"))?;
            let nasdaq = Nasdaq::new().map_err(|e| format!("{e:#}"))?;
            // Swap in the shared EDGAR client here once it is merged; until then history is
            // Nasdaq's last four reported quarters.
            Ok(Engine { store: Mutex::new(store), nasdaq, edgar: Box::new(NoEdgar) })
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub async fn handle(req: EarnReq, provider: &Provider, send: &(dyn Fn(EarnResp) + Sync)) {
    let now = Utc::now().timestamp();
    let today = tr_data::earnings::today_et();
    match req {
        EarnReq::Week { pane, monday } => {
            let eng = engine();
            for day in week_days(monday) {
                let rows = match &eng {
                    Ok(e) => calendar_day(&e.store, &e.nasdaq, day, today, now).await.map_err(|e| format!("{e:#}")),
                    Err(m) => Err(m.clone()),
                };
                let failed = rows.is_err();
                send(EarnResp::Day { pane, monday, day, rows });
                if failed {
                    break;
                }
            }
        }
        EarnReq::Ticker { pane, symbol } => {
            let data = ticker(provider, &symbol, today, now).await.map(Box::new);
            send(EarnResp::Ticker { pane, symbol, data });
        }
        EarnReq::Today { symbols } => {
            let Ok(e) = engine() else { return };
            if let Ok(rows) = calendar_day(&e.store, &e.nasdaq, today, today, now).await {
                let mine: Vec<CalendarRow> =
                    rows.into_iter().filter(|r| symbols.iter().any(|s| s.eq_ignore_ascii_case(&r.symbol))).collect();
                if !mine.is_empty() {
                    send(EarnResp::Today(mine));
                }
            }
        }
    }
}

async fn ticker(provider: &Provider, symbol: &str, today: NaiveDate, now: i64) -> Result<TickerEarnings, String> {
    let sec = tr_data::security(symbol);
    if sec.class != tr_core::AssetClass::Equity {
        return Err(format!("{} has no earnings reports", sec.key()));
    }
    let e = engine()?;
    let mut doc = ticker_history(&e.store, &e.nasdaq, e.edgar.as_ref(), &sec.ticker, today, now)
        .await
        .map_err(|e| format!("{e:#}"))?;
    // Price reactions from daily bars (a failure just leaves the column empty).
    if let Ok(bars) = provider.history(&sec.key(), Timeframe::D1, None, 1_600).await {
        for ev in &mut doc.events {
            ev.move_pct = price_reaction(&bars, ev.date, ev.timing);
        }
    }
    Ok(doc)
}
