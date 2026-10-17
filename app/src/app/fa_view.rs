//! Financial analysis (`FA`): income statement, balance sheet, cash flow and ratios from
//! SEC XBRL filings, annual or quarterly, with year-over-year change and a bar chart of the
//! selected line.

use egui::{Align2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use tr_data::edgar::{Company, Metric, Period, Statement, Unit, prior_year};

use super::edgar_view::EdgarPane;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;

/// Periods shown at most.
const PERIODS: usize = 10;
const LABEL_W: f32 = 168.0;
const COL_W: f32 = 76.0;

/// `391.04B`, `-12.3M`, `850K`.
pub(super) fn fmt_compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.0}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// `$391.04B`.
pub(super) fn fmt_money(v: f64) -> String {
    if v < 0.0 { format!("-${}", fmt_compact(-v)) } else { format!("${}", fmt_compact(v)) }
}

/// A ratio metric's value: `46.2%` or `1.45x`.
pub(super) fn fmt_ratio(m: Metric, v: f64) -> String {
    match m.unit() {
        Unit::Times => format!("{v:.2}x"),
        _ => format!("{:.1}%", v * 100.0),
    }
}

fn fmt_value(m: Metric, v: f64) -> String {
    match m.unit() {
        Unit::Usd => fmt_compact(v),
        Unit::PerShare => format!("{v:.2}"),
        Unit::Pct | Unit::Times => fmt_ratio(m, v),
    }
}

/// Year-over-year change of `m` at `periods[i]`: percent for amounts, percentage points
/// for ratios.
fn yoy(periods: &[Period], i: usize, m: Metric) -> Option<f64> {
    let now = periods[i].get(m)?;
    let then = prior_year(periods, i)?.get(m)?;
    match m.unit() {
        Unit::Usd | Unit::PerShare => (then.abs() > f64::EPSILON).then(|| (now - then) / then.abs() * 100.0),
        Unit::Pct => Some((now - then) * 100.0),
        Unit::Times => Some(now - then),
    }
}

fn fmt_yoy(m: Metric, d: f64) -> String {
    match m.unit() {
        Unit::Usd | Unit::PerShare => format!("{d:+.1}%"),
        Unit::Pct => format!("{d:+.1}pp"),
        Unit::Times => format!("{d:+.2}x"),
    }
}

pub(super) fn fa(ui: &mut Ui, p: &mut EdgarPane, c: &Company, price: Option<f64>) {
    let f = match &c.financials {
        Ok(f) => f,
        Err(e) => {
            ui.label(
                RichText::new(format!("No financial statements available: {e}")).font(ui_font(12.0)).color(muted()),
            );
            return;
        }
    };
    ui.horizontal_wrapped(|ui| {
        let labels: Vec<&str> = Statement::ALL.iter().map(|s| s.label()).collect();
        if let Some(i) = widgets::segmented(ui, ("fa-tab", &p.symbol), &labels, Some(p.tab), 20.0) {
            p.tab = i;
            p.selected = None;
        }
        if let Some(i) = widgets::segmented(
            ui,
            ("fa-freq", &p.symbol),
            &["Annual", "Quarterly"],
            Some(usize::from(p.quarterly)),
            20.0,
        ) {
            p.quarterly = i == 1;
        }
    });
    if p.statement() == Statement::Ratios {
        let v = f.valuation(price);
        let mut parts = Vec::new();
        if let Some(m) = v.market_cap {
            parts.push(format!("Mkt cap {}", fmt_money(m)));
        }
        for (name, x) in [("P/E", v.pe), ("P/S", v.ps), ("P/B", v.pb)] {
            if let Some(x) = x {
                parts.push(format!("{name} {x:.1}x"));
            }
        }
        let line = if parts.is_empty() {
            "Valuation ratios need a live price".to_string()
        } else {
            format!("{} · live price, TTM", parts.join(" · "))
        };
        ui.label(RichText::new(line).font(mono(11.5)).color(theme::cyan()));
    }

    let periods = if p.quarterly { &f.quarterly } else { &f.annual };
    if periods.is_empty() {
        ui.label(RichText::new("No periods reported in XBRL.").font(ui_font(12.0)).color(muted()));
        return;
    }
    let w = ui.available_width();
    let n = (((w - LABEL_W) / COL_W).floor().max(1.0) as usize).min(PERIODS).min(periods.len());
    let first = periods.len() - n;
    let metrics: Vec<Metric> = Metric::of(p.statement()).collect();
    let row_h = dp(18.0);
    let col_x = |left: f32, j: usize| left + LABEL_W + (j + 1) as f32 * COL_W - 6.0;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Header: fiscal period and end date, newest on the right.
        let (hdr, _) = ui.allocate_exact_size(vec2(w, row_h * 2.0), Sense::hover());
        let painter = ui.painter_at(hdr);
        painter.rect_filled(hdr, theme::CONTROL_RADIUS, theme::header());
        painter.text(pos2(hdr.left() + 6.0, hdr.center().y), Align2::LEFT_CENTER, "USD", ui_font(11.0), muted());
        for (j, per) in periods[first..].iter().enumerate() {
            let x = col_x(hdr.left(), j);
            painter.text(pos2(x, hdr.top() + row_h * 0.5), Align2::RIGHT_CENTER, &per.label, mono(11.5), amber());
            let end = per.end.format("%m/%y").to_string();
            painter.text(pos2(x, hdr.top() + row_h * 1.5), Align2::RIGHT_CENTER, end, mono(10.5), muted());
        }

        for &m in &metrics {
            let selected = p.selected == Some(m);
            let (r, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
            widgets::hover_wash(ui, r, &resp, selected);
            let painter = ui.painter_at(r);
            let label_col = if selected { amber() } else { text() };
            painter.text(pos2(r.left() + 6.0, r.center().y), Align2::LEFT_CENTER, m.label(), ui_font(12.0), label_col);
            for (j, per) in periods[first..].iter().enumerate() {
                let s = per.get(m).map_or_else(|| "–".into(), |v| fmt_value(m, v));
                let col = if per.get(m).is_some_and(|v| v < 0.0) { theme::down() } else { text() };
                painter.text(pos2(col_x(r.left(), j), r.center().y), Align2::RIGHT_CENTER, s, mono(11.5), col);
            }
            if resp.clicked() {
                p.selected = if selected { None } else { Some(m) };
            }
            resp.on_hover_text("Click to chart");

            // Year-over-year change under each line.
            let (r, _) = ui.allocate_exact_size(vec2(w, row_h * 0.85), Sense::hover());
            let painter = ui.painter_at(r);
            painter.text(pos2(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, "YoY", ui_font(10.5), muted());
            for j in 0..n {
                if let Some(d) = yoy(periods, first + j, m) {
                    let col = if d >= 0.0 { theme::up() } else { theme::down() };
                    painter.text(
                        pos2(col_x(r.left(), j), r.center().y),
                        Align2::RIGHT_CENTER,
                        fmt_yoy(m, d),
                        mono(10.5),
                        col,
                    );
                }
            }
        }

        if let Some(m) = p.selected.filter(|m| metrics.contains(m)) {
            ui.add_space(8.0);
            bar_chart(ui, m, &periods[first..], w);
        }
        ui.add_space(4.0);
        let note = if p.quarterly {
            "Source: SEC XBRL company facts · Q4 and cash-flow quarters derived from year-to-date totals"
        } else {
            "Source: SEC XBRL company facts (10-K) · latest filed values, including restatements"
        };
        ui.label(RichText::new(note).font(ui_font(10.5)).color(muted()));
    });
}

/// Bars of one metric across the shown periods.
fn bar_chart(ui: &mut Ui, m: Metric, periods: &[Period], w: f32) {
    let h = dp(120.0);
    let (r, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter_at(r);
    painter.rect_filled(r, theme::CONTROL_RADIUS, theme::card());
    painter.text(pos2(r.left() + 6.0, r.top() + 9.0), Align2::LEFT_CENTER, m.label(), ui_font(11.5), amber());
    let vals: Vec<Option<f64>> = periods.iter().map(|p| p.get(m)).collect();
    let hi = vals.iter().flatten().fold(0.0f64, |a, v| a.max(*v));
    let lo = vals.iter().flatten().fold(0.0f64, |a, v| a.min(*v));
    if hi - lo <= 0.0 {
        return;
    }
    let plot = Rect::from_min_max(pos2(r.left() + LABEL_W, r.top() + 20.0), pos2(r.right() - 4.0, r.bottom() - 16.0));
    let y = |v: f64| plot.bottom() - ((v - lo) / (hi - lo)) as f32 * plot.height();
    painter.hline(plot.x_range(), y(0.0), Stroke::new(1.0, theme::grid()));
    for (j, (v, per)) in vals.iter().zip(periods).enumerate() {
        let x_right = plot.left() + (j + 1) as f32 * COL_W - 6.0;
        let bar = Rect::from_x_y_ranges(
            x_right - COL_W * 0.6..=x_right,
            y(v.unwrap_or(0.0)).min(y(0.0))..=y(v.unwrap_or(0.0)).max(y(0.0)),
        );
        if let Some(v) = v {
            let col = if *v >= 0.0 { theme::accent() } else { theme::down() };
            painter.rect_filled(bar, 2.0, col);
        }
        painter.text(pos2(bar.center().x, r.bottom() - 8.0), Align2::CENTER_CENTER, &per.label, mono(9.5), muted());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_compact(391_035_000_000.0), "391.04B");
        assert_eq!(fmt_compact(-12_340_000.0), "-12.3M");
        assert_eq!(fmt_compact(3.2e12), "3.20T");
        assert_eq!(fmt_money(-2.5e9), "-$2.50B");
        assert_eq!(fmt_value(Metric::GrossMargin, 0.4621), "46.2%");
        assert_eq!(fmt_value(Metric::DebtToEquity, 1.456), "1.46x");
        assert_eq!(fmt_value(Metric::EpsDiluted, 6.084), "6.08");
        assert_eq!(fmt_yoy(Metric::NetMargin, 1.25), "+1.2pp");
    }

    #[test]
    fn year_over_year() {
        let per = |end: &str, rev: f64, gm: f64| Period {
            end: chrono::NaiveDate::parse_from_str(end, "%Y-%m-%d").unwrap(),
            label: end.into(),
            values: [(Metric::Revenue, rev), (Metric::GrossMargin, gm)].into_iter().collect(),
        };
        let ps = vec![per("2023-12-31", 100.0, 0.40), per("2024-12-31", 110.0, 0.42)];
        assert!((yoy(&ps, 1, Metric::Revenue).unwrap() - 10.0).abs() < 1e-9);
        assert!((yoy(&ps, 1, Metric::GrossMargin).unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(yoy(&ps, 0, Metric::Revenue), None);
    }
}
