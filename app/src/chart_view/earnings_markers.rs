//! Earnings "E" markers on equity charts: one per past report date and one for the next date,
//! drawn in a strip along the bottom of the price pane, with a hover card (date, EPS,
//! surprise, price move). Separate from the congressional trade markers.

use egui::{Align2, Color32, Pos2, Rect, Stroke, StrokeKind, Ui, pos2, vec2};
use tr_chart::viewport::Viewport;
use tr_data::earnings::{EarningsEvent, NextEarnings};

use super::ChartState;
use crate::earnings_view::fmt_eps;
use crate::theme::{self, amber, amber_dim, mono, muted, text};

/// Marker half-size in points.
const R: f32 = 7.0;

fn noon(date: chrono::NaiveDate) -> Option<i64> {
    date.and_hms_opt(12, 0, 0).map(|d| d.and_utc().timestamp())
}

impl ChartState {
    pub(super) fn paint_earnings(&self, ui: &Ui, rect: Rect, v: &Viewport, hover: Option<Pos2>) {
        let e = &self.earnings;
        if (e.events.is_empty() && e.next.is_none()) || self.bars.is_empty() {
            return;
        }
        let p = ui.painter_at(rect);
        let len = self.bars.len();
        let (lo, hi) = v.visible_range(len);
        let y = rect.bottom() - R - 4.0;
        let mut best: Option<(f32, Card)> = None;
        let mut consider = |c: Pos2, card: Card| {
            if let Some(h) = hover {
                let d = h.distance(c);
                if d <= R + 3.0 && best.as_ref().is_none_or(|b| d < b.0) {
                    best = Some((d, card));
                }
            }
        };
        for ev in &e.events {
            let Some(ts) = noon(ev.date) else { continue };
            let idx = self.index_of_ts(ts).floor();
            if idx < lo as f64 || idx >= hi as f64 || idx < 0.0 || idx >= len as f64 {
                continue;
            }
            let c = pos2(rect.left() + v.x_center(idx as usize) as f32, y);
            let col = match ev.surprise_pct {
                Some(s) if s > 0.0 => theme::up(),
                Some(s) if s < 0.0 => theme::down(),
                _ => amber(),
            };
            p.rect_filled(Rect::from_center_size(c, vec2(2.0 * R, 2.0 * R)), 3.0, col);
            p.text(c, Align2::CENTER_CENTER, "E", mono(10.5), Color32::WHITE);
            consider(c, Card::Past(ev.clone()));
        }
        if let Some(n) = &e.next
            && let Some(ts) = noon(n.date)
        {
            let x = rect.left() + v.x_of(self.index_of_ts(ts)) as f32;
            if x > rect.left() + R && x < rect.right() - R {
                let c = pos2(x, y);
                let r = Rect::from_center_size(c, vec2(2.0 * R, 2.0 * R));
                p.rect_filled(r, 3.0, Color32::from_rgba_unmultiplied(0, 0, 0, 90));
                p.rect_stroke(r, 3.0, Stroke::new(1.5, theme::cyan()), StrokeKind::Inside);
                p.text(c, Align2::CENTER_CENTER, "E", mono(10.5), theme::cyan());
                consider(c, Card::Next(n.clone()));
            }
        }
        if let Some((_, card)) = best {
            let at = match &card {
                Card::Past(ev) => noon(ev.date).map(|t| self.index_of_ts(t).floor() as usize),
                Card::Next(_) => None,
            };
            let at = at.map_or_else(|| hover.unwrap_or(rect.center()), |i| pos2(rect.left() + v.x_center(i) as f32, y));
            card_ui(ui, at, &card);
        }
    }
}

enum Card {
    Past(EarningsEvent),
    Next(NextEarnings),
}

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |v| format!("{v:+.1}%"))
}

fn card_ui(ui: &Ui, at: Pos2, card: &Card) {
    let lines: Vec<(String, Color32)> = match card {
        Card::Past(e) => vec![
            (format!("Earnings  {}  {}", e.date, e.timing.label()), amber()),
            (format!("Quarter   {}", e.fiscal_quarter), muted()),
            (format!("EPS (SEC) {}", fmt_eps(e.eps_gaap)), text()),
            (format!("EPS adj   {}  est {}", fmt_eps(e.eps_actual), fmt_eps(e.eps_estimate)), text()),
            (
                format!("Surprise  {}", pct(e.surprise_pct)),
                e.surprise_pct.map_or(muted(), |s| if s >= 0.0 { theme::up() } else { theme::down() }),
            ),
            (
                format!("Move      {}", pct(e.move_pct)),
                e.move_pct.map_or(muted(), |s| if s >= 0.0 { theme::up() } else { theme::down() }),
            ),
        ],
        Card::Next(n) => vec![
            (format!("Next earnings  {}  {}", n.date, n.timing.label()), theme::cyan()),
            (if n.projected { "Projected from cadence".into() } else { "Announced".to_string() }, muted()),
            (format!("EPS est   {}", fmt_eps(n.eps_estimate)), text()),
        ],
    };
    let w = lines.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(10) as f32 * 7.2 + 14.0;
    let h = lines.len() as f32 * 14.0 + 8.0;
    let screen = ui.ctx().content_rect();
    let mut min = at + vec2(12.0, -h - 12.0);
    if min.x + w > screen.right() {
        min.x = at.x - w - 12.0;
    }
    if min.y < screen.top() {
        min.y = at.y + 12.0;
    }
    let card_rect = Rect::from_min_size(min, vec2(w, h));
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("earnings-card"));
    let p = ui.ctx().layer_painter(layer);
    p.rect_filled(card_rect, 0.0, Color32::from_rgb(16, 16, 16));
    p.rect_stroke(card_rect, 0.0, Stroke::new(1.0, amber_dim()), StrokeKind::Inside);
    for (k, (l, c)) in lines.iter().enumerate() {
        p.text(card_rect.left_top() + vec2(7.0, 4.0 + k as f32 * 14.0), Align2::LEFT_TOP, l, mono(11.5), *c);
    }
}
