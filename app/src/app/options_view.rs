//! Option chain monitor (OMON): calls and puts by strike for one expiration, with quotes,
//! volume, open interest, implied volatility and Black-Scholes greeks.

use egui::{Align2, Color32, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use tr_core::options::Greeks;
use tr_data::OptionContract;

use super::OptionsPane;
use crate::congress_view::Action;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;

/// A per-side column; listed innermost (next to the strike) first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Bid,
    Ask,
    Last,
    Iv,
    Delta,
    Volume,
    Oi,
    Gamma,
    Theta,
    Vega,
    Change,
}

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Bid => "Bid",
            Col::Ask => "Ask",
            Col::Last => "Last",
            Col::Iv => "IV",
            Col::Delta => "Δ",
            Col::Volume => "Vol",
            Col::Oi => "OI",
            Col::Gamma => "Γ",
            Col::Theta => "Θ/day",
            Col::Vega => "Vega",
            Col::Change => "Chg",
        }
    }

    fn greek(self) -> bool {
        matches!(self, Col::Delta | Col::Gamma | Col::Theta | Col::Vega)
    }
}

const COLS: [Col; 11] = [
    Col::Bid,
    Col::Ask,
    Col::Last,
    Col::Iv,
    Col::Delta,
    Col::Volume,
    Col::Oi,
    Col::Change,
    Col::Gamma,
    Col::Theta,
    Col::Vega,
];

fn fmt_count(v: Option<u64>) -> String {
    match v {
        None | Some(0) => "–".into(),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 10_000 => format!("{:.0}k", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

fn fmt_px(v: Option<f64>) -> String {
    v.filter(|x| *x > 0.0).map_or_else(|| "–".into(), |x| format!("{x:.2}"))
}

/// Cell text and colour for one contract column.
fn cell(col: Col, c: &OptionContract, g: Option<&Greeks>) -> (String, Color32) {
    let gv = |f: fn(&Greeks) -> f64, d: usize| g.map_or_else(|| "–".into(), |g| format!("{:.*}", d, f(g)));
    match col {
        Col::Bid => (fmt_px(c.bid), text()),
        Col::Ask => (fmt_px(c.ask), text()),
        Col::Last => (fmt_px(c.last), amber()),
        Col::Change => match c.change {
            Some(ch) if ch.abs() > 1e-9 => (format!("{ch:+.2}"), if ch >= 0.0 { theme::up() } else { theme::down() }),
            _ => ("–".into(), muted()),
        },
        Col::Iv => (c.iv.map_or_else(|| "–".into(), |v| format!("{:.1}%", v * 100.0)), theme::cyan()),
        Col::Volume => (fmt_count(c.volume), text()),
        Col::Oi => (fmt_count(c.open_interest), muted()),
        Col::Delta => (gv(|g| g.delta, 2), text()),
        Col::Gamma => (gv(|g| g.gamma, 4), muted()),
        Col::Theta => (gv(|g| g.theta, 3), muted()),
        Col::Vega => (gv(|g| g.vega, 3), muted()),
    }
}

/// One strike row: the call and put at that strike, if listed.
struct Row {
    strike: f64,
    call: Option<usize>,
    put: Option<usize>,
}

fn rows(o: &OptionsPane) -> Vec<Row> {
    let Some(ch) = &o.chain else { return Vec::new() };
    let mut strikes: Vec<f64> = ch.calls.iter().chain(&ch.puts).map(|c| c.strike).collect();
    strikes.sort_by(f64::total_cmp);
    strikes.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let find = |v: &[OptionContract], k: f64| v.iter().position(|c| (c.strike - k).abs() < 1e-9);
    let all: Vec<Row> =
        strikes.iter().map(|&k| Row { strike: k, call: find(&ch.calls, k), put: find(&ch.puts, k) }).collect();
    if o.strikes == 0 || all.len() <= o.strikes * 2 + 1 {
        return all;
    }
    let atm = all
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1.strike - ch.spot).abs().total_cmp(&(b.1.strike - ch.spot).abs()))
        .map_or(0, |(i, _)| i);
    let lo = atm.saturating_sub(o.strikes);
    let hi = (atm + o.strikes + 1).min(all.len());
    all.into_iter().skip(lo).take(hi - lo).collect()
}

/// Editable ticker: type a symbol and press Enter to load it; Escape keeps the old one.
fn symbol_box(ui: &mut Ui, o: &mut OptionsPane) {
    let mut buf = o.symbol_edit.take().unwrap_or_else(|| tr_data::security(&o.symbol).ticker);
    let r = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .font(mono(13.0))
            .text_color(amber())
            .hint_text("AAPL / SPY")
            .desired_width(theme::dp(72.0)),
    );
    let r = r.on_hover_text("Type a ticker and press Enter");
    if r.lost_focus() {
        let entered = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let typed = buf.trim().to_ascii_uppercase();
        if entered && !typed.is_empty() {
            let key = tr_data::security(&typed).key();
            if key != o.symbol {
                *o = OptionsPane::new(&key, o.strikes, o.show_greeks);
            }
        }
    } else if r.has_focus() {
        o.symbol_edit = Some(buf);
    }
}

/// Draw the chain. Returns an action (open the underlying's chart) and whether a refetch
/// is needed (expiration changed or refresh clicked).
pub fn options_chain(ui: &mut Ui, o: &mut OptionsPane) -> (Option<Action>, bool) {
    let mut action = None;
    let mut refetch = false;
    ui.horizontal_wrapped(|ui| {
        symbol_box(ui, o);
        let r = ui.label(RichText::new("↗").font(mono(13.0)).color(amber())).interact(Sense::click());
        if r.clicked() {
            action = Some(Action::OpenChart(o.symbol.clone()));
        }
        r.on_hover_text("Open chart");
        if let Some(ch) = &o.chain {
            ui.label(RichText::new(format!("{:.2}", ch.spot)).font(mono(13.0)).color(text()));
            let asof = chrono::DateTime::from_timestamp(ch.ts, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_default();
            ui.label(RichText::new(format!("{} · {asof}", ch.source)).font(ui_font(11.0)).color(muted()));
            if ch.simulated {
                ui.label(
                    RichText::new(" SIMULATED ").font(ui_font(10.5)).color(Color32::BLACK).background_color(amber()),
                )
                .on_hover_text("Offline or no options source: prices are synthetic (Black-Scholes).");
            }
        }
        if o.loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    if let Some(ch) = o.chain.as_deref() {
        let now = chrono::Utc::now().timestamp();
        let label = |e: i64| {
            let d = chrono::DateTime::from_timestamp(e, 0)
                .map(|d| d.format("%a %b %d '%y").to_string())
                .unwrap_or_default();
            format!("{d} · {}d", ((e + 20 * 3600 - now) as f64 / 86_400.0).floor().max(0.0) as i64)
        };
        let shown = o.expiration.unwrap_or(ch.expiration);
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt(("omon-exp", &o.symbol))
                .selected_text(RichText::new(label(shown)).font(ui_font(11.5)))
                .height(360.0)
                .show_ui(ui, |ui| {
                    for &e in &ch.expirations {
                        if ui.selectable_label(e == shown, label(e)).clicked() && e != shown {
                            o.expiration = Some(e);
                            o.loading = true;
                            refetch = true;
                        }
                    }
                });
            let choices = [10usize, 20, 0];
            let sel = choices.iter().position(|&n| n == o.strikes);
            if let Some(i) = widgets::segmented(ui, ("omon-strikes", &o.symbol), &["±10", "±20", "All"], sel, 20.0) {
                o.strikes = choices[i];
            }
            ui.checkbox(&mut o.show_greeks, RichText::new("Greeks").font(ui_font(11.5)));
            // At-the-money IV: average of the call and put nearest the spot.
            let atm_iv = |v: &[OptionContract]| {
                v.iter()
                    .min_by(|a, b| (a.strike - ch.spot).abs().total_cmp(&(b.strike - ch.spot).abs()))
                    .and_then(|c| c.iv)
            };
            let ivs: Vec<f64> = [atm_iv(&ch.calls), atm_iv(&ch.puts)].into_iter().flatten().collect();
            if !ivs.is_empty() {
                let iv = ivs.iter().sum::<f64>() / ivs.len() as f64;
                ui.label(RichText::new(format!("ATM IV {:.1}%", iv * 100.0)).font(ui_font(11.5)).color(theme::cyan()));
            }
            ui.label(RichText::new(format!("r {:.2}%", o.rate * 100.0)).font(ui_font(11.0)).color(muted()))
                .on_hover_text("Risk-free rate for greeks (3-month T-bill when the curve is loaded)");
            if ui.small_button("Refresh").clicked() {
                refetch = true;
            }
        });
    }
    if let Some(e) = &o.error {
        ui.label(RichText::new(format!("Options: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    let Some(ch) = o.chain.as_deref() else {
        if o.error.is_none() {
            ui.label(RichText::new("Loading option chain…").font(ui_font(12.0)).color(muted()));
        }
        return (action, refetch);
    };
    let rows = rows(o);
    if rows.is_empty() {
        ui.label(RichText::new("No contracts listed for this expiration.").font(ui_font(12.0)).color(muted()));
        return (action, refetch);
    }

    // Columns per side, dropped from the outside in as the pane narrows.
    let w = ui.available_width();
    let col_w = 58.0;
    let strike_w = 74.0;
    let fit = (((w - strike_w) / 2.0) / col_w).floor().max(1.0) as usize;
    let cols: Vec<Col> = COLS.iter().copied().filter(|c| o.show_greeks || !c.greek()).take(fit).collect();
    let side_w = cols.len() as f32 * col_w;
    let total = side_w * 2.0 + strike_w;
    let left = ui.cursor().left() + ((w - total) / 2.0).max(0.0);
    let strike_x = left + side_w;
    let row_h = dp(19.0);
    // Column x (right edge) for the call side (mirrored) and the put side.
    let call_x = |i: usize| strike_x - i as f32 * col_w - 6.0;
    let put_x = |i: usize| strike_x + strike_w + (i + 1) as f32 * col_w - 6.0;

    // Header.
    let (hdr, _) = ui.allocate_exact_size(vec2(w, row_h * 2.0), Sense::hover());
    let p = ui.painter_at(hdr);
    p.rect_filled(hdr, theme::CONTROL_RADIUS, theme::header());
    let y1 = hdr.top() + row_h * 0.5;
    let y2 = hdr.top() + row_h * 1.5;
    p.text(pos2(strike_x - side_w / 2.0, y1), Align2::CENTER_CENTER, "CALLS", ui_font(11.5), theme::up());
    p.text(pos2(strike_x + strike_w + side_w / 2.0, y1), Align2::CENTER_CENTER, "PUTS", ui_font(11.5), theme::down());
    p.text(pos2(strike_x + strike_w / 2.0, y2), Align2::CENTER_CENTER, "Strike", ui_font(11.0), muted());
    for (i, c) in cols.iter().enumerate() {
        p.text(pos2(call_x(i), y2), Align2::RIGHT_CENTER, c.label(), ui_font(11.0), muted());
        p.text(pos2(put_x(i), y2), Align2::RIGHT_CENTER, c.label(), ui_font(11.0), muted());
    }

    let spot = ch.spot;
    let itm_wash = theme::amber().gamma_multiply(0.07);
    egui::ScrollArea::vertical().id_salt(("omon", &o.symbol)).auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            let mut spot_line = None;
            for ri in range {
                let row = &rows[ri];
                let (r, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
                let p = ui.painter_at(r);
                let call_side = Rect::from_min_max(pos2(strike_x - side_w, r.top()), pos2(strike_x, r.bottom()));
                let put_side = Rect::from_min_max(
                    pos2(strike_x + strike_w, r.top()),
                    pos2(strike_x + strike_w + side_w, r.bottom()),
                );
                if row.strike < spot {
                    p.rect_filled(call_side, 0.0, itm_wash);
                }
                if row.strike > spot {
                    p.rect_filled(put_side, 0.0, itm_wash);
                }
                if resp.hovered() {
                    p.rect_filled(r, 3.0, theme::control());
                }
                let strike_r = Rect::from_min_size(pos2(strike_x, r.top()), vec2(strike_w, row_h));
                p.rect_filled(strike_r.shrink2(vec2(2.0, 1.0)), 3.0, theme::header());
                p.text(strike_r.center(), Align2::CENTER_CENTER, format!("{:.2}", row.strike), mono(12.0), text());
                let cy = r.center().y;
                if let Some(ci) = row.call {
                    let c = &ch.calls[ci];
                    let g = o.call_greeks.get(ci).and_then(Option::as_ref);
                    for (i, col) in cols.iter().enumerate() {
                        let (t, color) = cell(*col, c, g);
                        p.text(pos2(call_x(i), cy), Align2::RIGHT_CENTER, t, mono(11.5), color);
                    }
                }
                if let Some(pi) = row.put {
                    let c = &ch.puts[pi];
                    let g = o.put_greeks.get(pi).and_then(Option::as_ref);
                    for (i, col) in cols.iter().enumerate() {
                        let (t, color) = cell(*col, c, g);
                        p.text(pos2(put_x(i), cy), Align2::RIGHT_CENTER, t, mono(11.5), color);
                    }
                }
                // Spot marker between the strikes that bracket it.
                let next = rows.get(ri + 1).map(|n| n.strike);
                if row.strike <= spot && next.is_some_and(|n| n > spot) {
                    spot_line = Some(r);
                }
                resp.context_menu(|ui| {
                    if let Some(ci) = row.call
                        && ui.button(format!("Copy {}", ch.calls[ci].contract)).clicked()
                    {
                        ui.ctx().copy_text(ch.calls[ci].contract.clone());
                        ui.close();
                    }
                    if let Some(pi) = row.put
                        && ui.button(format!("Copy {}", ch.puts[pi].contract)).clicked()
                    {
                        ui.ctx().copy_text(ch.puts[pi].contract.clone());
                        ui.close();
                    }
                });
            }
            // Drawn after every row so the next row cannot paint over the badge.
            if let Some(r) = spot_line {
                let p = ui.painter();
                p.hline(r.x_range(), r.bottom(), Stroke::new(1.5, theme::accent()));
                let g = p.layout_no_wrap(format!("{spot:.2}"), mono(10.0), Color32::WHITE);
                let pill = Rect::from_min_size(
                    pos2(r.left() + 2.0, r.bottom() - g.size().y / 2.0 - 1.0),
                    g.size() + vec2(8.0, 2.0),
                );
                p.rect_filled(pill, 4.0, theme::accent());
                p.galley(pill.min + vec2(4.0, 1.0), g, Color32::WHITE);
            }
        },
    );
    (action, refetch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> OptionsPane {
        let mut o = OptionsPane::new("AAPL US Equity", 0, true);
        let chain = tr_data::sample::simulated_chain("AAPL US Equity", 230.0, None, 1_791_800_000);
        o.set_chain(Box::new(chain), 0.04, 1_791_800_000);
        o
    }

    #[test]
    fn rows_pair_calls_and_puts_and_filter_around_the_money() {
        let mut o = pane();
        let all = rows(&o);
        assert!(all.len() > 30);
        assert!(all.windows(2).all(|w| w[0].strike < w[1].strike));
        assert!(all.iter().all(|r| r.call.is_some() && r.put.is_some()));
        o.strikes = 5;
        let near = rows(&o);
        assert_eq!(near.len(), 11);
        assert!(near.first().unwrap().strike < 230.0 && near.last().unwrap().strike > 230.0);
    }

    #[test]
    fn greeks_follow_moneyness() {
        let o = pane();
        let ch = o.chain.as_ref().unwrap();
        let deltas: Vec<f64> = o.call_greeks.iter().map(|g| g.unwrap().delta).collect();
        assert!(deltas.windows(2).all(|w| w[0] >= w[1]), "call delta falls as strike rises");
        assert!(deltas[0] > 0.9 && *deltas.last().unwrap() < 0.1);
        let pd = o.put_greeks[ch.puts.len() / 2].unwrap().delta;
        assert!((-1.0..0.0).contains(&pd));
        assert!(o.call_greeks.iter().all(|g| g.unwrap().theta < 0.0));
        let (t, _) = cell(Col::Iv, &ch.calls[0], None);
        assert!(t.ends_with('%'));
        assert_eq!(fmt_count(Some(12_345)), "12k");
        assert_eq!(fmt_px(Some(0.0)), "–");
    }
}
