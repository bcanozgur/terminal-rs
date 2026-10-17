//! Security description (`DES`): SEC profile, live valuation and recent filings.

use egui::{RichText, Sense, Ui};
use tr_data::Quote;
use tr_data::edgar::{Company, Metric};

use super::fa_view::{fmt_money, fmt_ratio};
use crate::theme::{self, amber, mono, muted, text, ui_font};

/// Forms listed under recent filings.
const FORMS: [&str; 10] = ["10-K", "10-K/A", "10-Q", "10-Q/A", "8-K", "8-K/A", "DEF 14A", "S-1", "S-3", "SC 13D"];

/// Short descriptions of common 8-K items.
fn item_label(item: &str) -> Option<&'static str> {
    Some(match item {
        "1.01" => "Material agreement",
        "1.02" => "Agreement terminated",
        "2.01" => "Acquisition/disposition",
        "2.02" => "Results of operations",
        "2.03" => "Financial obligation",
        "2.05" => "Exit costs",
        "2.06" => "Impairment",
        "3.01" => "Listing notice",
        "3.02" => "Unregistered sale",
        "4.01" => "Auditor change",
        "5.02" => "Officers/directors",
        "5.03" => "Bylaws amendment",
        "5.07" => "Shareholder vote",
        "7.01" => "Reg FD disclosure",
        "8.01" => "Other events",
        _ => return None,
    })
}

fn filing_text(form: &str, items: &str, description: &str) -> String {
    if form.starts_with("8-K") && !items.is_empty() {
        let labels: Vec<&str> = items.split(',').filter_map(|i| item_label(i.trim())).collect();
        if !labels.is_empty() {
            return labels.join(" · ");
        }
    }
    match form {
        "10-K" => "Annual report".into(),
        "10-Q" => "Quarterly report".into(),
        "DEF 14A" => "Proxy statement".into(),
        _ => description.to_string(),
    }
}

fn kv(ui: &mut Ui, k: &str, v: impl Into<String>, color: egui::Color32) {
    ui.label(RichText::new(k).font(ui_font(11.5)).color(muted()));
    ui.label(RichText::new(v.into()).font(mono(12.0)).color(color));
    ui.end_row();
}

pub(super) fn des(ui: &mut Ui, c: &Company, quote: Option<&Quote>) {
    let p = &c.profile;
    let wide = ui.available_width() > 720.0;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let profile = |ui: &mut Ui| {
            egui::Grid::new(("des-profile", p.cik)).num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
                kv(ui, "Company", &p.name, amber());
                kv(ui, "CIK", format!("{:010}", p.cik), text());
                if !p.tickers.is_empty() {
                    let listings: Vec<String> = p
                        .tickers
                        .iter()
                        .zip(p.exchanges.iter().map(Some).chain(std::iter::repeat(None)))
                        .map(|(t, e)| e.map_or_else(|| t.clone(), |e| format!("{t} ({e})")))
                        .collect();
                    kv(ui, "Listings", listings.join(", "), text());
                }
                if !p.sic.is_empty() {
                    kv(ui, "Industry (SIC)", format!("{} {}", p.sic, p.sic_description), text());
                }
                if !p.category.is_empty() {
                    kv(ui, "Filer", &p.category, text());
                }
                if !p.state_of_incorporation.is_empty() {
                    kv(ui, "Incorporated", &p.state_of_incorporation, text());
                }
                if !p.fiscal_year_end.is_empty() {
                    kv(ui, "Fiscal year end", p.fiscal_year_end_label(), text());
                }
                if !p.business_address.is_empty() {
                    kv(ui, "Address", &p.business_address, text());
                }
                if !p.phone.is_empty() {
                    kv(ui, "Phone", &p.phone, text());
                }
                if !p.website.is_empty() {
                    ui.label(RichText::new("Website").font(ui_font(11.5)).color(muted()));
                    let url = if p.website.starts_with("http") {
                        p.website.clone()
                    } else {
                        format!("https://{}", p.website)
                    };
                    if ui.link(RichText::new(&p.website).font(mono(12.0))).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                    ui.end_row();
                }
                if let Some(prev) = p.former_names.first() {
                    kv(ui, "Formerly", prev, muted());
                }
            });
        };
        let stats = |ui: &mut Ui| {
            let Ok(f) = &c.financials else {
                if let Err(e) = &c.financials {
                    ui.label(RichText::new(format!("Financials: {e}")).font(ui_font(11.5)).color(muted()));
                }
                return;
            };
            let v = f.valuation(quote.map(|q| q.last));
            egui::Grid::new(("des-stats", p.cik)).num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
                let or_dash = |x: Option<String>| x.unwrap_or_else(|| "–".into());
                kv(ui, "Price", or_dash(quote.map(|q| format!("{:.2}", q.last))), text());
                kv(ui, "Market cap", or_dash(v.market_cap.map(fmt_money)), amber());
                let as_of = f
                    .shares_as_of
                    .map(|d| format!(" ({}, {})", f.shares_source, d.format("%b %-d %Y")))
                    .unwrap_or_default();
                kv(ui, "Shares out", or_dash(f.shares_outstanding.map(|s| format!("{:.3}B{as_of}", s / 1e9))), text());
                kv(ui, "Enterprise value", or_dash(v.enterprise_value.map(fmt_money)), text());
                kv(ui, "P/E (TTM)", or_dash(v.pe.map(|x| format!("{x:.1}x"))), text());
                kv(ui, "P/S (TTM)", or_dash(v.ps.map(|x| format!("{x:.1}x"))), text());
                kv(ui, "P/B", or_dash(v.pb.map(|x| format!("{x:.1}x"))), text());
                kv(ui, "Revenue (TTM)", or_dash(v.ttm_revenue.map(fmt_money)), text());
                kv(ui, "Net income (TTM)", or_dash(v.ttm_net_income.map(fmt_money)), text());
                if let Some(y) = f.annual.last() {
                    for m in [Metric::GrossMargin, Metric::OperatingMargin, Metric::NetMargin, Metric::Roe] {
                        kv(
                            ui,
                            &format!("{} {}", m.label(), y.label),
                            or_dash(y.get(m).map(|x| fmt_ratio(m, x))),
                            text(),
                        );
                    }
                }
            });
            if quote.is_none() {
                ui.label(RichText::new("Waiting for a live quote for valuation…").font(ui_font(11.0)).color(muted()));
            }
        };
        if wide {
            ui.columns(2, |cols| {
                profile(&mut cols[0]);
                stats(&mut cols[1]);
            });
        } else {
            profile(ui);
            ui.add_space(8.0);
            stats(ui);
        }

        ui.add_space(10.0);
        ui.label(RichText::new("Recent filings").font(ui_font(12.5)).color(text()));
        ui.separator();
        let filings: Vec<_> = p.filings_of(&FORMS).take(30).collect();
        if filings.is_empty() {
            ui.label(RichText::new("No recent periodic filings.").font(ui_font(11.5)).color(muted()));
        }
        egui::Grid::new(("des-filings", p.cik)).num_columns(4).spacing([14.0, 2.0]).striped(true).show(ui, |ui| {
            for f in filings {
                ui.label(RichText::new(f.filed.format("%Y-%m-%d").to_string()).font(mono(11.5)).color(muted()));
                let col = match f.form.as_str() {
                    "10-K" | "10-K/A" => amber(),
                    "10-Q" | "10-Q/A" => theme::cyan(),
                    _ => text(),
                };
                ui.label(RichText::new(&f.form).font(mono(11.5)).color(col));
                let period = f.report.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                ui.label(RichText::new(period).font(mono(11.5)).color(muted()));
                let r = ui
                    .label(
                        RichText::new(filing_text(&f.form, &f.items, &f.description)).font(ui_font(11.5)).color(text()),
                    )
                    .interact(Sense::click())
                    .on_hover_text(&f.url);
                if r.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&f.url));
                }
                ui.end_row();
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Source: SEC EDGAR · click a filing to open it on sec.gov")
                .font(ui_font(10.5))
                .color(muted()),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filing_descriptions() {
        assert_eq!(filing_text("8-K", "2.02,9.01", ""), "Results of operations");
        assert_eq!(filing_text("8-K", "5.02,5.07", ""), "Officers/directors · Shareholder vote");
        assert_eq!(filing_text("8-K", "9.01", "FORM 8-K"), "FORM 8-K");
        assert_eq!(filing_text("10-K", "", "10-K"), "Annual report");
    }
}
