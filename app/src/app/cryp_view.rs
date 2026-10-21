//! Crypto market overview (`CRYP`): global cards, Fear & Greed, Binance perpetuals, 24h movers
//! and the Turkish USDT/TRY premium. Click a coin to chart it. Data layer: [`tr_data::cryptomkt`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, CursorIcon, RichText, Sense, Ui, vec2};
use tr_data::cryptomkt::{Deriv, FearGreed, Global, Mover, Overview, Turkey};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::cryp::{CrypReq, CrypResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::worker::Request;

/// How often a visible pane re-fetches.
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct CrypPane {
    ov: Option<Arc<Overview>>,
    error: Option<String>,
    loading: bool,
    last_req: Option<Instant>,
}

impl CrypPane {
    pub fn title(&self) -> String {
        "Crypto".into()
    }

    pub fn code(&self) -> &'static str {
        "CRYP"
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `CRYP`: focus the existing pane or open one.
    pub(super) fn cryp_command(&mut self) {
        match self.panes.iter().find(|p| matches!(p.kind, PaneKind::Cryp(_))).map(|p| p.id) {
            Some(id) => {
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Cryp(Box::default())),
        }
    }

    /// Fetch on first show and every minute while the pane is visible.
    pub(super) fn cryp_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Cryp(c) = &mut self.panes[i].kind else { continue };
            if !c.loading && c.last_req.is_none_or(|t| t.elapsed() >= REFRESH) {
                c.loading = true;
                c.last_req = Some(Instant::now());
                self.worker.send(Request::Cryp(CrypReq::Load { pane: id }));
            }
        }
    }

    pub(super) fn on_cryp(&mut self, resp: CrypResp) {
        let CrypResp::Overview { pane, ov } = resp;
        let Some(PaneKind::Cryp(c)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        c.loading = false;
        match ov {
            Ok(o) => {
                c.ov = Some(o);
                c.error = None;
            }
            Err(e) => c.error = Some(e),
        }
    }
}

// ---- formatting ---------------------------------------------------------------------------

/// 1.2K / 3.4M / 5.6B / 7.8T, with a `$` in front.
fn usd(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("${:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("${:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("${:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("${:.1}K", v / 1e3)
    } else {
        format!("${v:.0}")
    }
}

fn price(p: f64) -> String {
    if p >= 1000.0 {
        format!("{p:.0}")
    } else if p >= 1.0 {
        format!("{p:.2}")
    } else if p >= 0.01 {
        format!("{p:.4}")
    } else {
        format!("{p:.6}")
    }
}

fn pct(v: f64) -> String {
    format!("{v:+.2}%")
}

fn tint(v: f64) -> Color32 {
    if v > 0.0 {
        theme::up()
    } else if v < 0.0 {
        theme::down()
    } else {
        text()
    }
}

/// "in 3h 12m" until a Unix-millisecond time.
fn until(ms: i64, now_ms: i64) -> String {
    let m = ((ms - now_ms) / 60_000).max(0);
    if m >= 60 { format!("in {}h {:02}m", m / 60, m % 60) } else { format!("in {m}m") }
}

fn sentiment_color(v: u32) -> Color32 {
    match v {
        0..=24 => theme::down(),
        25..=44 => Color32::from_rgb(230, 140, 60),
        45..=55 => muted(),
        56..=75 => Color32::from_rgb(140, 190, 80),
        _ => theme::up(),
    }
}

/// The chart key for a Binance base asset: the catalog pair when listed, else `<COIN>USDT`.
fn chart_key(base: &str) -> String {
    let listed = format!("{base}USD Curncy");
    if tr_data::security(&listed).name().is_some() { listed } else { format!("{base}USDT Curncy") }
}

// ---- drawing ------------------------------------------------------------------------------

fn section_title(ui: &mut Ui, s: &str) {
    ui.add_space(dp(6.0));
    ui.label(RichText::new(s).font(ui_font(11.5)).color(amber()));
    ui.separator();
}

fn section_error(ui: &mut Ui, what: &str, e: &str) {
    ui.label(RichText::new(format!("{what}: {e}")).font(ui_font(12.0)).color(theme::down()));
}

fn card(ui: &mut Ui, label: &str, value: String, sub: Option<(String, Color32)>, width: f32) {
    egui::Frame::new()
        .fill(theme::card())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(dp(8.0) as i8, dp(5.0) as i8))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.label(RichText::new(label).font(ui_font(10.5)).color(muted()));
            ui.label(RichText::new(value).font(mono(15.0)).color(text()));
            match sub {
                Some((s, c)) => ui.label(RichText::new(s).font(mono(11.5)).color(c)),
                None => ui.label(RichText::new(" ").font(mono(11.5))),
            };
        });
}

fn fear_greed_card(ui: &mut Ui, f: &FearGreed) {
    let col = sentiment_color(f.value);
    egui::Frame::new()
        .fill(theme::card())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(dp(8.0) as i8, dp(5.0) as i8))
        .show(ui, |ui| {
            ui.set_width(dp(210.0));
            ui.label(RichText::new("FEAR & GREED").font(ui_font(10.5)).color(muted()));
            ui.horizontal(|ui| {
                ui.label(RichText::new(f.value.to_string()).font(mono(15.0)).color(col));
                ui.label(RichText::new(&f.label).font(ui_font(12.0)).color(col));
            });
            let (rect, _) = ui.allocate_exact_size(vec2(dp(190.0), dp(22.0)), Sense::hover());
            let (lo, hi) = f.history.iter().fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
            if f.history.len() > 1 && hi > lo {
                let n = (f.history.len() - 1) as f32;
                let pts: Vec<egui::Pos2> = f
                    .history
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        egui::pos2(
                            rect.left() + rect.width() * i as f32 / n,
                            rect.bottom() - rect.height() * ((v - lo) / (hi - lo)) as f32,
                        )
                    })
                    .collect();
                ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.3, col)));
            }
        });
}

fn global_cards(ui: &mut Ui, g: &Global) {
    let w = dp(128.0);
    let ch = |v: Option<f64>| v.map(|v| (format!("{} 24h", pct(v)), tint(v)));
    card(ui, "MARKET CAP", usd(g.market_cap), ch(g.market_cap_change_24h), w);
    card(ui, "24H VOLUME", usd(g.volume_24h), ch(g.volume_change_24h), w);
    card(ui, "BTC DOMINANCE", format!("{:.1}%", g.btc_dominance), None, w);
    card(ui, "ETH DOMINANCE", format!("{:.1}%", g.eth_dominance), None, w);
    if let Some(s) = g.stable_cap {
        card(ui, "STABLECOINS", usd(s), ch(g.stable_change_24h), w);
    }
}

fn head(ui: &mut Ui, h: &str, left: bool) {
    if left {
        ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
    } else {
        num(ui, h.to_string(), muted());
    }
}

fn num(ui: &mut Ui, s: String, color: Color32) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(RichText::new(s).font(mono(12.5)).color(color));
    });
}

/// A clickable coin label that charts the coin (right-click for more).
fn coin(ui: &mut Ui, base: &str, actions: &mut Vec<Action>) {
    let key = chart_key(base);
    let r = ui
        .add(egui::Label::new(RichText::new(base).font(mono(12.5)).color(amber())).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Click: chart · right-click: more");
    if r.clicked() {
        actions.push(Action::OpenChart(key.clone()));
    }
    r.context_menu(|ui| {
        if ui.button("Open Chart").clicked() {
            actions.push(Action::OpenChart(key.clone()));
            ui.close();
        }
        if ui.button("Open Chart as New").clicked() {
            actions.push(Action::OpenChartNew(key.clone()));
            ui.close();
        }
        if ui.button("Add to Watchlist").clicked() {
            actions.push(Action::AddToWatchlist(key.clone()));
            ui.close();
        }
    });
}

fn derivs_table(ui: &mut Ui, d: &[Deriv], actions: &mut Vec<Action>) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    egui::Grid::new("cryp-derivs").num_columns(8).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
        for (i, h) in
            ["Perp", "Mark", "Funding/8h", "APR", "Next", "Open int.", "OI 24h", "Long/Short"].into_iter().enumerate()
        {
            head(ui, h, i == 0);
        }
        ui.end_row();
        for x in d {
            coin(ui, &x.coin, actions);
            num(ui, price(x.mark), text());
            num(ui, format!("{:+.4}%", x.funding * 100.0), tint(x.funding));
            num(ui, format!("{:+.1}%", x.funding_apr()), tint(x.funding));
            num(ui, until(x.next_funding_ms, now_ms), muted());
            num(ui, x.oi_usd.map_or("–".into(), usd), text());
            match x.oi_change_24h {
                Some(c) => num(ui, pct(c), tint(c)),
                None => num(ui, "–".into(), muted()),
            }
            match (x.ls_ratio, x.long_pct) {
                (Some(r), Some(l)) => num(ui, format!("{r:.2} ({l:.0}% long)"), text()),
                _ => num(ui, "–".into(), muted()),
            }
            ui.end_row();
        }
    });
}

fn movers_table(ui: &mut Ui, id: &str, rows: &[Mover], actions: &mut Vec<Action>) {
    egui::Grid::new(id).num_columns(4).striped(true).spacing([dp(16.0), dp(4.0)]).show(ui, |ui| {
        for (i, h) in ["Coin", "Price", "24h", "Volume"].into_iter().enumerate() {
            head(ui, h, i == 0);
        }
        ui.end_row();
        for m in rows {
            coin(ui, &m.base, actions);
            num(ui, price(m.price), text());
            num(ui, pct(m.change_pct), tint(m.change_pct));
            num(ui, usd(m.quote_volume), muted());
            ui.end_row();
        }
    });
}

fn turkey_card(ui: &mut Ui, t: &Turkey) {
    let opt = |v: Option<f64>, f: &dyn Fn(f64) -> String| v.map_or("–".to_string(), f);
    egui::Grid::new("cryp-tr").num_columns(4).striped(true).spacing([dp(18.0), dp(4.0)]).show(ui, |ui| {
        for (i, h) in ["", "BtcTurk (TRY)", "Reference (TRY)", "Premium"].into_iter().enumerate() {
            head(ui, h, i == 0);
        }
        ui.end_row();
        ui.label(RichText::new("USDT").font(mono(12.5)).color(amber()));
        num(ui, format!("{:.3}", t.usdt_try), text());
        num(ui, opt(t.usdtry_fx, &|v| format!("{v:.3}")), muted());
        match t.usdt_premium {
            Some(p) => num(ui, pct(p), tint(p)),
            None => num(ui, "–".into(), muted()),
        }
        ui.end_row();
        ui.label(RichText::new("BTC").font(mono(12.5)).color(amber()));
        num(ui, opt(t.btc_try, &|v| format!("{v:.0}")), text());
        num(ui, opt(t.btc_usd.zip(t.usdtry_fx).map(|(u, f)| u * f), &|v| format!("{v:.0}")), muted());
        match t.btc_premium {
            Some(p) => num(ui, pct(p), tint(p)),
            None => num(ui, "–".into(), muted()),
        }
        ui.end_row();
    });
    ui.label(
        RichText::new("USDT: BtcTurk USDT/TRY vs the USDTRY dollar rate. BTC: BtcTurk BTC/TRY vs Binance BTCUSDT x USDTRY.")
            .font(ui_font(11.0))
            .color(muted()),
    );
}

pub fn cryp(ui: &mut Ui, p: &mut CrypPane) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("Refresh").on_hover_text("Fetch every section now").clicked() {
            p.last_req = None;
        }
        if p.loading {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
        let mut s = String::from("CoinGecko · alternative.me · Binance · BtcTurk · refreshes every minute");
        if let Some(t) = p.ov.as_ref().and_then(|o| chrono::DateTime::from_timestamp(o.fetched, 0)) {
            s.push_str(&format!(" · updated {}", t.with_timezone(&chrono::Local).format("%H:%M:%S")));
        }
        ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
    });
    if let Some(e) = &p.error {
        ui.label(RichText::new(format!("Could not refresh: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    let Some(ov) = p.ov.clone() else {
        if p.error.is_none() {
            ui.label(RichText::new("loading…").font(ui_font(12.0)).color(muted()));
        }
        return actions;
    };
    egui::ScrollArea::both().id_salt("cryp-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(dp(6.0), dp(6.0));
            match &ov.global {
                Ok(g) => global_cards(ui, g),
                Err(e) => section_error(ui, "Global stats", e),
            }
            match &ov.fear_greed {
                Ok(f) => fear_greed_card(ui, f),
                Err(e) => section_error(ui, "Fear & Greed", e),
            }
        });
        section_title(ui, "BINANCE PERPETUALS (USDⓈ-M)");
        match &ov.derivs {
            Ok(d) => derivs_table(ui, d, &mut actions),
            Err(e) => section_error(ui, "Futures unavailable (geo-blocked in some regions)", e),
        }
        section_title(ui, "24H MOVERS · BINANCE USDT SPOT (≥ $5M VOLUME)");
        match &ov.movers {
            Ok(m) => {
                ui.columns(2, |cols| {
                    cols[0].label(RichText::new("Gainers").font(ui_font(12.0)).color(theme::up()));
                    movers_table(&mut cols[0], "cryp-gain", &m.gainers, &mut actions);
                    cols[1].label(RichText::new("Losers").font(ui_font(12.0)).color(theme::down()));
                    movers_table(&mut cols[1], "cryp-lose", &m.losers, &mut actions);
                });
            }
            Err(e) => section_error(ui, "Movers", e),
        }
        section_title(ui, "TURKEY · BTCTURK PREMIUM");
        match &ov.turkey {
            Ok(t) => turkey_card(ui, t),
            Err(e) => section_error(ui, "BtcTurk", e),
        }
    });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_prices() {
        assert_eq!(usd(2.849e12), "$2.85T");
        assert_eq!(usd(923_450_214.0), "$923.5M");
        assert_eq!(price(84_760.8), "84761");
        assert_eq!(price(0.0827), "0.0827");
        assert_eq!(price(0.00165), "0.001650");
    }

    #[test]
    fn countdown() {
        assert_eq!(until(3 * 3_600_000 + 12 * 60_000, 0), "in 3h 12m");
        assert_eq!(until(0, 5_000), "in 0m");
    }

    #[test]
    fn chart_keys() {
        assert_eq!(chart_key("BTC"), "BTCUSD Curncy");
        assert_eq!(chart_key("IOST"), "IOSTUSDT Curncy");
    }
}
