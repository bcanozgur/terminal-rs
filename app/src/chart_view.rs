//! Chart pane: GPU-rendered price/volume/indicator panes with egui-drawn axes, legend,
//! crosshair, drawings and congressional trade markers.

mod earnings_markers;
mod extras;

use std::sync::Arc;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2,
    vec2,
};
use tr_chart::drawings::{self, Anchor, Drawing, Shape, Tool};
use tr_chart::gpu::{ChartCallback, Frame, LineKind, LineRef, Style};
use tr_chart::scale;
use tr_chart::scene::{self, ChartType, Overlay, PricePane, SeriesGpu};
use tr_chart::viewport::{PriceScale, Viewport};
use tr_congress::{Trade, TxType};
use tr_core::{AssetClass, Bar, Security, Timeframe};
use tr_data::Timeliness;

use crate::alerts::{ChartAlert, Condition, Metric, Op, RuleOp, Spec, Target};
use crate::command::{IndicatorSpec, Range};
use crate::congress_view::Action;
use crate::quote_tag;
use crate::studies::{self, BuiltStudy, StudyConfig};
use crate::theme::{self, amber, amber_dim, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{BuiltIndicator, PaneId, Request, Worker, initial_limit};

const AXIS_W: f32 = 66.0;
const TIME_H: f32 = 16.0;
/// Fallback header row, used when the pane's tab strip has no room for the controls.
const HEADER_H: f32 = 24.0;
const RANGE_H: f32 = 18.0;
/// Height of the interval / chart-type segmented controls.
pub const CONTROL_H: f32 = 20.0;
const DEFAULT_VISIBLE: usize = 160;

/// State of the compact alert popup: a new alert at a clicked price, or an existing one.
struct AlertDraft {
    /// The rule being edited (`None` = a new alert).
    edit: Option<u64>,
    price: String,
    op: Op,
    note: String,
    /// Where the pointer was; the popup opens beside it.
    at: Pos2,
    focus_price: bool,
}

/// Condition choices in the alert popup.
const ALERT_OPS: [(Op, &str); 5] = [
    (Op::CrossAbove, "Crossing up"),
    (Op::CrossBelow, "Crossing down"),
    (Op::Cross, "Crossing"),
    (Op::Above, "Greater than or equal"),
    (Op::Below, "Less than or equal"),
];

/// Price at a vertical position within the price pane.
fn price_at_y(s: &PriceScale, top: f32, y: f32) -> f64 {
    s.price_at((y - top) as f64)
}

/// Distance in points within which a click picks an existing alert line or its axis tag.
const ALERT_HIT: f32 = 8.0;

#[derive(Default)]
struct DrawState {
    tool: Option<Tool>,
    /// Points placed so far for the tool being drawn.
    pending: Vec<Anchor>,
    selected: Option<usize>,
    /// The selected drawing being dragged: index, how it looked at the start, pointer travel.
    moving: Option<(usize, Drawing, egui::Vec2)>,
    /// The note whose text is being edited.
    editing: Option<usize>,
    /// The next click on a bar anchors a VWAP.
    pick_avwap: bool,
}

type VpKey = (usize, usize, usize, usize, Option<u64>);

pub struct ChartState {
    pub id: PaneId,
    pub symbol: String,
    pub tf: Timeframe,
    pub chart_type: ChartType,
    pub log: bool,
    pub magnet: bool,
    /// Display currency for prices (`None` = as quoted). Chart-only; not persisted.
    pub currency: Option<String>,
    /// Whether the symbol is already on the watchlist (set by the app each frame).
    pub watched: bool,
    /// Armed price alerts on this security, drawn as lines (set by the app each frame).
    pub alert_lines: Vec<ChartAlert>,
    /// The create / edit alert popup, when open.
    alert_draft: Option<AlertDraft>,
    /// The alert line under the pointer at the last right-click (id, pointer position).
    menu_alert: Option<(u64, Pos2)>,
    /// Price under the pointer at the last right-click, and the price of a horizontal line
    /// drawn there (the context menu's "Alert at" entries).
    menu_at: Option<(f64, Option<f64>)>,
    /// Timestamp of the bar and the drawing under the pointer at the last right-click.
    menu_ts: Option<i64>,
    menu_drawing: Option<usize>,
    /// Plot rectangle, viewport and price scale of the last frame (hit tests before painting).
    last_geom: Option<(Rect, Viewport, PriceScale)>,
    /// An anchored VWAP chosen by click or menu, added once the frame ends.
    pending_avwap: Option<i64>,
    /// Volume profile of the visible bars, keyed by view, row count and last volume.
    vp_cache: Option<(VpKey, Option<tr_core::indicators_ext::VolumeProfile>)>,
    pub show_volume: bool,
    pub show_congress: bool,
    /// Earnings "E" markers (report dates with EPS, surprise and price move).
    pub earnings: crate::earnings_view::ChartEarnings,
    pub indicators: Vec<IndicatorSpec>,
    /// Plug-in studies ([`crate::studies`]) with their settings.
    pub studies: Vec<StudyConfig>,
    pub drawings: Vec<Drawing>,
    pub compare: Option<String>,

    bars: Arc<Vec<Bar>>,
    gpu: Option<SeriesGpu>,
    generation: u64,
    viewport: Option<Viewport>,
    manual_scale: Option<PriceScale>,
    loading: bool,
    paging: bool,
    exhausted: bool,
    error: Option<String>,
    built: Vec<BuiltIndicator>,
    built_studies: Vec<BuiltStudy>,
    /// Study tables collapsed to their title strip (per chart; new charts show them).
    study_tables_hidden: bool,
    /// Width (points) the study tables cover at the plot's right edge; the latest bar is kept
    /// left of it while following.
    table_inset: f32,
    /// The view is pinned to the latest bar (until the user pans away).
    follow_latest: bool,
    compare_line: Option<LineRef>,
    compare_scale: f64,
    markers: Vec<Trade>,
    pending_range: Option<Range>,
    draw: DrawState,
    hover_marker: Option<usize>,
    /// Resolved security for `symbol`.
    sec: Security,
    last_live: Option<std::time::Instant>,
    last_indicator_req: Option<std::time::Instant>,
    /// App-level actions requested from this chart (context menu), drained by the app.
    pub actions: Vec<Action>,
    /// False while a pane is being dragged over the workspace: no crosshair or hover cards.
    pub hover_enabled: bool,
    /// Latest quote for the symbol (set by the app each frame): source, delay, staleness.
    pub quote: Option<tr_data::Quote>,
    /// The workspace drew this chart's controls in the pane's tab strip, so skip the
    /// header row and give the plot the space.
    pub controls_in_strip: bool,
}

impl ChartState {
    pub fn new(id: PaneId, symbol: &str, tf: Timeframe) -> Self {
        Self {
            id,
            symbol: tr_data::security(symbol).key(),
            tf,
            chart_type: if tr_data::security(symbol).class == AssetClass::Govt {
                ChartType::Line
            } else {
                ChartType::Candles
            },
            log: false,
            magnet: false,
            currency: None,
            watched: false,
            alert_lines: Vec::new(),
            alert_draft: None,
            menu_alert: None,
            menu_at: None,
            menu_ts: None,
            menu_drawing: None,
            last_geom: None,
            pending_avwap: None,
            vp_cache: None,
            show_volume: true,
            show_congress: true,
            earnings: Default::default(),
            indicators: vec![IndicatorSpec::Sma(20), IndicatorSpec::Sma(50)],
            studies: studies::defaults(),
            drawings: Vec::new(),
            compare: None,
            bars: Arc::new(Vec::new()),
            gpu: None,
            generation: 0,
            viewport: None,
            manual_scale: None,
            loading: false,
            paging: false,
            exhausted: false,
            error: None,
            built: Vec::new(),
            built_studies: Vec::new(),
            study_tables_hidden: false,
            table_inset: 0.0,
            follow_latest: true,
            compare_line: None,
            compare_scale: 1.0,
            markers: Vec::new(),
            pending_range: None,
            draw: DrawState::default(),
            hover_marker: None,
            sec: tr_data::security(symbol),
            last_live: None,
            last_indicator_req: None,
            actions: Vec::new(),
            hover_enabled: true,
            quote: None,
            controls_in_strip: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// (Re)load history for the current symbol and timeframe.
    pub fn load(&mut self, w: &Worker) {
        self.generation += 1;
        self.loading = true;
        self.paging = false;
        self.exhausted = false;
        self.error = None;
        self.viewport = None;
        self.follow_latest = true;
        self.manual_scale = None;
        self.compare_line = None;
        self.markers.clear();
        self.earnings.clear();
        // Never show the previous symbol's bars under the new title.
        self.bars = Arc::new(Vec::new());
        self.gpu = None;
        self.built.clear();
        self.built_studies.clear();
        w.send(Request::History {
            pane: self.id,
            generation: self.generation,
            symbol: self.symbol.clone(),
            tf: self.tf,
            limit: initial_limit(self.tf),
            currency: self.currency.clone(),
        });
        self.last_live = None;
        if self.sec.has_congress_trades() {
            w.send(Request::Markers { pane: self.id, symbol: self.symbol.clone() });
        }
        if self.sec.class == AssetClass::Equity {
            w.send(Request::Earnings(crate::earnings_worker::EarnReq::Ticker {
                pane: self.id,
                symbol: self.symbol.clone(),
            }));
        }
    }

    pub fn set_symbol(&mut self, symbol: &str, w: &Worker) {
        let sec = tr_data::security(symbol);
        if sec.class == AssetClass::Govt {
            self.chart_type = ChartType::Line;
        } else if self.sec.class == AssetClass::Govt {
            self.chart_type = ChartType::Candles;
        }
        self.symbol = sec.key();
        self.sec = sec;
        self.draw = DrawState::default();
        self.load(w);
    }

    pub fn set_currency(&mut self, currency: Option<&str>, w: &Worker) {
        if self.currency.as_deref() != currency {
            self.currency = currency.map(str::to_string);
            self.load(w);
        }
    }

    /// `TRY->USD` while prices are shown in a currency other than the one quoted.
    fn currency_note(&self) -> Option<String> {
        let to = self.currency.as_deref()?;
        let from = tr_data::native_currency(&self.symbol);
        (from != to).then(|| format!("{from}→{to}"))
    }

    pub fn set_timeframe(&mut self, tf: Timeframe, w: &Worker) {
        if tf != self.tf {
            self.tf = tf;
            self.load(w);
        }
    }

    pub fn set_range(&mut self, r: Range, w: &Worker) {
        self.pending_range = Some(r);
        if r.timeframe() != self.tf {
            self.tf = r.timeframe();
            self.load(w);
        } else {
            self.apply_range();
        }
    }

    fn apply_range(&mut self) {
        let Some(r) = self.pending_range.take() else { return };
        let (Some(v), Some(last)) = (self.viewport.as_mut(), self.bars.last()) else { return };
        let count = match r.span_secs(last.ts) {
            Some(span) => self.bars.len() - self.bars.partition_point(|b| b.ts < last.ts - span),
            None => self.bars.len(),
        };
        *v = Viewport::fit_last(self.bars.len(), count.max(2), v.width_px);
        self.manual_scale = None;
        self.follow_latest = true;
    }

    pub fn toggle_indicator(&mut self, spec: IndicatorSpec, w: &Worker) {
        if let Some(i) = self.indicators.iter().position(|s| s.same_kind(&spec)) {
            if self.indicators[i] == spec {
                self.indicators.remove(i);
            } else {
                self.indicators[i] = spec;
            }
        } else {
            self.indicators.push(spec);
        }
        self.request_indicators(w);
    }

    pub fn clear_indicators(&mut self) {
        self.indicators.clear();
        self.built.clear();
        self.studies.clear();
        self.built_studies.clear();
    }

    /// Add or remove a plug-in study (by id).
    pub fn toggle_study(&mut self, id: &str, w: &Worker) {
        if let Some(i) = self.studies.iter().position(|s| s.id == id) {
            self.studies.remove(i);
            self.built_studies.retain(|b| studies::get(id).is_none_or(|s| s.name() != b.name));
        } else if studies::get(id).is_some() {
            self.studies.push(StudyConfig::new(id));
        }
        self.request_studies(w);
    }

    fn request_studies(&self, w: &Worker) {
        if self.bars.is_empty() {
            return;
        }
        w.send(Request::Studies {
            pane: self.id,
            generation: self.generation,
            symbol: self.symbol.clone(),
            tf: self.tf,
            bars: self.bars.clone(),
            configs: self.studies.iter().filter(|c| studies::get(&c.id).is_some()).cloned().collect(),
        });
    }

    pub fn set_compare(&mut self, symbol: Option<String>, w: &Worker) {
        self.compare = symbol;
        self.compare_line = None;
        self.request_compare(w);
    }

    pub fn start_tool(&mut self, tool: Tool) {
        self.draw = DrawState { tool: Some(tool), ..DrawState::default() };
    }

    /// Esc: cancel an in-progress drawing, then deselect. Returns whether it consumed the key.
    pub fn cancel(&mut self) -> bool {
        if self.draw.tool.is_some() || self.draw.selected.is_some() {
            self.draw = DrawState::default();
            true
        } else {
            false
        }
    }

    pub fn delete_selected(&mut self) -> bool {
        match self.draw.selected.take() {
            Some(i) if i < self.drawings.len() => {
                self.drawings.remove(i);
                true
            }
            _ => false,
        }
    }

    pub fn reset_view(&mut self) {
        if let Some(v) = self.viewport.as_mut() {
            *v = Viewport::fit_last(self.bars.len(), DEFAULT_VISIBLE, v.width_px);
        }
        self.manual_scale = None;
        self.follow_latest = true;
    }

    pub fn pan_bars(&mut self, n: f64) {
        if let Some(v) = self.viewport.as_mut() {
            v.pan_bars(n);
            v.clamp(self.bars.len());
            self.follow_latest = false;
        }
    }

    pub fn zoom(&mut self, factor: f64) {
        if let Some(v) = self.viewport.as_mut() {
            let anchor = v.width_px * 0.85;
            v.zoom(factor, anchor);
            v.clamp(self.bars.len());
        }
    }

    fn request_indicators(&self, w: &Worker) {
        self.request_studies(w);
        if !self.bars.is_empty() {
            w.send(Request::Indicators {
                pane: self.id,
                generation: self.generation,
                bars: self.bars.clone(),
                tf: self.tf,
                specs: self.indicators.clone(),
                palette: theme::palette(),
            });
        }
    }

    fn request_compare(&self, w: &Worker) {
        if let (Some(sym), false) = (&self.compare, self.bars.is_empty()) {
            w.send(Request::Compare {
                pane: self.id,
                generation: self.generation,
                symbol: sym.clone(),
                tf: self.tf,
                base: self.bars.clone(),
            });
        }
    }

    // ---- worker responses -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn on_history(
        &mut self,
        generation: u64,
        bars: Arc<Vec<Bar>>,
        gpu: SeriesGpu,
        prepended: usize,
        from_cache: bool,
        exhausted: bool,
        live: bool,
        w: &Worker,
    ) {
        if generation != self.generation {
            return;
        }
        if live {
            self.last_live = Some(std::time::Instant::now());
        }
        let old_len = self.bars.len();
        let new_len = bars.len();
        self.bars = bars;
        self.gpu = Some(gpu);
        self.paging = false;
        self.exhausted = exhausted || self.exhausted && prepended == 0;
        if !from_cache {
            self.loading = false;
        }
        self.error = None;
        match self.viewport.as_mut() {
            None => {
                self.viewport = Some(Viewport::fit_last(new_len, DEFAULT_VISIBLE, 800.0));
            }
            Some(v) if prepended > 0 => v.right += prepended as f64,
            Some(v) => {
                // A refresh replaced the data: keep the same distance from the latest bar.
                let from_end = old_len as f64 - v.right;
                v.right = new_len as f64 - from_end;
            }
        }
        self.apply_range();
        // Live ticks only move the last bar: refresh indicators at most every 1.5 s and
        // leave the compare series alone.
        let due = self.last_indicator_req.is_none_or(|t| t.elapsed().as_millis() >= 1500);
        if !live || due {
            self.request_indicators(w);
            self.last_indicator_req = Some(std::time::Instant::now());
        }
        if !live {
            self.request_compare(w);
        }
    }

    pub fn on_history_error(&mut self, generation: u64, error: String) {
        if generation == self.generation {
            self.loading = false;
            self.paging = false;
            self.exhausted = true;
            if self.bars.is_empty() {
                self.error = Some(error);
            }
        }
    }

    pub fn on_indicators(&mut self, generation: u64, built: Vec<BuiltIndicator>) {
        if generation == self.generation {
            self.built = built;
        }
    }

    pub fn on_studies(&mut self, generation: u64, built: Vec<BuiltStudy>) {
        if generation == self.generation {
            self.built_studies = built;
        }
    }

    pub fn on_compare(&mut self, generation: u64, symbol: &str, line: LineRef) {
        if generation == self.generation && self.compare.as_deref() == Some(symbol) {
            self.compare_line = Some(line);
        }
    }

    pub fn on_earnings(&mut self, symbol: &str, data: tr_data::earnings::TickerEarnings) {
        if tr_data::security(symbol).key() == self.symbol {
            self.earnings.set(symbol, data);
        }
    }

    pub fn on_markers(&mut self, symbol: &str, trades: Vec<Trade>) {
        if symbol == self.symbol {
            self.markers = trades;
        }
    }

    // ---- coordinate helpers -----------------------------------------------------------

    /// Fractional bar index for a timestamp (extrapolated past either end).
    fn index_of_ts(&self, ts: i64) -> f64 {
        let b = &self.bars;
        let (Some(first), Some(last)) = (b.first(), b.last()) else { return 0.0 };
        let step = self.tf.seconds() as f64;
        if ts > last.ts {
            return (b.len() - 1) as f64 + (ts - last.ts) as f64 / step + 0.5;
        }
        if ts < first.ts {
            return (ts - first.ts) as f64 / step + 0.5;
        }
        b.partition_point(|x| x.ts <= ts).saturating_sub(1) as f64 + 0.5
    }

    fn ts_of_index(&self, idx: f64) -> i64 {
        let b = &self.bars;
        let Some(last) = b.last() else { return 0 };
        let i = idx.floor();
        if i >= b.len() as f64 {
            return last.ts + ((i - (b.len() - 1) as f64) * self.tf.seconds() as f64) as i64;
        }
        if i < 0.0 {
            return b[0].ts + (i * self.tf.seconds() as f64) as i64;
        }
        b[i as usize].ts
    }

    // ---- rendering --------------------------------------------------------------------

    /// Draw the pane into `rect`. Returns true if the pane was clicked (focus request).
    /// Width of [`Self::controls`] (interval and chart-type segmented controls).
    pub fn controls_width(ui: &Ui) -> f32 {
        let tf: Vec<&str> = Timeframe::ALL.iter().map(|t| t.label()).collect();
        let ct: Vec<&str> = ChartType::ALL.iter().map(|c| c.label()).collect();
        widgets::segmented_width(ui, &tf, CONTROL_H) + widgets::segmented_width(ui, &ct, CONTROL_H) + 8.0 + 8.0 + 24.0
    }

    /// Interval and chart-type controls, laid out left to right in `ui`. Returns whether
    /// one was clicked.
    pub fn controls(&mut self, ui: &mut Ui, w: &Worker) -> bool {
        let mut clicked = false;
        let tf_labels: Vec<&str> = Timeframe::ALL.iter().map(|t| t.label()).collect();
        let selected_tf = Timeframe::ALL.iter().position(|t| *t == self.tf);
        if let Some(i) = widgets::segmented(ui, (self.id, "tf"), &tf_labels, selected_tf, CONTROL_H) {
            self.set_timeframe(Timeframe::ALL[i], w);
            clicked = true;
        }
        ui.add_space(8.0);
        let ct_labels: Vec<&str> = ChartType::ALL.iter().map(|c| c.label()).collect();
        let selected_ct = ChartType::ALL.iter().position(|c| *c == self.chart_type);
        if let Some(i) = widgets::segmented(ui, (self.id, "ct"), &ct_labels, selected_ct, CONTROL_H) {
            self.chart_type = ChartType::ALL[i];
            clicked = true;
        }
        ui.add_space(8.0);
        clicked |= self.watch_button(ui);
        clicked
    }

    /// Star toggle: on the watchlist (filled) or not (hollow).
    fn watch_button(&mut self, ui: &mut Ui) -> bool {
        let tip = if self.watched { "Remove from Watchlist" } else { "Add to Watchlist" };
        let resp = widgets::star_button(ui, self.watched, tip);
        if resp.clicked() {
            self.actions.push(if self.watched {
                Action::RemoveFromWatchlist(self.symbol.clone())
            } else {
                Action::AddToWatchlist(self.symbol.clone())
            });
            self.watched = !self.watched;
        }
        resp.clicked()
    }

    pub fn show(&mut self, ui: &mut Ui, rect: Rect, focused: bool, w: &Worker) -> bool {
        let painter = ui.painter_at(rect);
        // Sits under a tab strip inside a card: square top, rounded bottom.
        let r = theme::CARD_RADIUS.saturating_sub(1);
        painter.rect_filled(rect, CornerRadius { nw: 0, ne: 0, sw: r, se: r }, theme::card());
        let mut clicked = false;

        // Controls live in the tab strip when it has room; otherwise a slim header row.
        let header_h = if self.controls_in_strip { 0.0 } else { HEADER_H };
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), header_h));
        if header_h > 0.0 {
            painter.hline(header.x_range(), header.bottom(), Stroke::new(1.0, theme::border()));
            let mut hui = ui.new_child(
                UiBuilder::new()
                    .max_rect(header.shrink2(vec2(8.0, 0.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center))
                    .id_salt((self.id, "header")),
            );
            hui.spacing_mut().item_spacing.x = 6.0;
            hui.label(RichText::new(&self.sec.ticker).font(mono(12.0)).color(amber()));
            if hui.available_width() > Self::controls_width(&hui) {
                clicked |= self.controls(&mut hui, w);
            } else {
                // Narrow pane: the same controls as compact menus.
                hui.menu_button(RichText::new(self.tf.label()).font(ui_font(11.5)), |ui| {
                    for tf in Timeframe::ALL {
                        if ui.radio(self.tf == tf, tf.label()).clicked() {
                            self.set_timeframe(tf, w);
                            clicked = true;
                            ui.close();
                        }
                    }
                });
                hui.menu_button(RichText::new(self.chart_type.label()).font(ui_font(11.5)), |ui| {
                    for ct in ChartType::ALL {
                        if ui.radio(self.chart_type == ct, ct.label()).clicked() {
                            self.chart_type = ct;
                            clicked = true;
                            ui.close();
                        }
                    }
                });
                clicked |= self.watch_button(&mut hui);
            }
        }
        // The badge only says the chart just refreshed; delayed data is not live, so it says "SYNC".
        let badge = match self.quote.as_ref().map(|q| q.timeliness()) {
            Some(Timeliness::Simulated) => None,
            Some(Timeliness::Delayed(_) | Timeliness::EndOfDay) => Some(("SYNC", muted())),
            _ => Some(("LIVE", theme::up())),
        };
        if let Some((label, color)) = badge.filter(|_| self.last_live.is_some_and(|t| t.elapsed().as_secs() < 5)) {
            // Pulsing live dot in the plot's top-right corner (clear of the price axis labels).
            let pulse = 0.6 + 0.4 * ((ui.input(|i| i.time) * 3.0).sin() as f32).abs();
            let c = pos2(rect.right() - AXIS_W + 12.0, header.bottom() + 10.0);
            painter.circle_filled(c, 3.0, color.gamma_multiply(pulse));
            painter.text(c + vec2(7.0, 0.0), Align2::LEFT_CENTER, label, ui_font(10.0), color);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Bottom quick-range row.
        let range_row = Rect::from_min_max(pos2(rect.left(), rect.bottom() - RANGE_H), rect.max);
        let mut rx = range_row.left() + 6.0;
        for r in Range::ALL {
            let lr = Rect::from_min_size(pos2(rx, range_row.top()), vec2(r.label().len() as f32 * 7.4 + 8.0, RANGE_H));
            let resp = ui.interact(lr, ui.id().with((self.id, "range", r.label())), Sense::click());
            painter.text(
                lr.center(),
                Align2::CENTER_CENTER,
                r.label(),
                mono(11.0),
                if resp.hovered() { amber() } else { muted() },
            );
            if resp.clicked() {
                self.set_range(r, w);
                clicked = true;
            }
            rx = lr.right();
        }
        let status = match (&self.error, self.loading, self.paging) {
            (Some(e), _, _) => format!("ERR {e}"),
            (_, true, _) => "loading…".into(),
            (_, _, true) => "loading history…".into(),
            _ => format!(
                "{} bars{}{}{}",
                self.bars.len(),
                self.currency_note().map(|n| format!("  {n}")).unwrap_or_default(),
                if self.log { "  LOG" } else { "" },
                if self.magnet { "  MAGNET" } else { "" }
            ),
        };
        painter.text(
            pos2(range_row.right() - 6.0, range_row.center().y),
            Align2::RIGHT_CENTER,
            status,
            mono(10.5),
            muted(),
        );

        let body = Rect::from_min_max(pos2(rect.left(), header.bottom()), pos2(rect.right(), range_row.top()));
        let plot_all = Rect::from_min_max(body.min, pos2(body.right() - AXIS_W, body.bottom() - TIME_H));
        if plot_all.width() < 40.0 || plot_all.height() < 40.0 {
            return clicked;
        }

        let Some(gpu) = self.gpu.clone() else {
            let msg = self.error.clone().unwrap_or_else(|| format!("Loading {} {}…", self.symbol, self.tf));
            painter.text(plot_all.center(), Align2::CENTER_CENTER, msg, mono(13.0), amber_dim());
            return clicked;
        };

        // Split plot height between the price pane and oscillator sub-panes (indicators, then studies).
        let subs = self.sub_panes();
        let n_sub = subs.len().min(4);
        let sub_h = if n_sub > 0 { (plot_all.height() * 0.2).clamp(50.0, 160.0) } else { 0.0 };
        let price_rect =
            Rect::from_min_max(plot_all.min, pos2(plot_all.right(), plot_all.bottom() - sub_h * n_sub as f32));

        let len = self.bars.len();
        let v =
            self.viewport.get_or_insert_with(|| Viewport::fit_last(len, DEFAULT_VISIBLE, price_rect.width() as f64));
        v.set_width(price_rect.width() as f64);

        // ---- input ----
        let plot_resp = ui.interact(plot_all, ui.id().with((self.id, "plot")), Sense::click_and_drag());
        let axis_rect =
            Rect::from_min_max(pos2(price_rect.right(), price_rect.top()), pos2(body.right(), price_rect.bottom()));
        let axis_resp = ui.interact(axis_rect, ui.id().with((self.id, "paxis")), Sense::click_and_drag());
        let time_rect =
            Rect::from_min_max(pos2(plot_all.left(), plot_all.bottom()), pos2(plot_all.right(), body.bottom()));
        let time_resp = ui.interact(time_rect, ui.id().with((self.id, "taxis")), Sense::click_and_drag());
        if plot_resp.clicked() || plot_resp.drag_started() || axis_resp.clicked() || time_resp.clicked() {
            clicked = true;
        }
        let hover = plot_resp.hover_pos().filter(|p| self.hover_enabled && plot_all.contains(*p));
        if axis_resp.double_clicked() {
            self.alert_draft = None;
        }
        // The menu opens after the drawings pass has recorded the right-clicked price.
        let menu_ok = self.draw.tool.is_none();

        if plot_resp.hovered() || axis_resp.hovered() || time_resp.hovered() {
            let (scroll, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            let v = self.viewport.as_mut().expect("viewport");
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                let anchor = (p.x - price_rect.left()).clamp(0.0, price_rect.width()) as f64;
                if scroll.y != 0.0 {
                    v.zoom((scroll.y as f64 * 0.003).exp(), anchor);
                }
                if zoom_delta != 1.0 {
                    v.zoom(zoom_delta as f64, anchor);
                }
            }
            if scroll.x != 0.0 {
                v.pan_px(scroll.x as f64);
                self.follow_latest = false;
            }
        }
        self.begin_drawing_drag(ui, &plot_resp);
        if self.draw.tool.is_none() && self.draw.moving.is_none() && plot_resp.dragged() {
            let d = plot_resp.drag_delta();
            self.viewport.as_mut().expect("viewport").pan_px(d.x as f64);
            if d.x != 0.0 {
                self.follow_latest = false;
            }
            if let Some(s) = self.manual_scale.as_mut() {
                s.pan_px(d.y as f64);
            }
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
        if time_resp.dragged() {
            let v = self.viewport.as_mut().expect("viewport");
            let w_px = v.width_px;
            v.zoom((-time_resp.drag_delta().x as f64 * 0.005).exp(), w_px);
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if time_resp.double_clicked() || plot_resp.double_clicked() && self.draw.tool.is_none() {
            self.reset_view();
        }
        let v = *self.viewport.as_ref().expect("viewport");
        let mut v = v;
        // Following the latest bar: keep it clear of the study tables (and re-pin when the
        // user pans back to the right end). Recomputed every frame, so resizing the pane,
        // zooming and hiding/showing a table all keep the gap right.
        let latest_right = len as f64 + 3.0 + self.table_inset as f64 / v.bar_px;
        if !self.follow_latest && v.right >= latest_right {
            self.follow_latest = true;
        }
        if self.follow_latest {
            v.right = latest_right;
        }
        v.clamp(len);
        self.viewport = Some(v);

        // Page older history when the left edge approaches loaded data.
        if !self.paging && !self.loading && !self.exhausted && v.left() < v.visible_bars() * 0.3 {
            self.paging = true;
            w.send(Request::MoreHistory {
                pane: self.id,
                generation: self.generation,
                symbol: self.symbol.clone(),
                tf: self.tf,
                existing: self.bars.clone(),
                limit: initial_limit(self.tf),
                currency: self.currency.clone(),
            });
        }

        // ---- price pane GPU frame ----
        let mut overlays: Vec<Overlay> = self
            .built
            .iter()
            .filter(|b| !b.spec.is_pane())
            .flat_map(|b| b.overlays.iter().map(|o| o.1.clone()))
            .chain(self.built_studies.iter().flat_map(|b| b.lines.iter().map(|l| l.1.clone())))
            .collect();
        if let Some(c) = &self.compare_line {
            let (lo, hi) = self.viewport.as_ref().map(|v| v.visible_range(len)).unwrap_or((0, len));
            let scale = scene::rebase_factor(&gpu.closes, c, lo, hi);
            self.compare_scale = scale;
            overlays.push(Overlay {
                line: c.clone(),
                color: theme::rgba(theme::series(5)),
                kind: LineKind::Line,
                scale,
            });
        }
        let style = Style {
            up: theme::rgba(theme::candle_up()),
            down: theme::rgba(theme::candle_down()),
            color: theme::rgba(amber()),
            pixels_per_point: ui.ctx().pixels_per_point(),
            ..Style::default()
        };
        let out = scene::price_pane(&PricePane {
            series: &gpu,
            chart_type: self.chart_type,
            show_volume: self.show_volume,
            overlays: &overlays,
            viewport: &v,
            height_px: price_rect.height() as f64,
            log: self.log,
            manual_scale: self.manual_scale.map(|mut s| {
                s.height_px = price_rect.height() as f64;
                s
            }),
            style,
        });
        let mut scale = out.scale;
        scale.height_px = price_rect.height() as f64;
        if axis_resp.dragged() {
            let mut s = self.manual_scale.unwrap_or(scale);
            s.stretch((axis_resp.drag_delta().y as f64 * -0.01).exp());
            self.manual_scale = Some(s);
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }
        if axis_resp.double_clicked() {
            self.manual_scale = None;
        }

        // Grid first (under the GPU geometry is not possible inside one pass, so draw it
        // faintly; candles are opaque on top visually).
        let price_painter = ui.painter_at(price_rect);
        let ticks = scale::price_ticks(scale.lo, scale.hi, (price_rect.height() / 42.0).max(2.0) as usize);
        let step = if ticks.len() > 1 { ticks[1] - ticks[0] } else { 1.0 };
        let dec = scale::decimals_for(step).max(self.sec.decimals(scale.hi).min(3));
        for t in &ticks {
            let y = price_rect.top() + scale.y_of(*t) as f32;
            price_painter.hline(price_rect.x_range(), y, Stroke::new(1.0, theme::grid()));
        }
        let frame_no = ui.ctx().cumulative_frame_nr();
        let frame = Frame { pane: self.id * 8, frame_no, cmds: out.cmds };
        ui.painter().add(tr_chart::egui_wgpu::Callback::new_paint_callback(price_rect, ChartCallback(Arc::new(frame))));

        // Sub panes.
        let mut sub_rects = Vec::new();
        for (k, sp) in subs.iter().take(n_sub).enumerate() {
            let top = price_rect.bottom() + sub_h * k as f32;
            let r = Rect::from_min_max(pos2(plot_all.left(), top), pos2(plot_all.right(), top + sub_h));
            let ovs: Vec<Overlay> = sp.overlays.iter().map(|o| o.1.clone()).collect();
            let mut s = style;
            s.thickness_px = 1.3;
            let so = scene::sub_pane(&ovs, &v, r.height() as f64, sp.fixed, s);
            let p = ui.painter_at(r);
            p.hline(r.x_range(), r.top(), Stroke::new(1.0, theme::border()));
            for (lvl, c) in &sp.levels {
                p.hline(r.x_range(), r.top() + so.scale.y_of(*lvl) as f32, Stroke::new(1.0, *c));
            }
            let frame = Frame { pane: self.id * 8 + 1 + k as u64, frame_no, cmds: so.cmds };
            ui.painter().add(tr_chart::egui_wgpu::Callback::new_paint_callback(r, ChartCallback(Arc::new(frame))));
            sub_rects.push((r, k, so.scale));
        }

        // ---- axes ----
        let axis_painter = ui.painter_at(Rect::from_min_max(pos2(plot_all.right(), body.top()), body.max));
        axis_painter.vline(plot_all.right(), body.y_range(), Stroke::new(1.0, theme::border()));
        let positive_prices = self.bars.last().is_some_and(|b| b.low > 0.0);
        for t in &ticks {
            let y = price_rect.top() + scale.y_of(*t) as f32;
            // The volume headroom can dip below zero; don't label impossible prices.
            if positive_prices && *t < 0.0 {
                continue;
            }
            if y > price_rect.top() + 6.0 && y < price_rect.bottom() - 6.0 {
                axis_painter.text(
                    pos2(plot_all.right() + 6.0, y),
                    Align2::LEFT_CENTER,
                    scale::format_price(*t, dec),
                    mono(11.0),
                    muted(),
                );
            }
        }
        for (r, k, s) in &sub_rects {
            let label = subs[*k].label.clone();
            ui.painter_at(*r).text(r.left_top() + vec2(6.0, 3.0), Align2::LEFT_TOP, label, mono(10.5), muted());
            for t in scale::price_ticks(s.lo, s.hi, 3) {
                let y = r.top() + s.y_of(t) as f32;
                if y > r.top() + 8.0 && y < r.bottom() - 4.0 {
                    axis_painter.text(
                        pos2(plot_all.right() + 6.0, y),
                        Align2::LEFT_CENTER,
                        if t.abs() >= 10_000.0 { scale::format_volume(t) } else { format!("{t:.1}") },
                        mono(10.5),
                        muted(),
                    );
                }
            }
        }
        // Time axis labels (sampled so 1M visible bars stays cheap).
        let (lo, hi) = v.visible_range(len);
        let time_painter = ui.painter_at(time_rect);
        time_painter.hline(time_rect.x_range(), time_rect.top(), Stroke::new(1.0, theme::border()));
        if hi > lo {
            let stride = ((hi - lo) / 600).max(1);
            let idxs: Vec<usize> = (lo..hi).step_by(stride).collect();
            let times: Vec<i64> = idxs.iter().map(|&i| self.bars[i].ts).collect();
            let gap = 90.0 / (v.bar_px * stride as f64);
            for (j, label) in scale::time_ticks(&times, 0, times.len(), gap, self.tf) {
                let x = price_rect.left() + v.x_center(idxs[j]) as f32;
                ui.painter_at(plot_all).vline(x, plot_all.y_range(), Stroke::new(1.0, theme::grid()));
                time_painter.text(pos2(x, time_rect.center().y), Align2::CENTER_CENTER, label, mono(10.5), muted());
            }
        }

        // Last price line + axis tag.
        if let Some(last) = self.bars.last() {
            let y = price_rect.top() + scale.y_of(last.close) as f32;
            if price_rect.y_range().contains(y) {
                let col = if last.is_up() { theme::up() } else { theme::down() };
                let mut xx = price_rect.left();
                while xx < price_rect.right() {
                    price_painter.hline(
                        xx..=(xx + 3.0).min(price_rect.right()),
                        y,
                        Stroke::new(1.0, col.gamma_multiply(0.6)),
                    );
                    xx += 6.0;
                }
                let tag = Rect::from_min_size(pos2(plot_all.right() + 1.0, y - 8.0), vec2(AXIS_W - 2.0, 16.0));
                axis_painter.rect_filled(tag, 0.0, col);
                axis_painter.text(
                    tag.left_center() + vec2(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    scale::format_price(last.close, self.sec.decimals(last.close)),
                    mono(11.0),
                    Color32::BLACK,
                );
            }
        }

        // ---- studies: fills, segments, markers (under the user's drawings) ----
        let mut study_tip = None;
        if !self.built_studies.is_empty() {
            let f = studies::Frame { rect: price_rect, v: &v, s: &scale, bars: &self.bars };
            for b in &self.built_studies {
                studies::paint_fills(ui, &f, &b.output.fills);
                studies::paint_segments(ui, &f, &b.output.segments);
                if let Some(t) = studies::paint_markers(ui, &f, &b.output.markers, hover) {
                    study_tip = Some(t);
                }
            }
        }

        // ---- indicator shading (Ichimoku cloud, volume profile), under the drawings ----
        self.paint_cloud(ui, price_rect, &v, &scale);
        self.paint_profile(ui, price_rect, &v, &scale);

        // ---- drawings ----
        self.paint_drawings(ui, price_rect, &v, &scale, hover, &plot_resp);
        if let Some(ts) = self.pending_avwap.take() {
            self.toggle_indicator(IndicatorSpec::AnchoredVwap(ts), w);
        }
        self.alert_overlay(ui, price_rect, &axis_resp, &scale);
        if menu_ok {
            plot_resp.context_menu(|ui| self.context_menu(ui, w));
        }

        // ---- congress markers ----
        self.hover_marker = None;
        if self.show_congress {
            self.paint_markers(ui, price_rect, &v, &scale, hover);
        }
        if self.earnings.show {
            self.paint_earnings(ui, price_rect, &v, hover);
        }

        // ---- crosshair + legend ----
        let hovered_bar = hover.and_then(|p| v.bar_at((p.x - price_rect.left()) as f64, len));
        if let Some(p) = hover {
            let ch = Stroke::new(1.0, Color32::from_gray(110));
            let plot_painter = ui.painter_at(plot_all);
            let xi = hovered_bar.map(|i| price_rect.left() + v.x_center(i) as f32).unwrap_or(p.x);
            dashed_v(&plot_painter, xi, plot_all.y_range(), ch);
            dashed_h(&plot_painter, plot_all.x_range(), p.y, ch);
            if price_rect.contains(p) {
                let price = scale.price_at((p.y - price_rect.top()) as f64);
                let tag = Rect::from_min_size(pos2(plot_all.right() + 1.0, p.y - 8.0), vec2(AXIS_W - 2.0, 16.0));
                axis_painter.rect_filled(tag, 0.0, Color32::from_gray(70));
                axis_painter.text(
                    tag.left_center() + vec2(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    scale::format_price(price, self.sec.decimals(price)),
                    mono(11.0),
                    text(),
                );
            }
            if let Some(i) = hovered_bar {
                let label = scale::crosshair_label(self.bars[i].ts, self.tf);
                let gw = label.len() as f32 * 7.0 + 10.0;
                let tag = Rect::from_center_size(pos2(xi, time_rect.center().y), vec2(gw, TIME_H));
                time_painter.rect_filled(tag, 0.0, Color32::from_gray(70));
                time_painter.text(tag.center(), Align2::CENTER_CENTER, label, mono(10.5), text());
            }
            if self.draw.tool.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            }
        }
        self.paint_legend(ui, price_rect, hovered_bar.unwrap_or(len.saturating_sub(1)), &sub_rects, &subs);
        let has_trade_buttons = matches!(self.sec.class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf);
        if has_trade_buttons {
            clicked |= self.trade_buttons(ui, plot_all);
        }
        // Study tables in the top-right corner, under the Buy/Sell buttons, stacked.
        let mut table_top = if has_trade_buttons { 32.0 } else { 6.0 };
        let mut toggle = false;
        let mut inset: f32 = 0.0;
        for (k, b) in self.built_studies.iter().enumerate() {
            if let Some(t) = &b.output.table {
                let id = ui.id().with((self.id, "study-table", k));
                let (r, clicked) =
                    studies::paint_table(ui, price_rect, table_top, &b.name, t, self.study_tables_hidden, id);
                toggle |= clicked;
                if !self.study_tables_hidden {
                    inset = inset.max(price_rect.right() - r.left() + 10.0);
                }
                table_top = r.bottom() - price_rect.top() + 6.0;
            }
        }
        if toggle {
            self.study_tables_hidden = !self.study_tables_hidden;
            clicked = true;
        }
        if (inset - self.table_inset).abs() > 0.5 {
            // The table appeared, changed width or was hidden: redraw with the new gap.
            self.table_inset = inset;
            ui.ctx().request_repaint();
        }
        if let Some((at, tip)) = study_tip {
            studies::tooltip_card(ui, at, &tip);
        }

        let _ = focused; // the surrounding card shows focus
        clicked
    }

    /// Buy / Sell buttons in the plot's top-right corner: they open the order ticket.
    fn trade_buttons(&mut self, ui: &mut Ui, plot: Rect) -> bool {
        let h = 20.0;
        let mut x = plot.right() - 8.0;
        let mut clicked = false;
        for (label, cmd, color, tip) in
            [("SELL", "SELL", theme::down(), "Sell (⇧S)"), ("BUY", "BUY", theme::up(), "Buy (⇧B)")]
        {
            let g = ui.painter().layout_no_wrap(label.into(), mono(11.0), Color32::WHITE);
            let w = g.size().x + 18.0;
            let r = Rect::from_min_size(pos2(x - w, plot.top() + 6.0), vec2(w, h));
            x -= w + 6.0;
            let resp = ui.interact(r, ui.id().with((self.id, "trade", label)), Sense::click()).on_hover_text(tip);
            let fill = if resp.hovered() { color } else { color.gamma_multiply(0.8) };
            let p = ui.painter();
            p.rect_filled(r, h / 2.0, fill);
            p.galley(r.center() - g.size() / 2.0, g, Color32::WHITE);
            if resp.clicked() {
                self.actions.push(Action::Command(format!("{cmd} {}", self.symbol)));
                clicked = true;
            }
        }
        clicked
    }

    /// Amber alert lines across the whole plot with their price on the axis, the click-to-create
    /// popup on the price axis, and its edit / create / cancel handling.
    fn alert_overlay(&mut self, ui: &Ui, rect: Rect, axis: &egui::Response, s: &PriceScale) {
        if self.currency.is_some() {
            self.alert_draft = None;
            return; // alerts are in the quoted currency
        }
        let axis_area = Rect::from_min_max(pos2(rect.right(), rect.top()), pos2(rect.right() + AXIS_W, rect.bottom()));
        let p = ui.painter_at(rect.union(axis_area));
        let color = theme::amber();
        let y_of = |level: f64| rect.top() + s.y_of(level) as f32;
        for a in &self.alert_lines {
            let y = y_of(a.level);
            if y < rect.top() || y > rect.bottom() {
                continue;
            }
            p.hline(rect.x_range(), y, Stroke::new(1.2, color));
            let price = scale::format_price(a.level, self.sec.decimals(a.level));
            let tag = Rect::from_min_size(pos2(rect.right() + 1.0, y - 8.0), vec2(AXIS_W - 2.0, 16.0));
            p.rect_filled(tag, 0.0, color);
            p.text(tag.left_center() + vec2(5.0, 0.0), Align2::LEFT_CENTER, price, mono(11.0), Color32::BLACK);
            let name = ALERT_OPS.iter().find(|(o, _)| *o == a.op).map_or("", |(_, n)| n);
            let label = if a.note.is_empty() { format!("ALRT {name}") } else { format!("ALRT {name} · {}", a.note) };
            p.text(pos2(rect.left() + 6.0, y - 2.0), Align2::LEFT_BOTTOM, label, mono(10.0), color);
        }

        // A click on the axis: an alert tag edits that alert, anywhere else starts a new one.
        if axis.clicked()
            && let Some(pos) = axis.interact_pointer_pos().filter(|p| rect.y_range().contains(p.y))
        {
            let hit = self.alert_lines.iter().find(|a| (y_of(a.level) - pos.y).abs() < ALERT_HIT).cloned();
            self.alert_draft = Some(match hit {
                Some(a) => self.draft_for(&a, pos),
                None => {
                    let price = price_at_y(s, rect.top(), pos.y);
                    self.new_draft(price, pos)
                }
            });
        }
        if axis.hovered()
            && let Some(pos) = axis.hover_pos()
            && self.alert_lines.iter().any(|a| (y_of(a.level) - pos.y).abs() < ALERT_HIT)
        {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // Right-clicking an alert line remembers it for the context menu.
        if axis.secondary_clicked() || ui.input(|i| i.pointer.secondary_clicked()) {
            self.menu_alert = ui.input(|i| i.pointer.interact_pos()).and_then(|pos| {
                self.alert_lines
                    .iter()
                    .find(|a| (y_of(a.level) - pos.y).abs() < ALERT_HIT && rect.union(axis_area).contains(pos))
                    .map(|a| (a.id, pos))
            });
        }
        self.alert_popup(ui, rect);
    }

    fn new_draft(&self, price: f64, at: Pos2) -> AlertDraft {
        let last = self.bars.last().map(|b| b.close);
        AlertDraft {
            edit: None,
            price: format!("{price:.d$}", d = self.sec.decimals(price)),
            op: crate::alerts::default_chart_op(price, last),
            note: String::new(),
            at,
            focus_price: true,
        }
    }

    fn draft_for(&self, a: &ChartAlert, at: Pos2) -> AlertDraft {
        AlertDraft {
            edit: Some(a.id),
            price: format!("{:.d$}", a.level, d = self.sec.decimals(a.level)),
            op: a.op,
            note: a.note.clone(),
            at,
            focus_price: true,
        }
    }

    /// The compact popup: price, condition, note, Create / Cancel (Save / Delete when editing).
    fn alert_popup(&mut self, ui: &Ui, rect: Rect) {
        let Some(mut d) = self.alert_draft.take() else { return };
        let w = 230.0;
        let x = (rect.right() - w - 6.0).max(rect.left());
        let y = (d.at.y - 60.0).clamp(rect.top(), (rect.bottom() - 150.0).max(rect.top()));
        let mut keep = true;
        let mut submit = false;
        let mut delete = false;
        egui::Area::new(ui.id().with((self.id, "alert-popup")))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(x, y))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(w);
                    ui.label(
                        RichText::new(format!(
                            "{} alert · {} · {}",
                            if d.edit.is_some() { "Edit" } else { "New" },
                            self.sec.ticker,
                            self.tf.label()
                        ))
                        .font(mono(11.5))
                        .color(amber()),
                    );
                    egui::Grid::new("alert-grid").num_columns(2).spacing([6.0, 4.0]).show(ui, |ui| {
                        ui.label("Price");
                        let r = ui.add(egui::TextEdit::singleline(&mut d.price).desired_width(130.0).font(mono(12.0)));
                        if d.focus_price {
                            r.request_focus();
                            d.focus_price = false;
                        }
                        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.end_row();
                        ui.label("When");
                        egui::ComboBox::from_id_salt("alert-op")
                            .width(130.0)
                            .selected_text(ALERT_OPS.iter().find(|(o, _)| *o == d.op).map_or("", |(_, n)| n))
                            .show_ui(ui, |ui| {
                                for (o, name) in ALERT_OPS {
                                    ui.selectable_value(&mut d.op, o, name);
                                }
                            });
                        ui.end_row();
                        ui.label("Note");
                        ui.add(egui::TextEdit::singleline(&mut d.note).desired_width(130.0).hint_text("optional"));
                        ui.end_row();
                    });
                    let valid = d.price.trim().parse::<f64>().is_ok_and(|p| p.is_finite() && p > 0.0);
                    ui.horizontal(|ui| {
                        let label = if d.edit.is_some() { "Save" } else { "Create" };
                        submit |= ui.add_enabled(valid, egui::Button::new(label)).clicked();
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                        if d.edit.is_some() && ui.button("Delete").clicked() {
                            delete = true;
                        }
                    });
                });
            });
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep = false;
        }
        let price = d.price.trim().parse::<f64>().ok().filter(|p| p.is_finite() && *p > 0.0);
        if delete && let Some(id) = d.edit {
            self.actions.push(Action::AlertOp(RuleOp::Delete(id)));
        } else if submit && let Some(price) = price {
            let cond = Condition { metric: Metric::Price, op: d.op, target: Target::Level(price) };
            let note = d.note.trim().to_string();
            self.actions.push(match d.edit {
                Some(id) => Action::AlertOp(RuleOp::Edit(id, cond, note)),
                None => Action::AddAlert(Spec {
                    symbol: Some(self.symbol.clone()),
                    cond,
                    repeat: false,
                    tf: Some(self.tf),
                    note,
                }),
            });
        } else if keep {
            self.alert_draft = Some(d);
        }
    }

    /// Arm a one-shot alert at `price` (crossing up or down, by the side of the last price),
    /// remembering the chart's timeframe.
    fn alert_at(&mut self, price: f64) {
        let op = crate::alerts::default_chart_op(price, self.bars.last().map(|b| b.close));
        let cond = Condition { metric: Metric::Price, op, target: Target::Level(price) };
        self.actions.push(Action::AddAlert(Spec {
            symbol: Some(self.symbol.clone()),
            cond,
            repeat: false,
            tf: Some(self.tf),
            note: String::new(),
        }));
    }

    fn context_menu(&mut self, ui: &mut Ui, w: &Worker) {
        ui.set_min_width(190.0);
        if ui.button("Reset View").clicked() {
            self.reset_view();
            ui.close();
        }
        if let Some((id, at)) = self.menu_alert
            && let Some(a) = self.alert_lines.iter().find(|a| a.id == id).cloned()
        {
            let price = scale::format_price(a.level, self.sec.decimals(a.level));
            if ui.button(format!("Edit alert at {price}")).clicked() {
                self.alert_draft = Some(self.draft_for(&a, at));
                ui.close();
            }
            if ui.button(format!("Delete alert at {price}")).clicked() {
                self.actions.push(Action::AlertOp(RuleOp::Delete(id)));
                ui.close();
            }
            ui.separator();
        }
        self.drawing_menu(ui);
        self.anchor_menu(ui);
        if let Some((price, line)) = self.menu_at.filter(|(p, _)| *p > 0.0) {
            let sec = self.sec.clone();
            let fmt = |p: f64| format!("{p:.d$}", d = sec.decimals(p));
            if self.currency.is_some() {
                ui.add_enabled(false, egui::Button::new(format!("Alert at {}", fmt(price))))
                    .on_disabled_hover_text("Alerts use the quoted currency: set Currency to As quoted");
            } else {
                if ui.button(format!("Alert at {}", fmt(price))).on_hover_text("One-shot price alert (ALRT)").clicked()
                {
                    self.alert_at(price);
                    ui.close();
                }
                if let Some(l) = line
                    && ui.button(format!("Alert at Line {}", fmt(l))).clicked()
                {
                    self.alert_at(l);
                    ui.close();
                }
            }
        }
        ui.menu_button("Chart Type", |ui| {
            for ct in ChartType::ALL {
                if ui.radio(self.chart_type == ct, ct.label()).clicked() {
                    self.chart_type = ct;
                    ui.close();
                }
            }
        });
        self.indicator_menu(ui, w);
        if !studies::all().is_empty() {
            ui.menu_button("Studies", |ui| {
                for st in studies::all() {
                    let on = self.studies.iter().any(|c| c.id == st.id());
                    let label = match st.command() {
                        Some(c) => format!("{}  ({c})", st.name()),
                        None => st.name().to_string(),
                    };
                    let resp = ui.checkbox(&mut on.clone(), label);
                    let resp = if st.description().is_empty() { resp } else { resp.on_hover_text(st.description()) };
                    if resp.clicked() {
                        self.toggle_study(st.id(), w);
                    }
                }
                let active: Vec<usize> = (0..self.studies.len())
                    .filter(|&i| studies::get(&self.studies[i].id).is_some_and(|s| !s.params().is_empty()))
                    .collect();
                if !active.is_empty() {
                    ui.separator();
                }
                for i in active {
                    let st = studies::get(&self.studies[i].id).expect("registered");
                    ui.menu_button(format!("{} Settings", st.name()), |ui| {
                        if study_settings(ui, st.params(), &mut self.studies[i].params) {
                            self.request_studies(w);
                        }
                    });
                }
            });
        }
        self.draw_menu(ui);
        ui.menu_button("Currency", |ui| {
            let native = tr_data::native_currency(&self.symbol);
            if ui.radio(self.currency.is_none(), format!("As quoted ({native})")).clicked() {
                self.set_currency(None, w);
                ui.close();
            }
            for c in ["USD", "EUR", "GBP", "TRY", "JPY", "CHF", "CAD"] {
                if ui.radio(self.currency.as_deref() == Some(c), c).clicked() {
                    self.set_currency(Some(c), w);
                    ui.close();
                }
            }
        });
        ui.checkbox(&mut self.log, "Log Scale");
        ui.checkbox(&mut self.magnet, "Magnet");
        ui.checkbox(&mut self.show_volume, "Volume");
        if self.sec.class == AssetClass::Equity {
            ui.checkbox(&mut self.earnings.show, "Earnings Markers (E)");
        }
        ui.separator();
        if ui.button("Add to Watchlist").clicked() {
            self.actions.push(Action::AddToWatchlist(self.symbol.clone()));
            ui.close();
        }
        if let Some(code) = crate::widgets::research_menu(ui, &self.symbol) {
            self.actions.push(Action::Command(format!("{} {code}", self.symbol)));
        }
        if self.sec.has_congress_trades() {
            ui.checkbox(&mut self.show_congress, "Congress Trade Markers");
            if ui.button("Who in Congress Traded This…").clicked() {
                self.actions.push(Action::OpenTicker(self.symbol.clone()));
                ui.close();
            }
        }
    }

    /// Oscillator sub-panes: built-in indicators first, then plug-in studies.
    fn sub_panes(&self) -> Vec<SubPane> {
        let ind = self.built.iter().filter(|b| b.spec.is_pane()).map(|b| {
            let band =
                |lo: f64, hi: f64| vec![(lo, Color32::from_rgb(60, 45, 80)), (hi, Color32::from_rgb(60, 45, 80))];
            let (fixed, levels) = match b.spec {
                IndicatorSpec::Rsi(_) => (Some((0.0, 100.0)), band(30.0, 70.0)),
                IndicatorSpec::Stoch(..) => (Some((0.0, 100.0)), band(20.0, 80.0)),
                IndicatorSpec::Adx(_) => (None, vec![(25.0, Color32::from_rgb(60, 45, 80))]),
                _ => (None, Vec::new()),
            };
            SubPane { label: b.spec.label(), overlays: b.overlays.clone(), fixed, levels }
        });
        let st = self.built_studies.iter().flat_map(|b| {
            b.panes.iter().map(|p| SubPane {
                label: p.title.clone(),
                overlays: p.overlays.clone(),
                fixed: p.range,
                levels: p.levels.clone(),
            })
        });
        ind.chain(st).collect()
    }

    fn paint_legend(
        &self,
        ui: &Ui,
        price_rect: Rect,
        i: usize,
        sub_rects: &[(Rect, usize, PriceScale)],
        subs: &[SubPane],
    ) {
        let Some(b) = self.bars.get(i) else { return };
        let p = ui.painter_at(price_rect);
        let prev = if i > 0 { self.bars[i - 1].close } else { b.open };
        let chg = b.close - prev;
        let col = if chg >= 0.0 { theme::up() } else { theme::down() };
        let dec = self.sec.decimals(b.close);
        let mut x = price_rect.left() + 6.0;
        let y = price_rect.top() + 4.0;
        let put = |text: String, color: Color32, x: &mut f32| {
            let g = p.layout_no_wrap(text, mono(11.5), color);
            p.galley(pos2(*x, y), g.clone(), color);
            *x += g.size().x + 8.0;
        };
        put(format!("{} · {}", self.sec.ticker, self.tf), amber(), &mut x);
        for (k, val) in [("O", b.open), ("H", b.high), ("L", b.low), ("C", b.close)] {
            put(format!("{k}{val:.dec$}"), col, &mut x);
        }
        put(format!("{:+.dec$} ({:+.2}%)", chg, if prev != 0.0 { chg / prev * 100.0 } else { 0.0 }), col, &mut x);
        put(format!("Vol {}", scale::format_volume(b.volume)), muted(), &mut x);

        // Source and freshness on a line of its own, kept clear of the Buy/Sell pills: the full
        // text when it fits, the short form otherwise.
        let mut ly = y + 16.0;
        if let Some(q) = &self.quote {
            let now = quote_tag::now();
            if let Some(t) = quote_tag::tag(q, now) {
                let room = price_rect.width() - 12.0 - 110.0;
                let galley = [t.text, t.short].into_iter().find_map(|s| {
                    let g = p.layout_no_wrap(s, mono(11.0), t.color);
                    (g.size().x <= room).then_some(g)
                });
                if let Some(g) = galley {
                    let r = Rect::from_min_size(pos2(price_rect.left() + 6.0, ly), g.size());
                    p.galley(r.min, g, t.color);
                    ui.interact(r, ui.id().with((self.id, "quote-tag")), Sense::hover())
                        .on_hover_text(quote_tag::tooltip(q, now));
                    ly += 16.0;
                }
            }
        }

        // Overlay indicator values, one per line.
        let value_at = |l: &LineRef| l.pyramid.data.get(i).map(|p| p[1] as f64).filter(|v| v.is_finite());
        for bi in self.built.iter().filter(|b| !b.spec.is_pane()) {
            let vals: Vec<String> =
                bi.overlays.iter().filter_map(|(_, o)| value_at(&o.line).map(|v| format!("{v:.2}"))).collect();
            let c = bi
                .overlays
                .get(bi.overlays.len() / 2)
                .map(|o| {
                    Color32::from_rgba_unmultiplied(
                        (o.1.color[0] * 255.0) as u8,
                        (o.1.color[1] * 255.0) as u8,
                        (o.1.color[2] * 255.0) as u8,
                        255,
                    )
                })
                .unwrap_or(text());
            p.text(
                pos2(price_rect.left() + 6.0, ly),
                Align2::LEFT_TOP,
                format!("{}  {}", bi.spec.label(), vals.join(" ")),
                mono(11.0),
                c,
            );
            ly += 14.0;
        }
        for b in &self.built_studies {
            let (name, summary, vals) = studies::legend_parts(b, i);
            let mut x = price_rect.left() + 6.0;
            let mut put = |t: String, c: Color32| {
                let g = p.layout_no_wrap(t, mono(11.0), c);
                p.galley(pos2(x, ly), g.clone(), c);
                x += g.size().x + 8.0;
            };
            put(name, amber());
            if let Some((t, c)) = summary {
                put(t, c);
            }
            for (t, c) in vals {
                put(t, c);
            }
            ly += 14.0;
        }
        if let (Some(sym), Some(line)) = (&self.compare, &self.compare_line) {
            // Raw price plus performance relative to the left edge of the view.
            let raw = value_at(line);
            let rel = raw.zip(self.bars.get(i)).map(|(r, b)| (r * self.compare_scale / b.close - 1.0) * 100.0);
            let text = match (raw, rel) {
                (Some(r), Some(rel)) => format!("CMP {sym}  {r:.2}  ({rel:+.2}% vs {})", self.symbol),
                _ => format!("CMP {sym}"),
            };
            p.text(pos2(price_rect.left() + 6.0, ly), Align2::LEFT_TOP, text, mono(11.0), theme::series(5));
            ly += 14.0;
        }
        if self.draw.pick_avwap {
            let msg = "Anchored VWAP: click a bar to anchor it (Esc cancels)";
            p.text(pos2(price_rect.left() + 6.0, ly), Align2::LEFT_TOP, msg, mono(11.0), amber());
        } else if let Some(tool) = self.draw.tool {
            let msg = if tool.points() == 1 {
                "click to place".to_string()
            } else {
                format!("click point {} of {}", self.draw.pending.len() + 1, tool.points())
            };
            p.text(
                pos2(price_rect.left() + 6.0, ly),
                Align2::LEFT_TOP,
                format!("{}: {msg} (Esc cancels)", tool.label()),
                mono(11.0),
                amber(),
            );
        }
        for (r, k, _) in sub_rects {
            let b = &subs[*k];
            let vals: Vec<String> =
                b.overlays.iter().filter_map(|(n, o)| value_at(&o.line).map(|v| format!("{n} {v:.2}"))).collect();
            ui.painter_at(*r).text(
                r.left_top() + vec2(90.0, 3.0),
                Align2::LEFT_TOP,
                vals.join("  "),
                mono(10.5),
                text(),
            );
        }
    }

    fn paint_markers(&mut self, ui: &Ui, rect: Rect, v: &Viewport, s: &PriceScale, hover: Option<Pos2>) {
        if self.markers.is_empty() || self.bars.is_empty() {
            return;
        }
        let p = ui.painter_at(rect);
        let len = self.bars.len();
        let (lo, hi) = v.visible_range(len);
        // Stack multiple trades on the same bar.
        let mut stack: std::collections::HashMap<(usize, bool), u32> = std::collections::HashMap::new();
        let r = (v.bar_px as f32 * 0.45).clamp(4.0, 7.0);
        let mut best: Option<(f32, usize, Pos2)> = None;
        for (mi, t) in self.markers.iter().enumerate() {
            let Some(ts) = t.tx_date.and_hms_opt(12, 0, 0).map(|d| d.and_utc().timestamp()) else { continue };
            let idx = self.index_of_ts(ts).floor();
            if idx < lo as f64 || idx >= hi as f64 || idx < 0.0 || idx >= len as f64 {
                continue;
            }
            let i = idx as usize;
            let bar = self.bars[i];
            let sell = t.tx_type.is_sell();
            let n = stack.entry((i, sell)).or_insert(0);
            let x = rect.left() + v.x_center(i) as f32;
            let off = 6.0 + r + *n as f32 * (2.0 * r + 1.0);
            let y = if sell {
                rect.top() + s.y_of(bar.high) as f32 - off
            } else {
                rect.top() + s.y_of(bar.low) as f32 + off
            };
            *n += 1;
            let c = Pos2::new(x, y);
            let col = theme::party_color(t.party);
            p.circle_filled(c, r, col);
            if r >= 5.0 {
                let letter = match t.tx_type {
                    TxType::Purchase => "B",
                    TxType::Sale | TxType::SalePartial => "S",
                    _ => "E",
                };
                p.text(c, Align2::CENTER_CENTER, letter, mono(r * 1.6), Color32::WHITE);
            } else {
                p.circle_stroke(c, r, Stroke::new(1.0, Color32::WHITE));
            }
            if let Some(h) = hover {
                let d = h.distance(c);
                if d <= r + 3.0 && best.is_none_or(|b| d < b.0) {
                    best = Some((d, mi, c));
                }
            }
        }
        if let Some((_, mi, c)) = best {
            self.hover_marker = Some(mi);
            let t = &self.markers[mi];
            hover_card(ui, c, t);
        }
    }
}

/// Settings menu for one study, grouped like its parameter list. Returns whether a value changed.
fn study_settings(ui: &mut Ui, defs: &[tr_study::Param], values: &mut tr_study::Params) -> bool {
    use tr_study::{ParamKind, ParamValue};
    ui.set_min_width(240.0);
    let mut changed = false;
    let mut group = "";
    for d in defs {
        if d.group != group {
            group = d.group;
            if !group.is_empty() {
                ui.label(RichText::new(group).font(ui_font(11.0)).color(muted()));
            }
        }
        let cur = d.sanitize(values.get(d.key));
        let new = match (&d.kind, &cur) {
            (ParamKind::Bool, ParamValue::Bool(b)) => {
                let mut b = *b;
                let r = ui.checkbox(&mut b, d.label);
                if d.help.is_empty() {
                    r
                } else {
                    r.on_hover_text(d.help)
                };
                ParamValue::Bool(b)
            }
            (ParamKind::Int { min, max }, ParamValue::Int(i)) => {
                let mut i = *i;
                ui.horizontal(|ui| {
                    ui.label(d.label);
                    let r = ui.add(egui::DragValue::new(&mut i).range(*min..=*max));
                    if !d.help.is_empty() {
                        r.on_hover_text(d.help);
                    }
                });
                ParamValue::Int(i)
            }
            (ParamKind::Float { min, max, step }, ParamValue::Float(f)) => {
                let mut f = *f;
                ui.horizontal(|ui| {
                    ui.label(d.label);
                    let r = ui.add(egui::DragValue::new(&mut f).range(*min..=*max).speed(*step).max_decimals(3));
                    if !d.help.is_empty() {
                        r.on_hover_text(d.help);
                    }
                });
                ParamValue::Float(f)
            }
            (ParamKind::Choice(opts), ParamValue::Text(t)) => {
                let mut t = t.clone();
                ui.horizontal(|ui| {
                    ui.label(d.label);
                    for o in *opts {
                        ui.radio_value(&mut t, o.to_string(), *o);
                    }
                });
                ParamValue::Text(t)
            }
            _ => cur.clone(),
        };
        if new != cur {
            values.insert(d.key.to_string(), new);
            changed = true;
        }
    }
    ui.separator();
    if ui.button("Reset to Defaults").clicked() {
        changed |= !values.is_empty();
        values.clear();
    }
    changed
}

/// An oscillator pane under the price pane.
struct SubPane {
    label: String,
    overlays: Vec<(String, Overlay)>,
    fixed: Option<(f64, f64)>,
    levels: Vec<(f64, Color32)>,
}

fn dashed_h(p: &egui::Painter, x: egui::Rangef, y: f32, s: Stroke) {
    let mut a = x.min;
    while a < x.max {
        p.hline(a..=(a + 4.0).min(x.max), y, s);
        a += 7.0;
    }
}

fn dashed_v(p: &egui::Painter, x: f32, y: egui::Rangef, s: Stroke) {
    let mut a = y.min;
    while a < y.max {
        p.vline(x, a..=(a + 4.0).min(y.max), s);
        a += 7.0;
    }
}

/// Hover card for a congressional trade marker, including the disclosure lag.
fn hover_card(ui: &Ui, at: Pos2, t: &Trade) {
    let lines = [
        (
            format!("{} ({}-{})", t.politician, t.party.code(), t.state.as_deref().unwrap_or("??")),
            theme::party_color(t.party),
        ),
        (format!("{} · {}", t.chamber.as_str(), t.owner.as_deref().unwrap_or("Self")), muted()),
        (
            format!(
                "{} {}  {}",
                match t.tx_type {
                    TxType::Purchase => "BUY",
                    TxType::Sale => "SELL",
                    TxType::SalePartial => "SELL (partial)",
                    TxType::Exchange => "EXCHANGE",
                    TxType::Other => "OTHER",
                },
                t.ticker.as_deref().unwrap_or("-"),
                t.amount.map(|a| a.to_string()).unwrap_or_else(|| "amount n/a".into())
            ),
            if t.tx_type.is_buy() { theme::up() } else { theme::down() },
        ),
        (format!("Traded  {}", t.tx_date), text()),
        (format!("Filed   {}", t.filed_date), text()),
        (
            format!("Disclosure lag {} days", t.disclosure_lag_days()),
            if t.disclosure_lag_days() > 45 { theme::down() } else { amber() },
        ),
    ];
    let w = lines.iter().map(|(l, _)| l.len()).max().unwrap_or(10) as f32 * 7.2 + 14.0;
    let h = lines.len() as f32 * 14.0 + 8.0;
    let screen = ui.ctx().content_rect();
    let mut min = at + vec2(12.0, 12.0);
    if min.x + w > screen.right() {
        min.x = at.x - w - 12.0;
    }
    if min.y + h > screen.bottom() {
        min.y = at.y - h - 12.0;
    }
    let card = Rect::from_min_size(min, vec2(w, h));
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("congress-card"));
    let p = ui.ctx().layer_painter(layer);
    p.rect_filled(card, 0.0, Color32::from_rgb(16, 16, 16));
    p.rect_stroke(card, 0.0, Stroke::new(1.0, amber_dim()), StrokeKind::Inside);
    for (k, (l, c)) in lines.iter().enumerate() {
        p.text(card.left_top() + vec2(7.0, 4.0 + k as f32 * 14.0), Align2::LEFT_TOP, l, mono(11.5), *c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_position_maps_to_price() {
        // 100..200 over 400 px, pane starting at y = 50.
        let s = PriceScale::new(100.0, 200.0, false, 400.0);
        assert!((price_at_y(&s, 50.0, 50.0) - 200.0).abs() < 1e-9, "top is the high");
        assert!((price_at_y(&s, 50.0, 450.0) - 100.0).abs() < 1e-9, "bottom is the low");
        assert!((price_at_y(&s, 50.0, 250.0) - 150.0).abs() < 1e-9);
        // A price drawn at y reads back as the same price (what the axis tag shows).
        let y = 50.0 + s.y_of(137.5) as f32;
        assert!((price_at_y(&s, 50.0, y) - 137.5).abs() < 0.01);
    }
}
