//! Market movers (`MOST`): US day gainers, day losers and most active stocks from Yahoo's
//! predefined screeners. Click a symbol for its chart, right-click for more. Data layer:
//! [`tr_data::most`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, CursorIcon, RichText, Sense, Ui};
use tr_data::most::{Board, List, Mover};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::most::{MostReq, MostResp};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::worker::Request;

/// How often a visible pane re-fetches.
const REFRESH: Duration = Duration::from_secs(60);

pub struct MostPane {
    pub list: List,
    board: Option<Arc<Board>>,
    error: Option<String>,
    loading: bool,
    last_req: Option<Instant>,
    filter: String,
}

impl MostPane {
    pub fn new(list: &str) -> Self {
        let list = match list.to_ascii_lowercase().as_str() {
            "losers" | "loser" | "l" => List::Losers,
            "active" | "actives" | "volume" | "a" => List::Active,
            _ => List::Gainers,
        };
        Self { list, board: None, error: None, loading: false, last_req: None, filter: String::new() }
    }

    pub fn title(&self) -> String {
        format!("Movers · {}", self.list.label())
    }

    pub fn code(&self) -> &'static str {
        "MOST"
    }

    /// The list for saved layouts.
    pub fn settings(&self) -> &'static str {
        match self.list {
            List::Gainers => "gainers",
            List::Losers => "losers",
            List::Active => "active",
        }
    }

    pub fn set_list(&mut self, list: List) {
        self.list = list;
    }
}

// ---- app glue -----------------------------------------------------------------------------

impl TerminalApp {
    /// `MOST`, `MOST LOSERS`, `MOST ACTIVE`: focus (or open) the pane on that list.
    pub(super) fn most_command(&mut self, arg: &str) {
        let list = MostPane::new(arg).list;
        let id = self
            .idx(self.focus)
            .filter(|&i| matches!(self.panes[i].kind, PaneKind::Most(_)))
            .map(|i| self.panes[i].id)
            .or_else(|| self.panes.iter().find(|p| matches!(p.kind, PaneKind::Most(_))).map(|p| p.id));
        match id {
            Some(id) => {
                if let Some(PaneKind::Most(m)) = self.pane_mut(id).map(|p| &mut p.kind) {
                    m.set_list(list);
                }
                self.dock.activate(id);
                self.focus = id;
            }
            None => self.open_panel(PaneKind::Most(Box::new(MostPane::new(arg)))),
        }
    }

    /// Fetch on first show and every minute while the pane is visible.
    pub(super) fn most_periodic(&mut self, visible: &[usize]) {
        for &i in visible {
            let id = self.panes[i].id;
            let PaneKind::Most(m) = &mut self.panes[i].kind else { continue };
            if !m.loading && m.last_req.is_none_or(|t| t.elapsed() >= REFRESH) {
                m.loading = true;
                m.last_req = Some(Instant::now());
                self.worker.send(Request::Most(MostReq::Load { pane: id }));
            }
        }
    }

    pub(super) fn on_most(&mut self, resp: MostResp) {
        let MostResp::Board { pane, board } = resp;
        let Some(PaneKind::Most(m)) = self.pane_mut(pane).map(|p| &mut p.kind) else { return };
        m.loading = false;
        match board {
            Ok(b) => {
                m.board = Some(b);
                m.error = None;
            }
            // Keep the previous lists on screen, but say they are old.
            Err(e) => m.error = Some(e),
        }
    }
}

// ---- drawing ------------------------------------------------------------------------------

/// 1.2K / 3.4M / 5.6B / 7.8T.
fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e12 {
        format!("{:.2}T", v / 1e12)
    } else if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

fn signed(v: f64, decimals: usize) -> String {
    format!("{v:+.decimals$}")
}

fn price_digits(p: f64) -> usize {
    if p.abs() < 1.0 { 4 } else { 2 }
}

fn matches_filter(m: &Mover, f: &str) -> bool {
    let f = f.trim().to_ascii_lowercase();
    f.is_empty() || m.symbol.to_ascii_lowercase().contains(&f) || m.name.to_ascii_lowercase().contains(&f)
}

fn link(ui: &mut Ui, label: String, color: Color32, font: egui::FontId) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(label).font(font).color(color)).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn num(ui: &mut Ui, s: String, color: Color32) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(RichText::new(s).font(mono(12.5)).color(color));
    });
}

pub fn most(ui: &mut Ui, p: &mut MostPane) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.horizontal_wrapped(|ui| {
        for l in List::ALL {
            if ui.selectable_label(p.list == l, RichText::new(l.label()).font(ui_font(12.5))).clicked() {
                p.list = l;
            }
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut p.filter)
                .hint_text(RichText::new("Filter symbol or name…").font(ui_font(12.0)).color(muted()))
                .font(mono(12.5))
                .desired_width(dp(160.0)),
        );
        if ui.small_button("Refresh").on_hover_text("Fetch the lists now").clicked() {
            p.last_req = None;
        }
        if p.loading {
            ui.add(egui::Spinner::new().size(dp(12.0)));
        }
    });
    status_line(ui, p);
    if let Some(e) = &p.error {
        ui.label(RichText::new(format!("Could not refresh: {e}")).font(ui_font(12.0)).color(theme::down()));
    }
    ui.add_space(dp(4.0));
    let Some(board) = p.board.clone() else {
        if p.error.is_none() {
            ui.label(RichText::new("loading…").font(ui_font(12.0)).color(muted()));
        }
        return actions;
    };
    let rows: Vec<&Mover> = board.list(p.list).iter().filter(|m| matches_filter(m, &p.filter)).collect();
    if rows.is_empty() {
        let msg =
            if p.filter.trim().is_empty() { "Yahoo returned an empty list." } else { "No rows match the filter." };
        ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
        return actions;
    }
    egui::ScrollArea::both().id_salt("most-list").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("most-grid").num_columns(9).striped(true).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            for (i, h) in ["Symbol", "Name", "Price", "Change", "% Chg", "Volume", "Avg Vol (3M)", "Rel Vol", "Mkt Cap"]
                .into_iter()
                .enumerate()
            {
                if i < 2 {
                    ui.label(RichText::new(h).font(ui_font(11.0)).color(muted()));
                } else {
                    num(ui, h.to_string(), muted());
                }
            }
            ui.end_row();
            for m in rows {
                row(ui, m, &mut actions);
            }
        });
    });
    actions
}

/// Yahoo's `marketState` in words. The board ranks regular-session changes, so outside regular
/// hours they are the last session's.
fn market_state_text(st: &str) -> String {
    match st.to_ascii_uppercase().as_str() {
        "REGULAR" => "market open".into(),
        "PRE" => "pre-market (changes are the last session's)".into(),
        "POST" | "POSTPOST" => "after hours (changes are the regular session's)".into(),
        "PREPRE" | "CLOSED" => "market closed (changes are the last session's)".into(),
        other => format!("market {}", other.to_ascii_lowercase()),
    }
}

fn status_line(ui: &mut Ui, p: &MostPane) {
    let mut s = String::from("Yahoo screener");
    if let Some(b) = &p.board {
        match b.delay_min() {
            Some(0) => s.push_str(" · live"),
            Some(d) => s.push_str(&format!(" · {d}m delayed")),
            None => s.push_str(" · delayed"),
        }
        if let Some(st) = b.market_state() {
            s.push_str(" · ");
            s.push_str(&market_state_text(st));
        }
        if let Some(t) = chrono::DateTime::from_timestamp(b.fetched, 0) {
            s.push_str(&format!(" · updated {}", t.with_timezone(&chrono::Local).format("%H:%M:%S")));
        }
    }
    s.push_str(" · US stocks · refreshes every minute");
    ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
}

fn row(ui: &mut Ui, m: &Mover, actions: &mut Vec<Action>) {
    let sym = m.symbol.clone();
    let sym_resp = link(ui, sym.clone(), amber(), mono(12.5)).on_hover_text("Click: chart · right-click: more");
    let name_resp = link(ui, trunc(&m.name, 34), text(), ui_font(12.5)).on_hover_text(if m.exchange.is_empty() {
        m.name.clone()
    } else {
        format!("{} · {}", m.name, m.exchange)
    });
    for r in [&sym_resp, &name_resp] {
        if r.clicked() {
            actions.push(Action::OpenChart(sym.clone()));
        }
        r.context_menu(|ui| {
            if ui.button("Open Chart").clicked() {
                actions.push(Action::OpenChart(sym.clone()));
                ui.close();
            }
            if ui.button("Open Chart as New").clicked() {
                actions.push(Action::OpenChartNew(sym.clone()));
                ui.close();
            }
            if let Some(code) = crate::widgets::research_menu(ui, &sym) {
                actions.push(Action::Command(format!("{sym} {code}")));
            }
            if ui.button("Add to Watchlist").clicked() {
                actions.push(Action::AddToWatchlist(sym.clone()));
                ui.close();
            }
        });
    }
    let color = if m.change_pct >= 0.0 { theme::up() } else { theme::down() };
    let d = price_digits(m.price);
    num(ui, format!("{:.d$}", m.price), text());
    num(ui, signed(m.change, d), color);
    num(ui, format!("{}%", signed(m.change_pct, 2)), color);
    num(ui, m.volume.map_or("–".into(), compact), text());
    num(ui, m.avg_volume.map_or("–".into(), compact), muted());
    num(
        ui,
        m.rel_volume().map_or("–".into(), |r| format!("{r:.1}x")),
        if m.rel_volume().is_some_and(|r| r >= 2.0) { amber() } else { text() },
    );
    num(ui, m.market_cap.map_or("–".into(), compact), muted());
    ui.end_row();
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_states_in_words() {
        assert_eq!(market_state_text("PREPRE"), "market closed (changes are the last session's)");
        assert_eq!(market_state_text("REGULAR"), "market open");
        assert_eq!(market_state_text("HALTED"), "market halted");
    }

    #[test]
    fn compact_numbers() {
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(12_345.0), "12.3K");
        assert_eq!(compact(4_867_393.0), "4.87M");
        assert_eq!(compact(29_861_658_624.0), "29.86B");
    }

    #[test]
    fn pane_list_names() {
        assert_eq!(MostPane::new("losers").list, List::Losers);
        assert_eq!(MostPane::new("ACTIVE").settings(), "active");
        assert_eq!(MostPane::new("").settings(), "gainers");
    }
}
