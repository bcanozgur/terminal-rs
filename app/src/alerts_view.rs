//! Price/indicator alert rules and their fired history, shown at the top of the ALERTS pane
//! above the Congress filing alerts.

use egui::{Color32, RichText, ScrollArea, Sense, Ui};

use crate::alerts::{AlertBook, RuleOp};
use crate::theme::{self, amber, mono, muted, text};

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn cell(ui: &mut Ui, s: impl Into<String>, color: Color32) {
    ui.add(egui::Label::new(RichText::new(s.into()).font(mono(12.0)).color(color)).extend());
}

fn button(ui: &mut Ui, s: &str, color: Color32, tip: &str) -> bool {
    let r = ui
        .add(egui::Label::new(RichText::new(s).font(mono(12.0)).color(color)).extend().sense(Sense::click()))
        .on_hover_text(tip);
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

fn pad(s: &str, n: usize) -> String {
    let mut t: String = s.chars().take(n).collect();
    while t.chars().count() < n {
        t.push(' ');
    }
    t
}

/// The rules table and fired history. `q` is the pane's lowercase filter. Returns the changes
/// the user asked for and the rule whose row was clicked (to show it on a chart).
pub fn rules(ui: &mut Ui, book: &AlertBook, q: &str) -> (Vec<RuleOp>, Option<u64>) {
    let mut ops = Vec::new();
    let mut open = None;
    ui.horizontal(|ui| {
        cell(ui, "PRICE & INDICATOR ALERTS", amber());
        let mut notify = book.notify;
        if ui
            .checkbox(&mut notify, RichText::new("Notification Center").font(mono(11.5)))
            .on_hover_text("Also post a macOS notification when a rule fires")
            .changed()
        {
            ops.push(RuleOp::Notify(notify));
        }
        cell(ui, "ALRT AAPL > 250 · ALRT NVDA RSI > 70 · ALRT MSFT > SMA 50 · right-click a chart", muted());
    });
    let matches = |s: &str| q.is_empty() || s.to_ascii_lowercase().contains(q);
    let rules: Vec<_> = book.rules.iter().filter(|r| matches(&r.symbol) || matches(&r.cond.label())).collect();
    let fired: Vec<_> = book.fired.iter().filter(|f| matches(&f.message)).collect();
    let max = (ui.available_height() * 0.55).max(80.0);
    ScrollArea::vertical().id_salt("price-alerts").max_height(max).auto_shrink([false, true]).show(ui, |ui| {
        if book.rules.is_empty() {
            cell(ui, "No price alerts yet. Type ALRT <ticker> > <price> or right-click a chart.", muted());
        } else {
            ui.add(
                egui::Label::new(
                    RichText::new(" ON   SECURITY  CONDITION                          TF    MODE    STATUS")
                        .font(mono(11.5))
                        .color(amber())
                        .background_color(theme::header()),
                )
                .extend(),
            );
        }
        for r in rules {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (on, color) = if r.enabled { ("[ON ]", theme::up()) } else { ("[OFF]", muted()) };
                if button(ui, on, color, if r.enabled { "Disable" } else { "Enable (re-arms the rule)" }) {
                    ops.push(RuleOp::Enable(r.id, !r.enabled));
                }
                let fg = if r.enabled { text() } else { muted() };
                let tip = "Open a chart of this security";
                if button(ui, &pad(&r.ticker(), 8), if r.enabled { amber() } else { muted() }, tip) {
                    open = Some(r.id);
                }
                let label =
                    if r.note.is_empty() { r.cond.label() } else { format!("{} · {}", r.cond.label(), r.note) };
                if button(ui, &pad(&label, 33), fg, tip) {
                    open = Some(r.id);
                }
                cell(ui, pad(r.tf.map_or("", |t| t.label()), 4), muted());
                let mode = if r.repeat { "REPEAT" } else { "ONCE  " };
                if button(ui, mode, theme::cyan(), "One-shot or repeating: click to switch") {
                    ops.push(RuleOp::Repeat(r.id, !r.repeat));
                }
                let status = match (r.enabled, r.last_fired) {
                    (true, None) => "armed".to_string(),
                    (true, Some(t)) => format!("armed · fired {}× last {}", r.fired, fmt_ts(t)),
                    (false, Some(t)) => format!("triggered {}", fmt_ts(t)),
                    (false, None) => "off".to_string(),
                };
                cell(ui, pad(&status, 34), muted());
                if button(ui, "✕", theme::down(), "Delete this rule") {
                    ops.push(RuleOp::Delete(r.id));
                }
            });
        }
        if !book.fired.is_empty() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let unseen = book.unseen();
                cell(ui, format!("TRIGGERED  {unseen} unread"), if unseen > 0 { amber() } else { muted() });
                if button(ui, "CLEAR", theme::cyan(), "Clear the triggered list") {
                    ops.push(RuleOp::ClearFired);
                }
            });
            for f in fired {
                ui.horizontal(|ui| {
                    cell(ui, fmt_ts(f.ts), if f.seen { muted() } else { amber() });
                    cell(ui, &f.message, if f.seen { muted() } else { text() });
                });
            }
        }
    });
    ui.separator();
    cell(ui, "CONGRESS FILINGS", amber());
    (ops, open)
}
