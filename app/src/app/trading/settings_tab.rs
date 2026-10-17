//! Settings › Trading and the brokerage access note.

use super::*;

impl TerminalApp {
    /// Settings › Brokerage access line, which depends on whether trading is enabled.
    pub(in crate::app) fn brokerage_access_note(&self) -> String {
        if self.trading_enabled() {
            "Live trading is enabled (Settings › Trading): confirmed orders are sent to your broker. Connect Brokerage \
             requests read-only access; use Reconnect with Trading Access in Settings › Trading for brokers that trade."
                .into()
        } else {
            "Read-only: terminal-rs reads accounts, balances, positions, orders and activity. Live trading is off \
             (Settings › Trading), and the Connection Portal requests read-only access."
                .into()
        }
    }

    pub(super) fn save_prefs_now(&mut self) {
        if let Err(e) = self.prefs.save(&crate::app::prefs_path()) {
            self.status = format!("Could not save preferences: {e}");
        }
    }

    // ---- settings -------------------------------------------------------------------------

    /// Settings › Trading.
    pub(in crate::app) fn trading_tab(&mut self, ui: &mut Ui, cmds: &mut Vec<SettingsCmd>) {
        let section = |ui: &mut Ui, title: &str| {
            ui.add_space(dp(10.0));
            ui.label(RichText::new(title).font(ui_font(13.5)).color(text()));
            ui.separator();
        };
        let note = |ui: &mut Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };

        section(ui, "Account");
        let demo_mode = self.demo_mode();
        ui.horizontal(|ui| {
            mode_badge(ui, !demo_mode);
            ui.label(
                RichText::new(if demo_mode {
                    "Demo account: paper trading with virtual cash."
                } else {
                    "Live accounts: your real brokerage accounts through SnapTrade."
                })
                .font(ui_font(12.5))
                .color(text()),
            );
        });
        ui.horizontal(|ui| {
            if demo_mode {
                if ui.button(RichText::new("Switch to Live Accounts…").color(theme::down())).clicked() {
                    self.set_account_mode(AccountMode::Live, false);
                }
            } else if ui.button("Switch to Demo Account").clicked() {
                self.set_account_mode(AccountMode::Demo, false);
            }
        });
        note(ui, "The toolbar badge shows which account is active; click it to switch.");

        section(ui, "Demo account");
        let b = &self.demo.book;
        let equity = b.equity();
        let pnl = equity - b.starting_cash;
        let open = b.orders.iter().filter(|o| o.is_open()).count();
        egui::Grid::new("demo-stats").num_columns(2).spacing([dp(14.0), dp(4.0)]).show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String, c: Color32| {
                ui.label(RichText::new(k).font(ui_font(12.0)).color(muted()));
                ui.label(RichText::new(v).font(mono(12.5)).color(c));
                ui.end_row();
            };
            row(ui, "Value", money(equity, "USD"), text());
            row(ui, "Cash", money(b.cash, "USD"), text());
            row(ui, "Buying power", money(b.buying_power(), "USD"), text());
            let c = if pnl >= 0.0 { theme::up() } else { theme::down() };
            row(ui, "P&L since start", format!("{}{}", if pnl >= 0.0 { "+" } else { "−" }, money(pnl.abs(), "USD")), c);
            row(ui, "Positions · open orders", format!("{} · {open}", b.positions.len()), text());
            if let Some(t) = b.created {
                row(ui, "Started", t.with_timezone(&chrono::Local).format("%d %b %Y").to_string(), muted());
            }
        });
        ui.horizontal(|ui| {
            if self.demo.confirm_reset {
                ui.label(RichText::new("Start over with").font(ui_font(12.0)));
                ui.add(
                    egui::DragValue::new(&mut self.demo.reset_cash)
                        .range(1_000.0..=10_000_000.0)
                        .speed(1000.0)
                        .prefix("$"),
                );
                if ui.add(filled("Reset Demo Account", theme::down(), true)).clicked() {
                    let cash = self.demo.reset_cash;
                    self.reset_demo(cash);
                }
                if ui.button("Cancel").clicked() {
                    self.demo.confirm_reset = false;
                }
            } else if ui
                .button("Reset Demo Account…")
                .on_hover_text("Clear demo positions, orders and history and start again with virtual cash")
                .clicked()
            {
                self.demo.confirm_reset = true;
                self.demo.reset_cash = tr_broker::demo::STARTING_CASH;
            }
        });
        if self.demo.confirm_reset {
            note(ui, "Positions, orders and history in the demo account are deleted. Live accounts are not affected.");
        }
        if let Some(e) = &self.demo.save_error {
            ui.label(
                RichText::new(format!("Could not save the demo account: {e}. Retrying every few seconds."))
                    .font(ui_font(12.0))
                    .color(theme::down()),
            );
        }
        note(
            ui,
            "Market orders fill at the terminal's last quote; limit and stop orders fill when the quote crosses their price. No commissions, no short selling, Day orders expire at the 4 p.m. New York close.",
        );

        section(ui, "Live trading (SnapTrade)");
        let on = self.prefs.trading.enabled;
        ui.label(
            RichText::new(if on {
                "Live orders are ON: confirmed orders are sent to your broker."
            } else {
                "Live orders are OFF."
            })
            .font(ui_font(12.5))
            .color(if on { theme::down() } else { theme::accent() }),
        );
        note(
            ui,
            "With live accounts selected, every order is checked by your broker through SnapTrade (estimated cost and fees) and needs one final confirmation. Open orders can be cancelled from ORD.",
        );
        ui.horizontal(|ui| {
            if on {
                if ui.button("Turn Live Orders Off").clicked() {
                    self.prefs.trading.enabled = false;
                    self.trading.message = Some(("Live trading disabled.".into(), false));
                    if let Some(t) = &mut self.trading.ticket {
                        t.confirming = false;
                    }
                    cmds.push(SettingsCmd::SavePrefs);
                }
            } else if ui.button(RichText::new("Enable Live Trading…").strong()).clicked() {
                self.trading.enable_prompt = true;
                self.trading.ack = false;
            }
        });

        section(ui, "Safety");
        ui.horizontal(|ui| {
            let mut limited = self.prefs.trading.max_order_value.is_some();
            if ui.checkbox(&mut limited, "Maximum live order value").changed() {
                self.prefs.trading.max_order_value = limited.then_some(5_000.0);
                cmds.push(SettingsCmd::SavePrefs);
            }
            if let Some(v) = &mut self.prefs.trading.max_order_value {
                let r = ui.add(egui::DragValue::new(v).range(1.0..=10_000_000.0).speed(50.0).prefix("$"));
                if r.changed() {
                    cmds.push(SettingsCmd::SavePrefs);
                }
            }
        });
        note(ui, "Live orders whose estimated value is above the limit are refused before they are sent.");
        let accounts: Vec<Account> = self.broker.portfolio.as_ref().map(|p| p.accounts.clone()).unwrap_or_default();
        if !accounts.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Default live account").font(ui_font(12.5)));
                let current = self
                    .prefs
                    .trading
                    .default_account
                    .as_ref()
                    .and_then(|id| accounts.iter().find(|a| &a.id == id))
                    .map_or("Ask each time".to_string(), account_line);
                let before = self.prefs.trading.default_account.clone();
                egui::ComboBox::from_id_salt("trading-default-account").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.prefs.trading.default_account, None, "Ask each time");
                    for a in &accounts {
                        ui.selectable_value(
                            &mut self.prefs.trading.default_account,
                            Some(a.id.clone()),
                            account_line(a),
                        );
                    }
                });
                if self.prefs.trading.default_account != before {
                    cmds.push(SettingsCmd::SavePrefs);
                }
            });
        }

        self.safety_settings_ui(ui);

        section(ui, "Brokerage access");
        let live = self.broker.mode() == BrokerMode::Live;
        if live && !demo_mode {
            self.ensure_capabilities(false);
        }
        let mut by_broker: std::collections::BTreeMap<String, (usize, usize, Option<String>)> = Default::default();
        for a in &accounts {
            if let Some(c) = self.capability(&a.id) {
                let e = by_broker.entry(c.brokerage.clone()).or_default();
                e.0 += 1;
                e.1 += usize::from(c.can_trade());
                e.2 = c.blocker();
            }
        }
        for (broker, (n, ok, why)) in &by_broker {
            let (line, color) = if *ok == *n {
                (format!("{broker}: trading available on {n} account{}", if *n == 1 { "" } else { "s" }), theme::up())
            } else {
                (format!("{broker}: {}", why.clone().unwrap_or_else(|| "read-only".into())), amber())
            };
            ui.label(RichText::new(line).font(ui_font(12.0)).color(color));
        }
        if live {
            let checked = self
                .trading
                .caps
                .as_ref()
                .and_then(|c| c.fetched)
                .map(|t| format!("Checked {}", t.with_timezone(&chrono::Local).format("%d %b %H:%M")));
            let pending = self.trading.caps_inflight.is_some()
                || self.trading.caps_throttled.is_some_and(|t| t > Instant::now())
                || self.trading.caps_error.is_some();
            ui.horizontal(|ui| {
                if let Some(c) = &checked {
                    note(ui, c);
                }
                if pending {
                    let n = self.caps_pending_note();
                    ui.label(RichText::new(n).font(ui_font(11.5)).color(muted()));
                }
                if ui
                    .add_enabled(self.trading.caps_inflight.is_none(), egui::Button::new("Refresh"))
                    .on_hover_text("Read trading access again now (cached for an hour otherwise)")
                    .clicked()
                {
                    self.trading.caps_retry_at = None;
                    self.trading.caps_loaded = true;
                    if demo_mode {
                        // Not used while the demo account is selected, but the user asked.
                        self.trading.caps_inflight = Some(Instant::now());
                        self.worker
                            .send(Request::Trade(TradeRequest::Capabilities { creds: self.broker.creds.clone() }));
                    } else {
                        self.ensure_capabilities(true);
                    }
                }
                if self.trading.caps_inflight.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        }
        note(
            ui,
            "Connections made for portfolio tracking are read-only. To trade, reconnect the broker through SnapTrade's Connection Portal with trading access (supported brokers only).",
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(live && !self.trading.portal_busy, egui::Button::new("Reconnect with Trading Access…"))
                .on_disabled_hover_text("Set up SnapTrade in Settings › Brokerage first")
                .clicked()
            {
                self.trading.portal_busy = true;
                self.trading.message = None;
                let dark = self.applied_theme.as_ref().is_none_or(|t| t.dark);
                self.worker.send(Request::Trade(TradeRequest::Portal { creds: self.broker.creds.clone(), dark }));
            }
            if self.trading.portal_busy {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
        if let Some((m, err)) = &self.trading.message {
            ui.label(RichText::new(m).font(ui_font(12.0)).color(if *err { theme::down() } else { theme::up() }));
        }

        section(ui, "Using it");
        let keys = |a| self.prefs.hotkeys.chord(a).map_or(String::new(), |c| c.display());
        note(
            ui,
            &format!(
                "{} buys and {} sells the security in focus. Buy / Sell buttons sit on charts, watchlist rows and positions (with Close).",
                keys(crate::prefs::Action::Buy),
                keys(crate::prefs::Action::Sell)
            ),
        );
        note(ui, "Commands (or ⌘K): BUY AAPL 10 · SELL ALL TSLA · BUY NVDA $500 · SELL MSFT 5 LMT 410 · DEMO · LIVE");
    }
}
