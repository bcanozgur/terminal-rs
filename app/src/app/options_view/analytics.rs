//! Analytics and strategy tabs of the option pane: IV smile and skew, term structure,
//! put/call ratios, max pain and open-interest walls (`Analytics`), and a multi-leg payoff
//! builder (`Payoff`). Everything is computed from the chains the pane has already loaded
//! (the maths is [`tr_core::optstrat`]); other expirations are fetched one at a time through
//! the pane's own refresh, never twice.

use std::collections::BTreeMap;

use egui::{Align2, Color32, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use tr_core::options::black_scholes;
use tr_core::optstrat::{self as os, Bound, Leg, LegKind, MaxPain, Preset, PutCall, Quote, SkewBasis};
use tr_data::{OptionChain, OptionContract};

use crate::app::OptionsPane;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;

/// Expirations loaded per batch for the term structure and totals.
const BATCH: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Chain,
    Analytics,
    Payoff,
}

#[derive(Default)]
pub struct AnState {
    pub tab: Tab,
    /// Every chain received for this symbol, by expiration.
    pub cache: BTreeMap<i64, Box<OptionChain>>,
    /// Expirations still to fetch (popped from the end).
    pub queue: Vec<i64>,
    /// The first batch has been queued.
    started: bool,
    pub legs: Vec<Leg>,
    /// Last thing that went wrong adding a leg or preset.
    note: Option<String>,
    add_kind: Option<LegKind>,
    add_strike: Option<f64>,
}

impl AnState {
    /// Queue the next [`BATCH`] expirations that are not loaded yet, nearest first.
    fn load_more(&mut self, expirations: &[i64]) {
        let mut todo: Vec<i64> =
            expirations.iter().copied().filter(|e| !self.cache.contains_key(e)).take(BATCH).collect();
        todo.reverse();
        self.queue = todo;
    }

    /// Add the chain contract as a leg (long when `qty > 0`). `None` without a usable price.
    pub fn add_contract(&mut self, ch: &OptionChain, call: bool, c: &OptionContract, qty: i32) -> bool {
        match option_leg(ch, call, c, qty) {
            Some(l) => {
                self.legs.push(l);
                self.note = None;
                true
            }
            None => {
                self.note = Some(format!("{} has no bid/ask/last price, so it was not added.", c.contract));
                false
            }
        }
    }
}

fn option_leg(ch: &OptionChain, call: bool, c: &OptionContract, qty: i32) -> Option<Leg> {
    let premium = c.mid().filter(|p| *p > 0.0)?;
    Some(Leg {
        kind: if call { LegKind::Call } else { LegKind::Put },
        strike: c.strike,
        expiry: ch.expiration,
        qty,
        premium,
        iv: c.iv.filter(|v| *v > 0.0),
    })
}

/// Years to expiry for greeks, floored at half a day like the chain's own greeks.
fn greek_t(expiry: i64, now: i64) -> f64 {
    os::years_to_expiry(expiry, now).max(0.5 / 365.0)
}

/// Analytics inputs for one side of a chain, with deltas derived from each contract's IV.
fn quotes(ch: &OptionChain, call: bool, rate: f64, now: i64) -> Vec<Quote> {
    let side = if call { &ch.calls } else { &ch.puts };
    let t = greek_t(ch.expiration, now);
    side.iter()
        .map(|c| Quote {
            strike: c.strike,
            iv: c.iv.filter(|v| *v > 0.0),
            delta: c
                .iv
                .and_then(|iv| black_scholes(call, ch.spot, c.strike, t, rate, ch.dividend_yield, iv))
                .map(|(_, g)| g.delta),
            volume: c.volume,
            open_interest: c.open_interest,
        })
        .collect()
}

// ---- formatting -----------------------------------------------------------------------

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".into(), |v| format!("{:.1}%", v * 100.0))
}

fn ratio(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".into(), |v| format!("{v:.2}"))
}

fn count(v: Option<u64>) -> String {
    v.map_or_else(|| "n/a".into(), |n| group(n as f64))
}

/// 1234567.0 → "1,234,567".
fn group(v: f64) -> String {
    let s = format!("{:.0}", v.abs());
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if v < -0.5 { format!("-{out}") } else { out }
}

fn money(v: f64) -> String {
    format!("{}${}", if v < -0.5 { "-" } else { "" }, group(v.abs()))
}

fn signed_money(v: f64) -> String {
    format!("{}${}", if v < -0.5 { "-" } else { "+" }, group(v.abs()))
}

fn date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%b %d '%y").to_string()).unwrap_or_default()
}

fn dte(exp: i64, now: i64) -> i64 {
    ((exp + 20 * 3600 - now) as f64 / 86_400.0).floor().max(0.0) as i64
}

// ---- tiny line chart ------------------------------------------------------------------

struct Series {
    pts: Vec<(f64, f64)>,
    color: Color32,
    name: &'static str,
    dots: bool,
}

struct VLine {
    x: f64,
    color: Color32,
    label: String,
}

fn plot(
    ui: &mut Ui,
    height: f32,
    series: &[Series],
    vlines: &[VLine],
    fx: &dyn Fn(f64) -> String,
    fy: &dyn Fn(f64) -> String,
) {
    let (resp, p) = ui.allocate_painter(vec2(ui.available_width(), dp(height)), Sense::hover());
    let r = resp.rect;
    p.rect_filled(r, 3.0, theme::card());
    let all: Vec<(f64, f64)> = series.iter().flat_map(|s| s.pts.iter().copied()).collect();
    if all.len() < 2 {
        p.text(r.center(), Align2::CENTER_CENTER, "n/a: not enough data", ui_font(12.0), muted());
        return;
    }
    let plot_r = Rect::from_min_max(
        pos2(r.left() + dp(48.0), r.top() + dp(6.0)),
        pos2(r.right() - dp(8.0), r.bottom() - dp(18.0)),
    );
    let (mut x0, mut x1) = all.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    if x1 - x0 < 1e-12 {
        x0 -= 1.0;
        x1 += 1.0;
    }
    let (mut y0, mut y1) = all.iter().fold((f64::MAX, f64::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    if y0 < 0.0 && y1 > 0.0 || y0 == 0.0 || y1 == 0.0 {
        y0 = y0.min(0.0);
        y1 = y1.max(0.0);
    }
    let pad = ((y1 - y0) * 0.08).max(1e-9);
    let (y0, y1) = (y0 - pad, y1 + pad);
    let px = |x: f64| plot_r.left() + plot_r.width() * ((x - x0) / (x1 - x0)) as f32;
    let py = |y: f64| plot_r.bottom() - plot_r.height() * ((y - y0) / (y1 - y0)) as f32;
    for k in 0..=3 {
        let v = y0 + (y1 - y0) * k as f64 / 3.0;
        let y = py(v);
        p.line_segment([pos2(plot_r.left(), y), pos2(plot_r.right(), y)], Stroke::new(1.0, theme::grid()));
        p.text(pos2(plot_r.left() - 4.0, y), Align2::RIGHT_CENTER, fy(v), mono(9.5), muted());
    }
    for (k, al) in [(0.0, Align2::LEFT_TOP), (0.5, Align2::CENTER_TOP), (1.0, Align2::RIGHT_TOP)] {
        let v = x0 + (x1 - x0) * k;
        p.text(pos2(px(v), plot_r.bottom() + 3.0), al, fx(v), mono(9.5), muted());
    }
    if y0 < 0.0 && y1 > 0.0 {
        p.line_segment([pos2(plot_r.left(), py(0.0)), pos2(plot_r.right(), py(0.0))], Stroke::new(1.2, muted()));
    }
    let clip = ui.painter_at(plot_r);
    for (i, v) in vlines.iter().enumerate().filter(|(_, v)| v.x >= x0 && v.x <= x1) {
        let x = px(v.x);
        clip.line_segment([pos2(x, plot_r.top()), pos2(x, plot_r.bottom())], Stroke::new(1.0, v.color));
        let g = clip.layout_no_wrap(v.label.clone(), mono(9.5), v.color);
        let tx = if x + g.size().x + 4.0 > plot_r.right() { x - g.size().x - 3.0 } else { x + 3.0 };
        clip.galley(pos2(tx, plot_r.top() + 2.0 + i as f32 * 11.0), g, v.color);
    }
    for s in series {
        let pts: Vec<Pos2> = s.pts.iter().map(|&(x, y)| pos2(px(x), py(y))).collect();
        if s.dots {
            for q in &pts {
                clip.circle_filled(*q, 2.5, s.color);
            }
        }
        if pts.len() > 1 {
            clip.add(Shape::line(pts, Stroke::new(1.6, s.color)));
        }
    }
    if let Some(m) = resp.hover_pos().filter(|m| plot_r.contains(*m)) {
        let xv = x0 + (x1 - x0) * ((m.x - plot_r.left()) / plot_r.width()) as f64;
        clip.line_segment([pos2(m.x, plot_r.top()), pos2(m.x, plot_r.bottom())], Stroke::new(1.0, muted()));
        let mut tip = vec![format!("x {}", fx(xv))];
        for s in series {
            if let Some(&(_, y)) = s.pts.iter().min_by(|a, b| (a.0 - xv).abs().total_cmp(&(b.0 - xv).abs())) {
                tip.push(format!("{} {}", s.name, fy(y)));
            }
        }
        let g = clip.layout(tip.join("\n"), mono(10.0), text(), 200.0);
        let at = pos2(
            if m.x > plot_r.center().x { plot_r.left() + 6.0 } else { plot_r.right() - g.size().x - 6.0 },
            plot_r.bottom() - g.size().y - 4.0,
        );
        clip.rect_filled(Rect::from_min_size(at, g.size()).expand(3.0), 3.0, theme::window().gamma_multiply(0.9));
        clip.galley(at, g, text());
    }
    // Legend.
    let mut x = plot_r.left() + 4.0;
    for s in series.iter().filter(|s| !s.name.is_empty()) {
        let g = p.layout_no_wrap(s.name.to_string(), ui_font(10.0), s.color);
        p.galley(pos2(x, r.bottom() - g.size().y - 1.0), g.clone(), s.color);
        x += g.size().x + 10.0;
    }
}

/// Call/put volume, OI and their ratios, six table cells.
fn pc_cells(ui: &mut Ui, pc: &PutCall) {
    for t in [
        count(pc.call_volume),
        count(pc.put_volume),
        ratio(pc.volume_ratio()),
        count(pc.call_oi),
        count(pc.put_oi),
        ratio(pc.oi_ratio()),
    ] {
        ui.label(RichText::new(t).font(mono(11.0)).color(text()));
    }
}

fn heading(ui: &mut Ui, t: &str) {
    ui.add_space(dp(4.0));
    ui.label(RichText::new(t).font(ui_font(12.0)).strong().color(amber()));
}

fn dim(ui: &mut Ui, t: impl Into<String>) {
    ui.label(RichText::new(t.into()).font(ui_font(11.0)).color(muted()));
}

/// The tab strip. Counts the legs on the Payoff tab.
pub fn tabs(ui: &mut Ui, o: &mut OptionsPane) {
    let payoff = if o.an.legs.is_empty() { "Payoff".to_string() } else { format!("Payoff ({})", o.an.legs.len()) };
    let labels = ["Chain", "Analytics", payoff.as_str()];
    let sel = match o.an.tab {
        Tab::Chain => 0,
        Tab::Analytics => 1,
        Tab::Payoff => 2,
    };
    if let Some(i) = widgets::segmented(ui, ("omon-tab", &o.symbol), &labels, Some(sel), 20.0) {
        o.an.tab = [Tab::Chain, Tab::Analytics, Tab::Payoff][i];
    }
}

// ---- Analytics tab --------------------------------------------------------------------

struct ExpStats {
    exp: i64,
    atm_iv: Option<f64>,
    pc: PutCall,
    pain: Option<MaxPain>,
}

fn stats(ch: &OptionChain, rate: f64, now: i64) -> ExpStats {
    let (calls, puts) = (quotes(ch, true, rate, now), quotes(ch, false, rate, now));
    ExpStats {
        exp: ch.expiration,
        atm_iv: os::atm_iv(&calls, &puts, ch.spot),
        pc: PutCall::of(&calls, &puts),
        pain: os::max_pain(&calls, &puts),
    }
}

/// Draw the Analytics tab. Sets `refetch` when more expirations were queued or one was selected.
pub fn analytics(ui: &mut Ui, o: &mut OptionsPane, refetch: &mut bool) {
    let Some(main) = o.chain.as_deref() else { return };
    let (spot, expirations) = (main.spot, main.expirations.clone());
    if !o.an.started {
        o.an.started = true;
        o.an.load_more(&expirations);
        *refetch = true;
    }
    let now = chrono::Utc::now().timestamp();
    let sel = o.expiration.unwrap_or(main.expiration);
    let rate = o.rate;
    let Some(ch) = o.an.cache.get(&sel).map(|c| &**c).or(Some(main)) else { return };
    let (calls, puts) = (quotes(ch, true, rate, now), quotes(ch, false, rate, now));
    let all: Vec<ExpStats> = o.an.cache.values().map(|c| stats(c, rate, now)).collect();
    let (loaded, listed) = (all.len(), expirations.len());
    let pending = !o.an.queue.is_empty();
    let more = o.an.cache.len() < listed;
    let mut load_more = false;

    egui::ScrollArea::vertical().id_salt(("omon-an", &o.symbol)).auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            dim(ui, format!("Expiry {} · {}d", date(sel), dte(sel, now)));
            dim(ui, format!("{loaded} of {listed} expirations loaded"));
            if pending {
                ui.add(egui::Spinner::new().size(11.0));
            } else if more && ui.small_button(format!("Load next {BATCH}")).clicked() {
                load_more = true;
            }
        });

        // Summary for the selected expiration.
        let atm = os::atm_iv(&calls, &puts, ch.spot);
        let sk = os::skew(&calls, &puts, ch.spot);
        let pc = PutCall::of(&calls, &puts);
        let pain = os::max_pain(&calls, &puts);
        ui.horizontal_wrapped(|ui| {
            let kv = |ui: &mut Ui, k: &str, v: String, c: Color32| {
                ui.label(RichText::new(k).font(ui_font(11.0)).color(muted()));
                ui.label(RichText::new(v).font(mono(12.0)).color(c));
                ui.add_space(dp(8.0));
            };
            kv(ui, "ATM IV", pct(atm), theme::cyan());
            match sk {
                Some(s) => {
                    let basis = match s.basis {
                        SkewBasis::Delta => "25Δ",
                        SkewBasis::Moneyness => "95/105%",
                    };
                    kv(ui, &format!("Skew {basis}"), format!("{:+.1} vol", s.skew * 100.0), if s.skew > 0.0 { theme::down() } else { theme::up() });
                }
                None => kv(ui, "Skew", "n/a".into(), muted()),
            }
            kv(ui, "P/C vol", ratio(pc.volume_ratio()), text());
            kv(ui, "P/C OI", ratio(pc.oi_ratio()), text());
            match &pain {
                Some(m) => kv(ui, "Max pain", format!("{:.2} ({:+.1}% vs spot)", m.strike, (m.strike / spot - 1.0) * 100.0), amber()),
                None => kv(ui, "Max pain", "n/a".into(), muted()),
            }
        });
        if let Some(s) = sk {
            let how = match s.basis {
                SkewBasis::Delta => format!("put {:.2} ({}) vs call {:.2} ({}), nearest ±0.25Δ", s.put_strike, pct(Some(s.put_iv)), s.call_strike, pct(Some(s.call_iv))),
                SkewBasis::Moneyness => format!("put {:.2} ({}) vs call {:.2} ({}); no usable deltas, so 95% / 105% of spot", s.put_strike, pct(Some(s.put_iv)), s.call_strike, pct(Some(s.call_iv))),
            };
            let read = if s.skew > 0.005 { "puts richer than calls (downside demand)" } else if s.skew < -0.005 { "calls richer than puts" } else { "flat" };
            dim(ui, format!("Skew {:+.1} vol: {read}. {how}.", s.skew * 100.0));
        } else {
            dim(ui, "Skew n/a: no pair of strikes with IV near the wings for this expiration.");
        }

        // Smile and term structure.
        let wide = ui.available_width() > 640.0;
        let smile_series = |q: &[Quote], color, name| Series {
            pts: q.iter().filter_map(|q| q.iv.map(|v| (q.strike, v))).collect(),
            color,
            name,
            dots: false,
        };
        let smile = [smile_series(&calls, theme::up(), "Calls"), smile_series(&puts, theme::down(), "Puts")];
        let spot_line = [VLine { x: spot, color: theme::accent(), label: format!("spot {spot:.2}") }];
        let term = [Series {
            pts: all.iter().filter_map(|s| s.atm_iv.map(|v| (dte(s.exp, now) as f64, v))).collect(),
            color: theme::cyan(),
            name: "ATM IV",
            dots: true,
        }];
        let draw_smile = |ui: &mut Ui| {
            heading(ui, "IV smile (IV vs strike)");
            plot(ui, 170.0, &smile, &spot_line, &|x| format!("{x:.0}"), &|y| format!("{:.0}%", y * 100.0));
        };
        let draw_term = |ui: &mut Ui| {
            heading(ui, "ATM IV term structure (vs days to expiry)");
            plot(ui, 170.0, &term, &[], &|x| format!("{x:.0}d"), &|y| format!("{:.0}%", y * 100.0));
        };
        if wide {
            ui.columns(2, |c| {
                draw_smile(&mut c[0]);
                draw_term(&mut c[1]);
            });
        } else {
            draw_smile(ui);
            draw_term(ui);
        }
        if all.len() < 2 {
            dim(ui, "Term structure needs more than one expiration; the rest load in the background.");
        }

        // Put/call per expiration.
        heading(ui, "Put/call ratio by expiration");
        let total = all.iter().fold(PutCall::default(), |a, s| a.plus(s.pc));
        egui::Grid::new(("omon-pc", &o.symbol)).striped(true).spacing(vec2(dp(10.0), 2.0)).show(ui, |ui| {
            for h in ["Expiry", "Days", "Call vol", "Put vol", "P/C vol", "Call OI", "Put OI", "P/C OI", "ATM IV", "Max pain"] {
                ui.label(RichText::new(h).font(ui_font(10.5)).color(muted()));
            }
            ui.end_row();
            for s in &all {
                if ui.selectable_label(s.exp == sel, RichText::new(date(s.exp)).font(mono(11.0))).clicked() && s.exp != sel {
                    o.expiration = Some(s.exp);
                    o.loading = true;
                    *refetch = true;
                }
                ui.label(RichText::new(dte(s.exp, now).to_string()).font(mono(11.0)).color(muted()));
                pc_cells(ui, &s.pc);
                ui.label(RichText::new(pct(s.atm_iv)).font(mono(11.0)).color(theme::cyan()));
                ui.label(RichText::new(s.pain.as_ref().map_or_else(|| "n/a".into(), |m| format!("{:.2}", m.strike))).font(mono(11.0)).color(amber()));
                ui.end_row();
            }
            ui.label(RichText::new(format!("Total ({loaded})")).font(ui_font(11.0)).strong());
            ui.label("");
            pc_cells(ui, &total);
            ui.end_row();
        });
        let reading = |label: &str, r: Option<f64>| r.map(|r| format!("{label} {r:.2}: {}", os::interpret_put_call(r)));
        let lines: Vec<String> = [reading("Volume", total.volume_ratio()), reading("Open interest", total.oi_ratio())].into_iter().flatten().collect();
        if lines.is_empty() {
            dim(ui, "Put/call n/a: the source reported no volume or open interest.");
        } else {
            dim(ui, format!("Across {loaded} loaded expirations. {}. Puts are also used as hedges, so read this as positioning, not a forecast.", lines.join(" · ")));
        }

        // Max pain and walls for the selected expiration.
        heading(ui, &format!("Max pain · {}", date(sel)));
        match &pain {
            Some(m) => {
                let series = [Series { pts: m.curve.iter().map(|&(k, v)| (k, v / 1e6)).collect(), color: amber(), name: "Payout to holders ($M)", dots: false }];
                let lines = [
                    VLine { x: m.strike, color: amber(), label: format!("max pain {:.2}", m.strike) },
                    VLine { x: spot, color: theme::accent(), label: format!("spot {spot:.2}") },
                ];
                plot(ui, 150.0, &series, &lines, &|x| format!("{x:.0}"), &|y| format!("{y:.1}M"));
                dim(ui, format!("Writers pay least ({}) if it settles at {:.2}, {:+.1}% from spot {spot:.2}. Based on open interest from the prior close; a gravitational pull is folklore, not a guarantee.", money(m.payout), m.strike, (m.strike / spot - 1.0) * 100.0));
            }
            None => dim(ui, "Max pain n/a: no open interest reported for this expiration."),
        }
        let walls = |ui: &mut Ui, title: &str, q: &[Quote], color: Color32| {
            ui.label(RichText::new(title).font(ui_font(11.0)).color(color));
            let top = os::top_oi(q, 5);
            if top.is_empty() {
                dim(ui, "n/a");
            }
            for (k, oi) in top {
                ui.label(RichText::new(format!("{k:>9.2}  {:>9}  ({:+.1}%)", group(oi as f64), (k / spot - 1.0) * 100.0)).font(mono(11.0)).color(text()));
            }
        };
        ui.columns(2, |c| {
            walls(&mut c[0], "Call walls (top open interest)", &calls, theme::up());
            walls(&mut c[1], "Put walls (top open interest)", &puts, theme::down());
        });
        ui.add_space(dp(6.0));
        dim(ui, format!("{} · IV from the source; deltas for skew computed locally (Black-Scholes, r {:.2}%). Volume is intraday, open interest is as of the prior close.", ch.source, rate * 100.0));
    });
    if load_more {
        o.an.load_more(&expirations);
        *refetch = true;
    }
}

// ---- Payoff tab -----------------------------------------------------------------------

fn kind_word(k: LegKind) -> &'static str {
    match k {
        LegKind::Call => "Call",
        LegKind::Put => "Put",
        LegKind::Stock => "Stock",
    }
}

fn bound(b: Bound) -> (String, Color32) {
    match b {
        Bound::Unlimited => ("Unlimited".into(), theme::up()),
        Bound::Finite(v) => (signed_money(v), if v >= 0.0 { theme::up() } else { theme::down() }),
    }
}

pub fn payoff(ui: &mut Ui, o: &mut OptionsPane) {
    let Some(main) = o.chain.as_deref() else { return };
    let now = chrono::Utc::now().timestamp();
    let sel = o.expiration.unwrap_or(main.expiration);
    let rate = o.rate;
    // The chain a new leg is priced from: the selected expiration.
    let ch = o.an.cache.get(&sel).map_or(main, |c| &**c).clone();
    let spot = ch.spot;
    let mut strikes: Vec<f64> = ch.calls.iter().chain(&ch.puts).map(|c| c.strike).collect();
    strikes.sort_by(f64::total_cmp);
    strikes.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let find =
        |call: bool, k: f64| (if call { &ch.calls } else { &ch.puts }).iter().find(|c| (c.strike - k).abs() < 1e-9);

    egui::ScrollArea::vertical().id_salt(("omon-pay", &o.symbol)).auto_shrink([false, false]).show(ui, |ui| {
        heading(ui, "Strategy builder");
        ui.horizontal_wrapped(|ui| {
            dim(ui, format!("Presets on {} ({}d):", date(sel), dte(sel, now)));
            for p in Preset::ALL {
                if ui.small_button(p.label()).clicked() {
                    let built = os::preset_legs(p, &strikes, spot).and_then(|legs| {
                        legs.into_iter()
                            .map(|(kind, k, qty)| match kind {
                                LegKind::Stock => Some(Leg { kind, strike: spot, expiry: sel, qty, premium: spot, iv: None }),
                                _ => find(kind == LegKind::Call, k).and_then(|c| option_leg(&ch, kind == LegKind::Call, c, qty)),
                            })
                            .collect::<Option<Vec<Leg>>>()
                    });
                    match built {
                        Some(legs) => {
                            o.an.legs = legs;
                            o.an.note = None;
                        }
                        None => o.an.note = Some(format!("{} can't be built here: missing strikes or prices near the money.", p.label())),
                    }
                }
            }
            if !o.an.legs.is_empty() && ui.small_button("Clear").clicked() {
                o.an.legs.clear();
            }
        });
        // Add one leg.
        ui.horizontal_wrapped(|ui| {
            let kind = o.an.add_kind.unwrap_or(LegKind::Call);
            egui::ComboBox::from_id_salt(("omon-addkind", &o.symbol)).width(dp(70.0)).selected_text(kind_word(kind)).show_ui(ui, |ui| {
                for k in [LegKind::Call, LegKind::Put, LegKind::Stock] {
                    if ui.selectable_label(k == kind, kind_word(k)).clicked() {
                        o.an.add_kind = Some(k);
                    }
                }
            });
            let atm = strikes.iter().copied().min_by(|a, b| (a - spot).abs().total_cmp(&(b - spot).abs()));
            let k = o.an.add_strike.filter(|k| strikes.iter().any(|s| (s - k).abs() < 1e-9)).or(atm);
            if kind != LegKind::Stock {
                egui::ComboBox::from_id_salt(("omon-addk", &o.symbol)).width(dp(80.0)).height(300.0).selected_text(k.map_or("–".into(), |k| format!("{k:.2}"))).show_ui(ui, |ui| {
                    for &s in &strikes {
                        if ui.selectable_label(Some(s) == k, format!("{s:.2}")).clicked() {
                            o.an.add_strike = Some(s);
                        }
                    }
                });
            }
            for (label, qty) in [("Buy", 1), ("Sell", -1)] {
                if ui.small_button(label).clicked() {
                    match (kind, k) {
                        (LegKind::Stock, _) => {
                            o.an.legs.push(Leg { kind, strike: spot, expiry: sel, qty, premium: spot, iv: None });
                            o.an.note = None;
                        }
                        (_, Some(k)) => {
                            if let Some(c) = find(kind == LegKind::Call, k) {
                                o.an.add_contract(&ch, kind == LegKind::Call, c, qty);
                            }
                        }
                        _ => {}
                    }
                }
            }
            dim(ui, "or click a row in the Chain tab. Pick another expiry above for a calendar.");
        });
        if let Some(n) = o.an.note.clone() {
            ui.label(RichText::new(n).font(ui_font(11.0)).color(theme::down()));
        }
        if o.an.legs.is_empty() {
            ui.add_space(dp(8.0));
            dim(ui, "No legs yet. Choose a preset or add a leg to see its payoff.");
            return;
        }

        // Legs table.
        let mut remove = None;
        egui::Grid::new(("omon-legs", &o.symbol)).striped(true).spacing(vec2(dp(10.0), 3.0)).show(ui, |ui| {
            for h in ["Side", "Qty", "Type", "Strike", "Expiry", "Price", "IV", ""] {
                ui.label(RichText::new(h).font(ui_font(10.5)).color(muted()));
            }
            ui.end_row();
            for (i, l) in o.an.legs.iter_mut().enumerate() {
                let long = l.qty > 0;
                if ui.small_button(RichText::new(if long { "Buy" } else { "Sell" }).color(if long { theme::up() } else { theme::down() })).on_hover_text("Flip buy / sell").clicked() {
                    l.qty = -l.qty;
                }
                let mut n = l.qty.abs();
                if ui.add(egui::DragValue::new(&mut n).range(1..=999)).changed() {
                    l.qty = if long { n } else { -n };
                }
                ui.label(RichText::new(kind_word(l.kind)).font(mono(11.5)).color(text()));
                ui.label(RichText::new(if l.kind == LegKind::Stock { "–".into() } else { format!("{:.2}", l.strike) }).font(mono(11.5)).color(text()));
                ui.label(RichText::new(if l.kind == LegKind::Stock { "–".into() } else { date(l.expiry) }).font(mono(11.5)).color(muted()));
                ui.add(egui::DragValue::new(&mut l.premium).range(0.0..=1e7).speed(0.01).fixed_decimals(2)).on_hover_text("Entry price per share (edit to use your fill)");
                ui.label(RichText::new(if l.kind == LegKind::Stock { "–".into() } else { pct(l.iv) }).font(mono(11.5)).color(theme::cyan()));
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            o.an.legs.remove(i);
        }
        let legs = o.an.legs.clone();
        if legs.is_empty() {
            return;
        }

        // Summary.
        let net = os::net_premium(&legs);
        let single = os::single_expiry(&legs).is_some() || legs.iter().all(|l| l.kind == LegKind::Stock);
        let expiry_pnl = |s: f64| os::pnl_at_expiry(&legs, s);
        heading(ui, "Result");
        ui.horizontal_wrapped(|ui| {
            let kv = |ui: &mut Ui, k: &str, v: String, c: Color32| {
                ui.label(RichText::new(k).font(ui_font(11.0)).color(muted()));
                ui.label(RichText::new(v).font(mono(12.0)).color(c));
                ui.add_space(dp(8.0));
            };
            kv(ui, if net >= 0.0 { "Net debit" } else { "Net credit" }, money(net.abs()), text());
            if single {
                let (mp, mpc) = bound(os::max_profit(&legs));
                let (ml, mlc) = bound(os::max_loss(&legs));
                kv(ui, "Max profit", mp, mpc);
                kv(ui, "Max loss", ml, mlc);
                let be = os::breakevens(&legs);
                kv(ui, "Break-even", if be.is_empty() { "none".into() } else { be.iter().map(|b| format!("{b:.2}")).collect::<Vec<_>>().join(" / ") }, amber());
            } else {
                kv(ui, "Max profit / loss / break-even", "n/a (mixed expiries)".into(), muted());
            }
        });
        match os::position_greeks(&legs, spot, now, rate, ch.dividend_yield) {
            Some(g) => dim(ui, format!("Position greeks now: Δ {:+.1} sh · Γ {:+.2} · Θ {}/day · Vega {} per vol pt · ρ {} per 1% rate", g.delta, g.gamma, signed_money(g.theta), signed_money(g.vega), signed_money(g.rho))),
            None => dim(ui, "Position greeks n/a: a leg has no implied volatility."),
        }

        // Chart.
        let ks: Vec<f64> = legs.iter().filter(|l| l.kind != LegKind::Stock).map(|l| l.strike).chain([spot]).collect();
        let lo = ks.iter().copied().fold(f64::MAX, f64::min) * 0.9;
        let hi = ks.iter().copied().fold(f64::MIN, f64::max) * 1.1;
        let xs: Vec<f64> = (0..=160).map(|i| lo + (hi - lo) * i as f64 / 160.0).collect();
        let at_expiry = Series { pts: if single { xs.iter().map(|&s| (s, expiry_pnl(s))).collect() } else { Vec::new() }, color: amber(), name: "At expiry", dots: false };
        let today_pts: Option<Vec<(f64, f64)>> = xs.iter().map(|&s| os::pnl_today(&legs, s, now, rate, ch.dividend_yield).map(|v| (s, v))).collect();
        let today = Series { pts: today_pts.clone().unwrap_or_default(), color: theme::cyan(), name: "Today (Black-Scholes)", dots: false };
        let mut vl = vec![VLine { x: spot, color: theme::accent(), label: format!("spot {spot:.2}") }];
        if single {
            vl.extend(os::breakevens(&legs).into_iter().map(|b| VLine { x: b, color: muted(), label: format!("BE {b:.2}") }));
        }
        heading(ui, "Profit / loss by underlying price");
        plot(ui, 230.0, &[at_expiry, today], &vl, &|x| format!("{x:.0}"), &|y| signed_money(y));
        let mut notes = Vec::new();
        if !single {
            notes.push("Mixed expiries: the at-expiry curve is n/a; the today curve values every leg now.".to_string());
        }
        if today_pts.is_none() {
            notes.push("Today curve n/a: a leg has no implied volatility.".into());
        }
        notes.push(format!("Today curve: Black-Scholes at each leg's current IV held flat, r {:.2}%, no time decay beyond today. Entry prices are the quotes' mid (or last).", rate * 100.0));
        dim(ui, notes.join(" "));
        dim(ui, format!("{} · {} lot = 100 shares · not advice; no commissions, assignment or early exercise.", ch.source, if ch.simulated { "SIMULATED prices, 1" } else { "1" }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_800_000;

    fn pane() -> OptionsPane {
        let mut o = OptionsPane::new("AAPL US Equity", 0, true);
        let chain = tr_data::sample::simulated_chain("AAPL US Equity", 230.0, None, NOW);
        o.set_chain(Box::new(chain), 0.04, NOW);
        o
    }

    #[test]
    fn other_expirations_are_cached_without_replacing_the_shown_chain() {
        let mut o = pane();
        let main = o.chain.as_ref().unwrap().expiration;
        let other = o.chain.as_ref().unwrap().expirations[3];
        assert_ne!(main, other);
        o.fetched = Some(std::time::Instant::now());
        let c = tr_data::sample::simulated_chain("AAPL US Equity", 230.0, Some(other), NOW);
        o.set_chain(Box::new(c), 0.04, NOW);
        assert_eq!(o.chain.as_ref().unwrap().expiration, main, "the chain on screen is the selected one");
        assert!(o.an.cache.contains_key(&main) && o.an.cache.contains_key(&other));
        assert!(o.fetched.is_some(), "nothing left in the queue: normal 30s refresh");
    }

    #[test]
    fn queue_drives_one_request_at_a_time_then_returns_to_the_selection() {
        let mut o = pane();
        let exps = o.chain.as_ref().unwrap().expirations.clone();
        o.an.load_more(&exps);
        assert!(!o.an.queue.is_empty() && o.an.queue.len() <= BATCH);
        let first = o.next_expiration().unwrap();
        let nearest_unloaded = *exps.iter().find(|e| !o.an.cache.contains_key(e)).unwrap();
        assert_eq!(first, nearest_unloaded, "nearest expiration first");
        o.fetched = Some(std::time::Instant::now());
        let c = tr_data::sample::simulated_chain("AAPL US Equity", 230.0, Some(first), NOW);
        o.set_chain(Box::new(c), 0.04, NOW);
        if o.an.queue.is_empty() {
            assert_eq!(o.next_expiration(), o.expiration);
        } else {
            assert!(o.fetched.is_none(), "next queued expiration is requested straight away");
        }
    }

    #[test]
    fn simulated_chain_gives_deltas_and_sane_analytics() {
        let o = pane();
        let ch = o.chain.as_ref().unwrap();
        let (calls, puts) = (quotes(ch, true, 0.04, NOW), quotes(ch, false, 0.04, NOW));
        assert!(calls.iter().all(|q| q.delta.is_some_and(|d| (0.0..=1.0).contains(&d))));
        assert!(puts.iter().all(|q| q.delta.is_some_and(|d| (-1.0..=0.0).contains(&d))));
        let atm = os::atm_iv(&calls, &puts, ch.spot).unwrap();
        assert!((0.05..2.0).contains(&atm));
        // The sample smile has a negative slope in moneyness (puts richer): positive skew.
        assert!(os::skew(&calls, &puts, ch.spot).unwrap().skew > 0.0);
        let pain = os::max_pain(&calls, &puts).unwrap();
        assert!((ch.spot * 0.8..ch.spot * 1.2).contains(&pain.strike));
    }

    #[test]
    fn contracts_without_a_price_are_refused_and_priced_ones_become_legs() {
        let mut o = pane();
        let ch = o.chain.clone().unwrap();
        assert!(o.an.add_contract(&ch, true, &ch.calls[10], 1));
        let leg = o.an.legs[0];
        assert_eq!((leg.kind, leg.qty), (LegKind::Call, 1));
        assert_eq!(leg.expiry, ch.expiration);
        let mut dead = ch.puts[0].clone();
        dead.bid = None;
        dead.ask = None;
        dead.last = None;
        assert!(!o.an.add_contract(&ch, false, &dead, -1));
        assert_eq!(o.an.legs.len(), 1);
        assert!(o.an.note.is_some());
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(group(1_234_567.0), "1,234,567");
        assert_eq!(group(-950.0), "-950");
        assert_eq!(money(-1234.0), "-$1,234");
        assert_eq!(signed_money(5.0), "+$5");
        assert_eq!(pct(None), "n/a");
        assert_eq!(ratio(Some(0.5)), "0.50");
    }
}
