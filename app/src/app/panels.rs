//! Pane content for every panel kind.

use std::time::Instant;

use egui::{Align2, Color32, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use tr_data::OrderBook;

use super::{PaneKind, ReadState, Reading, TerminalApp};
use crate::congress_view::{self, Action};
use crate::markets_view;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::Request;

impl TerminalApp {
    /// Draw pane `i` into `rect`. Returns its actions and whether it was clicked.
    pub(super) fn render_content(&mut self, ui: &mut Ui, i: usize, rect: Rect, focused: bool) -> (Vec<Action>, bool) {
        let mut actions = Vec::new();
        let id = self.panes[i].id;
        let dragging = self.drag.is_some_and(|d| d.active);
        if let PaneKind::Chart(c) = &mut self.panes[i].kind {
            c.hover_enabled = !dragging;
            c.quote = self.quotes.get(&c.symbol).cloned();
            let clicked = c.show(ui, rect, focused, &self.worker);
            actions.append(&mut c.actions);
            return (actions, clicked);
        }
        let freshness = self.congress_freshness();
        let earn_mine = self.earnings_mine(i);
        let inner = rect.shrink2(vec2(dp(8.0), dp(4.0)));
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).id_salt(("pane", id)));
        child.set_clip_rect(inner.intersect(ui.clip_rect()));
        let ui = &mut child;
        let q = self.search_bar(ui, i);
        let q = q.as_str();
        let watch: Vec<super::top_view::Watched> = if matches!(self.panes[i].kind, PaneKind::Top(_)) {
            let keys = self.watchlist.iter().chain(self.groups.iter().flat_map(|g| &g.keys));
            keys.filter_map(|k| super::top_view::Watched::from_key(k)).collect()
        } else {
            Vec::new()
        };
        match &mut self.panes[i].kind {
            PaneKind::Feed { filter } => actions.extend(congress_view::feed(ui, &self.latest, filter, &freshness, q)),
            PaneKind::Ticker { symbol, trades, loading } => {
                if *loading {
                    ui.label(RichText::new("Loading…").color(muted()));
                } else {
                    actions.extend(congress_view::ticker_trades(ui, symbol, trades, q));
                }
            }
            PaneKind::Profile { query, data, loading } => {
                actions.extend(congress_view::profile(ui, query, data.as_deref(), *loading, q))
            }
            PaneKind::Politicians => actions.extend(congress_view::politicians(ui, &self.politicians, q)),
            PaneKind::Alerts => {
                let (ops, open) = crate::alerts_view::rules(ui, &self.price_alerts, q);
                for op in ops {
                    self.worker.send(Request::AlertRules(op));
                }
                if let Some(id) = open {
                    actions.push(Action::OpenAlertChart(id));
                }
                let (a, seen) = congress_view::alerts(ui, &self.alerts, q);
                actions.extend(a);
                if seen {
                    self.worker.send(Request::MarkAlertsSeen);
                }
            }
            PaneKind::Wei => {
                actions.extend(markets_view::wei(ui, &self.quotes, self.wei_updated.as_deref(), &mut self.wei_order, q))
            }
            PaneKind::Curve => actions.extend(markets_view::curve(ui, self.curves.as_deref())),
            PaneKind::Search { query } => {
                let hits = if self.net_hits.0 == *query { self.net_hits.1.as_slice() } else { &[] };
                let (a, changed) = markets_view::search(ui, query, hits);
                actions.extend(a);
                if changed && query.trim().len() >= 2 {
                    self.search_pending = Some((query.clone(), Instant::now()));
                }
            }
            PaneKind::News(n) => {
                let offline = self.provider.eq_ignore_ascii_case("sample");
                if n.reading.is_some() {
                    reader(ui, n);
                } else if let Some(refresh) = news(ui, n, offline, q) {
                    n.fetched = None;
                    n.loading = refresh;
                }
                if let Some(r) = n.reading.as_ref().filter(|r| matches!(r.state, ReadState::Loading))
                    && n.read_requested.as_deref() != Some(r.item.link.as_str())
                {
                    n.read_requested = Some(r.item.link.clone());
                    self.worker.send(Request::Article { pane: id, url: r.item.link.clone() });
                }
            }
            PaneKind::Options(o) => {
                let (a, refetch) = super::options_view::options_chain(ui, o);
                actions.extend(a);
                if refetch {
                    o.fetched = None;
                }
            }
            PaneKind::OrderBook(b) => {
                if let Some(a) = order_book(ui, b) {
                    actions.push(a);
                }
            }
            PaneKind::Portfolio(pp) => {
                pp.query = q.to_string();
                let demo =
                    (self.prefs.trading.account_mode == crate::prefs::AccountMode::Demo).then_some(&*self.demo.pf);
                for a in super::portfolio_view::portfolio(ui, pp, &self.broker, demo, &self.quotes) {
                    use super::portfolio_view::PortAction as P;
                    match a {
                        P::Core(a) => actions.push(a),
                        P::Refresh => actions.push(Action::Command("PORT REFRESH".into())),
                        P::Connect => actions.push(Action::Command("PORT CONNECT".into())),
                        P::Setup => actions.push(Action::Command("PORT SETUP".into())),
                        P::Options(k) => actions.push(Action::Command(format!("{k} OMON"))),
                        P::News(k) => actions.push(Action::Command(format!("{k} CN"))),
                        P::Trade { key, buy, account, units } => self.open_ticket(
                            crate::command::TradeArgs {
                                buy: Some(buy),
                                symbol: Some(key),
                                units,
                                ..Default::default()
                            },
                            account,
                        ),
                        P::CancelOrder { account, id } => self.request_cancel(&account, &id),
                        P::ClosePosition { account, key } => self.close_position(&account, &key),
                        P::TradingSettings => {
                            self.settings_tab = super::settings_view::SettingsTab::Trading;
                            self.open_panel(PaneKind::Settings);
                        }
                        P::Open(url) => {
                            if let Err(e) = super::broker::open_external(url) {
                                self.status = format!("Could not open {url}: {e}");
                            }
                        }
                    }
                }
            }
            PaneKind::Earnings(e) => {
                let cx =
                    crate::earnings_view::ViewCtx { today: tr_data::earnings::today_et(), mine: &earn_mine, query: q };
                actions.extend(crate::earnings_view::pane(ui, e, &cx));
            }
            PaneKind::Eco(e) => super::eco_view::eco(ui, e, q),
            PaneKind::Etf(e) => actions.extend(super::etf_view::etf(ui, e, q)),
            PaneKind::Risk(_) => actions.extend(self.risk_pane(ui, i)),
            PaneKind::Audit(_) => actions.extend(self.audit_pane(ui, i)),
            PaneKind::TrFunds(e) => actions.extend(super::trfunds_view::trfunds(ui, e, q)),
            PaneKind::Most(e) => actions.extend(super::most_view::most(ui, e)),
            PaneKind::Top(e) => actions.extend(super::top_view::top(ui, e, &watch)),
            PaneKind::Cryp(e) => actions.extend(super::cryp_view::cryp(ui, e)),
            PaneKind::Fut(e) => actions.extend(super::fut_view::fut(ui, e)),
            PaneKind::Smart(e) => actions.extend(super::smart_view::smart(ui, e)),
            PaneKind::Watchlist
            | PaneKind::List(_)
            | PaneKind::Settings
            | PaneKind::Help
            | PaneKind::Changelog
            | PaneKind::Chart(_) => {}
            PaneKind::Edgar(e) => {
                let out = super::edgar_view::edgar_pane(ui, e, &self.quotes, &mut self.prefs.panels);
                actions.extend(out.actions);
                if out.refetch {
                    e.fetched = None;
                    e.loading = false;
                }
                if out.save_prefs {
                    self.save_prefs();
                }
            }
        }
        match self.panes[i].kind {
            PaneKind::Watchlist => actions.extend(self.watchlist(ui, None, q)),
            PaneKind::List(ref name) => {
                let name = name.clone();
                let gi = self.groups.iter().position(|g| g.name == name);
                actions.extend(self.watchlist(ui, gi, q))
            }
            PaneKind::Help => self.help_panel(ui),
            PaneKind::Changelog => self.changelog_panel(ui),
            PaneKind::Settings => {
                let cmds = self.settings_panel(ui);
                self.run_settings_cmds(cmds);
            }
            _ => {}
        }
        (actions, false)
    }

    /// Pane `i`'s filter box, when open (see the strip's search button). Returns the
    /// trimmed, lowercased query, empty when the box is closed or blank.
    fn search_bar(&mut self, ui: &mut Ui, i: usize) -> String {
        let id = self.panes[i].id;
        let Some(hint) = self.panes[i].kind.search_hint() else { return String::new() };
        let Some(q) = self.pane_search.get_mut(&id) else { return String::new() };
        let r = ui.add(
            egui::TextEdit::singleline(q)
                .id(egui::Id::new(("pane-search", id)))
                .hint_text(RichText::new(hint).font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .text_color(amber())
                .desired_width(f32::INFINITY),
        );
        if self.search_focus == Some(id) {
            r.request_focus();
            self.search_focus = None;
        }
        let needle = q.trim().to_ascii_lowercase();
        if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Escape)) {
            self.pane_search.remove(&id);
            return String::new();
        }
        ui.add_space(4.0);
        needle
    }
}

/// Headlines list. Returns `Some(_)` when a refresh was requested.
fn news(ui: &mut Ui, n: &mut super::NewsPane, offline: bool, q: &str) -> Option<bool> {
    let mut refresh = None;
    ui.horizontal_wrapped(|ui| {
        let title = match &n.symbol {
            Some(s) => format!("{} headlines", tr_data::security(s).key()),
            None => "Top market headlines".into(),
        };
        ui.label(RichText::new(title).font(ui_font(12.5)).color(text()));
        if n.loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
        if n.symbol.is_some() && ui.small_button("Top News").on_hover_text("General market headlines").clicked() {
            n.symbol = None;
            n.items.clear();
            refresh = Some(true);
        }
        if ui.small_button("Refresh").clicked() {
            refresh = Some(true);
        }
    });
    ui.add_space(2.0);
    if let Some(e) = &n.error {
        ui.label(RichText::new(format!("Could not load news: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    if n.items.is_empty() && !n.loading {
        let msg = if offline {
            "No headlines in offline sample mode (TR_PROVIDER=sample)."
        } else {
            "No headlines for this security right now."
        };
        ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
    }
    let now = chrono::Utc::now().timestamp();
    let mut open: Option<tr_data::NewsItem> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let w = ui.available_width();
        let shown = n.items.iter().filter(|it| {
            q.is_empty() || it.title.to_ascii_lowercase().contains(q) || it.publisher.to_ascii_lowercase().contains(q)
        });
        for item in shown {
            let age = (now - item.ts).max(0);
            let when = if age < 3600 {
                format!("{}m", age / 60)
            } else if age < 86_400 {
                format!("{}h", age / 3600)
            } else {
                chrono::DateTime::from_timestamp(item.ts, 0).map(|d| d.format("%b %d").to_string()).unwrap_or_default()
            };
            let title = ui.painter().layout(item.title.clone(), ui_font(13.0), text(), (w - 70.0).max(80.0));
            let h = title.size().y + dp(20.0);
            let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
            widgets::hover_wash(ui, r, &resp, false);
            let p = ui.painter();
            p.text(pos2(r.left() + 6.0, r.top() + 4.0), Align2::LEFT_TOP, &when, mono(11.0), amber());
            let tpos = pos2(r.left() + 60.0, r.top() + 3.0);
            let th = title.size().y;
            p.galley(tpos, title, text());
            p.text(pos2(tpos.x, tpos.y + th + 2.0), Align2::LEFT_TOP, &item.publisher, ui_font(11.0), muted());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                open = Some(item.clone());
            }
            resp.on_hover_text("Click to read here · right-click for more").context_menu(|ui| {
                if ui.button("Read Here").clicked() {
                    open = Some(item.clone());
                    ui.close();
                }
                if ui.button("Open in Browser").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&item.link));
                    ui.close();
                }
                if ui.button("Copy Link").clicked() {
                    ui.ctx().copy_text(item.link.clone());
                    ui.close();
                }
            });
            p.hline(r.x_range(), r.bottom(), Stroke::new(1.0, theme::border()));
        }
    });
    if let Some(item) = open {
        n.read_requested = None;
        n.reading = Some(Reading { item, state: ReadState::Loading });
    }
    refresh
}

/// One article open in the News pane, with a Back button to the headline list.
fn reader(ui: &mut Ui, n: &mut super::NewsPane) {
    let mut back = false;
    let Some(r) = n.reading.as_ref() else { return };
    let link = r.item.link.clone();
    ui.horizontal(|ui| {
        if ui.button("◀ Back").clicked() {
            back = true;
        }
        if ui.small_button("Open in Browser").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&link));
        }
        if ui.small_button("Copy Link").clicked() {
            ui.ctx().copy_text(link.clone());
        }
        if matches!(r.state, ReadState::Loading) {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("loading article…").font(ui_font(11.5)).color(muted()));
        }
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical().id_salt(("news-reader", &link)).auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(ui.available_width() - 6.0);
        let item = &r.item;
        ui.add(egui::Label::new(RichText::new(&item.title).font(ui_font(17.0)).color(text())).wrap());
        let when = chrono::DateTime::from_timestamp(item.ts, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%b %d, %Y %H:%M").to_string())
            .unwrap_or_default();
        ui.label(RichText::new(format!("{} · {when}", item.publisher)).font(ui_font(11.5)).color(muted()));
        if !item.tickers.is_empty() {
            ui.label(RichText::new(item.tickers.join("  ")).font(mono(11.0)).color(amber()));
        }
        ui.add_space(8.0);
        match &r.state {
            ReadState::Loading => {}
            ReadState::Failed(e) => {
                ui.label(
                    RichText::new(format!("Could not read the article here: {e}"))
                        .font(ui_font(12.0))
                        .color(theme::down()),
                );
                ui.label(
                    RichText::new("Use Open in Browser to read it on the publisher's site.")
                        .font(ui_font(12.0))
                        .color(muted()),
                );
            }
            ReadState::Ready(a) => {
                for b in &a.blocks {
                    match b {
                        tr_data::article::Block::Heading(t) => {
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(RichText::new(t).font(ui_font(14.0)).color(text())).wrap());
                        }
                        tr_data::article::Block::Paragraph(t) => {
                            ui.add(egui::Label::new(RichText::new(t).font(ui_font(13.0)).color(text())).wrap());
                            ui.add_space(6.0);
                        }
                        tr_data::article::Block::Bullet(t) => {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new("•").font(ui_font(13.0)).color(muted()));
                                ui.add(egui::Label::new(RichText::new(t).font(ui_font(13.0)).color(text())).wrap());
                            });
                        }
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Source: {}", item.publisher)).font(ui_font(11.0)).color(muted()));
            }
        }
    });
    if back {
        n.reading = None;
        n.read_requested = None;
    }
}

/// Bucket sizes offered for a book around `mid`: raw plus three decades of the price scale.
pub fn group_steps(mid: f64) -> [f64; 4] {
    let base = 10f64.powi((mid.abs().max(1e-9).log10().floor() as i32) - 4);
    [0.0, base, base * 10.0, base * 100.0]
}

fn fmt_size(q: f64) -> String {
    if q >= 1e6 {
        format!("{:.2}M", q / 1e6)
    } else if q >= 1e3 {
        format!("{:.2}K", q / 1e3)
    } else if q >= 1.0 {
        format!("{q:.3}")
    } else {
        format!("{q:.5}")
    }
}

/// Price ladder: asks above, bids below, depth bars by cumulative size.
fn order_book(ui: &mut Ui, b: &mut super::BookPane) -> Option<Action> {
    let mut action = None;
    let key = tr_data::security(&b.symbol);
    let book: Option<OrderBook> = b.book.as_ref().map(|x| x.grouped(b.group));
    ui.horizontal_wrapped(|ui| {
        let r = ui.label(RichText::new(&key.ticker).font(mono(13.0)).color(amber())).interact(Sense::click());
        if r.clicked() {
            action = Some(Action::OpenChart(b.symbol.clone()));
        }
        r.on_hover_text("Open chart");
        if let Some(bk) = &book {
            ui.label(RichText::new(&bk.source).font(ui_font(11.5)).color(muted()));
            if bk.simulated {
                ui.label(
                    RichText::new(" SIMULATED ").font(ui_font(10.5)).color(Color32::BLACK).background_color(amber()),
                )
                .on_hover_text("No free public order book for this market (or offline); levels are synthetic.");
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        let depths = [10usize, 20, 50];
        let dl = ["10", "20", "50"];
        if let Some(i) =
            widgets::segmented(ui, ("book-depth", &b.symbol), &dl, depths.iter().position(|&x| x == b.depth), 20.0)
        {
            b.depth = depths[i];
            b.fetched = None;
        }
        let mid = b.book.as_ref().and_then(OrderBook::mid).unwrap_or(1.0);
        let steps = group_steps(mid);
        let d = key.decimals(mid);
        let labels: Vec<String> =
            steps.iter().map(|s| if *s == 0.0 { "Raw".into() } else { format!("{:.*}", d.min(8), s) }).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let sel = steps.iter().position(|s| (*s - b.group).abs() < 1e-12);
        if let Some(i) = widgets::segmented(ui, ("book-group", &b.symbol), &refs, sel, 20.0) {
            b.group = steps[i];
        }
    });
    if let Some(e) = &b.error {
        ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
    }
    let Some(book) = book else {
        ui.add(egui::Spinner::new());
        return action;
    };
    let rect = ui.available_rect_before_wrap();
    let d = key.decimals(book.mid().unwrap_or(1.0));
    let row = dp(18.0);
    let rows_each = (((rect.height() - row * 2.0) / row / 2.0).floor() as usize).max(1);
    let asks: Vec<_> = book.asks.iter().take(rows_each).copied().collect();
    let bids: Vec<_> = book.bids.iter().take(rows_each).copied().collect();
    let cum = |v: &[(f64, f64)]| {
        v.iter()
            .scan(0.0, |acc, &(p, q)| {
                *acc += q;
                Some((p, q, *acc))
            })
            .collect::<Vec<_>>()
    };
    let (asks, bids) = (cum(&asks), cum(&bids));
    let max_cum = asks.last().map_or(0.0, |x| x.2).max(bids.last().map_or(0.0, |x| x.2)).max(1e-12);
    let p = ui.painter_at(rect);
    let w = rect.width();
    // Columns sized from the widest price so narrow panes stay legible.
    let price_w =
        p.layout_no_wrap(format!("{:.*}", d, book.asks.last().map_or(1.0, |l| l.0)), mono(12.0), text()).size().x;
    let show_total = w > price_w + 170.0;
    let x_price = rect.left() + (price_w + 8.0).max(w * 0.3);
    let x_total = rect.right() - 6.0;
    let x_size = if show_total { (x_price + x_total) / 2.0 } else { x_total };
    let mut y = rect.top();
    for (label, x) in [("Price", x_price), ("Size", x_size), ("Total", x_total)] {
        if label != "Total" || show_total {
            p.text(pos2(x, y + row / 2.0), Align2::RIGHT_CENTER, label, ui_font(11.0), muted());
        }
    }
    y += row;
    let draw = |y: f64, level: (f64, f64, f64), up: bool| {
        let y = y as f32;
        let c = if up { theme::up() } else { theme::down() };
        let frac = (level.2 / max_cum) as f32;
        let bar = Rect::from_min_max(pos2(rect.right() - w * frac, y + 1.0), pos2(rect.right(), y + row - 1.0));
        p.rect_filled(bar, 2.0, c.gamma_multiply(0.14));
        p.text(pos2(x_price, y + row / 2.0), Align2::RIGHT_CENTER, format!("{:.*}", d, level.0), mono(12.0), c);
        p.text(pos2(x_size, y + row / 2.0), Align2::RIGHT_CENTER, fmt_size(level.1), mono(12.0), text());
        if show_total {
            p.text(pos2(x_total, y + row / 2.0), Align2::RIGHT_CENTER, fmt_size(level.2), mono(12.0), muted());
        }
    };
    // Asks: best ask nearest the spread row.
    for (k, level) in asks.iter().enumerate().rev() {
        let at = y as f64 + (asks.len() - 1 - k) as f64 * row as f64;
        draw(at, *level, false);
    }
    y += asks.len() as f32 * row;
    let spread_r = Rect::from_min_size(pos2(rect.left(), y), vec2(w, row));
    p.rect_filled(spread_r, 3.0, theme::header());
    if let (Some(mid), Some(spread)) = (book.mid(), book.spread()) {
        p.text(pos2(x_price, spread_r.center().y), Align2::RIGHT_CENTER, format!("{:.*}", d, mid), mono(12.5), text());
        let spread_text = if show_total {
            format!("spread {:.*} ({:.1} bp)", d, spread, spread / mid * 1e4)
        } else {
            format!("{:.1} bp", spread / mid * 1e4)
        };
        p.text(pos2(x_total, spread_r.center().y), Align2::RIGHT_CENTER, spread_text, ui_font(11.0), muted());
    }
    y += row;
    for (k, level) in bids.iter().enumerate() {
        draw((y + k as f32 * row) as f64, *level, true);
    }
    ui.allocate_rect(rect, Sense::hover());
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_steps_scale_with_price() {
        assert_eq!(group_steps(82_000.0), [0.0, 1.0, 10.0, 100.0]);
        let fx = group_steps(1.09);
        assert!((fx[1] - 1e-4).abs() < 1e-12);
        assert_eq!(fmt_size(1_500.0), "1.50K");
        assert_eq!(fmt_size(0.5), "0.50000");
    }
}
