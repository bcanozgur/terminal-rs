//! Chart extras: the drawing tools (placing, painting, hit testing, moving, colours, text),
//! the indicator and draw menus with editable parameters, the Ichimoku cloud and the
//! volume profile.

use egui::{Painter, Vec2};
use tr_core::indicators_ext::{self as ext, VolumeProfile};

use super::*;

/// The anchors of a drawing translated by a pointer drag of `(dx, dy)` pixels.
fn shifted_anchor(this: &ChartState, a: Anchor, (dx, dy): (f64, f64), v: &Viewport, s: &PriceScale) -> Anchor {
    let idx = this.index_of_ts(a.ts) + dx / v.bar_px;
    let y = s.y_of(a.price) + dy;
    Anchor { ts: this.ts_of_index(idx), price: s.price_at(y) }
}

/// Text on a filled chip, anchored by `align` at `pos`. Returns the chip's rectangle.
fn tag(p: &Painter, pos: Pos2, align: Align2, text: String, fg: Color32, bg: Color32) -> Rect {
    let g = p.layout_no_wrap(text, mono(10.5), fg);
    let r = align.anchor_size(pos, g.size()).expand2(vec2(3.0, 1.0));
    p.rect_filled(r, 2.0, bg);
    p.galley(r.min + vec2(3.0, 1.0), g, fg);
    r
}

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

impl ChartState {
    fn price_text(&self, p: f64) -> String {
        format!("{p:.d$}", d = self.sec.decimals(p))
    }

    /// Colour of a drawing: amber when selected, else its own colour or the theme's.
    fn drawing_color(d: &Drawing, selected: bool) -> Color32 {
        match (selected, d.color) {
            (true, _) => amber(),
            (false, Some([r, g, b])) => Color32::from_rgb(r, g, b),
            (false, None) => theme::cyan(),
        }
    }

    /// Screen x-range of a position drawing (entry to the stop click, with a minimum width).
    fn position_x(xa: f64, xb: f64) -> (f64, f64) {
        let (x0, x1) = (xa.min(xb), xa.max(xb));
        if x1 - x0 < 40.0 { (x0, x0 + 120.0) } else { (x0, x1) }
    }

    pub(super) fn drawing_shapes(
        &self,
        d: &Drawing,
        rect: Rect,
        v: &Viewport,
        s: &PriceScale,
    ) -> Vec<(Shape, Option<String>)> {
        let px =
            |a: &Anchor| [rect.left() as f64 + v.x_of(self.index_of_ts(a.ts)), rect.top() as f64 + s.y_of(a.price)];
        let y_of = |price: f64| rect.top() as f64 + s.y_of(price);
        let (pa, pb) = (px(&d.a), px(&d.b));
        match d.tool {
            Tool::Trendline => vec![(Shape::Segment(pa, pb), None)],
            Tool::Horizontal => vec![(Shape::HLine(pa[1]), Some(format!("{:.2}", d.a.price)))],
            Tool::Vertical => vec![(Shape::VLine(pa[0]), None)],
            Tool::Fib => {
                let (x0, x1) = (pa[0].min(pb[0]), pa[0].max(pb[0]));
                d.fib_levels()
                    .into_iter()
                    .map(|(r, price)| {
                        let y = y_of(price);
                        (Shape::Segment([x0, y], [x1, y]), Some(format!("{r:.3} ({price:.2})")))
                    })
                    .collect()
            }
            Tool::Rectangle => {
                let (x0, x1) = (pa[0].min(pb[0]), pa[0].max(pb[0]));
                let (y0, y1) = (pa[1].min(pb[1]), pa[1].max(pb[1]));
                vec![
                    (Shape::Segment([x0, y0], [x1, y0]), None),
                    (Shape::Segment([x1, y0], [x1, y1]), None),
                    (Shape::Segment([x1, y1], [x0, y1]), None),
                    (Shape::Segment([x0, y1], [x0, y0]), None),
                ]
            }
            Tool::Ray => vec![(Shape::Segment(pa, ray_end(pa, pb, rect)), None)],
            Tool::Channel => {
                let off = self.channel_offset(d, rect, v, s);
                vec![(Shape::Segment(pa, pb), None), (Shape::Segment([pa[0], pa[1] + off], [pb[0], pb[1] + off]), None)]
            }
            Tool::Text => {
                let w = d.text.chars().count().max(1) as f64 * 7.0;
                vec![(Shape::Segment([pa[0], pa[1] - 6.0], [pa[0] + w, pa[1] - 6.0]), None)]
            }
            Tool::Measure => vec![(Shape::Segment(pa, pb), None)],
            Tool::Long | Tool::Short => {
                let (x0, x1) = Self::position_x(pa[0], pb[0]);
                let target = y_of(d.target_price());
                [pa[1], pb[1], target]
                    .into_iter()
                    .map(|y| (Shape::Segment([x0, y], [x1, y]), None))
                    .chain([
                        (Shape::Segment([x0, pb[1]], [x0, target]), None),
                        (Shape::Segment([x1, pb[1]], [x1, target]), None),
                    ])
                    .collect()
            }
        }
    }

    /// Vertical pixel offset of a channel's second line from its first, at the third click.
    fn channel_offset(&self, d: &Drawing, rect: Rect, v: &Viewport, s: &PriceScale) -> f64 {
        let Some(c) = d.c else { return 0.0 };
        let px =
            |a: &Anchor| [rect.left() as f64 + v.x_of(self.index_of_ts(a.ts)), rect.top() as f64 + s.y_of(a.price)];
        let (pa, pb, pc) = (px(&d.a), px(&d.b), px(&c));
        let t = if (pb[0] - pa[0]).abs() < 1e-9 { 0.0 } else { (pc[0] - pa[0]) / (pb[0] - pa[0]) };
        pc[1] - (pa[1] + (pb[1] - pa[1]) * t)
    }

    /// The drawing nearest to `pos` within the pick distance, topmost first.
    fn hit_drawing(&self, pos: Pos2, rect: Rect, v: &Viewport, s: &PriceScale) -> Option<usize> {
        self.drawings.iter().enumerate().rev().find_map(|(i, d)| {
            self.drawing_shapes(d, rect, v, s)
                .iter()
                .any(|(sh, _)| drawings::distance(sh, [pos.x as f64, pos.y as f64]) < 6.0)
                .then_some(i)
        })
    }

    /// Start moving the selected drawing when a drag begins on it. Runs before the pan
    /// handling so the chart does not scroll under the drag.
    pub(super) fn begin_drawing_drag(&mut self, ui: &Ui, resp: &egui::Response) {
        if self.draw.tool.is_some() || self.draw.moving.is_some() || !resp.drag_started() {
            return;
        }
        let (Some((rect, v, s)), Some(sel), Some(pos)) =
            (self.last_geom, self.draw.selected, ui.input(|i| i.pointer.press_origin()))
        else {
            return;
        };
        if self.hit_drawing(pos, rect, &v, &s) == Some(sel)
            && let Some(d) = self.drawings.get(sel)
        {
            self.draw.moving = Some((sel, d.clone(), Vec2::ZERO));
        }
    }

    fn paint_extra(&self, p: &Painter, d: &Drawing, selected: bool, rect: Rect, v: &Viewport, s: &PriceScale) {
        let color = Self::drawing_color(d, selected);
        let stroke = Stroke::new(if selected { 2.0 } else { 1.3 }, color);
        let pt =
            |a: &Anchor| pos2(rect.left() + v.x_of(self.index_of_ts(a.ts)) as f32, rect.top() + s.y_of(a.price) as f32);
        let y_of = |price: f64| rect.top() + s.y_of(price) as f32;
        let (pa, pb) = (pt(&d.a), pt(&d.b));
        let mut handles = vec![pa, pb];
        match d.tool {
            Tool::Rectangle => {
                let r = Rect::from_two_pos(pa, pb);
                p.rect_filled(r, 0.0, with_alpha(color, 40));
                p.rect_stroke(r, 0.0, stroke, StrokeKind::Middle);
            }
            Tool::Ray => {
                let end = ray_end([pa.x as f64, pa.y as f64], [pb.x as f64, pb.y as f64], rect);
                p.line_segment([pa, pos2(end[0] as f32, end[1] as f32)], stroke);
            }
            Tool::Channel => {
                let off = self.channel_offset(d, rect, v, s) as f32;
                let (qa, qb) = (pa + vec2(0.0, off), pb + vec2(0.0, off));
                p.add(egui::Shape::convex_polygon(vec![pa, pb, qb, qa], with_alpha(color, 28), Stroke::NONE));
                p.line_segment([pa, pb], stroke);
                p.line_segment([qa, qb], stroke);
                p.add(egui::Shape::dashed_line(
                    &[pa + vec2(0.0, off / 2.0), pb + vec2(0.0, off / 2.0)],
                    Stroke::new(1.0, with_alpha(color, 160)),
                    5.0,
                    4.0,
                ));
                handles = vec![pa, pb];
                if let Some(c) = d.c {
                    handles.push(pt(&c));
                }
            }
            Tool::Text => {
                let g = p.layout_no_wrap(d.text.clone(), mono(12.0), color);
                let r = Align2::LEFT_BOTTOM.anchor_size(pa, g.size()).expand2(vec2(4.0, 2.0));
                p.rect_filled(r, 3.0, with_alpha(Color32::BLACK, 120));
                if selected {
                    p.rect_stroke(r, 3.0, Stroke::new(1.0, color), StrokeKind::Middle);
                }
                p.galley(r.min + vec2(4.0, 2.0), g, color);
                handles = vec![pa];
            }
            Tool::Measure => {
                let m =
                    drawings::measure(d.a, d.b, (self.index_of_ts(d.b.ts) - self.index_of_ts(d.a.ts)).round() as i64);
                let c = if selected {
                    color
                } else if m.delta >= 0.0 {
                    theme::up()
                } else {
                    theme::down()
                };
                let r = Rect::from_two_pos(pa, pb);
                p.rect_filled(r, 0.0, with_alpha(c, 30));
                p.line_segment([pa, pb], Stroke::new(stroke.width, c));
                let head = (pb - pa).normalized() * 8.0;
                if head.is_finite() {
                    let n = vec2(-head.y, head.x) * 0.5;
                    p.line_segment([pb, pb - head + n], Stroke::new(stroke.width, c));
                    p.line_segment([pb, pb - head - n], Stroke::new(stroke.width, c));
                }
                let l1 =
                    format!("{}{} ({:+.2}%)", if m.delta >= 0.0 { "+" } else { "" }, self.price_text(m.delta), m.pct);
                let l2 = format!("{} bars · {}", m.bars.abs(), drawings::format_span(m.secs.abs()));
                let below = pb.y >= pa.y;
                let at = pos2(pb.x, if below { r.bottom() + 4.0 } else { r.top() - 4.0 });
                let align = if below { Align2::CENTER_TOP } else { Align2::CENTER_BOTTOM };
                let bg = with_alpha(c, 220);
                let fg = Color32::BLACK;
                if below {
                    let r1 = tag(p, at, align, l1, fg, bg);
                    tag(p, pos2(at.x, r1.bottom() + 2.0), align, l2, fg, bg);
                } else {
                    let r2 = tag(p, at, align, l2, fg, bg);
                    tag(p, pos2(at.x, r2.top() - 2.0), align, l1, fg, bg);
                }
            }
            Tool::Long | Tool::Short => {
                let Some(pos) = d.position() else { return };
                let (x0, x1) = Self::position_x(pa.x as f64, pb.x as f64);
                let (x0, x1) = (x0 as f32, x1 as f32);
                let (ye, ys, yt) = (y_of(pos.entry), y_of(pos.stop), y_of(pos.target));
                let (green, red) = (theme::up(), theme::down());
                p.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(ys)..=ye.max(ys)), 0.0, with_alpha(red, 55));
                p.rect_filled(Rect::from_x_y_ranges(x0..=x1, ye.min(yt)..=ye.max(yt)), 0.0, with_alpha(green, 55));
                p.hline(x0..=x1, ye, stroke);
                p.hline(x0..=x1, ys, Stroke::new(1.0, red));
                p.hline(x0..=x1, yt, Stroke::new(1.0, green));
                let rr = pos.rr.map_or("–".to_string(), |r| format!("{r:.2}"));
                tag(
                    p,
                    pos2(x0 + 4.0, ye),
                    Align2::LEFT_CENTER,
                    format!(
                        "{} · R:R {rr} · risk {}",
                        if pos.long { "Long" } else { "Short" },
                        self.price_text(pos.risk)
                    ),
                    Color32::BLACK,
                    with_alpha(color, 230),
                );
                let ydir = |target_above: bool| if target_above { Align2::LEFT_BOTTOM } else { Align2::LEFT_TOP };
                tag(
                    p,
                    pos2(x0 + 4.0, yt),
                    ydir(yt <= ye),
                    format!("Target {} ({:+.2}%)", self.price_text(pos.target), pos.target_pct),
                    Color32::BLACK,
                    with_alpha(green, 230),
                );
                tag(
                    p,
                    pos2(x0 + 4.0, ys),
                    ydir(ys <= ye),
                    format!("Stop {} ({:+.2}%)", self.price_text(pos.stop), pos.stop_pct),
                    Color32::BLACK,
                    with_alpha(red, 230),
                );
                handles = vec![pos2(x0, ye), pos2(x1, ys), pos2(x1, yt)];
            }
            _ => {}
        }
        if selected {
            for h in handles {
                p.circle_filled(h, 3.5, amber());
            }
        }
    }

    pub(super) fn paint_drawings(
        &mut self,
        ui: &Ui,
        rect: Rect,
        v: &Viewport,
        s: &PriceScale,
        hover: Option<Pos2>,
        resp: &egui::Response,
    ) {
        let p = ui.painter_at(rect);
        let pointer_anchor = |this: &Self, pos: Pos2| -> Anchor {
            let idx = v.index_at((pos.x - rect.left()) as f64);
            let mut price = s.price_at((pos.y - rect.top()) as f64);
            if this.magnet
                && let Some(b) = this.bars.get(idx.floor().max(0.0) as usize)
            {
                price = drawings::magnet(price, [b.open, b.high, b.low, b.close]);
            }
            Anchor { ts: this.ts_of_index(idx), price }
        };
        self.last_geom = Some((rect, *v, *s));

        if resp.secondary_clicked() {
            let pos = resp.interact_pointer_pos().filter(|p| rect.contains(*p));
            self.menu_at = pos.map(|pos| {
                let price = s.price_at((pos.y - rect.top()) as f64);
                let line = self
                    .drawings
                    .iter()
                    .filter(|d| d.tool == Tool::Horizontal)
                    .find(|d| (rect.top() as f64 + s.y_of(d.a.price) - pos.y as f64).abs() < 6.0)
                    .map(|d| d.a.price);
                (price, line)
            });
            self.menu_ts = pos.and_then(|pos| {
                let i = v.bar_at((pos.x - rect.left()) as f64, self.bars.len())?;
                Some(self.bars[i].ts)
            });
            self.menu_drawing = pos.and_then(|pos| self.hit_drawing(pos, rect, v, s));
            if self.menu_drawing.is_some() {
                self.draw.selected = self.menu_drawing;
            }
        }

        // Placement: an anchored-VWAP pick, or the next point of the tool being drawn.
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        if self.draw.pick_avwap {
            if let Some(pos) = click {
                self.pending_avwap = v.bar_at((pos.x - rect.left()) as f64, self.bars.len()).map(|i| self.bars[i].ts);
                self.draw = DrawState::default();
            }
        } else if let (Some(tool), Some(pos)) = (self.draw.tool, click) {
            let a = pointer_anchor(self, pos);
            self.draw.pending.push(a);
            if self.draw.pending.len() >= tool.points()
                && let Some(d) = Drawing::from_points(tool, &self.draw.pending)
            {
                self.drawings.push(d);
                let i = self.drawings.len() - 1;
                self.draw = DrawState::default();
                self.draw.selected = Some(i);
                if tool == Tool::Text {
                    self.draw.editing = Some(i);
                }
            }
        } else if self.draw.tool.is_none()
            && let Some(pos) = click
        {
            // Selection by proximity.
            self.draw.selected = self.hit_drawing(pos, rect, v, s);
        }

        // Moving the selected drawing: the drag is applied to a snapshot from where it began.
        if let Some((i, origin, total)) = self.draw.moving.take()
            && resp.dragged()
            && i < self.drawings.len()
        {
            let total = total + resp.drag_delta();
            let by = (total.x as f64, total.y as f64);
            let mut d = origin.clone();
            for a in d.anchors_mut() {
                *a = shifted_anchor(self, *a, by, v, s);
            }
            self.drawings[i] = d;
            self.draw.moving = Some((i, origin, total));
            ui.ctx().set_cursor_icon(CursorIcon::Move);
        }

        let mut all: Vec<(Drawing, bool)> =
            self.drawings.iter().cloned().enumerate().map(|(i, d)| (d, self.draw.selected == Some(i))).collect();
        if let (Some(tool), false, Some(h)) = (self.draw.tool, self.draw.pending.is_empty(), hover) {
            let mut pts = self.draw.pending.clone();
            pts.push(pointer_anchor(self, h));
            all.extend(Drawing::from_points(tool, &pts).map(|d| (d, true)));
        }
        for (d, selected) in all {
            if d.tool.is_extra() {
                self.paint_extra(&p, &d, selected, rect, v, s);
                continue;
            }
            let color = Self::drawing_color(&d, selected);
            let stroke = Stroke::new(if selected { 2.0 } else { 1.3 }, color);
            for (shape, label) in self.drawing_shapes(&d, rect, v, s) {
                match shape {
                    Shape::Segment(a, b) => {
                        p.line_segment([pos2(a[0] as f32, a[1] as f32), pos2(b[0] as f32, b[1] as f32)], stroke);
                        if let Some(l) = label {
                            p.text(
                                pos2(a[0] as f32 + 3.0, a[1] as f32 - 1.0),
                                Align2::LEFT_BOTTOM,
                                l,
                                mono(10.0),
                                color,
                            );
                        }
                        if selected && d.tool == Tool::Trendline {
                            for q in [a, b] {
                                p.circle_filled(pos2(q[0] as f32, q[1] as f32), 3.5, amber());
                            }
                        }
                    }
                    Shape::HLine(y) => {
                        p.hline(rect.x_range(), y as f32, stroke);
                        if let Some(l) = label {
                            p.text(
                                pos2(rect.right() - 4.0, y as f32 - 1.0),
                                Align2::RIGHT_BOTTOM,
                                l,
                                mono(10.0),
                                color,
                            );
                        }
                    }
                    Shape::VLine(x) => {
                        p.vline(x as f32, rect.y_range(), stroke);
                    }
                }
            }
        }
        self.note_editor(ui, rect, v, s);
    }

    /// The small text box that edits a note's text.
    fn note_editor(&mut self, ui: &Ui, rect: Rect, v: &Viewport, s: &PriceScale) {
        let Some(i) = self.draw.editing else { return };
        let Some(d) = self.drawings.get(i).filter(|d| d.tool == Tool::Text) else {
            self.draw.editing = None;
            return;
        };
        let at =
            pos2(rect.left() + v.x_of(self.index_of_ts(d.a.ts)) as f32, rect.top() + s.y_of(d.a.price) as f32 + 6.0);
        let id = ui.id().with((self.id, "note-edit"));
        let mut text = d.text.clone();
        let mut done = false;
        egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(at).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut text).font(mono(12.0)).desired_width(180.0));
                r.request_focus();
                if r.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = true;
                }
            });
        });
        self.drawings[i].text = text;
        if done {
            self.draw.editing = None;
        }
    }

    // ---- menus --------------------------------------------------------------------------

    /// Entries of the right-click menu that act on the drawing under the pointer.
    pub(super) fn drawing_menu(&mut self, ui: &mut Ui) {
        let Some(i) = self.menu_drawing.filter(|i| *i < self.drawings.len()) else { return };
        ui.menu_button(format!("{}…", self.drawings[i].tool.label()), |ui| {
            if self.drawings[i].tool == Tool::Text && ui.button("Edit Text").clicked() {
                self.draw.editing = Some(i);
                ui.close();
            }
            ui.label(RichText::new("Colour").font(ui_font(11.0)).color(muted()));
            ui.horizontal(|ui| {
                for (name, [r, g, b]) in drawings::PALETTE {
                    let (rect, resp) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::click());
                    ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(r, g, b));
                    if resp.on_hover_text(name).clicked() {
                        self.drawings[i].color = Some([r, g, b]);
                        ui.close();
                    }
                }
            });
            if ui.button("Default Colour").clicked() {
                self.drawings[i].color = None;
                ui.close();
            }
            ui.separator();
            if ui.button("Delete").clicked() {
                self.drawings.remove(i);
                self.draw = DrawState::default();
                self.menu_drawing = None;
                ui.close();
            }
        });
        ui.separator();
    }

    /// "Anchored VWAP from here" for the bar under the last right-click.
    pub(super) fn anchor_menu(&mut self, ui: &mut Ui) {
        if let Some(ts) = self.menu_ts
            && ui.button("Anchored VWAP from Here").on_hover_text("VWAP accumulated from this bar (AVWAP)").clicked()
        {
            self.pending_avwap = Some(ts);
            ui.close();
        }
    }

    pub(super) fn indicator_menu(&mut self, ui: &mut Ui, w: &Worker) {
        ui.menu_button("Indicators", |ui| {
            for spec in [
                IndicatorSpec::Sma(20),
                IndicatorSpec::Sma(50),
                IndicatorSpec::Ema(20),
                IndicatorSpec::Vwap,
                IndicatorSpec::Bollinger(20, 2.0),
                IndicatorSpec::Ichimoku(9, 26, 52),
                IndicatorSpec::VolumeProfile(48),
                IndicatorSpec::Rsi(14),
                IndicatorSpec::Macd(12, 26, 9),
                IndicatorSpec::Stoch(14, 3, 3),
                IndicatorSpec::Adx(14),
                IndicatorSpec::Atr(14),
                IndicatorSpec::Obv,
            ] {
                // Parameters are editable, so match the family (SMA/EMA by their length).
                let at = self.indicators.iter().position(|s| s.same_kind(&spec));
                if ui.checkbox(&mut at.is_some(), spec.label()).clicked() {
                    match at {
                        Some(i) => {
                            self.indicators.remove(i);
                        }
                        None => self.indicators.push(spec),
                    }
                    self.request_indicators(w);
                }
            }
            if ui.button("Anchored VWAP… (click a bar)").on_hover_text("AVWAP: pick the bar to anchor to").clicked() {
                self.start_anchor_pick();
                ui.close();
            }
            let anchored: Vec<usize> = (0..self.indicators.len())
                .filter(|&i| matches!(self.indicators[i], IndicatorSpec::AnchoredVwap(_)))
                .collect();
            let editable: Vec<usize> =
                (0..self.indicators.len()).filter(|&i| spec_has_params(&self.indicators[i])).collect();
            if !anchored.is_empty() || !editable.is_empty() {
                ui.separator();
            }
            for i in anchored.into_iter().rev() {
                if ui.button(format!("Remove {}", self.indicators[i].label())).clicked() {
                    self.indicators.remove(i);
                    self.request_indicators(w);
                    ui.close();
                }
            }
            for i in editable {
                let label = format!("{} Settings", self.indicators[i].label());
                ui.menu_button(label, |ui| {
                    if edit_spec(ui, &mut self.indicators[i]) {
                        self.request_indicators(w);
                    }
                });
            }
            ui.separator();
            if ui.button("Remove All").clicked() {
                self.clear_indicators();
                ui.close();
            }
        });
    }

    pub(super) fn draw_menu(&mut self, ui: &mut Ui) {
        ui.menu_button("Draw", |ui| {
            let alt = if cfg!(target_os = "macos") { "⌥" } else { "Alt+" };
            for (tool, key) in Tool::ALL.into_iter().zip(["T", "H", "V", "F", "B", "Y", "C", "N", "D", "U", "J"]) {
                if ui.add(egui::Button::new(tool.label()).shortcut_text(format!("{alt}{key}"))).clicked() {
                    self.start_tool(tool);
                    ui.close();
                }
            }
            if ui.button("Remove All Drawings").clicked() {
                self.drawings.clear();
                self.draw = DrawState::default();
                ui.close();
            }
        });
    }

    /// Wait for a click on a bar and anchor a VWAP to it (`AVWAP`).
    pub fn start_anchor_pick(&mut self) {
        self.draw = DrawState::default();
        self.draw.pick_avwap = true;
    }

    // ---- indicator overlays drawn by egui --------------------------------------------------

    /// The Ichimoku cloud between the two leading spans, over the visible bars (and the
    /// `kijun` bars past the last one).
    pub(super) fn paint_cloud(&self, ui: &Ui, rect: Rect, v: &Viewport, s: &PriceScale) {
        let p = ui.painter_at(rect);
        let x = |i: usize| rect.left() + v.x_center(i) as f32;
        let y = |price: f64| rect.top() + s.y_of(price) as f32;
        for cloud in self.built.iter().filter_map(|b| b.cloud.as_ref()) {
            let n = cloud.a.len().min(cloud.b.len());
            let (lo, hi) = v.visible_range(n);
            let mut mesh = egui::Mesh::default();
            for i in lo.saturating_sub(1)..hi.saturating_sub(1).min(n.saturating_sub(1)) {
                let (a0, b0, a1, b1) = (cloud.a[i], cloud.b[i], cloud.a[i + 1], cloud.b[i + 1]);
                if !(a0.is_finite() && b0.is_finite() && a1.is_finite() && b1.is_finite()) {
                    continue;
                }
                let bullish = (a0 - b0) + (a1 - b1) >= 0.0;
                let c = with_alpha(if bullish { theme::up() } else { theme::down() }, 38);
                let base = mesh.vertices.len() as u32;
                for (px, py) in [(x(i), y(a0)), (x(i), y(b0)), (x(i + 1), y(a1)), (x(i + 1), y(b1))] {
                    mesh.colored_vertex(pos2(px, py), c);
                }
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base + 1, base + 3, base + 2);
            }
            if !mesh.is_empty() {
                p.add(egui::Shape::mesh(mesh));
            }
        }
    }

    /// Volume profile of the visible bars: a histogram along the right edge with the point of
    /// control and the 70% value area.
    pub(super) fn paint_profile(&mut self, ui: &Ui, rect: Rect, v: &Viewport, s: &PriceScale) {
        let Some(rows) = self.indicators.iter().find_map(|i| match i {
            IndicatorSpec::VolumeProfile(r) => Some((*r).clamp(4, 200)),
            _ => None,
        }) else {
            return;
        };
        let (lo, hi) = v.visible_range(self.bars.len());
        let key = (lo, hi, self.bars.len(), rows, self.bars.last().map(|b| b.volume.to_bits()));
        if self.vp_cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let profile = (hi > lo).then(|| ext::volume_profile(&self.bars[lo..hi], rows)).flatten();
            self.vp_cache = Some((key, profile));
        }
        let Some(profile) = self.vp_cache.as_ref().and_then(|(_, p)| p.as_ref()) else { return };
        paint_volume_profile(ui, rect, s, profile, &self.sec);
    }
}

fn paint_volume_profile(ui: &Ui, rect: Rect, s: &PriceScale, pr: &VolumeProfile, sec: &Security) {
    let p = ui.painter_at(rect);
    let max = pr.volumes.iter().copied().fold(0.0, f64::max);
    if max <= 0.0 {
        return;
    }
    let y = |price: f64| rect.top() + s.y_of(price) as f32;
    let width = rect.width() * 0.22;
    let right = rect.right();
    let accent = theme::cyan();
    for (r, vol) in pr.volumes.iter().enumerate() {
        let (y1, y0) = (y(pr.row_bottom(r)), y(pr.row_bottom(r + 1)));
        let w = (*vol / max) as f32 * width;
        let inside = r >= pr.va.0 && r <= pr.va.1;
        let c = if r == pr.poc { amber() } else { accent };
        let alpha = if inside { 95 } else { 45 };
        p.rect_filled(
            Rect::from_min_max(pos2(right - w, y0), pos2(right, (y1 - 0.5).max(y0 + 1.0))),
            0.0,
            with_alpha(c, alpha),
        );
    }
    let fmt = |price: f64| format!("{price:.d$}", d = sec.decimals(price));
    let poc = y(pr.poc_price());
    p.add(egui::Shape::dashed_line(
        &[pos2(rect.left(), poc), pos2(right, poc)],
        Stroke::new(1.0, with_alpha(amber(), 170)),
        6.0,
        4.0,
    ));
    p.text(
        pos2(right - width - 4.0, poc - 1.0),
        Align2::RIGHT_BOTTOM,
        format!("POC {}", fmt(pr.poc_price())),
        mono(10.0),
        amber(),
    );
    for (label, price) in [("VAH", pr.va_high()), ("VAL", pr.va_low())] {
        let yy = y(price);
        p.hline(right - width..=right, yy, Stroke::new(1.0, with_alpha(accent, 200)));
        let align = if label == "VAH" { Align2::RIGHT_BOTTOM } else { Align2::RIGHT_TOP };
        p.text(pos2(right - width - 4.0, yy), align, format!("{label} {}", fmt(price)), mono(10.0), accent);
    }
}

/// Where a ray from `a` through `b` leaves the plot: far enough to cross it.
fn ray_end(a: [f64; 2], b: [f64; 2], rect: Rect) -> [f64; 2] {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return b;
    }
    let reach = 2.0 * (rect.width() + rect.height()) as f64 + len;
    [a[0] + dx / len * reach, a[1] + dy / len * reach]
}

fn spec_has_params(spec: &IndicatorSpec) -> bool {
    !matches!(spec, IndicatorSpec::Vwap | IndicatorSpec::Obv | IndicatorSpec::AnchoredVwap(_))
}

/// Drag values for an indicator's parameters. Returns whether one changed.
fn edit_spec(ui: &mut Ui, spec: &mut IndicatorSpec) -> bool {
    fn int(ui: &mut Ui, label: &str, v: &mut usize, max: usize) -> bool {
        ui.horizontal(|ui| {
            ui.label(label);
            ui.add(egui::DragValue::new(v).range(1..=max)).changed()
        })
        .inner
    }
    ui.set_min_width(170.0);
    match spec {
        IndicatorSpec::Sma(n)
        | IndicatorSpec::Ema(n)
        | IndicatorSpec::Rsi(n)
        | IndicatorSpec::Atr(n)
        | IndicatorSpec::Adx(n) => int(ui, "Length", n, 500),
        IndicatorSpec::Bollinger(n, k) => {
            let a = int(ui, "Length", n, 500);
            let b = ui
                .horizontal(|ui| {
                    ui.label("Std dev");
                    ui.add(egui::DragValue::new(k).range(0.1..=10.0).speed(0.05)).changed()
                })
                .inner;
            a | b
        }
        IndicatorSpec::Macd(f, s, g) => {
            let a = int(ui, "Fast", f, 200);
            let b = int(ui, "Slow", s, 400);
            let c = int(ui, "Signal", g, 200);
            a | b | c
        }
        IndicatorSpec::Stoch(k, s, d) => {
            let a = int(ui, "%K length", k, 200);
            let b = int(ui, "%K smoothing", s, 50);
            let c = int(ui, "%D length", d, 50);
            a | b | c
        }
        IndicatorSpec::Ichimoku(t, k, s) => {
            let a = int(ui, "Tenkan", t, 200);
            let b = int(ui, "Kijun", k, 200);
            let c = int(ui, "Senkou B", s, 400);
            a | b | c
        }
        IndicatorSpec::VolumeProfile(r) => int(ui, "Rows", r, 200),
        IndicatorSpec::Vwap | IndicatorSpec::Obv | IndicatorSpec::AnchoredVwap(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ray_runs_past_the_plot_in_the_pointing_direction() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(400.0, 300.0));
        let end = ray_end([10.0, 10.0], [20.0, 20.0], rect);
        assert!(end[0] > 400.0 && end[1] > 300.0 && (end[0] - 10.0 - (end[1] - 10.0)).abs() < 1e-6);
        let left = ray_end([100.0, 50.0], [90.0, 50.0], rect);
        assert!(left[0] < 0.0 && left[1] == 50.0);
        assert_eq!(ray_end([5.0, 5.0], [5.0, 5.0], rect), [5.0, 5.0]);
    }

    #[test]
    fn editing_a_spec_reports_changes_only_for_parameterised_ones() {
        assert!(spec_has_params(&IndicatorSpec::Atr(14)));
        assert!(!spec_has_params(&IndicatorSpec::Obv));
        assert!(!spec_has_params(&IndicatorSpec::AnchoredVwap(5)));
    }

    #[test]
    fn position_box_has_a_minimum_width() {
        assert_eq!(ChartState::position_x(100.0, 110.0), (100.0, 220.0));
        assert_eq!(ChartState::position_x(300.0, 100.0), (100.0, 300.0));
    }
}
