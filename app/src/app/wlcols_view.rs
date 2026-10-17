//! The watchlist table with its optional extra columns (relative volume, RSI, ATR%, market
//! cap, 52-week position, day volume). Right-click the header to choose columns, click it to
//! sort. Statistics: [`crate::wlcols`].

use std::cmp::Ordering;

use egui::{Align2, Color32, Rect, RichText, Sense, Ui, pos2, vec2};

use super::{PaneKind, TerminalApp};
use crate::congress_view::Action;
use crate::markets_view;
use crate::quote_tag;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;
use crate::wlcols::{self, Col, ListCols, MAIN, SortKey, WlReq, WlResp};
use crate::worker::Request;

/// Fixed room a standard column keeps before the extras start.
const MIN_BASE: f32 = 250.0;

/// Order two optional numbers with missing values last, whichever the direction.
fn cmp_opt(a: Option<f64>, b: Option<f64>, desc: bool) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) if desc => y.total_cmp(&x),
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

impl TerminalApp {
    /// Fetch statistics for the visible watchlists that have extra columns, a batch at a time.
    pub(super) fn wl_periodic(&mut self, visible: &[usize]) {
        let mut keys: Vec<String> = Vec::new();
        for &i in visible {
            let (name, list) = match &self.panes[i].kind {
                PaneKind::Watchlist => (MAIN.to_string(), self.watchlist.clone()),
                PaneKind::List(n) => match self.groups.iter().find(|g| g.name == *n) {
                    Some(g) => (n.clone(), g.keys.clone()),
                    None => continue,
                },
                _ => continue,
            };
            if !self.wl.list(&name).cols.is_empty() {
                for k in list {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
        }
        let due = self.wl.due(&keys);
        if !due.is_empty() {
            self.worker.send(Request::Wl(WlReq::Fetch(due)));
        }
    }

    pub(super) fn on_wl(&mut self, resp: WlResp) {
        let WlResp::Stats(rows) = resp;
        self.wl.store(rows);
    }

    fn sort_keys(&self, keys: &mut [String], lc: &ListCols) {
        let Some((key, desc)) = lc.sort else { return };
        let value = |k: &str| -> Option<f64> {
            match key {
                SortKey::Security => None,
                SortKey::Last => self.quotes.get(k).map(|q| q.last),
                SortKey::Change => self.quotes.get(k).map(|q| q.change()),
                SortKey::Pct => self.quotes.get(k).map(|q| q.change_pct()),
                SortKey::Col(c) => self.wl.get(k).and_then(|s| s.get(c)),
            }
        };
        keys.sort_by(|a, b| match key {
            SortKey::Security => {
                let (x, y) = (
                    tr_data::security(a).ticker.to_ascii_lowercase(),
                    tr_data::security(b).ticker.to_ascii_lowercase(),
                );
                if desc { y.cmp(&x) } else { x.cmp(&y) }
            }
            _ => cmp_opt(value(a), value(b), desc),
        });
    }

    /// The watchlist table; `group` selects a sidebar list instead of the main watchlist.
    pub(super) fn watchlist(&mut self, ui: &mut Ui, group: Option<usize>, q: &str) -> Vec<Action> {
        let name = group.map_or(MAIN.to_string(), |gi| self.groups[gi].name.clone());
        let lc = self.wl.list(&name);
        let mut keys: Vec<String> = match group {
            Some(gi) => self.groups[gi].keys.clone(),
            None => self.watchlist.clone(),
        };
        let mut out = Vec::new();
        let filtering = !q.is_empty();
        if filtering {
            keys.retain(|k| {
                tr_data::security(k).ticker.to_ascii_lowercase().contains(q) || k.to_ascii_lowercase().contains(q)
            });
        }
        let sorted = lc.sort.is_some();
        self.sort_keys(&mut keys, &lc);

        let row_h = dp(22.0);
        let w = ui.available_width();
        let n = lc.cols.len();
        let col_w = if n == 0 { 0.0 } else { ((w - MIN_BASE) / n as f32).clamp(52.0, 70.0) };
        let extras_w = col_w * n as f32;
        // Right edge of extra column `k` in a row.
        let ex = |r: Rect, k: usize| r.right() - 8.0 - (n - 1 - k) as f32 * col_w;
        let cols = [0.0, 0.34, 0.58, 0.79, 1.0];
        let base_of = |r: Rect| Rect::from_min_max(r.min, pos2(r.right() - extras_w, r.max.y));
        let col = |r: Rect, k: usize| {
            let r = base_of(r);
            r.left() + 8.0 + (r.width() - 16.0) * cols[k]
        };

        // ---- header: click to sort, right-click for the column menu ----
        let (hdr, hresp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click());
        ui.painter().rect_filled(hdr, theme::CONTROL_RADIUS, theme::header());
        let mark = |key: SortKey, label: &str| match lc.sort {
            Some((k, desc)) if k == key => format!("{label}{}", if desc { " ↓" } else { " ↑" }),
            _ => label.to_string(),
        };
        let p = ui.painter();
        p.text(
            pos2(col(hdr, 0), hdr.center().y),
            Align2::LEFT_CENTER,
            mark(SortKey::Security, "Security"),
            ui_font(11.5),
            muted(),
        );
        // (right edge, width, key) of every header cell but the security name.
        let mut cells: Vec<(f32, f32, SortKey)> = Vec::new();
        for (k, (key, h)) in [(2, (SortKey::Last, "Last")), (3, (SortKey::Change, "Change")), (4, (SortKey::Pct, "%"))]
        {
            let x = col(hdr, k);
            p.text(pos2(x, hdr.center().y), Align2::RIGHT_CENTER, mark(key, h), ui_font(11.5), muted());
            cells.push((x, 64.0, key));
        }
        for (k, c) in lc.cols.iter().enumerate() {
            let x = ex(hdr, k);
            let key = SortKey::Col(*c);
            p.text(pos2(x, hdr.center().y), Align2::RIGHT_CENTER, mark(key, c.label()), ui_font(11.5), muted());
            cells.push((x, col_w, key));
        }
        if hresp.clicked()
            && let Some(pos) = hresp.interact_pointer_pos()
        {
            let hit = cells.iter().find(|(x, cw, _)| pos.x <= *x + 4.0 && pos.x > *x - cw).map(|c| c.2);
            let key = hit.unwrap_or(SortKey::Security);
            // Only the name side counts as "Security"; a click in a gap does nothing.
            if hit.is_some() || pos.x < col(hdr, 2) - 70.0 {
                self.wl.edit(&name, |l| l.click(key));
            }
        }
        let hresp = hresp.on_hover_text("Click a header to sort · right-click to choose columns");
        let mut toggled = None;
        let mut reset_sort = false;
        hresp.context_menu(|ui| {
            ui.label(RichText::new("Columns").font(ui_font(11.0)).color(muted()));
            for c in Col::ALL {
                if ui.checkbox(&mut lc.cols.contains(&c), c.name()).on_hover_text(c.tip()).clicked() {
                    toggled = Some(c);
                }
            }
            ui.separator();
            if ui.add_enabled(lc.sort.is_some(), egui::Button::new("Clear Sort")).clicked() {
                reset_sort = true;
                ui.close();
            }
        });
        if toggled.is_some() || reset_sort {
            self.wl.edit(&name, |l| {
                if let Some(c) = toggled {
                    l.toggle(c);
                }
                if reset_sort {
                    l.sort = None;
                }
            });
        }

        let mut remove = None;
        let mut moved = None;
        let now = quote_tag::now();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut reorder = widgets::Reorder::new(egui::Id::new("wl-pane-reorder"));
            for s in &keys {
                let sec = tr_data::security(s);
                let (r, resp) = ui.allocate_exact_size(vec2(w, row_h), Sense::click_and_drag());
                reorder.row(r, &resp);
                widgets::hover_wash(ui, r, &resp, false);
                let p = ui.painter();
                let name = p.text(pos2(col(r, 0), r.center().y), Align2::LEFT_CENTER, &sec.ticker, mono(12.5), amber());
                let mut tip = None;
                if let Some(q) = self.quotes.get(s)
                    && let Some(c) = quote_tag::marker(q, now)
                {
                    p.circle_filled(pos2(name.right() + 6.0, r.center().y), 2.5, c);
                    tip = Some(quote_tag::tooltip(q, now));
                }
                p.text(
                    pos2(col(r, 0) + 74.0, r.center().y),
                    Align2::LEFT_CENTER,
                    sec.class.yellow_key().label(),
                    ui_font(11.0),
                    muted(),
                );
                match self.quotes.get(s) {
                    Some(q) => {
                        let c = if q.change() >= 0.0 { theme::up() } else { theme::down() };
                        let d = sec.decimals(q.last);
                        p.text(
                            pos2(col(r, 2), r.center().y),
                            Align2::RIGHT_CENTER,
                            format!("{:.d$}", q.last),
                            mono(12.5),
                            text(),
                        );
                        p.text(
                            pos2(col(r, 3), r.center().y),
                            Align2::RIGHT_CENTER,
                            markets_view::change_text(&sec, q),
                            mono(12.5),
                            c,
                        );
                        p.text(
                            pos2(col(r, 4), r.center().y),
                            Align2::RIGHT_CENTER,
                            format!("{:+.2}%", q.change_pct()),
                            mono(12.5),
                            c,
                        );
                    }
                    None => {
                        p.text(pos2(col(r, 4), r.center().y), Align2::RIGHT_CENTER, "…", mono(12.5), muted());
                    }
                }
                for (k, c) in lc.cols.iter().enumerate() {
                    let (label, color) = self.wl_cell(s, *c);
                    p.text(pos2(ex(r, k), r.center().y), Align2::RIGHT_CENTER, label, mono(12.0), color);
                }
                let tradable = matches!(sec.class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf);
                let pills = if tradable { widgets::buy_sell_pills().to_vec() } else { Vec::new() };
                let tips = [format!("Buy {} (⇧B)", sec.ticker), format!("Sell {} (⇧S)", sec.ticker)];
                let tips: Vec<&str> = tips.iter().map(String::as_str).collect();
                match widgets::row_pills(ui, r, ("wl-pills", s), &pills, &tips) {
                    Some(i) => out.push(Action::Command(format!("{} {s}", if i == 0 { "BUY" } else { "SELL" }))),
                    None if resp.clicked() => out.push(Action::OpenChart(s.clone())),
                    None => {}
                }
                let resp = match tip {
                    Some(t) => resp.on_hover_text(t),
                    None => resp,
                };
                resp.context_menu(|ui| {
                    if tradable {
                        for (label, side) in [("Buy", "BUY"), ("Sell", "SELL")] {
                            if ui.button(format!("{label} {}…", sec.ticker)).clicked() {
                                out.push(Action::Command(format!("{side} {s}")));
                                ui.close();
                            }
                        }
                        ui.separator();
                    }
                    if ui.button("Open Chart").clicked() {
                        out.push(Action::OpenChart(s.clone()));
                        ui.close();
                    }
                    if ui.button("Open Chart as New").clicked() {
                        out.push(Action::OpenChartNew(s.clone()));
                        ui.close();
                    }
                    if let Some(code) = widgets::research_menu(ui, s) {
                        out.push(Action::Command(format!("{s} {code}")));
                    }
                    if sec.has_congress_trades() && ui.button("Who in Congress Traded This").clicked() {
                        out.push(Action::OpenTicker(sec.ticker.clone()));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Remove from Watchlist").clicked() {
                        remove = Some(s.clone());
                        ui.close();
                    }
                });
            }
            moved = reorder.finish(ui);
            ui.add_space(4.0);
            ui.label(
                RichText::new("WL ADD <KEY> to add · drag to reorder · right-click a row for options · right-click the header for columns")
                    .font(ui_font(11.0))
                    .color(muted()),
            );
        });
        let list = match group {
            Some(gi) => &mut self.groups[gi].keys,
            None => &mut self.watchlist,
        };
        if let Some(s) = remove {
            list.retain(|x| *x != s);
        }
        // Dragging only makes sense in the list's own order.
        if let Some((from, to)) = moved.filter(|_| !filtering && !sorted) {
            let item = list.remove(from);
            list.insert(to, item);
        }
        out
    }

    /// Text and colour of one extra cell: the value, "…" while it loads, "n/a" when the
    /// sources have none.
    fn wl_cell(&self, key: &str, c: Col) -> (String, Color32) {
        match self.wl.get(key) {
            Some(s) => match s.get(c) {
                Some(v) => {
                    let color = match c {
                        Col::Rsi if v >= 70.0 => theme::down(),
                        Col::Rsi if v <= 30.0 => theme::up(),
                        Col::RelVol if v >= 2.0 => amber(),
                        _ => text(),
                    };
                    (wlcols::format(c, v), color)
                }
                None => ("n/a".into(), muted()),
            },
            None if self.wl.pending(key) => ("…".into(), muted()),
            None => ("n/a".into(), muted()),
        }
    }
}
