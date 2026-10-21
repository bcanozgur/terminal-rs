//! Economic calendar pane: `ECO` week view (US FRED releases plus other countries' events from
//! ForexFactory) and the `ECOF` release history.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate};
use egui::{Align2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use tr_data::eco::{self, DateSource, EcoRow, EcoSeries, EcoWeek, GlobalRow, RowState};

use super::{PaneKind, TerminalApp};
use crate::eco::{EcoRequest, EcoResponse};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{PaneId, Request};

pub struct EcoPane {
    /// Weeks from the current one.
    pub week: i32,
    /// Lowest importance shown (1-3).
    pub min_importance: u8,
    pub data: Option<Box<EcoWeek>>,
    pub loading: bool,
    /// `None` = fetch on the next tick.
    pub fetched: Option<Instant>,
    /// Showing the `ECOF` history of this release.
    pub series_code: Option<String>,
    pub series: Option<Result<Box<EcoSeries>, String>>,
    series_requested: bool,
    /// Years of history shown in the series view (0 = all).
    pub years: u32,
    /// Countries shown (`eco::COUNTRIES` codes); empty = all.
    pub countries: BTreeSet<String>,
    /// Releases already announced by a toast.
    alerted: HashSet<String>,
}

impl EcoPane {
    pub fn new(min_importance: u8, series: Option<String>) -> Self {
        Self {
            week: 0,
            min_importance: min_importance.clamp(1, 3),
            data: None,
            loading: true,
            fetched: None,
            series_code: series.and_then(|s| eco::event(&s)).map(|e| e.code.to_string()),
            series: None,
            series_requested: false,
            years: 10,
            countries: BTreeSet::new(),
            alerted: HashSet::new(),
        }
    }

    pub fn with_countries(mut self, countries: &[String]) -> Self {
        self.countries = countries.iter().filter_map(|c| eco::parse_country(c)).map(str::to_string).collect();
        self
    }

    /// Whether events of `country` are shown.
    fn shows(&self, country: &str) -> bool {
        self.countries.is_empty() || self.countries.contains(country)
    }

    /// The Monday of the shown week (weeks start on the US Eastern date).
    pub fn monday(&self) -> NaiveDate {
        eco::monday(today_et()) + chrono::Duration::weeks(self.week as i64)
    }

    fn set_week(&mut self, week: i32) {
        if week != self.week {
            self.week = week;
            self.data = None;
            self.fetched = None;
            self.loading = true;
        }
    }

    fn show_series(&mut self, code: Option<&str>) {
        let code = code.and_then(eco::event).map(|e| e.code.to_string());
        if code != self.series_code {
            self.series = None;
            self.series_requested = false;
        }
        self.series_code = code;
    }

    pub fn title(&self) -> String {
        match self.series_code.as_deref().and_then(eco::event) {
            Some(e) => format!("{} History", e.name),
            None => "Economic Calendar".into(),
        }
    }

    pub fn code(&self) -> &'static str {
        if self.series_code.is_some() { "ECOF" } else { "ECO" }
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Today's date in New York.
fn today_et() -> NaiveDate {
    let utc = chrono::Utc::now();
    let hours = if eco::us_dst(utc.date_naive()) { 4 } else { 5 };
    (utc - chrono::Duration::hours(hours)).date_naive()
}

fn local_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

fn importance_bars(p: &egui::Painter, at: Pos2, importance: u8) {
    for i in 0..3u8 {
        let h = dp(4.0) + dp(3.0) * i as f32;
        let r = Rect::from_min_size(pos2(at.x + i as f32 * dp(5.0), at.y - h), vec2(dp(3.5), h));
        let c = if i < importance { amber() } else { theme::control() };
        p.rect_filled(r, 1.0, c);
    }
}

/// Draw the pane.
pub fn eco(ui: &mut Ui, pane: &mut EcoPane, q: &str) {
    if pane.series_code.is_some() {
        series_view(ui, pane);
    } else {
        calendar(ui, pane, q);
    }
}

fn calendar(ui: &mut Ui, pane: &mut EcoPane, q: &str) {
    let monday = pane.monday();
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("◀ Prev").on_hover_text("ECO PREV").clicked() {
            pane.set_week(pane.week - 1);
        }
        if ui.small_button("This Week").clicked() {
            pane.set_week(0);
        }
        if ui.small_button("Next ▶").on_hover_text("ECO NEXT").clicked() {
            pane.set_week(pane.week + 1);
        }
        let sunday = monday + chrono::Days::new(6);
        ui.label(
            RichText::new(format!("Week of {} – {}", monday.format("%b %d"), sunday.format("%b %d, %Y")))
                .font(ui_font(12.5))
                .color(text()),
        );
        if pane.loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
        ui.add_space(dp(8.0));
        ui.label(RichText::new("Importance").font(ui_font(11.5)).color(muted()));
        let sel = Some(pane.min_importance.clamp(1, 3) as usize - 1);
        if let Some(i) = widgets::segmented(ui, "eco-imp", &["All", "●●+", "●●●"], sel, 20.0) {
            pane.min_importance = i as u8 + 1;
        }
        if ui.small_button("Refresh").clicked() {
            pane.fetched = None;
            pane.loading = true;
        }
    });
    let week = pane.data.as_deref().filter(|w| w.monday == monday);
    country_chips(ui, &mut pane.countries, week);
    ui.add_space(2.0);
    let Some(week) = week else {
        ui.label(RichText::new("Loading releases (FRED, ForexFactory)…").font(ui_font(12.0)).color(muted()));
        return;
    };
    let now = now_ts();
    let us = pane.shows("US");
    let rows: Vec<Item> = merge(
        week.rows
            .iter()
            .filter(|_| us)
            .filter(|r| r.event().importance >= pane.min_importance)
            .filter(|r| {
                q.is_empty()
                    || r.event().name.to_ascii_lowercase().contains(q)
                    || r.code.to_ascii_lowercase().contains(q)
            }),
        week.global
            .iter()
            .filter(|g| pane.shows(&g.country))
            // Holidays count as the lowest importance, so only "High"/"Med" hide them.
            .filter(|g| g.importance().max(1) >= pane.min_importance)
            .filter(|g| {
                q.is_empty() || g.title.to_ascii_lowercase().contains(q) || g.country.to_ascii_lowercase().contains(q)
            }),
    );
    if !week.global_covered {
        ui.label(
            RichText::new(
                "No world events for this week: ForexFactory publishes only this week (and next or last week on \
                 weekends). Showing US releases from FRED.",
            )
            .font(ui_font(11.0))
            .color(amber()),
        );
    }
    let mut open = None;
    let footer_h = dp(56.0);
    // Below this width the right-aligned number columns run into the event names, so a
    // narrow pane scrolls sideways instead.
    egui::ScrollArea::both()
        .id_salt("eco-rows")
        .max_height((ui.available_height() - footer_h).max(dp(80.0)))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let w = ui.available_width().max(dp(560.0));
            let cols = Columns::new(w);
            header(ui, &cols);
            if rows.is_empty() {
                let msg = match week.source {
                    DateSource::Unavailable => "No scheduled release dates (see the note below).",
                    _ => "No releases for these countries and this importance this week.",
                };
                ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
            }
            let mut day = None;
            for item in rows {
                if day != Some(item.date()) {
                    day = Some(item.date());
                    ui.add_space(dp(3.0));
                    let label = item.date().format("%A %b %d").to_string();
                    let today = item.date() == today_et();
                    ui.label(
                        RichText::new(if today { format!("{label} · today") } else { label })
                            .font(ui_font(11.5))
                            .color(if today { amber() } else { muted() }),
                    );
                }
                match item {
                    Item::Us(r) => {
                        if row(ui, &cols, r, now) {
                            open = Some(r.code);
                        }
                    }
                    Item::Global(g) => global_row(ui, &cols, g, now),
                }
            }
            if !week.latest.is_empty() && us {
                ui.add_space(dp(8.0));
                ui.label(RichText::new("Latest published values").font(ui_font(12.0)).color(text()));
                for l in week
                    .latest
                    .iter()
                    .filter(|l| eco::event(l.code).is_some_and(|e| e.importance >= pane.min_importance))
                {
                    let r = EcoRow {
                        code: l.code,
                        date: l.period,
                        et: (0, 0),
                        ts: 0,
                        period: Some(l.period),
                        actual: Some(l.actual),
                        prior: l.prior,
                        state: RowState::Released,
                        consensus: None,
                    };
                    if latest_row(ui, &cols, &r) {
                        open = Some(l.code);
                    }
                }
            }
        });
    footer(ui, week);
    if let Some(code) = open {
        pane.show_series(Some(code));
    }
}

/// Column x offsets for the week table.
struct Columns {
    w: f32,
    et: f32,
    local: f32,
    ctry: f32,
    event: f32,
    period: f32,
    cons: f32,
    actual: f32,
    prior: f32,
    change: f32,
    imp: f32,
}

impl Columns {
    fn new(w: f32) -> Self {
        let num = dp(64.0);
        let imp = w - dp(22.0);
        let change = imp - dp(10.0);
        let prior = change - num;
        let actual = prior - num;
        let cons = actual - num;
        let period = cons - num - dp(6.0);
        Self { w, et: dp(6.0), local: dp(52.0), ctry: dp(100.0), event: dp(134.0), period, cons, actual, prior, change, imp }
    }
}

fn header(ui: &mut Ui, c: &Columns) {
    let (r, _) = ui.allocate_exact_size(vec2(c.w, dp(18.0)), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, 2.0, theme::header());
    let y = r.center().y;
    let f = mono(10.5);
    let t = |x: f32, align: Align2, s: &str| {
        p.text(pos2(r.left() + x, y), align, s, f.clone(), amber());
    };
    t(c.et, Align2::LEFT_CENTER, "ET");
    t(c.local, Align2::LEFT_CENTER, "LOCAL");
    t(c.ctry, Align2::LEFT_CENTER, "CTRY");
    t(c.event, Align2::LEFT_CENTER, "EVENT");
    t(c.period, Align2::RIGHT_CENTER, "PERIOD");
    t(c.cons, Align2::RIGHT_CENTER, "CONS");
    t(c.actual, Align2::RIGHT_CENTER, "ACTUAL");
    t(c.prior, Align2::RIGHT_CENTER, "PRIOR");
    t(c.change, Align2::RIGHT_CENTER, "CHG");
    t(c.imp - dp(2.0), Align2::LEFT_CENTER, "");
}

/// One release row. Returns true when clicked (open its history).
fn row(ui: &mut Ui, c: &Columns, r: &EcoRow, now: i64) -> bool {
    let e = r.event();
    let (rect, resp) = ui.allocate_exact_size(vec2(c.w, dp(20.0)), Sense::click());
    let soon = r.state == RowState::Upcoming && r.ts - now <= 24 * 3600;
    if soon {
        ui.painter().rect_filled(rect, 2.0, theme::amber_wash());
    }
    widgets::hover_wash(ui, rect, &resp, false);
    let p = ui.painter();
    let y = rect.center().y;
    let x = |dx: f32| rect.left() + dx;
    let f = mono(11.5);
    p.text(pos2(x(c.et), y), Align2::LEFT_CENTER, format!("{:02}:{:02}", r.et.0, r.et.1), f.clone(), muted());
    p.text(pos2(x(c.local), y), Align2::LEFT_CENTER, local_time(r.ts), f.clone(), muted());
    p.text(pos2(x(c.ctry), y), Align2::LEFT_CENTER, "US", f.clone(), muted());
    let name_w = (c.period - c.event - dp(44.0)).max(dp(40.0));
    let name = p.layout_no_wrap(e.name.to_string(), ui_font(12.5), text());
    let clip = Rect::from_min_max(pos2(x(c.event), rect.top()), pos2(x(c.event) + name_w, rect.bottom()));
    p.with_clip_rect(clip).galley(pos2(x(c.event), y - name.size().y / 2.0), name, text());
    p.text(pos2(x(c.period), y), Align2::RIGHT_CENTER, r.period_label(), f.clone(), muted());
    if let Some(cons) = &r.consensus {
        p.text(pos2(x(c.cons), y), Align2::RIGHT_CENTER, cons, f.clone(), text());
    }
    match r.state {
        RowState::Released => {
            let a = r.actual.unwrap_or_default();
            // Against consensus when it is comparable (the surprise), otherwise against prior.
            // Compared at the displayed precision, so an in-line print stays neutral.
            let shown = e.fmt(a).trim_end_matches(e.unit).parse().unwrap_or(a);
            let surprise = r.consensus.as_deref().and_then(|s| eco::consensus_value(e, s)).map(|c| shown - c);
            let col = match surprise.or(r.change()) {
                Some(d) if d > 1e-9 => theme::up(),
                Some(d) if d < -1e-9 => theme::down(),
                _ => amber(),
            };
            let ar = Rect::from_min_max(
                pos2(x(c.actual) - dp(60.0), rect.top() + 2.0),
                pos2(x(c.actual) + 3.0, rect.bottom() - 2.0),
            );
            p.rect_filled(ar, 2.0, col.gamma_multiply(0.16));
            p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, e.fmt(a), mono(12.0), col);
            if let Some(pr) = r.prior {
                p.text(pos2(x(c.prior), y), Align2::RIGHT_CENTER, e.fmt(pr), f.clone(), text());
            }
            if let Some(d) = r.change() {
                p.text(pos2(x(c.change), y), Align2::RIGHT_CENTER, e.fmt_change(d), f.clone(), col);
            }
        }
        RowState::Upcoming => {
            let left = r.ts - now;
            let s = if left < 3600 {
                format!("in {}m", (left / 60).max(1))
            } else if left < 24 * 3600 {
                format!("in {}h", left / 3600)
            } else {
                String::new()
            };
            p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, s, f.clone(), if soon { amber() } else { muted() });
        }
        RowState::Pending => {
            p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, "pending", f.clone(), muted());
        }
        RowState::Unavailable => {
            p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, "n/a", f.clone(), muted());
        }
    }
    importance_bars(p, pos2(x(c.imp), rect.bottom() - dp(5.0)), e.importance);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let cons = r.consensus.as_deref().map_or(String::new(), |c| format!("\nConsensus {c} (ForexFactory)"));
    let tip = format!(
        "{} · FRED {} ({})\n{} {:02}:{:02} ET · {} local{cons}\nClick for history (ECOF {})",
        e.name,
        e.series,
        e.measure(),
        r.date.format("%a %b %d"),
        r.et.0,
        r.et.1,
        local_time(r.ts),
        e.code
    );
    resp.on_hover_text(tip).clicked()
}

/// A row of the merged calendar: a US release from FRED or another country's event.
enum Item<'a> {
    Us(&'a EcoRow),
    Global(&'a GlobalRow),
}

impl Item<'_> {
    fn ts(&self) -> i64 {
        match self {
            Item::Us(r) => r.ts,
            Item::Global(g) => g.ts,
        }
    }

    fn date(&self) -> NaiveDate {
        match self {
            Item::Us(r) => r.date,
            Item::Global(g) => g.date,
        }
    }

    fn importance(&self) -> u8 {
        match self {
            Item::Us(r) => r.event().importance,
            Item::Global(g) => g.importance(),
        }
    }
}

/// Both row kinds in one list, by time and then importance.
fn merge<'a>(us: impl Iterator<Item = &'a EcoRow>, global: impl Iterator<Item = &'a GlobalRow>) -> Vec<Item<'a>> {
    let mut v: Vec<Item> = us.map(Item::Us).chain(global.map(Item::Global)).collect();
    v.sort_by(|a, b| a.ts().cmp(&b.ts()).then(b.importance().cmp(&a.importance())));
    v
}

/// Country filter chips: "All" plus one per country with a source (or with rows).
fn country_chips(ui: &mut Ui, countries: &mut BTreeSet<String>, week: Option<&EcoWeek>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Countries").font(ui_font(11.5)).color(muted()));
        if ui.selectable_label(countries.is_empty(), "All").on_hover_text("ECO ALL").clicked() {
            countries.clear();
        }
        for k in eco::COUNTRIES {
            let has_rows = week.is_some_and(|w| w.global.iter().any(|g| g.country == k.code));
            if !k.ff && !has_rows {
                continue;
            }
            let on = countries.contains(k.code);
            let tip = format!("{} ({}) · ECO {}", k.name, k.currency, k.code);
            if ui.selectable_label(on, k.code).on_hover_text(tip).clicked() {
                if on {
                    countries.remove(k.code);
                } else {
                    countries.insert(k.code.to_string());
                }
            }
        }
    });
}

/// One non-FRED event. Not clickable: there is no history behind it.
fn global_row(ui: &mut Ui, c: &Columns, g: &GlobalRow, now: i64) {
    let (rect, resp) = ui.allocate_exact_size(vec2(c.w, dp(20.0)), Sense::hover());
    let soon = g.ts > now && g.ts - now <= 24 * 3600 && !g.all_day;
    if soon {
        ui.painter().rect_filled(rect, 2.0, theme::amber_wash());
    }
    widgets::hover_wash(ui, rect, &resp, false);
    let p = ui.painter();
    let y = rect.center().y;
    let x = |dx: f32| rect.left() + dx;
    let f = mono(11.5);
    if g.all_day {
        p.text(pos2(x(c.et), y), Align2::LEFT_CENTER, "all day", f.clone(), muted());
    } else {
        p.text(pos2(x(c.et), y), Align2::LEFT_CENTER, format!("{:02}:{:02}", g.et.0, g.et.1), f.clone(), muted());
        p.text(pos2(x(c.local), y), Align2::LEFT_CENTER, local_time(g.ts), f.clone(), muted());
    }
    let ctry_col = if g.country == "US" { muted() } else { text() };
    p.text(pos2(x(c.ctry), y), Align2::LEFT_CENTER, &g.country, f.clone(), ctry_col);
    let name_w = (c.period - c.event - dp(44.0)).max(dp(40.0));
    let name = p.layout_no_wrap(g.title.clone(), ui_font(12.5), if g.all_day { muted() } else { text() });
    let clip = Rect::from_min_max(pos2(x(c.event), rect.top()), pos2(x(c.event) + name_w, rect.bottom()));
    p.with_clip_rect(clip).galley(pos2(x(c.event), y - name.size().y / 2.0), name, text());
    if let Some(cons) = &g.forecast {
        p.text(pos2(x(c.cons), y), Align2::RIGHT_CENTER, cons, f.clone(), text());
    }
    if let Some(pr) = &g.previous {
        p.text(pos2(x(c.prior), y), Align2::RIGHT_CENTER, pr, f.clone(), text());
    }
    if let Some(a) = &g.actual {
        // Against the forecast, else the previous figure.
        let diff = [g.forecast.as_deref(), g.previous.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|r| Some(eco::figure(a)? - eco::figure(r)?));
        let col = match diff {
            Some(d) if d > 1e-9 => theme::up(),
            Some(d) if d < -1e-9 => theme::down(),
            _ => amber(),
        };
        p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, a, mono(12.0), col);
    } else if soon {
        let left = g.ts - now;
        let s = if left < 3600 { format!("in {}m", (left / 60).max(1)) } else { format!("in {}h", left / 3600) };
        p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, s, f.clone(), amber());
    }
    importance_bars(p, pos2(x(c.imp), rect.bottom() - dp(5.0)), g.importance());
    let country = eco::COUNTRIES.iter().find(|k| k.code == g.country).map_or(g.country.as_str(), |k| k.name);
    let line = |l: &str, v: &Option<String>| v.as_deref().map_or(String::new(), |v| format!("\n{l} {v}"));
    let when = if g.all_day {
        format!("{} (all day)", g.date.format("%a %b %d"))
    } else {
        format!("{} {:02}:{:02} ET · {} local", g.date.format("%a %b %d"), g.et.0, g.et.1, local_time(g.ts))
    };
    resp.on_hover_text(format!(
        "{} · {country}\n{when}{}{}{}\nSource: ForexFactory",
        g.title,
        line("Forecast", &g.forecast),
        line("Previous", &g.previous),
        line("Actual", &g.actual),
    ));
}

/// A week change typed in `ECO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeekOp {
    /// Weeks from the one shown.
    Rel(i32),
    /// Weeks from the current one.
    Abs(i32),
}

/// What an `ECO ...` command asks for; unmentioned parts stay as they are.
#[derive(Debug, Default, PartialEq)]
struct EcoOptions {
    week: Option<WeekOp>,
    importance: Option<u8>,
    /// `Some(empty)` = every country.
    countries: Option<Vec<&'static str>>,
}

/// Parse the words after `ECO`: weeks (`NEXT`, `PREV`, `THIS`, `+2`), importance (`HIGH`,
/// `MED`, `LOW`) and countries (`US EU GB`, or `ALL` for every country). Err = the first
/// word not understood.
fn parse_options(arg: &str) -> Result<EcoOptions, String> {
    let mut o = EcoOptions::default();
    for w in arg.split_whitespace() {
        let u = w.to_ascii_uppercase();
        match u.as_str() {
            "THIS" | "NOW" | "TODAY" => o.week = Some(WeekOp::Abs(0)),
            "NEXT" | "N" | "FWD" => o.week = Some(WeekOp::Rel(1)),
            "PREV" | "P" | "BACK" | "LAST" => o.week = Some(WeekOp::Rel(-1)),
            "HIGH" | "3" => o.importance = Some(3),
            "MED" | "MEDIUM" | "2" => o.importance = Some(2),
            "LOW" | "1" => o.importance = Some(1),
            "ALL" | "WORLD" | "GLOBAL" => o.countries = Some(Vec::new()),
            _ => {
                if let Some(c) = eco::parse_country(&u) {
                    o.countries.get_or_insert_with(Vec::new).push(c);
                } else if let Ok(n) = u.parse::<i32>() {
                    o.week = Some(WeekOp::Abs(n));
                } else {
                    return Err(w.to_string());
                }
            }
        }
    }
    Ok(o)
}

impl EcoOptions {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A latest-value row (no release date known).
fn latest_row(ui: &mut Ui, c: &Columns, r: &EcoRow) -> bool {
    let e = r.event();
    let (rect, resp) = ui.allocate_exact_size(vec2(c.w, dp(20.0)), Sense::click());
    widgets::hover_wash(ui, rect, &resp, false);
    let p = ui.painter();
    let y = rect.center().y;
    let x = |dx: f32| rect.left() + dx;
    let f = mono(11.5);
    p.text(pos2(x(c.event), y), Align2::LEFT_CENTER, e.name, ui_font(12.5), text());
    p.text(pos2(x(c.period), y), Align2::RIGHT_CENTER, r.period_label(), f.clone(), muted());
    if let Some(a) = r.actual {
        p.text(pos2(x(c.actual), y), Align2::RIGHT_CENTER, e.fmt(a), mono(12.0), amber());
    }
    if let Some(pr) = r.prior {
        p.text(pos2(x(c.prior), y), Align2::RIGHT_CENTER, e.fmt(pr), f.clone(), text());
    }
    if let Some(d) = r.change() {
        p.text(pos2(x(c.change), y), Align2::RIGHT_CENTER, e.fmt_change(d), f, muted());
    }
    importance_bars(p, pos2(x(c.imp), rect.bottom() - dp(5.0)), e.importance);
    resp.on_hover_text(format!("FRED {} · click for history", e.series)).clicked()
}

fn footer(ui: &mut Ui, week: &EcoWeek) {
    ui.add_space(dp(4.0));
    let source = match week.source {
        DateSource::Api => "Dates: FRED API.".to_string(),
        DateSource::Calendar => {
            "Dates: FRED's public release calendar. Add a free FRED API key (Settings → Panels) for the official API."
                .to_string()
        }
        DateSource::Unavailable => format!(
            "Upcoming release dates need a free FRED API key ({}; Settings → Panels or FRED_API_KEY). Showing FOMC and the latest values.",
            eco::KEY_URL
        ),
    };
    let mut note = |s: &str| {
        ui.label(RichText::new(s).font(ui_font(11.0)).color(muted()));
    };
    note(&source);
    note(
        "US rows: FRED releases; ACTUAL is green/red against the consensus when comparable, otherwise against PRIOR, and is as first published that day (ALFRED). ISM is proprietary and not on FRED.",
    );
    note(
        "Other countries (and US events FRED lacks): ForexFactory's free weekly calendar (CONS = forecast; that feed has no actuals), this week and, when published, next and last week. Turkey is not covered yet.",
    );
    for n in &week.notes {
        ui.label(RichText::new(n).font(ui_font(11.0)).color(theme::down()));
    }
}

fn series_view(ui: &mut Ui, pane: &mut EcoPane) {
    let Some(e) = pane.series_code.as_deref().and_then(eco::event) else {
        pane.show_series(None);
        return;
    };
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("◀ Calendar").on_hover_text("Back to ECO").clicked() {
            pane.show_series(None);
        }
        ui.label(RichText::new(e.name).font(ui_font(13.0)).color(text()));
        ui.label(RichText::new(format!("FRED {} · {}", e.series, e.measure())).font(ui_font(11.5)).color(muted()));
        let spans = [2u32, 5, 10, 0];
        let sel = spans.iter().position(|&y| y == pane.years);
        if let Some(i) = widgets::segmented(ui, "ecof-span", &["2Y", "5Y", "10Y", "All"], sel, 20.0) {
            pane.years = spans[i];
        }
        if pane.series.is_none() {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    if pane.series_code.is_none() {
        return;
    }
    let s = match &pane.series {
        None => {
            ui.label(RichText::new("Loading history (fredgraph.csv)…").font(ui_font(12.0)).color(muted()));
            return;
        }
        Some(Err(err)) => {
            ui.label(
                RichText::new(format!("Could not load {}: {err}", e.series)).font(ui_font(12.0)).color(theme::down()),
            );
            return;
        }
        Some(Ok(s)) => s,
    };
    let last = s.values.last().map(|v| v.0).unwrap_or(NaiveDate::MIN);
    let from = if pane.years == 0 {
        NaiveDate::MIN
    } else {
        last.with_year(last.year() - pane.years as i32).unwrap_or(NaiveDate::MIN)
    };
    let pts: Vec<(NaiveDate, f64)> = s.values.iter().copied().filter(|v| v.0 >= from).collect();
    let avail = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(vec2(avail.x, (avail.y * 0.62).max(dp(140.0))), Sense::hover());
    line_plot(ui, rect, &resp, e, &pts);
    // Recent values, newest first.
    ui.label(
        RichText::new(format!(" {:<10} {:>12} {:>12}", "PERIOD", "VALUE", "CHG"))
            .font(mono(11.5))
            .color(amber())
            .background_color(theme::header()),
    );
    egui::ScrollArea::vertical().id_salt("ecof-table").auto_shrink([false, false]).show(ui, |ui| {
        for i in (0..s.values.len()).rev().take(36) {
            let (d, v) = s.values[i];
            let chg = i.checked_sub(1).map(|j| e.fmt_change(v - s.values[j].1)).unwrap_or_default();
            ui.label(
                RichText::new(format!(" {:<10} {:>12} {:>12}", d.format("%Y-%m-%d"), e.fmt(v), chg))
                    .font(mono(12.0))
                    .color(text()),
            );
        }
    });
}

/// A simple line chart with a crosshair readout.
fn line_plot(ui: &Ui, rect: Rect, resp: &egui::Response, e: &eco::EcoEvent, pts: &[(NaiveDate, f64)]) {
    let p = ui.painter_at(rect);
    let plot = Rect::from_min_max(rect.min + vec2(dp(8.0), dp(8.0)), rect.max - vec2(dp(64.0), dp(22.0)));
    if pts.len() < 2 {
        p.text(plot.center(), Align2::CENTER_CENTER, "Not enough data", ui_font(12.0), muted());
        return;
    }
    let (lo, hi) = pts.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &(_, v)| (a.min(v), b.max(v)));
    let pad = ((hi - lo) * 0.08).max(1e-6);
    let (lo, hi) = (lo - pad, hi + pad);
    let day = |d: NaiveDate| d.num_days_from_ce() as f64;
    let (t0, t1) = (day(pts[0].0), day(pts[pts.len() - 1].0));
    let x_of = |d: NaiveDate| plot.left() + plot.width() * ((day(d) - t0) / (t1 - t0).max(1.0)) as f32;
    let y_of = |v: f64| plot.bottom() - plot.height() * ((v - lo) / (hi - lo)) as f32;
    for t in tr_chart::scale::price_ticks(lo, hi, 6) {
        let y = y_of(t);
        p.hline(plot.x_range(), y, Stroke::new(1.0, theme::grid()));
        p.text(pos2(plot.right() + 6.0, y), Align2::LEFT_CENTER, e.fmt(t), mono(10.5), muted());
    }
    if lo < 0.0 && hi > 0.0 {
        p.hline(plot.x_range(), y_of(0.0), Stroke::new(1.0, muted()));
    }
    let (y0, y1) = (pts[0].0.year(), pts[pts.len() - 1].0.year());
    let step = ((y1 - y0) / 8).max(1);
    for y in (y0..=y1).step_by(step as usize) {
        let Some(d) = NaiveDate::from_ymd_opt(y, 1, 1).filter(|d| day(*d) >= t0) else { continue };
        let x = x_of(d);
        p.vline(x, plot.y_range(), Stroke::new(1.0, theme::grid()));
        p.text(pos2(x, plot.bottom() + dp(10.0)), Align2::CENTER_CENTER, y.to_string(), mono(10.5), muted());
    }
    let changes = matches!(e.transform, eco::Transform::Change | eco::Transform::PctChange);
    if changes && pts.len() <= 160 {
        // Bars for period-over-period changes, as on ECOF.
        let bw = (plot.width() / pts.len() as f32 * 0.7).max(1.0);
        let base = y_of(0.0f64.clamp(lo, hi));
        for &(d, v) in pts {
            let x = x_of(d);
            let col = if v >= 0.0 { theme::up() } else { theme::down() };
            p.rect_filled(
                Rect::from_x_y_ranges(x - bw / 2.0..=x + bw / 2.0, base.min(y_of(v))..=base.max(y_of(v))),
                0.0,
                col,
            );
        }
    } else {
        let line: Vec<Pos2> = pts.iter().map(|&(d, v)| pos2(x_of(d), y_of(v))).collect();
        p.add(egui::Shape::line(line, Stroke::new(1.8, amber())));
    }
    if let Some(h) = resp.hover_pos().filter(|h| plot.contains(*h)) {
        let i = pts
            .iter()
            .enumerate()
            .min_by(|a, b| (x_of(a.1.0) - h.x).abs().total_cmp(&(x_of(b.1.0) - h.x).abs()))
            .map_or(0, |(i, _)| i);
        let (d, v) = pts[i];
        let (x, y) = (x_of(d), y_of(v));
        p.vline(x, plot.y_range(), Stroke::new(1.0, muted()));
        p.circle_filled(pos2(x, y), 3.0, amber());
        let label = format!("{}  {}", d.format("%b %Y"), e.fmt(v));
        let g = p.layout_no_wrap(label, mono(11.0), text());
        let at = pos2((x + 8.0).min(plot.right() - g.size().x - 4.0), plot.top() + 2.0);
        p.rect_filled(Rect::from_min_size(at - vec2(3.0, 1.0), g.size() + vec2(6.0, 2.0)), 2.0, theme::card());
        p.galley(at, g, text());
    }
}

impl TerminalApp {
    /// The pane `ECO` commands act on: the focused calendar, else any calendar.
    fn eco_target(&self) -> Option<PaneId> {
        let is_eco = |id: PaneId| self.idx(id).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::Eco(_)));
        if is_eco(self.focus) {
            return Some(self.focus);
        }
        self.panes.iter().find(|p| matches!(p.kind, PaneKind::Eco(_))).map(|p| p.id)
    }

    fn with_eco(&mut self, f: impl FnOnce(&mut EcoPane)) {
        match self.eco_target() {
            Some(id) => {
                if let Some(PaneKind::Eco(e)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    f(e);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => {
                let mut e = EcoPane::new(1, None);
                f(&mut e);
                self.open_panel(PaneKind::Eco(Box::new(e)));
            }
        }
    }

    /// `ECO`, `ECO NEXT|PREV|THIS`, `ECO +2`, `ECO HIGH|MED|LOW`, `ECO US EU GB…|ALL`; words
    /// combine (`ECO EU GB HIGH NEXT`).
    pub(super) fn eco_command(&mut self, arg: &str) {
        match parse_options(arg) {
            Ok(o) => self.with_eco(|e| {
                if o.is_empty() {
                    e.show_series(None);
                    e.set_week(0);
                }
                if let Some(w) = o.week {
                    e.show_series(None);
                    e.set_week(match w {
                        WeekOp::Rel(d) => e.week + d,
                        WeekOp::Abs(w) => w,
                    });
                }
                if let Some(i) = o.importance {
                    e.min_importance = i;
                }
                if let Some(c) = o.countries {
                    e.show_series(None);
                    e.countries = c.into_iter().map(str::to_string).collect();
                }
            }),
            Err(bad) => {
                self.status = format!(
                    "ECO: unknown option {bad} (NEXT, PREV, +N, HIGH, MED, LOW, ALL or countries: {})",
                    eco::COUNTRIES.iter().filter(|k| k.ff).map(|k| k.code).collect::<Vec<_>>().join(" ")
                );
            }
        }
    }

    /// `ECOF <release>`: the release's history.
    pub(super) fn eco_series_command(&mut self, name: &str) {
        let name = if name.trim().is_empty() { "CPI" } else { name.trim() };
        match eco::event(name) {
            Some(ev) => self.with_eco(|e| e.show_series(Some(ev.code))),
            None => {
                let codes: Vec<&str> = eco::EVENTS.iter().map(|e| e.code).collect();
                self.status = format!("ECOF: unknown release {name}. Try {}", codes.join(", "));
            }
        }
    }

    /// Fetch due calendars and histories; toast 5 minutes before high-importance releases.
    pub(super) fn eco_periodic(&mut self, visible: &[usize]) {
        let now = now_ts();
        for i in 0..self.panes.len() {
            let id = self.panes[i].id;
            let shown = visible.contains(&i);
            let PaneKind::Eco(e) = &mut self.panes[i].kind else { continue };
            if shown {
                let live = e.data.as_ref().is_some_and(|w| {
                    w.rows
                        .iter()
                        .any(|r| r.state == RowState::Pending || (r.state == RowState::Upcoming && r.ts <= now))
                });
                let every = Duration::from_secs(if live { 120 } else { 1800 });
                if e.fetched.is_none_or(|t| t.elapsed() > every) {
                    e.fetched = Some(Instant::now());
                    e.loading = e.data.is_none();
                    self.worker.send(Request::Eco { pane: id, req: EcoRequest::Week { monday: e.monday() } });
                }
                if let Some(code) = e.series_code.clone().filter(|_| !e.series_requested) {
                    e.series_requested = true;
                    self.worker.send(Request::Eco { pane: id, req: EcoRequest::Series { code } });
                }
            }
            let Some(w) = &e.data else { continue };
            let mut due: Vec<(String, String, i64)> = Vec::new();
            if e.shows("US") {
                for r in &w.rows {
                    let left = r.ts - now;
                    if r.state == RowState::Upcoming && r.event().importance >= 3 && (0..=300).contains(&left) {
                        let msg = format!("{} at {:02}:{:02} ET", r.event().name, r.et.0, r.et.1);
                        due.push((format!("{}@{}", r.code, r.date), msg, left));
                    }
                }
            }
            for g in &w.global {
                let left = g.ts - now;
                if g.impact == eco::Impact::High && !g.all_day && e.shows(&g.country) && (0..=300).contains(&left) {
                    let msg = format!("{} {} at {:02}:{:02} ET", g.country, g.title, g.et.0, g.et.1);
                    due.push((format!("{}:{}@{}", g.country, g.title, g.ts), msg, left));
                }
            }
            for (key, msg, left) in due {
                if e.alerted.insert(key) {
                    self.toasts.push((format!("In {} min: {msg}", (left + 59) / 60), Instant::now()));
                }
            }
        }
    }

    pub(super) fn on_eco(&mut self, pane: PaneId, resp: EcoResponse) {
        let Some(PaneKind::Eco(e)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        match resp {
            EcoResponse::Week(w) => {
                if w.monday == e.monday() {
                    e.data = Some(w);
                    e.loading = false;
                }
            }
            EcoResponse::Series { code, series } => {
                if e.series_code.as_deref() == Some(code.as_str()) {
                    e.series = Some(series);
                }
            }
        }
    }

    /// FRED API key, shown at the end of Settings → Panels.
    pub(super) fn eco_key_settings(&mut self, ui: &mut Ui) {
        ui.add_space(dp(10.0));
        ui.label(RichText::new("Economic calendar (ECO)").font(ui_font(13.5)).color(text()));
        ui.separator();
        let store = crate::eco::key_store();
        let env = eco::KeyStore::from_env();
        let stored = store.stored();
        let status = match (&env, &stored) {
            (Some(k), _) => format!("Using FRED_API_KEY from the environment ({})", eco::mask(k)),
            (None, Some(k)) => format!("FRED API key saved ({}) in {}", eco::mask(k), store.describe()),
            (None, None) => "No FRED API key: release dates come from FRED's public calendar page.".into(),
        };
        ui.label(RichText::new(status).font(ui_font(11.5)).color(muted()));
        let id = egui::Id::new("eco-fred-key");
        let mut buf: String = ui.data_mut(|d| d.get_temp(id).unwrap_or_default());
        let mut msg: Option<(String, Color32)> = ui.data_mut(|d| d.get_temp(id.with("msg")));
        ui.horizontal_wrapped(|ui| {
            ui.label("FRED API key");
            ui.add(
                egui::TextEdit::singleline(&mut buf).password(true).desired_width(dp(260.0)).hint_text("32 characters"),
            );
            if ui.button("Save").clicked() {
                msg = Some(match store.save(&buf) {
                    Ok(()) => {
                        buf.clear();
                        for p in &mut self.panes {
                            if let PaneKind::Eco(e) = &mut p.kind {
                                e.fetched = None;
                            }
                        }
                        ("Saved. Calendars reload with the FRED API.".into(), theme::up())
                    }
                    Err(e) => (format!("{e:#}"), theme::down()),
                });
            }
            if stored.is_some() && ui.button("Remove").clicked() {
                msg = Some(match store.clear() {
                    Ok(()) => ("Removed.".into(), muted()),
                    Err(e) => (format!("{e:#}"), theme::down()),
                });
            }
            ui.hyperlink_to("Get a free key", eco::KEY_URL);
        });
        if let Some((m, c)) = &msg {
            ui.label(RichText::new(m).font(ui_font(11.5)).color(*c));
        }
        ui.data_mut(|d| {
            d.insert_temp(id, buf);
            match msg {
                Some(m) => {
                    d.insert_temp(id.with("msg"), m);
                }
                None => d.remove::<(String, Color32)>(id.with("msg")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eco_options_parse() {
        assert_eq!(parse_options(""), Ok(EcoOptions::default()));
        assert!(parse_options("").unwrap().is_empty());
        let o = parse_options("us eu").unwrap();
        assert_eq!(o.countries, Some(vec!["US", "EU"]));
        assert_eq!(parse_options("uk jpy cn").unwrap().countries, Some(vec!["GB", "JP", "CN"]));
        // ALL = every country; importance is HIGH/MED/LOW.
        assert_eq!(parse_options("ALL").unwrap().countries, Some(vec![]));
        assert_eq!(parse_options("ALL").unwrap().importance, None);
        assert_eq!(parse_options("low").unwrap().importance, Some(1));
        let o = parse_options("EU GB high next").unwrap();
        assert_eq!((o.importance, o.week), (Some(3), Some(WeekOp::Rel(1))));
        assert_eq!(parse_options("prev").unwrap().week, Some(WeekOp::Rel(-1)));
        assert_eq!(parse_options("+2").unwrap().week, Some(WeekOp::Abs(2)));
        assert_eq!(parse_options("EU nonsense"), Err("nonsense".into()));
        // Turkey is a known country code even before it has a source.
        assert_eq!(parse_options("TR").unwrap().countries, Some(vec!["TR"]));
    }

    #[test]
    fn country_filter_and_persistence() {
        let mut p = EcoPane::new(1, None);
        assert!(p.shows("US") && p.shows("JP"));
        p = p.with_countries(&["EU".into(), "uk".into(), "zz".into()]);
        assert!(p.shows("EU") && p.shows("GB") && !p.shows("US"));
        assert_eq!(p.countries.len(), 2);
    }
}
