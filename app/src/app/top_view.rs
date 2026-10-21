//! `TOP`: market headlines from several RSS feeds (World, Türkiye, Crypto, Central Banks), merged
//! newest-first. Headlines naming a watchlist symbol get a ticker chip; a click reads the story in
//! the pane (shared article reader) with Open in Browser as the fallback. Data layer:
//! [`tr_data::topnews`].

use std::time::{Duration, Instant};

use egui::{Align2, Color32, RichText, Sense, Stroke, Ui, pos2, vec2};
use tr_data::topnews::{self, FEEDS, Tab, TopItem};

use super::{PaneKind, ReadState, TerminalApp};
use crate::congress_view::Action;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::top::{TopReq, TopResp};
use crate::widgets;
use crate::worker::Request;

/// How often a visible pane re-fetches.
const REFRESH: Duration = Duration::from_secs(120);

/// Last good result and last error of one feed.
#[derive(Default)]
struct FeedState {
    items: Vec<TopItem>,
    error: Option<String>,
}

pub struct TopPane {
    pub tab: Tab,
    feeds: Vec<FeedState>,
    /// Every feed's rows merged, newest first and de-duplicated.
    merged: Vec<TopItem>,
    loading: bool,
    last_req: Option<Instant>,
    updated: Option<i64>,
    filter: String,
    reading: Option<TopItem>,
    read_state: Option<ReadState>,
    read_requested: Option<String>,
}

impl TopPane {
    pub fn new(tab: &str) -> Self {
        Self {
            tab: Tab::parse(tab),
            feeds: FEEDS.iter().map(|_| FeedState::default()).collect(),
            merged: Vec::new(),
            loading: false,
            last_req: None,
            updated: None,
            filter: String::new(),
            reading: None,
            read_state: None,
            read_requested: None,
        }
    }

    pub fn title(&self) -> String {
        match self.tab {
            Tab::All => "Top News".into(),
            t => format!("Top News · {}", t.label()),
        }
    }

    pub fn code(&self) -> &'static str {
        "TOP"
    }

    /// The tab for saved layouts.
    pub fn settings(&self) -> &'static str {
        self.tab.id()
    }

    fn apply(&mut self, results: Vec<Result<Vec<TopItem>, String>>) {
        for (st, r) in self.feeds.iter_mut().zip(results) {
            match r {
                Ok(items) => {
                    st.items = items;
                    st.error = None;
                }
                // Keep the feed's previous rows, but say it failed.
                Err(e) => st.error = Some(e),
            }
        }
        self.merged = topnews::merge(self.feeds.iter().flat_map(|f| f.items.iter().cloned()).collect());
        self.updated = Some(chrono::Utc::now().timestamp());
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `TOP`, `TOP TR`, `TOP CRYPTO`: focus (or open) the pane on that tab.
    pub(super) fn top_command(&mut self, arg: &str) {
        let tab = Tab::parse(arg);
        let id = self
            .idx(self.focus)
            .filter(|&i| matches!(self.panes[i].kind, PaneKind::Top(_)))
            .map(|i| self.panes[i].id)
            .or_else(|| self.panes.iter().find(|p| matches!(p.kind, PaneKind::Top(_))).map(|p| p.id));
        match id {
            Some(id) => {
                if let Some(PaneKind::Top(t)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    t.tab = tab;
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Top(Box::new(TopPane::new(arg)))),
        }
    }

    /// Fetch on first show and every two minutes while the pane is visible.
    pub(super) fn top_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Top(t) = &mut self.panes[i].kind else { continue };
            if !t.loading && t.last_req.is_none_or(|at| at.elapsed() >= REFRESH) {
                t.loading = true;
                t.last_req = Some(Instant::now());
                self.worker.send(Request::Top(TopReq::Load { pane: id }));
            }
            if let Some(item) = &t.reading
                && matches!(t.read_state, Some(ReadState::Loading))
                && t.read_requested.as_deref() != Some(item.item.link.as_str())
            {
                t.read_requested = Some(item.item.link.clone());
                self.worker.send(Request::Top(TopReq::Article { pane: id, url: item.item.link.clone() }));
            }
        }
    }

    pub(super) fn on_top(&mut self, resp: TopResp) {
        match resp {
            TopResp::Feeds { pane, feeds } => {
                let Some(PaneKind::Top(t)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
                t.loading = false;
                t.apply(feeds);
            }
            TopResp::Article { pane, url, article } => {
                let Some(PaneKind::Top(t)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
                if t.reading.as_ref().is_some_and(|r| r.item.link == url) {
                    t.read_state = Some(match article {
                        Ok(a) => ReadState::Ready(a),
                        Err(e) => ReadState::Failed(e),
                    });
                }
            }
        }
    }
}

// ---- watchlist matching -------------------------------------------------------------------

/// What a watchlist entry is called in headlines: its ticker (exact case) and, for crypto, the
/// coin's name (any case).
pub struct Watched {
    ticker: String,
    names: &'static [&'static str],
}

impl Watched {
    /// From a watchlist key such as `AAPL US Equity` or `BTCUSD Curncy`.
    pub fn from_key(key: &str) -> Option<Self> {
        let sec = tr_data::security(key);
        let mut ticker = sec.ticker.clone();
        if sec.class == tr_core::AssetClass::Crypto {
            ticker = ticker.strip_suffix("USDT").or_else(|| ticker.strip_suffix("USD")).unwrap_or(&ticker).to_string();
        }
        if ticker.chars().count() < 2 {
            return None;
        }
        let names: &'static [&'static str] = match ticker.as_str() {
            "BTC" => &["bitcoin"],
            "ETH" => &["ethereum", "ether"],
            "SOL" => &["solana"],
            "XRP" => &["ripple"],
            "DOGE" => &["dogecoin"],
            "ADA" => &["cardano"],
            _ => &[],
        };
        Some(Self { ticker, names })
    }
}

/// Watchlist tickers a headline mentions, in watchlist order (at most `max`).
fn mentions(title: &str, watch: &[Watched], max: usize) -> Vec<String> {
    let words: Vec<&str> = title.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let lower = title.to_lowercase();
    let lower_words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();
    for w in watch {
        let hit = words.contains(&w.ticker.as_str()) || w.names.iter().any(|n| lower_words.contains(n));
        if hit && !out.contains(&w.ticker) {
            out.push(w.ticker.clone());
            if out.len() == max {
                break;
            }
        }
    }
    out
}

fn matches_filter(it: &TopItem, f: &str) -> bool {
    f.is_empty() || it.item.title.to_lowercase().contains(f) || it.item.publisher.to_lowercase().contains(f)
}

/// `5m`, `3h`, or the date once it is a day old.
fn age_text(now: i64, ts: i64) -> String {
    if ts <= 0 {
        return "–".into();
    }
    let age = (now - ts).max(0);
    if age < 60 {
        "now".into()
    } else if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3600)
    } else {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%b %d").to_string())
            .unwrap_or_default()
    }
}

fn local_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string())
        .unwrap_or_default()
}

// ---- drawing ------------------------------------------------------------------------------

pub fn top(ui: &mut Ui, p: &mut TopPane, watch: &[Watched]) -> Vec<Action> {
    if p.reading.is_some() {
        reader(ui, p);
        return Vec::new();
    }
    let mut actions = Vec::new();
    ui.horizontal_wrapped(|ui| {
        for t in Tab::ALL {
            if ui.selectable_label(p.tab == t, RichText::new(t.label()).font(ui_font(12.5))).clicked() {
                p.tab = t;
            }
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut p.filter)
                .hint_text(RichText::new("Filter headlines…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(150.0)),
        );
        if ui.small_button("Refresh").on_hover_text("Fetch the feeds now").clicked() {
            p.last_req = None;
        }
        if p.loading {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
    });
    status_line(ui, p);
    ui.add_space(dp(2.0));

    let now = chrono::Utc::now().timestamp();
    let f = p.filter.trim().to_lowercase();
    let rows: Vec<&TopItem> = topnews::for_tab(&p.merged, p.tab).filter(|it| matches_filter(it, &f)).collect();
    if rows.is_empty() {
        let msg = if p.updated.is_none() {
            "loading…"
        } else if f.is_empty() {
            "No headlines yet."
        } else {
            "No headlines match the filter."
        };
        ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
        return actions;
    }
    let mut open: Option<TopItem> = None;
    egui::ScrollArea::vertical().id_salt("top-list").auto_shrink([false, false]).show(ui, |ui| {
        let w = ui.available_width();
        for it in rows {
            row(ui, w, now, it, watch, &mut open, &mut actions);
        }
    });
    if let Some(it) = open {
        p.read_requested = None;
        p.read_state = Some(ReadState::Loading);
        p.reading = Some(it);
    }
    actions
}

/// Feed health under the toolbar: when it was updated and which feeds failed.
fn status_line(ui: &mut Ui, p: &TopPane) {
    let mut s = String::new();
    if let Some(t) = p.updated.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
        s.push_str(&format!("updated {} · ", t.with_timezone(&chrono::Local).format("%H:%M:%S")));
    }
    s.push_str("refreshes every 2 min");
    ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
    for (feed, st) in FEEDS.iter().zip(&p.feeds) {
        if let Some(e) = &st.error
            && (p.tab == Tab::All || p.tab == feed.tab)
        {
            let kept = if st.items.is_empty() { "" } else { " (showing the last good copy)" };
            ui.label(
                RichText::new(format!("{}: {e}{kept}", feed.name)).font(ui_font(11.5)).color(theme::down()),
            );
        }
    }
}

fn row(
    ui: &mut Ui,
    w: f32,
    now: i64,
    it: &TopItem,
    watch: &[Watched],
    open: &mut Option<TopItem>,
    actions: &mut Vec<Action>,
) {
    let hits = mentions(&it.item.title, watch, 3);
    let title_color = if hits.is_empty() { text() } else { amber() };
    let title = ui.painter().layout(it.item.title.clone(), ui_font(13.0), title_color, (w - dp(70.0)).max(80.0));
    let th = title.size().y;
    let h = th + dp(20.0);
    let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    widgets::hover_wash(ui, r, &resp, false);
    let p = ui.painter();
    p.text(pos2(r.left() + 6.0, r.top() + 4.0), Align2::LEFT_TOP, age_text(now, it.item.ts), mono(11.0), amber());
    let tpos = pos2(r.left() + dp(60.0), r.top() + 3.0);
    p.galley(tpos, title, title_color);
    let sub = format!("{} · {}", it.item.publisher, local_time(it.item.ts));
    let sub_rect = p.text(pos2(tpos.x, tpos.y + th + 2.0), Align2::LEFT_TOP, sub, ui_font(11.0), muted());
    // Ticker chips after the source line; each opens that symbol's chart.
    let mut x = sub_rect.right() + dp(8.0);
    for sym in &hits {
        let cw = dp(10.0) + sym.chars().count() as f32 * dp(7.5);
        let chip = egui::Rect::from_min_size(pos2(x, sub_rect.top() - 1.0), vec2(cw, dp(14.0)));
        let cr = ui.interact(chip, resp.id.with(sym), Sense::click());
        let fill = if cr.hovered() { amber().gamma_multiply(0.85) } else { amber() };
        ui.painter().rect_filled(chip, 3.0, fill);
        ui.painter().text(chip.center(), Align2::CENTER_CENTER, sym, mono(10.5), Color32::BLACK);
        if cr.on_hover_text(format!("Open {sym} chart")).clicked() {
            actions.push(Action::OpenChart(sym.clone()));
        }
        x += cw + dp(4.0);
    }
    ui.painter().hline(r.x_range(), r.bottom(), Stroke::new(1.0, theme::border()));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *open = Some(it.clone());
    }
    resp.on_hover_text("Click to read here · right-click for more").context_menu(|ui| {
        if ui.button("Read Here").clicked() {
            *open = Some(it.clone());
            ui.close();
        }
        if ui.button("Open in Browser").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&it.item.link));
            ui.close();
        }
        if ui.button("Copy Link").clicked() {
            ui.ctx().copy_text(it.item.link.clone());
            ui.close();
        }
    });
}

/// One article open in the pane, with a Back button to the headline list.
fn reader(ui: &mut Ui, p: &mut TopPane) {
    let Some(it) = p.reading.clone() else { return };
    let link = it.item.link.clone();
    let mut back = false;
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
        if matches!(p.read_state, Some(ReadState::Loading)) {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("loading article…").font(ui_font(11.5)).color(muted()));
        }
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical().id_salt(("top-reader", &link)).auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(ui.available_width() - 6.0);
        ui.add(egui::Label::new(RichText::new(&it.item.title).font(ui_font(17.0)).color(text())).wrap());
        ui.label(
            RichText::new(format!("{} · {}", it.item.publisher, local_time(it.item.ts)))
                .font(ui_font(11.5))
                .color(muted()),
        );
        ui.add_space(8.0);
        match &p.read_state {
            None | Some(ReadState::Loading) => {}
            Some(ReadState::Failed(e)) => {
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
            Some(ReadState::Ready(a)) => {
                use tr_data::article::Block;
                for b in &a.blocks {
                    match b {
                        Block::Heading(t) => {
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(RichText::new(t).font(ui_font(14.0)).color(text())).wrap());
                        }
                        Block::Paragraph(t) => {
                            ui.add(egui::Label::new(RichText::new(t).font(ui_font(13.0)).color(text())).wrap());
                            ui.add_space(6.0);
                        }
                        Block::Bullet(t) => {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new("•").font(ui_font(13.0)).color(muted()));
                                ui.add(egui::Label::new(RichText::new(t).font(ui_font(13.0)).color(text())).wrap());
                            });
                        }
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Source: {}", it.item.publisher)).font(ui_font(11.0)).color(muted()));
            }
        }
    });
    if back {
        p.reading = None;
        p.read_state = None;
        p.read_requested = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(keys: &[&str]) -> Vec<Watched> {
        keys.iter().filter_map(|k| Watched::from_key(k)).collect()
    }

    #[test]
    fn headline_mentions_watchlist_symbols() {
        let watch = w(&["AAPL US Equity", "TSLA US Equity", "BTCUSD Curncy", "THYAO.IS"]);
        assert_eq!(mentions("Tesla (TSLA) beats; AAPL's chip deal", &watch, 3), ["AAPL", "TSLA"]);
        assert_eq!(mentions("Bitcoin rallies past $120k", &watch, 3), ["BTC"]);
        assert!(mentions("Aapl lowercase and ATSLA do not count", &watch, 3).is_empty());
        assert_eq!(mentions("AAPL TSLA BTC", &watch, 2).len(), 2);
    }

    #[test]
    fn relative_ages() {
        assert_eq!(age_text(1000, 990), "now");
        assert_eq!(age_text(1000, 700), "5m");
        assert_eq!(age_text(10_000, 10_000 - 7300), "2h");
        assert_eq!(age_text(1000, 0), "–");
    }

    #[test]
    fn pane_tabs_and_failed_feed_keeps_old_rows() {
        assert_eq!(TopPane::new("tr").settings(), "tr");
        assert_eq!(TopPane::new("").tab, Tab::All);
        let xml = include_str!("../../../crates/data/tests/fixtures/cnbc_rss.xml");
        let rows = topnews::parse_feed(xml, &FEEDS[0]).unwrap();
        let mut p = TopPane::new("");
        let mut res: Vec<Result<Vec<TopItem>, String>> = FEEDS.iter().map(|_| Ok(Vec::new())).collect();
        res[0] = Ok(rows);
        p.apply(res);
        assert_eq!(p.merged.len(), 5);
        let mut res: Vec<Result<Vec<TopItem>, String>> = FEEDS.iter().map(|_| Ok(Vec::new())).collect();
        res[0] = Err("HTTP 503".into());
        p.apply(res);
        assert_eq!(p.merged.len(), 5);
        assert!(p.feeds[0].error.is_some());
    }
}
