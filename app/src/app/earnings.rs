//! App glue for the earnings panes (ERN) and the chart "E" markers: responses, refresh
//! scheduling and the morning-of-earnings toast.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{PaneKind, TerminalApp};
use crate::earnings_view::ErnPane;
use crate::earnings_worker::{EarnReq, EarnResp};
use crate::worker::Request;

/// Minutes between "who reports today" checks for watchlist symbols.
const TOAST_CHECK_MIN: u64 = 30;

impl TerminalApp {
    /// Upper-case tickers on the watchlist, sidebar lists and open positions, when pane `i`
    /// is an earnings calendar (empty otherwise).
    pub(super) fn earnings_mine(&self, i: usize) -> HashSet<String> {
        if !matches!(self.panes[i].kind, PaneKind::Earnings(_)) {
            return HashSet::new();
        }
        self.mine_tickers()
    }

    fn mine_tickers(&self) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        let keys = self.watchlist.iter().chain(self.groups.iter().flat_map(|g| g.keys.iter()));
        out.extend(keys.map(|k| tr_data::security(k).ticker.to_ascii_uppercase()));
        let positions =
            self.demo.pf.positions.iter().chain(self.broker.portfolio.iter().flat_map(|p| p.positions.iter()));
        out.extend(positions.map(|p| p.symbol.to_ascii_uppercase()));
        out
    }

    pub(super) fn on_earnings(&mut self, r: EarnResp) {
        let today = tr_data::earnings::today_et();
        match r {
            EarnResp::Day { pane, monday, day, rows } => {
                if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                    && let PaneKind::Earnings(e) = &mut p.kind
                {
                    e.on_day(monday, today, day, rows);
                }
            }
            EarnResp::Ticker { pane, symbol, data } => {
                let Some(p) = self.panes.iter_mut().find(|p| p.id == pane) else { return };
                match &mut p.kind {
                    PaneKind::Earnings(e) => e.on_ticker(&symbol, data),
                    PaneKind::Chart(c) => {
                        if let Ok(d) = data {
                            c.on_earnings(&symbol, *d);
                        }
                    }
                    _ => {}
                }
            }
            EarnResp::Today(rows) => {
                for r in rows {
                    if self.earn.toasted.insert((r.date, r.symbol.clone())) {
                        let est = r.eps_estimate.map_or(String::new(), |e| {
                            format!(" · est EPS {}", crate::earnings_view::fmt_eps(Some(e)))
                        });
                        let when = match r.timing {
                            tr_data::earnings::Timing::PreMarket => " before the open",
                            tr_data::earnings::Timing::AfterHours => " after the close",
                            tr_data::earnings::Timing::Unknown => "",
                        };
                        self.toasts.push((format!("{} reports today{when}{est}", r.symbol), Instant::now()));
                    }
                }
            }
        }
    }

    /// Fetch visible earnings panes when due, and check for watchlist reports once in a while.
    pub(super) fn earnings_periodic(&mut self, visible: &[usize]) {
        let today = tr_data::earnings::today_et();
        for &i in visible {
            let p = &mut self.panes[i];
            if let PaneKind::Earnings(e) = &mut p.kind
                && let Some(req) = e.due(p.id, today)
            {
                self.worker.send(Request::Earnings(req));
            }
        }
        let offline = self.provider.eq_ignore_ascii_case("sample");
        if !offline && self.earn.last_check.is_none_or(|t| t.elapsed() > Duration::from_secs(TOAST_CHECK_MIN * 60)) {
            // Wait for the watchlist to load on a cold start.
            if self.watchlist.is_empty() {
                return;
            }
            self.earn.last_check = Some(Instant::now());
            let symbols: Vec<String> = self.watchlist.iter().map(|k| tr_data::security(k).ticker).collect();
            self.worker.send(Request::Earnings(EarnReq::Today { symbols }));
        }
    }

    /// Ticker panes of other kinds point here: open `symbol`'s earnings history.
    pub(super) fn open_earnings(&mut self, symbol: Option<&str>) {
        let pane = match symbol {
            Some(s) => ErnPane::ticker(&tr_data::security(s).key()),
            None => ErnPane::calendar(Default::default(), 1),
        };
        self.open_panel(PaneKind::Earnings(Box::new(pane)));
    }
}
