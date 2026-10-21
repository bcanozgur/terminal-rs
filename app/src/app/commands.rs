//! Executing actions and commands: dispatch, functions and alerts.

use super::*;

impl TerminalApp {
    pub(super) fn run_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::OpenChart(s) => self.open_security(&s),
                Action::OpenChartNew(s) => self.open_chart_new(&s),
                Action::OpenTicker(s) => {
                    self.open_panel(PaneKind::Ticker { symbol: s, trades: Vec::new(), loading: true })
                }
                Action::OpenProfile(q) => self.open_panel(PaneKind::Profile { query: q, data: None, loading: true }),
                Action::AddToWatchlist(s) => {
                    let key = tr_data::security(&s).key();
                    if !self.watchlist.contains(&key) {
                        self.status = format!("Added {key} to the watchlist");
                        self.watchlist.push(key);
                        self.last_quotes = None;
                    } else {
                        self.status = format!("{key} is already on the watchlist");
                    }
                }
                Action::RemoveFromWatchlist(s) => {
                    let key = tr_data::security(&s).key();
                    self.watchlist.retain(|k| *k != key);
                    self.status = format!("Removed {key} from the watchlist");
                }
                Action::LinkedSymbol(link, s) => self.broadcast(link, &s, None),
                Action::AddAlert(spec) => self.add_alert(spec),
                Action::OpenAlertChart(id) => self.open_alert_chart(id),
                Action::AlertOp(op) => self.worker.send(Request::AlertRules(op)),
                Action::Command(line) => match command::parse(&line) {
                    Some(cmd) => self.dispatch(cmd),
                    None => tracing::warn!("internal command did not parse: {line}"),
                },
                Action::Track(q, on) => {
                    self.worker.send(Request::Track { query: q.clone(), on });
                    // Refresh any open profile so the star updates.
                    for p in &self.panes {
                        if let PaneKind::Profile { query, .. } = &p.kind {
                            self.worker.send(Request::Profile { pane: p.id, query: query.clone() });
                        }
                    }
                }
            }
        }
    }

    pub(super) fn execute(&mut self, input: &str) {
        let Some(cmd) = command::parse(input) else {
            self.status = format!("Unknown command: {input}  (HELP lists functions)");
            return;
        };
        self.cmd_history.push(input.trim().to_string());
        self.dispatch(cmd);
    }

    pub(super) fn dispatch(&mut self, cmd: Command) {
        match cmd {
            Command::Symbol(s) => self.open_security(&s),
            Command::SymbolFunction(s, Function::Congress) => {
                self.open_panel(PaneKind::Ticker { symbol: s.clone(), trades: Vec::new(), loading: true });
                self.status = format!("{s}: congressional trades");
            }
            Command::SymbolFunction(s, Function::News) => {
                self.open_panel(PaneKind::News(Box::new(NewsPane::new(Some(s.clone())))));
                self.status = format!("{s}: news");
            }
            Command::SymbolFunction(s, Function::OrderBook(depth)) => {
                let depth = depth.unwrap_or(self.prefs.panels.book_depth).clamp(5, 200);
                self.open_panel(PaneKind::OrderBook(Box::new(BookPane::new(&s, depth, 0.0))));
                self.status = format!("{s}: order book");
            }
            Command::SymbolFunction(s, Function::Options) => {
                self.open_panel(PaneKind::Options(Box::new(OptionsPane::new(&s, 12, true))));
                self.status = format!("{s}: option chain");
            }
            Command::SymbolFunction(s, Function::Earnings) => {
                self.open_earnings(Some(&s));
                self.status = format!("{s}: earnings history");
            }
            Command::SymbolFunction(s, Function::Portfolio(view, arg)) => self.portfolio_command(view, &arg, Some(s)),
            Command::SymbolFunction(s, Function::Trade(mut t)) => {
                t.symbol = Some(s);
                self.open_ticket(t, None);
            }
            Command::SymbolFunction(s, Function::Edgar { fa }) => {
                self.open_panel(PaneKind::Edgar(Box::new(edgar_view::EdgarPane::new(&s, fa))));
                self.status = format!("{s}: {}", if fa { "financial analysis" } else { "description" });
            }
            Command::SymbolFunction(s, Function::Etf(_)) => {
                let ticker = tr_data::security(&s).ticker;
                self.etf_command(&format!("HELD {ticker}"));
                self.status = format!("{ticker}: ETFs that hold it");
            }
            Command::SymbolFunction(s, Function::Smart(arg)) => self.smart_command(&arg, Some(s)),
            Command::SymbolFunction(s, Function::TrFunds(_)) => {
                let ticker = tr_data::security(&s).ticker;
                self.trf_held(&ticker);
                self.status = format!("{ticker}: funds that hold it");
            }
            Command::SymbolFunction(s, Function::AddAlert(mut a)) => {
                a.symbol.get_or_insert(s);
                self.add_alert(a);
            }
            Command::SymbolFunction(s, f @ (Function::NewPanel(_) | Function::Split { .. })) => {
                self.apply_with_symbol(f, Some(s));
            }
            Command::SymbolFunction(s, f) => {
                self.open_security(&s);
                self.apply(f);
            }
            Command::Function(f) => self.apply(f),
        }
    }

    pub(super) fn apply(&mut self, f: Function) {
        self.apply_with_symbol(f, None);
    }

    pub(super) fn apply_with_symbol(&mut self, f: Function, symbol: Option<String>) {
        match f {
            Function::Timeframe(tf) => self.with_chart(|c, w| c.set_timeframe(tf, w)),
            Function::ChartType(t) => self.with_chart(|c, _| c.chart_type = t),
            Function::Study(id) => self.with_chart(|c, w| c.toggle_study(&id, w)),
            Function::Indicator(spec) => self.with_chart(|c, w| c.toggle_indicator(spec, w)),
            Function::ClearIndicators => self.with_chart(|c, _| c.clear_indicators()),
            Function::Compare(s) => self.with_chart(|c, w| c.set_compare(s, w)),
            Function::LogScale => self.with_chart(|c, _| c.log = !c.log),
            Function::Range(r) => self.with_chart(|c, w| c.set_range(r, w)),
            Function::Draw(t) => self.with_chart(|c, _| c.start_tool(t)),
            Function::AnchorVwap => self.with_chart(|c, _| c.start_anchor_pick()),
            Function::ClearDrawings => self.with_chart(|c, _| c.drawings.clear()),
            Function::Magnet => self.with_chart(|c, _| c.magnet = !c.magnet),
            Function::Congress => self.open_panel(PaneKind::Feed { filter: FeedFilter::default() }),
            Function::Politician(q) if q.trim().is_empty() => self.open_panel(PaneKind::Politicians),
            Function::Politician(q) => self.open_panel(PaneKind::Profile { query: q, data: None, loading: true }),
            Function::Track(q) => self.worker.send(Request::Track { query: q, on: true }),
            Function::Untrack(q) => self.worker.send(Request::Track { query: q, on: false }),
            Function::Alerts => {
                self.worker.send(Request::Alerts);
                self.open_panel(PaneKind::Alerts);
            }
            Function::AddAlert(mut a) => {
                if a.symbol.is_none() {
                    a.symbol = symbol.or_else(|| self.focused_symbol());
                }
                self.add_alert(a);
            }
            Function::Refresh => self.worker.send(Request::RefreshCongress),
            Function::Watchlist => self.open_panel(PaneKind::Watchlist),
            Function::WatchAdd(words) => {
                for key in keys_from_words(&words) {
                    if !self.watchlist.contains(&key) {
                        self.watchlist.push(key);
                    }
                }
                self.last_quotes = None;
            }
            Function::WatchRemove(words) => {
                let keys = keys_from_words(&words);
                self.watchlist.retain(|s| !keys.contains(s));
            }
            Function::Layout(arg) => self.layout_command(&arg),
            Function::Layouts => {
                self.settings_tab = SettingsTab::Layouts;
                self.open_panel(PaneKind::Settings);
                self.status = format!("Layouts: {}", self.layout_names.join(" · "));
            }
            Function::NewPanel(spec) => {
                let kind = self.panel_from_spec(&spec, symbol.as_deref());
                match kind {
                    Some(kind) => {
                        let target = self.focused_visible();
                        let id = self.new_pane(kind);
                        self.dock_pane(id, target, crate::dock::Zone::Right);
                    }
                    None => self.status = format!("Unknown panel {spec:?}; try CHART, NEWS, BOOK, WEI, GC, CONG, WL"),
                }
            }
            Function::Split { down, panel } => match self.panel_from_spec(&panel, symbol.as_deref()) {
                Some(kind) => {
                    let target = self.focused_visible();
                    let id = self.new_pane(kind);
                    let zone = if down { crate::dock::Zone::Bottom } else { crate::dock::Zone::Right };
                    self.dock_pane(id, target, zone);
                }
                None => self.status = format!("Unknown panel {panel:?}"),
            },
            Function::ClosePane => self.close_pane(self.focus),
            Function::PopOut => self.pop_out(self.focus),
            Function::Link(link) => {
                let focus = self.focus;
                self.set_link(focus, link);
            }
            Function::News => self.open_panel(PaneKind::News(Box::new(NewsPane::new(self.focused_symbol())))),
            Function::OrderBook(depth) => {
                let sym = self.focused_symbol().unwrap_or_else(|| "BTCUSD Curncy".into());
                let depth = depth.unwrap_or(self.prefs.panels.book_depth).clamp(5, 200);
                self.open_panel(PaneKind::OrderBook(Box::new(BookPane::new(&sym, depth, 0.0))));
            }
            Function::Settings => self.open_panel(PaneKind::Settings),
            Function::Options => {
                let sym = self.optionable_symbol();
                self.open_panel(PaneKind::Options(Box::new(OptionsPane::new(&sym, 12, true))));
            }
            Function::Earnings => self.open_earnings(symbol.as_deref()),
            Function::EarningsMarkers => self.with_chart(|c, _| c.earnings.show = !c.earnings.show),
            Function::Sidebar(arg) => {
                let side = &mut self.prefs.appearance.sidebar_side;
                match arg.as_str() {
                    "LEFT" => *side = prefs::SidebarSide::Left,
                    "RIGHT" => *side = prefs::SidebarSide::Right,
                    "SWAP" | "FLIP" | "OTHER" => *side = side.other(),
                    "SHOW" | "ON" => self.sidebar_open = true,
                    "HIDE" | "OFF" => self.sidebar_open = false,
                    _ => self.sidebar_open = !self.sidebar_open,
                }
                self.save_prefs();
            }
            Function::Theme(t) => {
                let system = self.system_theme;
                self.theme_command(&t, system);
            }
            Function::Title(t) => {
                let focus = self.focus;
                if let Some(p) = self.pane_mut(focus) {
                    p.title = (!t.trim().is_empty()).then(|| t.trim().to_string());
                }
            }
            Function::Help => self.open_panel(PaneKind::Help),
            Function::Changelog => self.open_panel(PaneKind::Changelog),
            Function::Wei => {
                self.last_wei = None;
                self.open_panel(PaneKind::Wei);
            }
            Function::Curve => {
                self.worker.send(Request::YieldCurves);
                self.last_curve = Some(Instant::now());
                self.open_panel(PaneKind::Curve);
            }
            Function::Search(q) => {
                if !q.trim().is_empty() {
                    self.worker.send(Request::Search(q.clone()));
                }
                self.open_panel(PaneKind::Search { query: q });
            }
            Function::Portfolio(view, arg) => self.portfolio_command(view, &arg, None),
            Function::Trade(t) => self.open_ticket(t, None),
            Function::AccountMode(live) => {
                let mode = if live { prefs::AccountMode::Live } else { prefs::AccountMode::Demo };
                if mode == self.prefs.trading.account_mode {
                    self.status = format!("Already using the {} account", if live { "LIVE" } else { "DEMO" });
                }
                self.set_account_mode(mode, false);
            }
            Function::Edgar { fa } => {
                let sym = self.focused_symbol().unwrap_or_default();
                self.open_panel(PaneKind::Edgar(Box::new(edgar_view::EdgarPane::new(&sym, fa))));
            }
            Function::Eco(arg) => self.eco_command(&arg),
            Function::EcoSeries(name) => self.eco_series_command(&name),
            Function::Etf(arg) => self.etf_command(&arg),
            Function::TrFunds(arg) => self.trf_command(&arg),
            Function::Most(arg) => self.most_command(&arg),
            Function::Top(arg) => self.top_command(&arg),
            Function::Fut(arg) => self.fut_command(&arg),
            Function::Smart(arg) => self.smart_command(&arg, None),
            Function::Risk(arg) => self.risk_command(&arg),
            Function::Audit(arg) => self.audit_command(&arg),
            Function::Kill(arg) => self.kill_command(&arg),
        }
    }

    /// Arm a price/indicator alert from the command line or a chart.
    pub(super) fn add_alert(&mut self, a: crate::alerts::Spec) {
        let Some(key) = a.symbol.clone() else {
            self.status = "ALRT needs a security: ALRT AAPL > 250 (or focus a chart)".into();
            return;
        };
        let sec = tr_data::security(&key);
        let mode = if a.repeat { "repeating" } else { "once" };
        self.status = format!("Alert set: {} {} ({mode}) — ALRT lists rules", sec.ticker, a.cond.label());
        if let Some(q) = self.quotes.get(&key) {
            let m = crate::alerts::Market { last: Some(q.last), prev_close: Some(q.prev_close), ..Default::default() };
            if a.cond.state(&m) == Some(true) && a.cond.triggers(None, true) {
                self.status += " — already met, fires on the next quote";
            }
        }
        self.worker.send(Request::AlertRules(crate::alerts::RuleOp::Add(a)));
    }

    /// Unread Congress and price alerts (the ALERTS badge).
    pub(crate) fn unread_alerts(&self) -> usize {
        self.alerts.iter().filter(|a| !a.seen).count() + self.price_alerts.unseen()
    }
}
