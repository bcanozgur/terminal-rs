//! `ERN` panel: a weekly earnings calendar grouped by day, and per-ticker earnings history
//! (`AAPL ERN`) with report dates, EPS, estimate, surprise and the price reaction. Also the
//! per-chart earnings state behind the "E" markers.

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use chrono::NaiveDate;
use egui::{Align2, Color32, RichText, ScrollArea, Sense, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};
use tr_data::earnings::{CalendarRow, EarningsEvent, NextEarnings, TickerEarnings, Timing, week_days};

use crate::congress_view::Action;
use crate::earnings_worker::EarnReq;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::PaneId;

/// Whose earnings the calendar lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Scope {
    #[default]
    All,
    /// The watchlist, sidebar lists and open positions.
    Mine,
}

/// Market-cap floors for the "all" view, in billions of dollars.
pub const CAP_FLOORS: [f64; 4] = [0.0, 1.0, 10.0, 100.0];
const CAP_LABELS: [&str; 4] = ["Any cap", "> $1B", "> $10B", "> $100B"];
/// Rows drawn per day (the rest are summarised).
const DAY_ROWS: usize = 120;
/// Seconds between refreshes of a visible pane.
const REFRESH_SECS: u64 = 900;

/// The ERN pane: the weekly calendar (`symbol == None`) or one ticker's history.
pub struct ErnPane {
    pub symbol: Option<String>,
    /// Weeks from the current one (calendar mode).
    pub week: i32,
    pub scope: Scope,
    /// Index into [`CAP_FLOORS`].
    pub cap: usize,
    days: BTreeMap<NaiveDate, Vec<CalendarRow>>,
    data: Option<TickerEarnings>,
    error: Option<String>,
    loading: bool,
    fetched: Option<Instant>,
}

impl ErnPane {
    pub fn calendar(scope: Scope, cap: usize) -> Self {
        Self {
            symbol: None,
            week: 0,
            scope,
            cap: cap.min(CAP_FLOORS.len() - 1),
            days: BTreeMap::new(),
            data: None,
            error: None,
            loading: true,
            fetched: None,
        }
    }

    pub fn ticker(symbol: &str) -> Self {
        Self { symbol: Some(symbol.into()), ..Self::calendar(Scope::All, 1) }
    }

    pub fn monday(&self, today: NaiveDate) -> NaiveDate {
        week_days(today)[0] + chrono::Duration::weeks(self.week as i64)
    }

    /// The request this pane needs now, if it is due (stamping the fetch time).
    pub fn due(&mut self, id: PaneId, today: NaiveDate) -> Option<EarnReq> {
        if self.fetched.is_some_and(|t| t.elapsed().as_secs() < REFRESH_SECS) {
            return None;
        }
        self.fetched = Some(Instant::now());
        Some(match &self.symbol {
            Some(s) => EarnReq::Ticker { pane: id, symbol: s.clone() },
            None => EarnReq::Week { pane: id, monday: self.monday(today) },
        })
    }

    pub fn refetch(&mut self) {
        self.fetched = None;
        self.loading = true;
    }

    pub fn on_day(
        &mut self,
        monday: NaiveDate,
        today: NaiveDate,
        day: NaiveDate,
        rows: Result<Vec<CalendarRow>, String>,
    ) {
        if monday != self.monday(today) || self.symbol.is_some() {
            return;
        }
        match rows {
            Ok(r) => {
                self.days.insert(day, r);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.loading = week_days(monday).iter().any(|d| !self.days.contains_key(d)) && self.error.is_none();
    }

    pub fn on_ticker(&mut self, symbol: &str, data: Result<Box<TickerEarnings>, String>) {
        if self.symbol.as_deref().is_none_or(|s| tr_data::security(s).key() != tr_data::security(symbol).key()) {
            return;
        }
        match data {
            Ok(d) => {
                self.data = Some(*d);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.loading = false;
    }

    /// Point a ticker pane at another security.
    pub fn set_symbol(&mut self, symbol: &str) {
        self.symbol = Some(symbol.into());
        self.data = None;
        self.error = None;
        self.refetch();
    }

    fn go_week(&mut self, week: i32) {
        self.week = week;
        self.days.clear();
        self.error = None;
        self.refetch();
    }
}

/// App-level earnings state (toasts already shown).
#[derive(Default)]
pub struct EarningsState {
    pub toasted: HashSet<(NaiveDate, String)>,
    pub last_check: Option<Instant>,
}

/// Chart-side earnings data behind the "E" markers.
pub struct ChartEarnings {
    pub show: bool,
    pub symbol: String,
    pub events: Vec<EarningsEvent>,
    pub next: Option<NextEarnings>,
}

impl Default for ChartEarnings {
    fn default() -> Self {
        Self { show: true, symbol: String::new(), events: Vec::new(), next: None }
    }
}

impl ChartEarnings {
    pub fn clear(&mut self) {
        self.symbol.clear();
        self.events.clear();
        self.next = None;
    }

    pub fn set(&mut self, symbol: &str, data: TickerEarnings) {
        self.symbol = symbol.into();
        self.events = data.events;
        self.next = data.next;
    }
}

/// What the view needs from the app.
pub struct ViewCtx<'a> {
    pub today: NaiveDate,
    /// Upper-case tickers on the watchlist, lists and positions.
    pub mine: &'a HashSet<String>,
    pub query: &'a str,
}

// ---- formatting -------------------------------------------------------------------------

pub fn fmt_cap(v: f64) -> String {
    match v {
        v if v >= 1e12 => format!("${:.2}T", v / 1e12),
        v if v >= 1e9 => format!("${:.1}B", v / 1e9),
        v if v >= 1e6 => format!("${:.0}M", v / 1e6),
        v => format!("${v:.0}"),
    }
}

pub fn fmt_eps(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |v| if v < 0.0 { format!("-${:.2}", -v) } else { format!("${v:.2}") })
}

fn fmt_pct(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |v| format!("{v:+.1}%"))
}

fn signed_color(v: Option<f64>) -> Color32 {
    match v {
        Some(v) if v > 0.0 => theme::up(),
        Some(v) if v < 0.0 => theme::down(),
        _ => muted(),
    }
}

fn timing_color(t: Timing) -> Color32 {
    match t {
        Timing::PreMarket => theme::cyan(),
        Timing::AfterHours => amber(),
        Timing::Unknown => muted(),
    }
}

// ---- pane -------------------------------------------------------------------------------

/// Draw the pane. Returns app actions.
pub fn pane(ui: &mut Ui, p: &mut ErnPane, cx: &ViewCtx) -> Vec<Action> {
    let mut out = Vec::new();
    match p.symbol.clone() {
        Some(s) => history(ui, p, &s, cx, &mut out),
        None => calendar(ui, p, cx, &mut out),
    }
    out
}

fn status_line(ui: &mut Ui, p: &ErnPane) -> bool {
    if let Some(e) = &p.error {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Earnings data unavailable: {e}")).color(theme::down()));
            if ui.small_button("Retry").clicked() {
                return true;
            }
            false
        })
        .inner
    } else {
        false
    }
}

fn calendar(ui: &mut Ui, p: &mut ErnPane, cx: &ViewCtx, out: &mut Vec<Action>) {
    let h = dp(20.0);
    let monday = p.monday(cx.today);
    let mut retry = false;
    ui.horizontal(|ui| {
        let nav = widgets::segmented(ui, "ern-week", &["‹ Prev", "This week", "Next ›"], (p.week == 0).then_some(1), h);
        match nav {
            Some(0) => p.go_week(p.week - 1),
            Some(1) => p.go_week(0),
            Some(2) => p.go_week(p.week + 1),
            _ => {}
        }
        ui.label(
            RichText::new(format!(
                "{} – {}",
                monday.format("%b %-d"),
                (monday + chrono::Duration::days(4)).format("%b %-d, %Y")
            ))
            .font(mono(12.0))
            .color(text()),
        );
        ui.add_space(dp(8.0));
        if let Some(i) = widgets::segmented(ui, "ern-scope", &["All", "Watchlist"], Some(p.scope as usize), h) {
            p.scope = if i == 0 { Scope::All } else { Scope::Mine };
        }
        if p.scope == Scope::All
            && let Some(i) = widgets::segmented(ui, "ern-cap", &CAP_LABELS, Some(p.cap), h)
        {
            p.cap = i;
        }
        if p.loading {
            ui.label(RichText::new("loading…").color(muted()));
        }
    });
    retry |= status_line(ui, p);
    if retry {
        p.refetch();
    }
    ui.add_space(4.0);

    let floor = CAP_FLOORS[p.cap] * 1e9;
    let q = cx.query;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for day in week_days(monday) {
            let Some(rows) = p.days.get(&day) else { continue };
            let mut shown: Vec<&CalendarRow> = rows
                .iter()
                .filter(|r| match p.scope {
                    Scope::All => r.market_cap.unwrap_or(0.0) >= floor,
                    Scope::Mine => cx.mine.contains(&r.symbol.to_ascii_uppercase()),
                })
                .filter(|r| {
                    q.is_empty()
                        || r.symbol.to_ascii_lowercase().contains(q)
                        || r.company.to_ascii_lowercase().contains(q)
                })
                .collect();
            shown.sort_by(|a, b| b.market_cap.unwrap_or(0.0).total_cmp(&a.market_cap.unwrap_or(0.0)));
            let today_tag = if day == cx.today { "  · today" } else { "" };
            ui.add_space(dp(6.0));
            ui.label(
                RichText::new(format!(
                    "{}  ·  {} report{}{today_tag}",
                    day.format("%A %b %-d"),
                    shown.len(),
                    if shown.len() == 1 { "" } else { "s" }
                ))
                .font(ui_font(13.0))
                .color(if day == cx.today { amber() } else { text() })
                .strong(),
            );
            if shown.is_empty() {
                ui.label(
                    RichText::new(if rows.is_empty() { "No reports" } else { "Nothing matches the filter" })
                        .color(muted()),
                );
                continue;
            }
            header_row(ui, &["Sym", "Time", "Company", "EPS est", "Prior / actual", "Surprise", "Mkt cap"]);
            for r in shown.iter().take(DAY_ROWS) {
                calendar_row(ui, r, out);
            }
            if shown.len() > DAY_ROWS {
                ui.label(
                    RichText::new(format!("+{} more (raise the cap filter)", shown.len() - DAY_ROWS)).color(muted()),
                );
            }
        }
    });
}

/// Column left edges as fractions of the row width, and which are right-aligned.
const COLS: [(f32, bool); 7] =
    [(0.0, false), (0.075, false), (0.14, false), (0.62, true), (0.76, true), (0.86, true), (1.0, true)];

fn header_row(ui: &mut Ui, labels: &[&str; 7]) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), dp(16.0)), Sense::hover());
    let p = ui.painter_at(rect);
    for (l, (f, right)) in labels.iter().zip(COLS) {
        let x = rect.left() + rect.width() * f;
        p.text(
            pos2(x + if right { 0.0 } else { 2.0 }, rect.center().y),
            if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER },
            *l,
            mono(10.5),
            muted(),
        );
    }
    p.hline(rect.x_range(), rect.bottom(), egui::Stroke::new(1.0, theme::control()));
}

fn calendar_row(ui: &mut Ui, r: &CalendarRow, out: &mut Vec<Action>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), dp(16.0)), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, theme::control());
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let key = tr_data::security(&r.symbol).key();
    if resp.clicked() {
        out.push(Action::OpenChart(key.clone()));
    }
    resp.context_menu(|ui| {
        if ui.button("Open Chart").clicked() {
            out.push(Action::OpenChart(key.clone()));
            ui.close();
        }
        if ui.button(format!("{} Earnings History", r.symbol)).clicked() {
            out.push(Action::Command(format!("{} ERN", r.symbol)));
            ui.close();
        }
        if ui.button("Add to Watchlist").clicked() {
            out.push(Action::AddToWatchlist(key.clone()));
            ui.close();
        }
    });
    let p = ui.painter_at(rect);
    let x = |i: usize| rect.left() + rect.width() * COLS[i].0 + if COLS[i].1 { 0.0 } else { 2.0 };
    let y = rect.center().y;
    p.text(pos2(x(0), y), Align2::LEFT_CENTER, &r.symbol, mono(12.0), amber());
    p.text(pos2(x(1), y), Align2::LEFT_CENTER, r.timing.label(), mono(11.0), timing_color(r.timing));
    let name_clip = egui::Rect::from_min_max(pos2(x(2), rect.top()), pos2(x(3) - dp(70.0), rect.bottom()));
    ui.painter_at(name_clip).text(pos2(x(2), y), Align2::LEFT_CENTER, &r.company, ui_font(12.0), text());
    p.text(pos2(x(3), y), Align2::RIGHT_CENTER, fmt_eps(r.eps_estimate), mono(12.0), text());
    match r.eps_actual {
        Some(a) => {
            p.text(pos2(x(4), y), Align2::RIGHT_CENTER, fmt_eps(Some(a)), mono(12.0), text());
            p.text(
                pos2(x(5), y),
                Align2::RIGHT_CENTER,
                fmt_pct(r.surprise_pct),
                mono(12.0),
                signed_color(r.surprise_pct),
            );
        }
        None => {
            p.text(pos2(x(4), y), Align2::RIGHT_CENTER, fmt_eps(r.eps_last_year), mono(12.0), muted());
            p.text(pos2(x(5), y), Align2::RIGHT_CENTER, "—", mono(12.0), muted());
        }
    }
    p.text(pos2(x(6), y), Align2::RIGHT_CENTER, r.market_cap.map_or("—".into(), fmt_cap), mono(12.0), muted());
}

fn history(ui: &mut Ui, p: &mut ErnPane, symbol: &str, cx: &ViewCtx, out: &mut Vec<Action>) {
    let sec = tr_data::security(symbol);
    ui.horizontal(|ui| {
        ui.label(RichText::new(sec.key()).font(ui_font(14.0)).color(amber()).strong());
        if p.loading {
            ui.label(RichText::new("loading…").color(muted()));
        }
        if ui.small_button("Chart").clicked() {
            out.push(Action::OpenChart(sec.key()));
        }
    });
    if status_line(ui, p) {
        p.refetch();
    }
    let Some(data) = &p.data else {
        if p.error.is_none() && !p.loading {
            ui.label(RichText::new("No earnings history").color(muted()));
        }
        return;
    };
    if let Some(n) = &data.next {
        let days = (n.date - cx.today).num_days();
        let when = match days {
            0 => "today".to_string(),
            1 => "tomorrow".to_string(),
            d if d > 1 => format!("in {d} days"),
            _ => format!("{} days ago", -days),
        };
        ui.label(
            RichText::new(format!(
                "Next report  {}  ({when}){}{}{}",
                n.date.format("%a %Y-%m-%d"),
                if n.projected { "  · projected from cadence, not yet announced" } else { "" },
                if n.timing == Timing::Unknown { String::new() } else { format!("  · {}", n.timing.label()) },
                n.eps_estimate.map_or(String::new(), |e| format!("  · est EPS {}", fmt_eps(Some(e)))),
            ))
            .font(mono(12.0))
            .color(text()),
        );
    }
    ui.add_space(6.0);
    let cols: [(f32, bool); 8] = [
        (0.0, false),
        (0.15, false),
        (0.27, false),
        (0.45, true),
        (0.60, true),
        (0.75, true),
        (0.88, true),
        (1.0, true),
    ];
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), dp(16.0)), Sense::hover());
    let pt = ui.painter_at(rect);
    let place = |rect: egui::Rect, i: usize| rect.left() + rect.width() * cols[i].0 + if cols[i].1 { 0.0 } else { 2.0 };
    for (i, l) in ["Reported", "Qtr", "Time", "EPS (SEC)", "EPS adj", "Estimate", "Surprise", "Move"].iter().enumerate()
    {
        pt.text(
            pos2(place(rect, i), rect.center().y),
            if cols[i].1 { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER },
            *l,
            mono(10.5),
            muted(),
        );
    }
    pt.hline(rect.x_range(), rect.bottom(), egui::Stroke::new(1.0, theme::control()));
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for e in &data.events {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), dp(17.0)), Sense::hover());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 3.0, theme::control());
            }
            let pt = ui.painter_at(rect);
            let y = rect.center().y;
            let put = |i: usize, s: String, c: Color32| {
                pt.text(pos2(place(rect, i), y), if cols[i].1 { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER }, s, mono(12.0), c);
            };
            put(0, e.date.format("%Y-%m-%d").to_string(), text());
            put(1, e.fiscal_quarter.clone(), muted());
            put(2, e.timing.label().into(), timing_color(e.timing));
            put(3, fmt_eps(e.eps_gaap), text());
            put(4, fmt_eps(e.eps_actual), text());
            put(5, fmt_eps(e.eps_estimate), muted());
            put(6, fmt_pct(e.surprise_pct), signed_color(e.surprise_pct));
            put(7, fmt_pct(e.move_pct), signed_color(e.move_pct));
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "EPS (SEC): diluted EPS as filed (EDGAR XBRL; Q4 is the year less nine months).  EPS adj / Estimate / Surprise: Nasdaq consensus.  Move: close-to-close around the report.",
            )
            .font(ui_font(10.5))
            .color(muted()),
        );
    });
}
