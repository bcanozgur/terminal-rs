//! Futures curve, VIX term structure and sector heatmap (`FUT`, `VIX`, `HEAT`). Data layer:
//! [`tr_data::fut`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CursorIcon, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use tr_data::fut::{self, Curve, Heat, Period, VixTerm};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::fut::{FutReq, FutResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets::lerp_color;
use crate::worker::Request;

/// How often a visible pane re-fetches.
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Curve,
    Vix,
    Heat,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Curve, Tab::Vix, Tab::Heat];

    fn label(self) -> &'static str {
        match self {
            Tab::Curve => "Futures Curve",
            Tab::Vix => "VIX Term Structure",
            Tab::Heat => "Sector Heatmap",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Tab::Curve => "curve",
            Tab::Vix => "vix",
            Tab::Heat => "heat",
        }
    }

    fn idx(self) -> usize {
        self as usize
    }
}

pub struct FutPane {
    tab: Tab,
    root: String,
    period: Period,
    world: bool,
    curve: Option<Arc<Curve>>,
    vix: Option<Arc<VixTerm>>,
    heat: Option<Arc<Heat>>,
    /// Per tab: the last error (kept beside the old data) and whether a request is out.
    error: [Option<String>; 3],
    loading: [bool; 3],
    last_req: [Option<Instant>; 3],
    /// The next request skips the worker cache (Refresh button).
    force: bool,
}

impl FutPane {
    /// `arg` as typed after `FUT`: a root (`CL`), `VIX`, or `HEAT` (`HEAT WORLD`).
    pub fn new(arg: &str) -> Self {
        let mut p = Self {
            tab: Tab::Curve,
            root: "ES".into(),
            period: Period::Day,
            world: false,
            curve: None,
            vix: None,
            heat: None,
            error: Default::default(),
            loading: [false; 3],
            last_req: [None; 3],
            force: false,
        };
        p.set_arg(arg);
        p
    }

    pub fn from_settings(tab: &str, root: &str, period: &str, world: bool) -> Self {
        let mut p = Self::new(root);
        p.tab = match tab {
            "vix" => Tab::Vix,
            "heat" => Tab::Heat,
            _ => Tab::Curve,
        };
        p.period = Period::ALL.into_iter().find(|x| x.label().eq_ignore_ascii_case(period)).unwrap_or(Period::Day);
        p.world = world;
        p
    }

    /// Select the tab / root named by a command argument.
    pub fn set_arg(&mut self, arg: &str) {
        let mut words = arg.split_whitespace();
        let first = words.next().unwrap_or("").to_ascii_uppercase();
        match first.as_str() {
            "VIX" | "VTS" | "TERM" => self.tab = Tab::Vix,
            "HEAT" | "HEATMAP" | "SECTORS" | "SECT" => {
                self.tab = Tab::Heat;
                if let Some(w) = words.next() {
                    self.world = matches!(w.to_ascii_uppercase().as_str(), "WORLD" | "GLOBAL" | "W");
                }
            }
            "" | "CURVE" => {}
            code => {
                // `FUT CL`; an unknown root keeps the current one rather than failing.
                if let Some(r) = fut::root(code) {
                    self.root = r.code.to_string();
                    self.tab = Tab::Curve;
                }
            }
        }
    }

    pub fn title(&self) -> String {
        match self.tab {
            Tab::Curve => format!("Futures · {}", self.root),
            Tab::Vix => "VIX Term Structure".into(),
            Tab::Heat => if self.world { "World Heatmap" } else { "Sector Heatmap" }.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        "FUT"
    }

    /// Tab, root, period and world flag for saved layouts.
    pub fn settings(&self) -> (&'static str, String, &'static str, bool) {
        (self.tab.key(), self.root.clone(), self.period.label(), self.world)
    }

    fn request_now(&mut self) {
        self.last_req[self.tab.idx()] = None;
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `FUT`, `FUT CL`, `VIX`, `HEAT`: focus (or open) the pane on that tab.
    pub(super) fn fut_command(&mut self, arg: &str) {
        let id = self
            .idx(self.focus)
            .filter(|&i| matches!(self.panes[i].kind, PaneKind::Fut(_)))
            .map(|i| self.panes[i].id)
            .or_else(|| self.panes.iter().find(|p| matches!(p.kind, PaneKind::Fut(_))).map(|p| p.id));
        match id {
            Some(id) => {
                if let Some(PaneKind::Fut(f)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    f.set_arg(arg);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Fut(Box::new(FutPane::new(arg)))),
        }
    }

    /// Fetch the shown tab on first show and every minute while the pane is visible.
    pub(super) fn fut_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Fut(f) = &mut self.panes[i].kind else { continue };
            let t = f.tab.idx();
            // The shown data belongs to another root / universe than the selection.
            let mismatch = match f.tab {
                Tab::Curve => f.curve.as_ref().is_some_and(|c| c.root != f.root),
                Tab::Vix => false,
                Tab::Heat => f.heat.as_ref().is_some_and(|h| h.world != f.world),
            };
            if mismatch && !f.loading[t] {
                f.last_req[t] = None;
            }
            if !f.loading[t] && f.last_req[t].is_none_or(|at| at.elapsed() >= REFRESH) {
                f.loading[t] = true;
                f.last_req[t] = Some(Instant::now());
                let force = std::mem::take(&mut f.force);
                let req = match f.tab {
                    Tab::Curve => FutReq::Curve { pane: id, root: f.root.clone(), force },
                    Tab::Vix => FutReq::Vix { pane: id, force },
                    Tab::Heat => FutReq::Heat { pane: id, world: f.world, force },
                };
                self.worker.send(Request::Fut(req));
            }
        }
    }

    pub(super) fn on_fut(&mut self, resp: FutResp) {
        let (pane, tab) = match &resp {
            FutResp::Curve { pane, .. } => (*pane, Tab::Curve),
            FutResp::Vix { pane, .. } => (*pane, Tab::Vix),
            FutResp::Heat { pane, .. } => (*pane, Tab::Heat),
        };
        let Some(PaneKind::Fut(f)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        f.loading[tab.idx()] = false;
        let err = &mut f.error[tab.idx()];
        // Keep the previous data on screen when a refresh fails, but say so.
        match resp {
            FutResp::Curve { res, .. } => match res {
                Ok(c) => {
                    f.curve = Some(c);
                    *err = None;
                }
                Err(e) => *err = Some(e),
            },
            FutResp::Vix { res, .. } => match res {
                Ok(v) => {
                    f.vix = Some(v);
                    *err = None;
                }
                Err(e) => *err = Some(e),
            },
            FutResp::Heat { res, .. } => match res {
                Ok(h) => {
                    f.heat = Some(h);
                    *err = None;
                }
                Err(e) => *err = Some(e),
            },
        }
    }
}

// ---- drawing ------------------------------------------------------------------------------

fn dim(ui: &mut Ui, s: impl Into<String>) {
    ui.label(RichText::new(s.into()).font(ui_font(11.5)).color(muted()));
}

fn change_color(v: f64) -> Color32 {
    if v >= 0.0 { theme::up() } else { theme::down() }
}

fn signed(v: f64, d: usize) -> String {
    format!("{v:+.d$}")
}

fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// Decimals for a price of this size.
fn digits(p: f64) -> usize {
    if p.abs() >= 1000.0 {
        1
    } else if p.abs() >= 10.0 {
        2
    } else {
        3
    }
}

fn time_str(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn num(ui: &mut Ui, s: String, color: Color32) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(RichText::new(s).font(mono(12.5)).color(color));
    });
}

fn head(ui: &mut Ui, s: &str, right: bool) {
    if right {
        num(ui, s.to_string(), muted());
    } else {
        ui.label(RichText::new(s).font(ui_font(11.0)).color(muted()));
    }
}

pub fn fut(ui: &mut Ui, p: &mut FutPane) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.horizontal_wrapped(|ui| {
        for t in Tab::ALL {
            if ui.selectable_label(p.tab == t, RichText::new(t.label()).font(ui_font(12.5))).clicked() {
                p.tab = t;
            }
        }
        ui.separator();
        match p.tab {
            Tab::Curve => {
                for r in fut::ROOTS {
                    let on = p.root == r.code;
                    if ui
                        .selectable_label(on, RichText::new(r.code).font(mono(12.0)))
                        .on_hover_text(format!("{} ({})", r.name, r.exchange))
                        .clicked()
                        && !on
                    {
                        p.root = r.code.to_string();
                        p.request_now();
                    }
                }
            }
            Tab::Vix => {}
            Tab::Heat => {
                for x in Period::ALL {
                    if ui.selectable_label(p.period == x, RichText::new(x.label()).font(ui_font(12.5))).clicked() {
                        p.period = x;
                    }
                }
                ui.separator();
                for (world, label) in [(false, "S&P 500 sectors"), (true, "World")] {
                    if ui.selectable_label(p.world == world, RichText::new(label).font(ui_font(12.5))).clicked()
                        && p.world != world
                    {
                        p.world = world;
                        p.request_now();
                    }
                }
            }
        }
        if ui.small_button("Refresh").on_hover_text("Fetch now").clicked() {
            p.request_now();
            p.force = true;
        }
        if p.loading[p.tab.idx()] {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
    });
    if let Some(e) = &p.error[p.tab.idx()] {
        ui.label(RichText::new(format!("Could not refresh: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    ui.add_space(dp(2.0));
    egui::ScrollArea::vertical().id_salt(("fut-scroll", p.tab.key())).auto_shrink([false, false]).show(
        ui,
        |ui| match p.tab {
            Tab::Curve => curve_tab(ui, p),
            Tab::Vix => vix_tab(ui, p),
            Tab::Heat => heat_tab(ui, p, &mut actions),
        },
    );
    actions
}

// ---- line chart -------------------------------------------------------------------------

struct Line {
    name: String,
    color: Color32,
    pts: Vec<(f64, f64)>,
}

/// A small line chart with markers, y grid, x tick labels and a hover read-out of the nearest point.
fn line_chart(ui: &mut Ui, height: f32, lines: &[Line], ticks: &[(f64, String)], decimals: usize) {
    let all = lines.iter().flat_map(|l| l.pts.iter().copied());
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for (x, y) in all {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    if x0 > x1 {
        return;
    }
    if x1 - x0 < 1e-9 {
        x0 -= 1.0;
        x1 += 1.0;
    }
    let pad = if y1 - y0 < 1e-9 { y0.abs().max(1.0) * 0.02 } else { (y1 - y0) * 0.12 };
    let (y0, y1) = (y0 - pad, y1 + pad);
    let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), height), Sense::hover());
    let r = resp.rect;
    let plot = Rect::from_min_max(
        pos2(r.left() + dp(58.0), r.top() + dp(8.0)),
        pos2(r.right() - dp(14.0), r.bottom() - dp(22.0)),
    );
    if plot.width() < 40.0 || plot.height() < 30.0 {
        return;
    }
    let to = |x: f64, y: f64| {
        pos2(
            plot.left() + ((x - x0) / (x1 - x0)) as f32 * plot.width(),
            plot.bottom() - ((y - y0) / (y1 - y0)) as f32 * plot.height(),
        )
    };
    for k in 0..=4 {
        let v = y0 + (y1 - y0) * f64::from(k) / 4.0;
        let y = to(x0, v).y;
        painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, theme::grid()));
        painter.text(
            pos2(plot.left() - dp(6.0), y),
            Align2::RIGHT_CENTER,
            format!("{v:.decimals$}"),
            mono(10.5),
            muted(),
        );
    }
    let mut last_label_right = f32::MIN;
    for (x, label) in ticks {
        let px = to(*x, y0).x;
        let w = label.chars().count() as f32 * dp(6.4);
        if px - w / 2.0 > last_label_right + dp(6.0) {
            painter.text(pos2(px, plot.bottom() + dp(4.0)), Align2::CENTER_TOP, label, mono(10.5), muted());
            last_label_right = px + w / 2.0;
        }
        painter.line_segment(
            [pos2(px, plot.bottom()), pos2(px, plot.bottom() + dp(2.0))],
            Stroke::new(1.0, theme::grid()),
        );
    }
    for l in lines {
        let pts: Vec<Pos2> = l.pts.iter().map(|&(x, y)| to(x, y)).collect();
        if pts.len() > 1 {
            painter.add(Shape::line(pts.clone(), Stroke::new(dp(1.8), l.color)));
        }
        for p in pts {
            painter.circle_filled(p, dp(3.0), l.color);
        }
    }
    if let Some(m) = resp.hover_pos().filter(|m| plot.expand(dp(6.0)).contains(*m)) {
        let nearest = lines
            .iter()
            .flat_map(|l| l.pts.iter().map(move |&(x, y)| (l, x, y)))
            .min_by(|a, b| (to(a.1, a.2) - m).length().total_cmp(&(to(b.1, b.2) - m).length()));
        if let Some((l, x, y)) = nearest {
            let at = to(x, y);
            painter.circle_stroke(at, dp(6.0), Stroke::new(1.5, text()));
            let label = ticks.iter().find(|t| (t.0 - x).abs() < 1e-9).map_or(String::new(), |t| format!(" · {}", t.1));
            resp.on_hover_text(format!("{}{label}: {y:.decimals$}", l.name));
        }
    }
    if lines.len() > 1 {
        let mut x = plot.left() + dp(8.0);
        for l in lines {
            painter.circle_filled(pos2(x, plot.top() + dp(8.0)), dp(4.0), l.color);
            let g = painter.text(
                pos2(x + dp(8.0), plot.top() + dp(8.0)),
                Align2::LEFT_CENTER,
                &l.name,
                ui_font(11.0),
                text(),
            );
            x = g.right() + dp(16.0);
        }
    }
}

// ---- futures curve tab ---------------------------------------------------------------------

fn curve_tab(ui: &mut Ui, p: &FutPane) {
    let Some(c) = p.curve.as_ref().filter(|c| c.root == p.root) else {
        if p.error[Tab::Curve.idx()].is_none() {
            dim(ui, "loading…");
        }
        return;
    };
    dim(
        ui,
        format!(
            "Yahoo · delayed (exchange data, roughly 10-15 minutes) · {} · {} · updated {} · refreshes every minute",
            c.name,
            c.unit,
            time_str(c.fetched)
        ),
    );
    ui.add_space(dp(4.0));
    match c.slope() {
        Some(s) => {
            let (word, color) = match s.shape {
                fut::Shape::Contango => ("Contango", amber()),
                fut::Shape::Backwardation => ("Backwardation", theme::cyan()),
                fut::Shape::Flat => ("Flat", muted()),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(word).font(ui_font(14.0)).strong().color(color));
                let d = digits(c.points[0].price);
                ui.label(
                    RichText::new(format!(
                        "{} → {} ({} months): {} ({}%), {}% a year",
                        s.front,
                        s.back,
                        s.months,
                        signed(s.spread, d),
                        signed(s.spread_pct, 2),
                        signed(s.annualised_pct, 1),
                    ))
                    .font(mono(12.5))
                    .color(text()),
                );
            });
            let advice = match s.shape {
                fut::Shape::Contango => format!(
                    "Roll yield for a long position: {}% a year (rolling into dearer contracts costs this, other things equal). Shorts earn it.",
                    signed(s.roll_yield_pct(), 1)
                ),
                fut::Shape::Backwardation => format!(
                    "Roll yield for a long position: {}% a year (rolling into cheaper contracts earns this, other things equal). Shorts pay it.",
                    signed(s.roll_yield_pct(), 1)
                ),
                fut::Shape::Flat => "The curve is flat: rolling costs or earns almost nothing.".into(),
            };
            dim(ui, advice);
        }
        None => dim(ui, "Fewer than two live contracts: no curve slope."),
    }
    ui.add_space(dp(4.0));
    let front = &c.points[0];
    let pts: Vec<(f64, f64)> = c.points.iter().map(|k| (f64::from(k.months_after(front)), k.price)).collect();
    let ticks: Vec<(f64, String)> = c.points.iter().zip(&pts).map(|(k, p)| (p.0, k.label())).collect();
    let line = Line { name: c.root.to_string(), color: amber(), pts };
    line_chart(ui, dp(230.0), &[line], &ticks, digits(front.price));
    ui.add_space(dp(6.0));
    egui::Grid::new("fut-curve-grid").num_columns(7).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
        for (i, h) in ["Contract", "Symbol", "Price", "Change", "% Chg", "Volume", "vs Front"].into_iter().enumerate() {
            head(ui, h, i >= 2);
        }
        ui.end_row();
        for k in &c.points {
            let d = digits(k.price);
            ui.label(RichText::new(k.label()).font(mono(12.5)).color(amber()));
            ui.label(RichText::new(&k.symbol).font(mono(12.0)).color(muted()));
            num(ui, format!("{:.d$}", k.price), text());
            match (k.change(), k.change_pct()) {
                (Some(ch), Some(pc)) => {
                    num(ui, signed(ch, d), change_color(ch));
                    num(ui, format!("{}%", signed(pc, 2)), change_color(pc));
                }
                _ => {
                    num(ui, "–".into(), muted());
                    num(ui, "–".into(), muted());
                }
            }
            num(ui, k.volume.map_or("–".into(), compact), muted());
            if front.price > 0.0 {
                let v = (k.price / front.price - 1.0) * 100.0;
                num(ui, format!("{}%", signed(v, 2)), muted());
            } else {
                num(ui, "–".into(), muted());
            }
            ui.end_row();
        }
    });
    ui.add_space(dp(6.0));
    let mut notes = Vec::new();
    if !c.missing.is_empty() {
        let n = c.missing.len();
        let shown: Vec<&str> = c.missing.iter().take(4).map(String::as_str).collect();
        notes.push(format!(
            "{n} probed contract month{} not listed or no longer trading on Yahoo ({}{}).",
            if n == 1 { "" } else { "s" },
            shown.join(", "),
            if n > shown.len() { ", …" } else { "" }
        ));
    }
    if c.failed > 0 {
        notes.push(format!("{} contract request(s) failed; the curve may have gaps.", c.failed));
    }
    notes.push("Volume is today's session. Open interest is not available from Yahoo.".into());
    dim(ui, notes.join(" "));
}

// ---- VIX tab -------------------------------------------------------------------------------

fn vix_tab(ui: &mut Ui, p: &FutPane) {
    let Some(v) = p.vix.as_ref() else {
        if p.error[Tab::Vix.idx()].is_none() {
            dim(ui, "loading…");
        }
        return;
    };
    let fut_note = match &v.futures_error {
        None => "VX futures: Cboe end-of-day settlement (previous close, not intraday)".to_string(),
        Some(e) => format!("VX futures unavailable ({e}); showing the Yahoo indices only"),
    };
    dim(
        ui,
        format!(
            "Indices: Yahoo (Cboe data, delayed) · {fut_note} · updated {} · refreshes every minute",
            time_str(v.fetched)
        ),
    );
    ui.add_space(dp(4.0));
    ui.horizontal_wrapped(|ui| {
        for i in &v.indices {
            ui.vertical(|ui| {
                ui.label(RichText::new(i.label).font(ui_font(11.0)).color(muted()));
                ui.label(RichText::new(format!("{:.2}", i.value)).font(mono(16.0)).color(text()));
                if let Some(pc) = i.change_pct() {
                    ui.label(RichText::new(format!("{}%", signed(pc, 2))).font(mono(11.5)).color(change_color(pc)));
                }
            });
            ui.add_space(dp(14.0));
        }
    });
    ui.add_space(dp(6.0));
    let idx_pts: Vec<(f64, f64)> = v.indices.iter().map(|i| (i.days, i.value)).collect();
    let mut ticks: Vec<(f64, String)> = v.indices.iter().map(|i| (i.days, i.label.to_string())).collect();
    let mut lines = vec![Line { name: "Cboe volatility indices".into(), color: amber(), pts: idx_pts }];
    let fut_pts: Vec<(f64, f64)> = v
        .live_futures()
        .map(|f| ((f.expiry - v.today).num_days() as f64, f.settle))
        .filter(|(d, _)| *d <= 400.0)
        .collect();
    if !fut_pts.is_empty() {
        lines.push(Line { name: "VX futures (settle)".into(), color: theme::cyan(), pts: fut_pts });
    }
    ticks.sort_by(|a, b| a.0.total_cmp(&b.0));
    dim(ui, "Horizontal axis: calendar days ahead (index horizon, or days to a VX future's expiry).");
    line_chart(ui, dp(240.0), &lines, &ticks, 2);
    ui.add_space(dp(6.0));
    if let Some(r) = v.ratio("VIX", "VIX3M") {
        ui.horizontal(|ui| {
            ui.label(RichText::new("VIX / VIX3M").font(ui_font(12.0)).color(muted()));
            let color = if r >= 1.0 {
                theme::down()
            } else if r >= 0.9 {
                amber()
            } else {
                theme::up()
            };
            ui.label(RichText::new(format!("{r:.3}")).font(mono(14.0)).color(color));
            let word = if r >= 1.0 { "backwardation" } else { "contango" };
            ui.label(RichText::new(word).font(ui_font(12.0)).color(color));
            if let Some(r2) = v.ratio("VIX9D", "VIX") {
                ui.add_space(dp(12.0));
                ui.label(RichText::new("VIX9D / VIX").font(ui_font(12.0)).color(muted()));
                ui.label(RichText::new(format!("{r2:.3}")).font(mono(14.0)).color(text()));
            }
        });
    }
    for line in v.interpretation() {
        ui.label(RichText::new(line).font(ui_font(12.5)).color(text()));
    }
    ui.add_space(dp(8.0));
    let live: Vec<_> = v.live_futures().collect();
    if live.is_empty() {
        if v.futures_error.is_none() {
            dim(ui, "No unexpired VX futures in the Cboe settlement file.");
        }
        return;
    }
    egui::Grid::new("fut-vx-grid").num_columns(5).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
        for (i, h) in ["Future", "Expiry", "Settle", "vs Prior", "vs Spot VIX"].into_iter().enumerate() {
            head(ui, h, i >= 2);
        }
        ui.end_row();
        let spot = v.index("VIX").map(|i| i.value);
        let mut prior: Option<f64> = None;
        for f in live {
            ui.label(RichText::new(&f.symbol).font(mono(12.5)).color(amber()));
            ui.label(RichText::new(f.expiry.format("%Y-%m-%d").to_string()).font(mono(12.0)).color(muted()));
            num(ui, format!("{:.2}", f.settle), text());
            match prior {
                Some(pr) => {
                    let d = (f.settle / pr - 1.0) * 100.0;
                    num(ui, format!("{}%", signed(d, 1)), change_color(d));
                }
                None => num(ui, "–".into(), muted()),
            }
            match spot {
                Some(s) if s > 0.0 => {
                    let d = f.settle - s;
                    num(ui, signed(d, 2), change_color(d));
                }
                _ => num(ui, "–".into(), muted()),
            }
            prior = Some(f.settle);
            ui.end_row();
        }
    });
    ui.add_space(dp(4.0));
    dim(
        ui,
        "Cboe's delayed-quote JSON for individual VX contracts needs a browser session, so the strip uses the settlement file: it changes once a day.",
    );
}

// ---- heatmap tab ----------------------------------------------------------------------------

/// Squarified treemap: one rectangle per weight (input order), filling `bounds`.
/// Non-positive weights get empty rectangles.
pub fn treemap(weights: &[f64], bounds: Rect) -> Vec<Rect> {
    let mut out = vec![Rect::from_min_size(bounds.min, vec2(0.0, 0.0)); weights.len()];
    let mut order: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0.0).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]));
    let total: f64 = order.iter().map(|&i| weights[i]).sum();
    if order.is_empty() || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return out;
    }
    let k = f64::from(bounds.width()) * f64::from(bounds.height()) / total;
    let areas: Vec<f64> = order.iter().map(|&i| weights[i] * k).collect();
    let mut rest = bounds;
    let mut start = 0;
    while start < order.len() {
        let side = f64::from(rest.width().min(rest.height()));
        let worst = |row: &[f64]| {
            let s: f64 = row.iter().sum();
            let (mx, mn) = row.iter().fold((f64::MIN, f64::MAX), |(a, b), &x| (a.max(x), b.min(x)));
            (side * side * mx / (s * s)).max(s * s / (side * side * mn))
        };
        let mut end = start + 1;
        while end < order.len() && worst(&areas[start..=end]) <= worst(&areas[start..end]) {
            end += 1;
        }
        let row = &areas[start..end];
        let s: f64 = row.iter().sum();
        let horizontal = rest.width() >= rest.height();
        // The row is a strip along the shorter side.
        let thick = (s / side) as f32;
        let mut cursor = if horizontal { rest.top() } else { rest.left() };
        for (n, a) in row.iter().enumerate() {
            let len = (a / f64::from(thick.max(f32::MIN_POSITIVE))) as f32;
            let len =
                if n + 1 == row.len() { (if horizontal { rest.bottom() } else { rest.right() }) - cursor } else { len };
            out[order[start + n]] = if horizontal {
                Rect::from_min_size(pos2(rest.left(), cursor), vec2(thick, len))
            } else {
                Rect::from_min_size(pos2(cursor, rest.top()), vec2(len, thick))
            };
            cursor += len;
        }
        rest = if horizontal {
            Rect::from_min_max(pos2(rest.left() + thick, rest.top()), rest.max)
        } else {
            Rect::from_min_max(pos2(rest.left(), rest.top() + thick), rest.max)
        };
        start = end;
    }
    out
}

/// Tile colour: grey at 0, saturating green / red at `scale` percent.
pub fn heat_color(pct: Option<f64>, scale: f64) -> Color32 {
    let Some(pct) = pct else { return theme::grid() };
    let t = (pct / scale).clamp(-1.0, 1.0);
    let target = if t >= 0.0 { theme::up() } else { theme::down() };
    lerp_color(theme::card(), target, (t.abs() as f32 * 0.85).max(0.05) * if t.abs() < 1e-9 { 0.0 } else { 1.0 })
}

fn heat_tab(ui: &mut Ui, p: &FutPane, actions: &mut Vec<Action>) {
    let Some(h) = p.heat.as_ref().filter(|h| h.world == p.world) else {
        if p.error[Tab::Heat.idx()].is_none() {
            dim(ui, "loading…");
        }
        return;
    };
    let what = if h.world { "country ETFs (iShares / Franklin)" } else { "SPDR sector ETFs" };
    dim(
        ui,
        format!(
            "Yahoo · {what} · delayed 15 min · updated {} · refreshes every minute · tile area = approximate fixed weights, not live market caps",
            time_str(h.fetched)
        ),
    );
    if h.failed > 0 {
        ui.label(
            RichText::new(format!("{} ETF(s) could not be fetched (grey tiles).", h.failed))
                .font(ui_font(11.5))
                .color(theme::down()),
        );
    }
    ui.add_space(dp(4.0));
    let height = (ui.available_height() - dp(30.0)).max(dp(260.0));
    let (resp, painter) = ui.allocate_painter(vec2(ui.available_width(), height), Sense::click());
    let weights: Vec<f64> = h.tiles.iter().map(|t| t.weight).collect();
    let rects = treemap(&weights, resp.rect);
    let scale = p.period.scale();
    let mut hovered = None;
    for (i, (t, r)) in h.tiles.iter().zip(&rects).enumerate() {
        let r = r.shrink(dp(1.5));
        if r.width() < 2.0 || r.height() < 2.0 {
            continue;
        }
        let change = t.perf.get(p.period);
        let hot = resp.hover_pos().is_some_and(|m| r.contains(m));
        if hot {
            hovered = Some(i);
        }
        let fill = heat_color(change, scale);
        painter.rect_filled(r, 3.0, fill);
        if hot {
            painter.rect_stroke(r, 3.0, Stroke::new(1.5, text()), egui::StrokeKind::Inside);
        }
        let big = r.width() > dp(80.0) && r.height() > dp(54.0);
        let mid = r.width() > dp(46.0) && r.height() > dp(34.0);
        if mid {
            let sym_font = if big { mono(16.0) } else { mono(12.0) };
            let top = r.center().y - if big { dp(18.0) } else { dp(8.0) };
            painter.text(pos2(r.center().x, top), Align2::CENTER_TOP, t.symbol, sym_font, Color32::WHITE);
            let pct_text = change.map_or("n/a".to_string(), |c| format!("{}%", signed(c, 2)));
            painter.text(
                pos2(r.center().x, top + if big { dp(20.0) } else { dp(14.0) }),
                Align2::CENTER_TOP,
                pct_text,
                mono(if big { 13.0 } else { 10.5 }),
                Color32::WHITE,
            );
            if big && r.height() > dp(80.0) {
                painter.text(
                    pos2(r.center().x, top + dp(40.0)),
                    Align2::CENTER_TOP,
                    t.name,
                    ui_font(10.5),
                    Color32::from_white_alpha(200),
                );
            }
        } else if r.width() > dp(26.0) {
            painter.text(r.center(), Align2::CENTER_CENTER, t.symbol, mono(9.0), Color32::WHITE);
        }
    }
    if let Some(i) = hovered {
        let t = &h.tiles[i];
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        let mut lines = vec![format!("{} · {}", t.symbol, t.name)];
        if let Some(pr) = t.price {
            lines.push(format!("Price {pr:.2}"));
        }
        for x in Period::ALL {
            lines.push(format!(
                "{}: {}",
                x.label(),
                t.perf.get(x).map_or("n/a".into(), |c| format!("{}%", signed(c, 2)))
            ));
        }
        lines.push(format!("Weight ≈ {:.1}% (approximate)", t.weight));
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), resp.id.with("tip"), egui::PopupAnchor::Pointer)
            .show(|ui| {
                ui.label(lines.join("\n"));
            });
        if resp.clicked() {
            actions.push(Action::OpenChart(t.symbol.to_string()));
        }
        resp.context_menu(|ui| {
            if ui.button("Open Chart").clicked() {
                actions.push(Action::OpenChart(t.symbol.to_string()));
                ui.close();
            }
            if ui.button("Open Chart as New").clicked() {
                actions.push(Action::OpenChartNew(t.symbol.to_string()));
                ui.close();
            }
            if ui.button("ETF Holdings").clicked() {
                actions.push(Action::Command(format!("ETF {}", t.symbol)));
                ui.close();
            }
        });
    }
    ui.add_space(dp(4.0));
    ui.horizontal(|ui| {
        dim(ui, format!("-{scale:.0}%"));
        for k in 0..=12 {
            let c = -scale + 2.0 * scale * f64::from(k) / 12.0;
            let (r, _) = ui.allocate_exact_size(vec2(dp(16.0), dp(10.0)), Sense::hover());
            ui.painter().rect_filled(r, 1.0, heat_color(Some(c), scale));
        }
        dim(ui, format!("+{scale:.0}%"));
        dim(ui, "· click a tile for its chart, right-click for more");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(r: &Rect) -> f32 {
        r.width() * r.height()
    }

    #[test]
    fn treemap_fills_bounds_without_overlap() {
        let bounds = Rect::from_min_size(pos2(10.0, 20.0), vec2(600.0, 300.0));
        let w = [32.0, 13.5, 10.5, 9.5, 9.0, 8.0, 5.5, 3.0, 2.5, 2.0, 2.0];
        let rects = treemap(&w, bounds);
        assert_eq!(rects.len(), w.len());
        let total: f32 = rects.iter().map(area).sum();
        assert!((total - area(&bounds)).abs() < 1.0, "tiles cover the bounds: {total}");
        for (i, a) in rects.iter().enumerate() {
            assert!(bounds.expand(0.01).contains_rect(*a), "tile {i} inside bounds: {a:?}");
            for b in &rects[i + 1..] {
                let o = a.intersect(*b);
                assert!(o.width() <= 0.01 || o.height() <= 0.01, "overlap {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn treemap_areas_follow_weights() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 200.0));
        let w = [6.0, 3.0, 2.0, 1.0];
        let rects = treemap(&w, bounds);
        for (r, w) in rects.iter().zip(w) {
            let want = area(&bounds) * (w / 12.0) as f32;
            assert!((area(r) - want).abs() < 1.0, "{} vs {want}", area(r));
        }
        // Tiles stay reasonably square.
        for r in &rects {
            let ratio = r.width().max(r.height()) / r.width().min(r.height());
            assert!(ratio < 4.0, "aspect {ratio}");
        }
    }

    #[test]
    fn treemap_keeps_input_order_and_skips_empty() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        let rects = treemap(&[1.0, 0.0, 3.0], bounds);
        assert!(area(&rects[2]) > area(&rects[0]), "larger weight, larger tile, original positions");
        assert_eq!(area(&rects[1]), 0.0);
        assert!(treemap(&[], bounds).is_empty());
        assert!(treemap(&[0.0, 0.0], bounds).iter().all(|r| area(r) == 0.0));
        let one = treemap(&[5.0], bounds);
        assert!((area(&one[0]) - 10_000.0).abs() < 1.0);
    }

    #[test]
    fn heat_colors() {
        assert_eq!(heat_color(None, 3.0), theme::grid());
        let flat = heat_color(Some(0.0), 3.0);
        let up = heat_color(Some(3.0), 3.0);
        let down = heat_color(Some(-3.0), 3.0);
        assert_ne!(up, down);
        assert_ne!(flat, up);
        assert_eq!(heat_color(Some(30.0), 3.0), up, "saturates at the scale");
    }

    #[test]
    fn command_arguments() {
        assert_eq!(FutPane::new("").title(), "Futures · ES");
        assert_eq!(FutPane::new("cl").settings().1, "CL");
        assert_eq!(FutPane::new("VIX").title(), "VIX Term Structure");
        let h = FutPane::new("heat world");
        assert_eq!(h.title(), "World Heatmap");
        assert_eq!(FutPane::new("HEAT").title(), "Sector Heatmap");
        // Unknown roots leave the default selected.
        assert_eq!(FutPane::new("ZZZ").settings().1, "ES");
        let r = FutPane::from_settings("heat", "NG", "ytd", true);
        assert_eq!(r.settings(), ("heat", "NG".to_string(), "YTD", true));
    }
}
