//! Plug-in chart studies ([`tr_study`]): the registry filled by [`crate::run`], each chart's
//! study settings, the worker-side build (GPU lines) and the egui-drawn parts (fills,
//! markers, segments and the corner table).

use std::sync::{Arc, OnceLock};

use egui::{Align2, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};
use tr_chart::gpu::LineKind;
use tr_chart::scene::{Overlay, line_ref};
use tr_chart::viewport::{PriceScale, Viewport};
use tr_core::{Bar, Timeframe};
use tr_study::{Input, MarkerShape, Output, Params, ParamsView, Rgba, Study};

use crate::theme::{self, mono, muted, text};

static REGISTRY: OnceLock<Vec<Arc<dyn Study>>> = OnceLock::new();

/// Called once at start-up.
pub fn register(studies: Vec<Arc<dyn Study>>) {
    for s in &studies {
        tracing::info!("study registered: {} ({})", s.name(), s.id());
    }
    let _ = REGISTRY.set(studies);
}

pub fn all() -> &'static [Arc<dyn Study>] {
    REGISTRY.get().map(Vec::as_slice).unwrap_or(&[])
}

pub fn get(id: &str) -> Option<&'static Arc<dyn Study>> {
    all().iter().find(|s| s.id() == id)
}

/// Studies that new charts start with.
pub fn defaults() -> Vec<StudyConfig> {
    all().iter().filter(|s| s.default_on()).map(|s| StudyConfig::new(s.id())).collect()
}

/// Study whose command-line function is `head` (case-insensitive).
pub fn by_command(head: &str) -> Option<&'static Arc<dyn Study>> {
    all().iter().find(|s| s.command().is_some_and(|c| c.eq_ignore_ascii_case(head)))
}

/// A study on a chart with its settings (persisted in layouts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudyConfig {
    pub id: String,
    #[serde(default)]
    pub params: Params,
}

impl StudyConfig {
    pub fn new(id: &str) -> Self {
        Self { id: id.into(), params: Params::new() }
    }
}

pub struct BuiltPane {
    pub title: String,
    pub overlays: Vec<(String, Overlay)>,
    pub levels: Vec<(f64, Color32)>,
    pub range: Option<(f64, f64)>,
}

/// A computed study, ready to draw.
pub struct BuiltStudy {
    pub name: String,
    pub output: Output,
    /// Price-pane lines on the GPU, with their legend flag.
    pub lines: Vec<(String, Overlay, bool)>,
    pub panes: Vec<BuiltPane>,
}

pub fn color(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

fn overlay(line: &tr_study::Line) -> Overlay {
    // The line shader blends straight alpha.
    let c = line.color;
    Overlay {
        line: line_ref(&line.values),
        color: [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, c[3] as f32 / 255.0],
        kind: if line.histogram { LineKind::Hist } else { LineKind::Line },
        scale: 1.0,
    }
}

/// Worker side: run every configured study and upload its lines.
pub fn build(symbol: &str, tf: Timeframe, bars: &[Bar], configs: &[StudyConfig]) -> Vec<BuiltStudy> {
    configs
        .iter()
        .filter_map(|cfg| {
            let study = get(&cfg.id)?;
            let input = Input { symbol, tf, bars, params: ParamsView::new(study.params(), &cfg.params) };
            let t0 = std::time::Instant::now();
            let output = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| study.compute(&input))) {
                Ok(o) => o,
                Err(_) => {
                    tracing::error!("study {} panicked on {symbol} {tf}", study.id());
                    return None;
                }
            };
            tracing::debug!("study {} on {} bars in {:?}", study.id(), bars.len(), t0.elapsed());
            let lines = output.lines.iter().map(|l| (l.name.clone(), overlay(l), l.legend)).collect();
            let panes = output
                .panes
                .iter()
                .map(|p| BuiltPane {
                    title: p.title.clone(),
                    overlays: p.lines.iter().map(|l| (l.name.clone(), overlay(l))).collect(),
                    levels: p.levels.iter().map(|(v, c)| (*v, color(*c))).collect(),
                    range: p.range,
                })
                .collect();
            Some(BuiltStudy { name: study.name().to_string(), output, lines, panes })
        })
        .collect()
}

// ---- egui-drawn parts --------------------------------------------------------------------

/// Where the price pane is on screen and how bars/prices map into it.
pub struct Frame<'a> {
    pub rect: Rect,
    pub v: &'a Viewport,
    pub s: &'a PriceScale,
    pub bars: &'a [Bar],
}

impl Frame<'_> {
    fn x(&self, i: usize) -> f32 {
        self.rect.left() + self.v.x_center(i) as f32
    }

    fn y(&self, price: f64) -> f32 {
        self.rect.top() + self.s.y_of(price) as f32
    }

    fn visible(&self) -> (usize, usize) {
        self.v.visible_range(self.bars.len())
    }
}

/// Shaded areas between two series, as one mesh over the visible bars.
pub fn paint_fills(ui: &Ui, f: &Frame<'_>, fills: &[tr_study::Fill]) {
    let p = ui.painter_at(f.rect);
    let (lo, hi) = f.visible();
    for fill in fills {
        let c = color(fill.color);
        let mut mesh = Mesh::default();
        let lo = lo.saturating_sub(1);
        let hi = (hi + 1).min(fill.a.len()).min(fill.b.len());
        for i in lo..hi.saturating_sub(1) {
            let (a0, b0, a1, b1) = (fill.a[i], fill.b[i], fill.a[i + 1], fill.b[i + 1]);
            if !(a0.is_finite() && b0.is_finite() && a1.is_finite() && b1.is_finite()) {
                continue;
            }
            let (x0, x1) = (f.x(i), f.x(i + 1));
            let base = mesh.vertices.len() as u32;
            for (x, y) in [(x0, f.y(a0)), (x0, f.y(b0)), (x1, f.y(a1)), (x1, f.y(b1))] {
                mesh.colored_vertex(pos2(x, y), c);
            }
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 1, base + 3, base + 2);
        }
        if !mesh.is_empty() {
            p.add(Shape::mesh(mesh));
        }
    }
}

pub fn paint_segments(ui: &Ui, f: &Frame<'_>, segments: &[tr_study::Segment]) {
    let p = ui.painter_at(f.rect);
    let (lo, hi) = f.visible();
    for s in segments {
        if s.to.0 < lo || s.from.0 >= hi || s.to.0 >= f.bars.len() {
            continue;
        }
        let a = pos2(f.x(s.from.0), f.y(s.from.1));
        let b = pos2(f.x(s.to.0), f.y(s.to.1));
        let stroke = Stroke::new(s.width, color(s.color));
        if s.dashed {
            p.extend(Shape::dashed_line(&[a, b], stroke, 6.0, 4.0));
        } else {
            p.line_segment([a, b], stroke);
        }
    }
}

/// Markers over the visible bars. Returns the tooltip of the marker under the pointer.
pub fn paint_markers(
    ui: &Ui,
    f: &Frame<'_>,
    markers: &[tr_study::Marker],
    hover: Option<Pos2>,
) -> Option<(Pos2, String)> {
    let p = ui.painter_at(f.rect);
    let (lo, hi) = f.visible();
    let small = (f.v.bar_px as f32 * 0.4).clamp(3.0, 6.0);
    let mut best: Option<(f32, Pos2, String)> = None;
    for m in markers {
        if m.bar < lo || m.bar >= hi || m.bar >= f.bars.len() {
            continue;
        }
        let bar = f.bars[m.bar];
        let x = f.x(m.bar);
        let c = color(m.color);
        let (center, hit_r) = match m.shape {
            MarkerShape::LabelUp | MarkerShape::LabelDown => {
                let up = m.shape == MarkerShape::LabelUp;
                let g = p.layout_no_wrap(m.text.clone(), mono(10.5), Color32::WHITE);
                let (w, h) = (g.size().x + 8.0, g.size().y + 3.0);
                let tip = if up { f.y(bar.low) + 4.0 } else { f.y(bar.high) - 4.0 };
                let body = if up {
                    Rect::from_min_size(pos2(x - w / 2.0, tip + 5.0), vec2(w, h))
                } else {
                    Rect::from_min_size(pos2(x - w / 2.0, tip - 5.0 - h), vec2(w, h))
                };
                let arrow_y = if up { body.top() } else { body.bottom() };
                p.add(Shape::convex_polygon(
                    vec![pos2(x, tip), pos2(x + 4.0, arrow_y), pos2(x - 4.0, arrow_y)],
                    c,
                    Stroke::NONE,
                ));
                p.rect_filled(body, 2.0, c);
                p.galley(body.center() - g.size() / 2.0, g, Color32::WHITE);
                (body.center(), w / 2.0)
            }
            MarkerShape::TriangleUp | MarkerShape::TriangleDown => {
                let up = m.shape == MarkerShape::TriangleUp;
                let y = if up { f.y(bar.low) + 4.0 + small * 2.0 } else { f.y(bar.high) - 4.0 - small * 2.0 };
                let (tip, base) = if up { (y - small, y + small) } else { (y + small, y - small) };
                p.add(Shape::convex_polygon(
                    vec![pos2(x, tip), pos2(x + small, base), pos2(x - small, base)],
                    c,
                    Stroke::NONE,
                ));
                (pos2(x, y), small + 2.0)
            }
            MarkerShape::Diamond { below } => {
                let y = if below { f.y(bar.low) + 6.0 + small } else { f.y(bar.high) - 6.0 - small };
                p.add(Shape::convex_polygon(
                    vec![pos2(x, y - small), pos2(x + small, y), pos2(x, y + small), pos2(x - small, y)],
                    c,
                    Stroke::NONE,
                ));
                (pos2(x, y), small + 2.0)
            }
            MarkerShape::Dot => {
                let pos = pos2(x, f.y(m.price));
                p.circle_filled(pos, small * 0.7, c);
                (pos, small + 2.0)
            }
        };
        if let Some(h) = hover
            && !m.tooltip.is_empty()
        {
            let d = h.distance(center);
            if d <= hit_r + 3.0 && best.as_ref().is_none_or(|b| d < b.0) {
                best = Some((d, center, m.tooltip.clone()));
            }
        }
    }
    best.map(|(_, c, t)| (c, t))
}

/// Multi-line hover card on the tooltip layer.
pub fn tooltip_card(ui: &Ui, at: Pos2, body: &str) {
    let lines: Vec<&str> = body.lines().collect();
    let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(10) as f32 * 7.0 + 14.0;
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
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("study-card"));
    let p = ui.ctx().layer_painter(layer);
    p.rect_filled(card, 3.0, theme::card());
    p.rect_stroke(card, 3.0, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    for (k, l) in lines.iter().enumerate() {
        p.text(card.left_top() + vec2(7.0, 4.0 + k as f32 * 14.0), Align2::LEFT_TOP, *l, mono(11.0), text());
    }
}

/// The study table in the top-right corner of `area`, starting `top` points down.
/// Cell tooltips show on hover. Returns the table's rectangle.
pub fn paint_table(ui: &Ui, area: Rect, top: f32, table: &tr_study::Table, salt: egui::Id) -> Rect {
    let font = mono(10.5);
    let p = ui.painter_at(area);
    let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return Rect::NOTHING;
    }
    let mut widths = vec![0.0f32; cols];
    let row_h = 15.0;
    for row in &table.rows {
        // A row with fewer cells spans its last cell across the rest (measured later).
        for (k, cell) in row.iter().enumerate() {
            if row.len() == cols || k + 1 < row.len() {
                let g = p.layout_no_wrap(cell.text.clone(), font.clone(), text());
                widths[k] = widths[k].max(g.size().x + 10.0);
            }
        }
    }
    // Widen the last column until every spanning cell fits.
    for row in table.rows.iter().filter(|r| !r.is_empty() && r.len() < cols) {
        let k = row.len() - 1;
        let g = p.layout_no_wrap(row[k].text.clone(), font.clone(), text());
        let have: f32 = widths[k..].iter().sum();
        let need = g.size().x + 12.0;
        if need > have {
            widths[cols - 1] += need - have;
        }
    }
    let total_w: f32 = widths.iter().sum();
    let total_h = row_h * table.rows.len() as f32;
    let rect = Rect::from_min_size(pos2(area.right() - total_w - 8.0, area.top() + top), vec2(total_w, total_h));
    p.rect_filled(rect, 3.0, theme::card().gamma_multiply(0.92));
    p.rect_stroke(rect, 3.0, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    let mut hovered: Option<(Pos2, String)> = None;
    for (r, row) in table.rows.iter().enumerate() {
        let y = rect.top() + r as f32 * row_h;
        let mut x = rect.left();
        for (k, cell) in row.iter().enumerate() {
            let w = if k + 1 == row.len() { rect.right() - x } else { widths[k] };
            let cr = Rect::from_min_size(pos2(x, y), vec2(w, row_h));
            if let Some(bg) = cell.background {
                p.rect_filled(cr.shrink(1.0), 2.0, color(bg));
            }
            let c = cell.color.map(color).unwrap_or_else(text);
            let align = if k == 0 { Align2::LEFT_CENTER } else { Align2::CENTER_CENTER };
            let at = if k == 0 { cr.left_center() + vec2(5.0, 0.0) } else { cr.center() };
            p.text(at, align, &cell.text, font.clone(), c);
            if !cell.tooltip.is_empty() {
                let resp = ui.interact(cr, salt.with((r, k)), Sense::hover());
                if resp.hovered() {
                    hovered = Some((cr.left_bottom(), cell.tooltip.clone()));
                }
            }
            x += w;
        }
        if r > 0 {
            p.hline(rect.x_range(), y, Stroke::new(0.5, theme::border()));
        }
    }
    if let Some((at, tip)) = hovered {
        tooltip_card(ui, at, &tip);
    }
    rect
}

/// A piece of legend text and its colour.
pub type Colored = (String, Color32);

/// Legend line for a study: name, summary, and the last values of its legend lines.
pub fn legend_parts(b: &BuiltStudy, i: usize) -> (String, Option<Colored>, Vec<Colored>) {
    let summary = b.output.summary.as_ref().map(|c| (c.text.clone(), c.color.map(color).unwrap_or_else(muted)));
    let vals = b
        .output
        .lines
        .iter()
        .filter(|l| l.legend)
        .filter_map(|l| l.values.get(i).filter(|v| v.is_finite()).map(|v| (format!("{v:.2}"), color(l.color))))
        .collect();
    (b.name.clone(), summary, vals)
}
