//! Confirmation, cancel and enable-live dialogs.

use super::*;

impl TerminalApp {
    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let Some(p) = t.current_preview().cloned() else {
            if let Some(t) = self.trading.ticket.as_mut() {
                t.confirming = false;
            }
            return;
        };
        let quote = self.ticket_quote(t);
        let account = self.broker.portfolio.as_ref().and_then(|pf| pf.account(&p.request.account_id)).cloned();
        let now = chrono::Utc::now();
        let expired = p.expired(now);
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("order-confirm")).show(ctx, |ui| {
            ui.set_width(dp(380.0));
            let r = &p.request;
            let side_color = if r.side == Side::Buy { theme::up() } else { theme::down() };
            ui.horizontal(|ui| {
                if !p.simulated {
                    mode_badge(ui, true);
                }
                ui.label(
                    RichText::new(format!("Confirm {} order", r.side.label().to_lowercase()))
                        .font(ui_font(16.0))
                        .strong(),
                );
            });
            ui.add_space(dp(4.0));
            egui::Grid::new("confirm-grid").num_columns(2).spacing([dp(14.0), dp(5.0)]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: RichText| {
                    ui.label(RichText::new(k).font(ui_font(12.0)).color(muted()));
                    ui.label(v);
                    ui.end_row();
                };
                let acct = account.as_ref().map_or(r.account_id.clone(), account_line);
                row(ui, "Account", RichText::new(acct).font(ui_font(12.5)).color(text()));
                let sym = if p.symbol.description.is_empty() {
                    p.symbol.symbol.clone()
                } else {
                    format!("{} — {}", p.symbol.symbol, p.symbol.description)
                };
                row(ui, "Symbol", RichText::new(sym).font(mono(13.0)).color(amber()));
                let size = match r.amount {
                    Amount::Units(u) => format!("{} {} shares", r.side.api(), qty(p.units.unwrap_or(u))),
                    Amount::Notional(n) => format!("{} {} (dollar amount)", r.side.api(), money(n, &p.currency)),
                };
                row(ui, "Order", RichText::new(size).font(mono(13.0)).strong().color(side_color));
                let mut kind = r.order_type.label().to_string();
                if let Some(s) = r.stop.filter(|_| r.order_type.needs_stop()) {
                    kind += &format!(" · stop {}", price(s));
                }
                if let Some(l) = r.limit.filter(|_| r.order_type.needs_limit()) {
                    kind += &format!(" · limit {}", price(l));
                }
                kind += &format!(" · {}", r.time_in_force.label());
                row(ui, "Type", RichText::new(kind).font(ui_font(12.5)).color(text()));
                let cost = if r.side == Side::Buy { "Est. cost" } else { "Est. proceeds" };
                let total =
                    p.estimated_total(quote).map_or("unknown (market price at fill)".into(), |v| money(v, &p.currency));
                row(ui, cost, RichText::new(total).font(mono(14.0)).strong().color(text()));
                if let Some(c) = p.commission.filter(|c| *c > 0.0) {
                    row(ui, "Commission", RichText::new(money(c, &p.currency)).font(mono(12.5)).color(text()));
                }
                if let Some(c) = p.remaining_cash {
                    row(ui, "Cash after", RichText::new(money(c, &p.currency)).font(mono(12.5)).color(text()));
                }
            });
            ui.add_space(dp(6.0));
            let warn = if p.simulated {
                "Simulated: the order is recorded in the sample portfolio only.".to_string()
            } else {
                format!(
                    "This sends a real order to {}. It may fill immediately and a filled order cannot be undone.",
                    account.as_ref().map_or("your broker", |a| a.institution.as_str())
                )
            };
            ui.label(RichText::new(warn).font(ui_font(11.5)).color(if p.simulated { theme::cyan() } else { amber() }));
            if expired {
                ui.label(
                    RichText::new("The preview expired: cancel and review again.")
                        .font(ui_font(12.0))
                        .color(theme::down()),
                );
            } else {
                let left = (p.expires() - now).num_seconds().max(0);
                ui.label(
                    RichText::new(format!("Broker estimate valid for {}:{:02}", left / 60, left % 60))
                        .font(ui_font(11.0))
                        .color(muted()),
                );
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
                let label = format!("{} {} — Send", r.side.label(), r.symbol);
                if ui.add_enabled(!expired, filled(&label, side_color, !expired)).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.ticket_place(),
            Some(false) => {
                if let Some(t) = self.trading.ticket.as_mut() {
                    t.confirming = false;
                }
            }
            None => {}
        }
    }

    pub(super) fn cancel_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.trading.cancel.clone() else { return };
        let account = self.broker.portfolio.as_ref().and_then(|pf| pf.account(&c.order.account_id)).cloned();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("cancel-confirm")).show(ctx, |ui| {
            ui.set_width(dp(340.0));
            let o = &c.order;
            ui.horizontal(|ui| {
                mode_badge(ui, true);
                ui.label(RichText::new("Cancel order?").font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            let mut line =
                format!("{} {} {}", o.action.replace('_', " "), o.quantity.map_or("—".into(), qty), o.short_label());
            if let Some(l) = o.limit_price {
                line += &format!(" limit {}", price(l));
            }
            if let Some(s) = o.stop_price {
                line += &format!(" stop {}", price(s));
            }
            ui.label(RichText::new(line).font(mono(13.0)).color(text()));
            ui.label(
                RichText::new(format!(
                    "{} · order #{} · {}",
                    account.as_ref().map_or(o.account_id.clone(), |a| format!("{} {}", a.label(), a.masked_number())),
                    o.id,
                    o.status.to_ascii_lowercase()
                ))
                .font(ui_font(11.5))
                .color(muted()),
            );
            ui.label(RichText::new("Shares that already filled stay filled.").font(ui_font(11.5)).color(muted()));
            if let Some(e) = &c.error {
                ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.add_enabled(!c.busy, egui::Button::new("Keep Order")).clicked() {
                    choice = Some(false);
                }
                let b =
                    egui::Button::new(RichText::new("Cancel Order").strong().color(Color32::WHITE)).fill(theme::down());
                if ui.add_enabled(!c.busy, b).clicked() {
                    choice = Some(true);
                }
                if c.busy {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        });
        if choice.is_none() && !c.busy && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.confirm_cancel(),
            Some(false) => self.trading.cancel = None,
            None => {}
        }
    }

    pub(super) fn enable_dialog(&mut self, ctx: &egui::Context) {
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("trading-enable")).show(ctx, |ui| {
            ui.set_width(dp(420.0));
            ui.label(RichText::new("Enable live trading?").font(ui_font(16.0)).strong());
            ui.add_space(dp(4.0));
            for line in [
                "Orders you confirm are sent to your real brokerage account through SnapTrade and can execute immediately with real money.",
                "Market orders fill at the prevailing price, which can differ from the preview. Quotes in terminal-rs may be delayed.",
                "terminal-rs is provided as is, without warranty. You are responsible for every order you place.",
                "Your brokerage connection must allow trading (reconnect with trading access if it was connected read-only).",
            ] {
                ui.label(RichText::new(format!("• {line}")).font(ui_font(12.0)).color(text()));
            }
            ui.add_space(dp(6.0));
            ui.checkbox(&mut self.trading.ack, "I understand that orders are real and I am responsible for them");
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Keep Live Trading Off").clicked() {
                    choice = Some(false);
                }
                if ui.add_enabled(self.trading.ack, filled("Enable Live Trading", theme::down(), self.trading.ack)).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        if let Some(on) = choice {
            self.trading.enable_prompt = false;
            self.trading.ack = false;
            if on {
                self.prefs.trading.enabled = true;
                self.save_prefs_now();
                self.trading.message = Some(("Live trading enabled.".into(), false));
                self.status = "Live trading enabled (Settings › Trading)".into();
                if self.demo_mode() {
                    self.trading.live_prompt = true;
                }
            }
        }
    }

    /// Confirm switching from the demo account to live accounts.
    pub(super) fn live_dialog(&mut self, ctx: &egui::Context) {
        let mut choice = None;
        let enabled = self.trading_enabled();
        let modal = egui::Modal::new(egui::Id::new("switch-live")).show(ctx, |ui| {
            ui.set_width(dp(400.0));
            ui.horizontal(|ui| {
                mode_badge(ui, true);
                ui.label(RichText::new("Switch to your live accounts?").font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            for line in [
                "Portfolio panes and the order ticket will use your real brokerage accounts (SnapTrade) instead of the demo account.",
                if enabled {
                    "Live orders are enabled: confirmed orders are sent to your broker with real money."
                } else {
                    "Live orders stay off until you enable them in Settings › Trading; until then you can only view."
                },
                "This choice is remembered. Switch back to DEMO any time from the toolbar badge.",
            ] {
                ui.label(RichText::new(format!("• {line}")).font(ui_font(12.0)).color(text()));
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Stay in Demo").clicked() {
                    choice = Some(false);
                }
                if ui.add(filled("Switch to LIVE", theme::down(), true)).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.set_account_mode(AccountMode::Live, true),
            Some(false) => self.trading.live_prompt = false,
            None => {}
        }
    }
}
