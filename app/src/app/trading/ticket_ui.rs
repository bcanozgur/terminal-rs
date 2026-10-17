//! The order ticket window and its form.

use super::*;

impl TerminalApp {
    pub(super) fn ticket_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let live = !self.demo_mode();
        let title = if live { "Order Ticket · LIVE" } else { "Order Ticket · Demo" };
        let mut act = None;
        let frame = egui::Frame::window(&ctx.global_style()).stroke(egui::Stroke::new(
            if live { 2.0 } else { 1.0 },
            if live { theme::down() } else { theme::border() },
        ));
        egui::Window::new(RichText::new(title).font(ui_font(13.0)).color(if live { theme::down() } else { text() }))
            .id(egui::Id::new("order-ticket"))
            .frame(frame)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(dp(320.0))
            .default_pos(ctx.content_rect().center() - egui::vec2(dp(160.0), dp(220.0)))
            .show(ctx, |ui| act = self.ticket_ui(ui));
        self.sync_sell_all();
        if !open
            || (ctx.input(|i| i.key_pressed(egui::Key::Escape))
                && !self.trading.ticket.as_ref().is_some_and(|t| t.confirming))
        {
            self.trading.ticket = None;
            return;
        }
        match act {
            Some(TicketAct::Submit { confirmed }) => self.ticket_submit(confirmed),
            Some(TicketAct::EnableLive) => {
                self.trading.enable_prompt = true;
                self.trading.ack = false;
            }
            Some(TicketAct::Mode(m)) => self.set_account_mode(m, false),
            None => {}
        }
    }

    fn ticket_ui(&mut self, ui: &mut Ui) -> Option<TicketAct> {
        let demo_mode = self.demo_mode();
        let enabled = demo_mode || self.trading_enabled();
        let accounts = self.trade_accounts();
        let pf_loading = !demo_mode && self.broker.loading && self.broker.portfolio.is_none();
        let quote = self.trading.ticket.as_ref().and_then(|t| self.ticket_quote(t));
        let change = self
            .trading
            .ticket
            .as_ref()
            .and_then(|t| t.market_key())
            .and_then(|k| self.quotes.get(&k))
            .map(|q| q.change_pct());
        let caps: HashMap<String, Option<Capability>> =
            accounts.iter().map(|a| (a.id.clone(), self.capability(&a.id))).collect();
        let pending_note = self.caps_pending_note();
        let simulated_broker = self.broker.mode() == BrokerMode::Simulated;
        // The demo estimate (cost, buying power, refusals) from the book itself.
        let demo_check = self
            .trading
            .ticket
            .as_ref()
            .filter(|t| t.account.as_deref() == Some(demo::ACCOUNT_ID))
            .map(|t| t.request().and_then(|r| self.demo.book.estimate(&r, quote).map(|e| (r, e))));
        let held = self
            .trading
            .ticket
            .as_ref()
            .and_then(|t| Some((t.account.clone()?, t.symbol.trim().to_string())))
            .and_then(|(a, s)| self.held_units(&a, &s));
        let demo_available = self.trading.ticket.as_ref().map(|t| self.demo.book.available(t.symbol.trim()));
        let demo_bp = self.demo.book.buying_power();
        let save_error = self.demo.save_error.clone();
        // Pre-trade checks of the order as it stands (blocks, warnings, kill switch).
        let checks = self.trading.ticket.as_ref().and_then(|t| t.request().ok()).map(|r| self.pretrade(&r, quote));
        let kill = self.kill_on();
        let t = self.trading.ticket.as_mut()?;
        let mut act = None;
        let mut enter = false;
        ui.spacing_mut().item_spacing.y = dp(6.0);
        let busy = t.busy();

        // Account: a DEMO / LIVE switch, then the account itself.
        ui.horizontal(|ui| {
            let sel = Some(usize::from(!demo_mode));
            if let Some(i) = widgets::segmented(ui, "ticket-mode", &["DEMO", "LIVE"], sel, 22.0)
                && !busy
            {
                act = Some(TicketAct::Mode(if i == 0 { AccountMode::Demo } else { AccountMode::Live }));
            }
            if demo_mode {
                ui.label(RichText::new("Paper trading · virtual cash").font(ui_font(12.0)).color(muted()));
            } else {
                ui.label(RichText::new("Real money").font(ui_font(12.0)).strong().color(theme::down()));
                if pf_loading {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            }
        });
        if !demo_mode {
            let current = t
                .account
                .as_ref()
                .and_then(|id| accounts.iter().find(|a| &a.id == id))
                .map_or("Choose an account…".to_string(), account_line);
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("ticket-account")
                    .width(ui.available_width())
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for a in &accounts {
                            let cap = caps.get(&a.id).cloned().flatten();
                            let read_only = cap.as_ref().is_some_and(|c| !c.can_trade());
                            let label = format!(
                                "{}{}{}",
                                account_line(a),
                                if read_only { " · read-only" } else { "" },
                                a.buying_power
                                    .or(a.cash)
                                    .map_or(String::new(), |c| format!(" · {} available", money(c, &a.currency)))
                            );
                            ui.selectable_value(&mut t.account, Some(a.id.clone()), label);
                        }
                    });
            });
            if accounts.is_empty() && !pf_loading {
                ui.label(
                    RichText::new("No live accounts loaded: connect one in Settings › Brokerage.")
                        .font(ui_font(11.5))
                        .color(muted()),
                );
            }
            if simulated_broker {
                ui.label(
                    RichText::new("Offline sample portfolio: orders are simulated locally.")
                        .font(ui_font(11.5))
                        .color(theme::cyan()),
                );
            }
        }
        let blocker = match t.account.as_ref().map(|id| caps.get(id).cloned().flatten()) {
            Some(Some(c)) => c.blocker(),
            Some(None) if !demo_mode && self.broker.mode() == BrokerMode::Live => Some(pending_note.clone()),
            Some(None) if !demo_mode => Some("No connection found for this account; refresh with PORT REFRESH.".into()),
            _ => None,
        };
        if let Some(b) = &blocker {
            ui.label(RichText::new(b).font(ui_font(11.5)).color(amber()));
        }
        if !demo_mode && !enabled {
            egui::Frame::new().fill(theme::accent_wash()).corner_radius(4.0).inner_margin(dp(6.0)).show(ui, |ui| {
                ui.label(
                    RichText::new("Live orders are off. Enable them to send orders to your broker.")
                        .font(ui_font(12.0))
                        .color(amber()),
                );
                if ui.button("Enable Live Trading…").clicked() {
                    act = Some(TicketAct::EnableLive);
                }
            });
        }

        // Side: two large buttons.
        ui.horizontal(|ui| {
            let w = (ui.available_width() - dp(6.0)) / 2.0;
            for (side, color) in [(Side::Buy, theme::up()), (Side::Sell, theme::down())] {
                let on = t.side == side;
                let b = if on {
                    egui::Button::new(RichText::new(side.label()).strong().color(Color32::WHITE)).fill(color)
                } else {
                    egui::Button::new(RichText::new(side.label()).color(color))
                };
                if ui.add_enabled(!busy, b.min_size(egui::vec2(w, dp(26.0)))).clicked() {
                    t.side = side;
                }
            }
        });

        let label = |ui: &mut Ui, s: &str| {
            ui.add_sized([dp(64.0), dp(20.0)], egui::Label::new(RichText::new(s).font(ui_font(12.0)).color(muted())));
        };
        // Symbol and last price.
        ui.horizontal(|ui| {
            label(ui, "Symbol");
            let r = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut t.symbol).hint_text("AAPL").font(mono(13.0)).desired_width(dp(84.0)),
            );
            if r.changed() {
                t.symbol = t.symbol.to_ascii_uppercase();
            }
            enter |= r.lost_focus() && fresh_enter(ui);
            match quote {
                Some(q) => {
                    ui.label(RichText::new(price(q)).font(mono(12.5)).color(text()));
                    if let Some(c) = change {
                        let col = if c >= 0.0 { theme::up() } else { theme::down() };
                        ui.label(RichText::new(format!("{c:+.2}%")).font(mono(11.5)).color(col));
                    }
                }
                None if !t.symbol.trim().is_empty() => {
                    ui.label(RichText::new("quote…").font(ui_font(11.5)).color(muted()));
                }
                None => {}
            }
        });

        // Size: shares or dollars, with Max / Sell all helpers.
        ui.horizontal(|ui| {
            label(ui, if t.dollars { "Amount" } else { "Quantity" });
            let r = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut t.qty)
                    .hint_text(if t.dollars { "$" } else { "shares" })
                    .font(mono(13.0))
                    .desired_width(dp(84.0)),
            );
            if t.focus_qty > 0 {
                t.focus_qty -= 1;
                if r.has_focus() {
                    t.focus_qty = 0;
                } else {
                    ui.ctx().memory_mut(|m| m.surrender_focus(egui::Id::new("command-line")));
                    r.request_focus();
                }
            }
            enter |= r.lost_focus() && fresh_enter(ui);
            let sel = Some(usize::from(t.dollars));
            if let Some(i) = widgets::segmented(ui, "ticket-size", &["Shares", "$"], sel, 20.0)
                && !busy
            {
                t.dollars = i == 1;
                if t.dollars {
                    t.order_type = OrderType::Market;
                    t.tif = TimeInForce::Day;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(dp(70.0));
            let sellable = if demo_mode { demo_available } else { held }.filter(|u| *u > 0.0);
            if let Some(units) = held.filter(|u| *u != 0.0) {
                ui.label(RichText::new(format!("Holding {}", qty(units))).font(ui_font(11.5)).color(muted()));
            }
            if t.side == Side::Sell
                && let Some(u) = sellable
                && !busy
                && ui.small_button("Sell all").clicked()
            {
                t.dollars = false;
                t.qty = fmt_input(u);
            }
            if t.side == Side::Buy && demo_mode && !busy {
                let px = t.limit.trim().parse::<f64>().ok().filter(|_| t.order_type.needs_limit()).or(quote);
                if let Some(px) = px.filter(|p| *p > 0.0)
                    && ui.small_button("Max").on_hover_text("As many whole shares as buying power allows").clicked()
                {
                    t.dollars = false;
                    t.qty = fmt_input((demo_bp / px).floor());
                }
            }
        });

        // Market / Limit, and the stop types behind "More".
        ui.horizontal(|ui| {
            label(ui, "Type");
            let basic = ["Market", "Limit"];
            let sel = match t.order_type {
                OrderType::Market => Some(0),
                OrderType::Limit => Some(1),
                _ => None,
            };
            if let Some(i) = widgets::segmented(ui, "ticket-type", &basic, sel, 20.0)
                && !busy
            {
                t.order_type = if i == 0 { OrderType::Market } else { OrderType::Limit };
                if t.order_type == OrderType::Limit {
                    t.dollars = false;
                    if t.limit.trim().is_empty()
                        && let Some(q) = quote
                    {
                        t.limit = fmt_input((q * 100.0).round() / 100.0);
                    }
                }
            }
            if ui
                .selectable_label(t.advanced, RichText::new("More").font(ui_font(11.5)))
                .on_hover_text("Stop orders and time in force")
                .clicked()
            {
                t.advanced = !t.advanced;
            }
        });
        if t.advanced {
            ui.horizontal(|ui| {
                label(ui, "");
                let adv = ["Stop", "Stop Limit"];
                let sel = match t.order_type {
                    OrderType::Stop => Some(0),
                    OrderType::StopLimit => Some(1),
                    _ => None,
                };
                if let Some(i) = widgets::segmented(ui, "ticket-type-adv", &adv, sel, 20.0)
                    && !busy
                {
                    t.order_type = if i == 0 { OrderType::Stop } else { OrderType::StopLimit };
                    t.dollars = false;
                }
            });
        }
        if t.order_type.needs_stop() {
            ui.horizontal(|ui| {
                label(ui, "Stop");
                let r = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut t.stop)
                        .hint_text("trigger")
                        .font(mono(13.0))
                        .desired_width(dp(84.0)),
                );
                enter |= r.lost_focus() && fresh_enter(ui);
            });
        }
        if t.order_type.needs_limit() {
            ui.horizontal(|ui| {
                label(ui, "Limit");
                let r = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut t.limit)
                        .hint_text("price")
                        .font(mono(13.0))
                        .desired_width(dp(84.0)),
                );
                enter |= r.lost_focus() && fresh_enter(ui);
                if let Some(q) = quote
                    && !busy
                    && ui.small_button("Last").on_hover_text("Use the last price").clicked()
                {
                    t.limit = fmt_input(q);
                }
            });
        }
        if t.advanced {
            ui.horizontal(|ui| {
                label(ui, "Time");
                let labels: Vec<&str> = TimeInForce::ALL.iter().map(|x| x.label()).collect();
                let sel = TimeInForce::ALL.iter().position(|x| *x == t.tif);
                if let Some(i) = widgets::segmented(ui, "ticket-tif", &labels, sel, 20.0)
                    && !busy
                {
                    t.tif = TimeInForce::ALL[i];
                }
            })
            .response
            .on_hover_text(t.tif.describe());
        }

        // Live estimate: cost or proceeds against buying power.
        ui.separator();
        let ready = t.request();
        let account = t.account.as_ref().and_then(|id| accounts.iter().find(|a| &a.id == id));
        let ccy = account.map_or("USD".to_string(), |a| a.currency.clone());
        let row = |ui: &mut Ui, k: &str, v: String, strong: bool| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [dp(110.0), dp(16.0)],
                    egui::Label::new(RichText::new(k).font(ui_font(11.5)).color(muted())),
                );
                let rt = RichText::new(v).font(mono(if strong { 13.0 } else { 12.0 })).color(text());
                ui.label(if strong { rt.strong() } else { rt });
            });
        };
        let mut problem: Option<String> = None;
        match (&demo_check, &ready) {
            (Some(Ok((r, e))), _) => {
                let what = if r.side == Side::Buy { "Est. cost" } else { "Est. proceeds" };
                row(ui, what, money(e.value, "USD"), true);
                if matches!(r.amount, Amount::Notional(_)) {
                    row(ui, "Est. shares", qty(e.units), false);
                }
                match r.side {
                    Side::Buy => row(
                        ui,
                        "Buying power",
                        format!("{} → {}", money(e.buying_power, "USD"), money(e.buying_power - e.value, "USD")),
                        false,
                    ),
                    Side::Sell => row(ui, "Can sell", format!("{} {}", qty(e.available), r.symbol), false),
                }
                if r.side == Side::Buy && e.value > e.buying_power + 1e-6 {
                    problem = Some(format!("Not enough buying power ({} available).", money(e.buying_power, "USD")));
                } else if r.side == Side::Sell && e.units > e.available + 1e-6 {
                    problem = Some(if e.available <= 0.0 {
                        format!("You don't hold {} to sell (no short selling in the demo account).", r.symbol)
                    } else {
                        format!("You can sell up to {} {}.", qty(e.available), r.symbol)
                    });
                }
            }
            (Some(Err(e)), _) => {
                row(ui, "Buying power", money(demo_bp, "USD"), false);
                if ready.is_ok() {
                    problem = Some(e.clone());
                }
            }
            (None, Ok(r)) => {
                let px = r.reference_price(quote);
                let value = match r.amount {
                    Amount::Notional(n) => Some(n),
                    Amount::Units(u) => px.map(|p| u * p),
                };
                let what = if r.side == Side::Buy { "Est. cost" } else { "Est. proceeds" };
                row(ui, what, value.map_or("at market".into(), |v| money(v, &ccy)), true);
                if let Some(bp) = account.and_then(|a| a.buying_power.or(a.cash)) {
                    let after = match (r.side, value) {
                        (Side::Buy, Some(v)) => format!(" → {}", money(bp - v, &ccy)),
                        _ => String::new(),
                    };
                    row(ui, "Buying power", format!("{}{after}", money(bp, &ccy)), false);
                }
            }
            (None, Err(_)) => {
                if let Some(bp) = account.and_then(|a| a.buying_power.or(a.cash)) {
                    row(ui, "Buying power", money(bp, &ccy), false);
                }
            }
        }
        if let Some(p) = &problem {
            ui.label(RichText::new(p).font(ui_font(11.5)).color(amber()));
        }
        if let Some(e) = &t.error {
            ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
        }
        if kill {
            ui.label(
                RichText::new("KILL SWITCH ON: new orders are blocked (KILL OFF releases it).")
                    .font(ui_font(12.0))
                    .color(theme::down()),
            );
        }
        let mut blocked = false;
        let mut warned = false;
        if let Some(a) = &checks {
            for f in &a.findings {
                let (tag, color) = match f.verdict {
                    crate::safety::Verdict::Block => ("Blocked", theme::down()),
                    _ => ("Warning", amber()),
                };
                match f.verdict {
                    crate::safety::Verdict::Block => blocked = true,
                    _ => warned = true,
                }
                // The kill switch has its own line above.
                if f.check != "kill switch" {
                    ui.label(RichText::new(format!("{tag}: {}", f.reason)).font(ui_font(11.5)).color(color));
                }
            }
        }
        if demo_mode && let Some(e) = &save_error {
            ui.label(
                RichText::new(format!("The demo account could not be saved ({e}); retrying."))
                    .font(ui_font(11.5))
                    .color(theme::down()),
            );
        }
        if let Some(d) = &t.done {
            ui.label(RichText::new(d).font(ui_font(12.0)).color(theme::up()));
        }

        // The one button.
        let fill = if t.side == Side::Buy { theme::up() } else { theme::down() };
        let label = match (&ready, demo_mode) {
            (Ok(r), true) => {
                let size = match r.amount {
                    Amount::Units(u) => format!("{} {}", trading::fmt_units(u), r.symbol),
                    Amount::Notional(n) => format!("${n:.2} of {}", r.symbol),
                };
                match r.order_type {
                    OrderType::Market => format!("{} {size}", r.side.label()),
                    o => format!("Place {} {} {size}", r.side.label().to_lowercase(), o.label().to_lowercase()),
                }
            }
            (_, true) => format!("{} (demo)", t.side.label()),
            (_, false) => format!("Review {}…", t.side.label()),
        };
        let label = if warned && !blocked { format!("Place anyway: {label}") } else { label };
        let can = enabled && ready.is_ok() && blocker.is_none() && problem.is_none() && !busy && !blocked;
        ui.horizontal(|ui| {
            let b = filled(&label, fill, can).min_size(egui::vec2(ui.available_width() - dp(24.0), dp(30.0)));
            let reason = match &ready {
                Err(e) => e.clone(),
                Ok(_) if !enabled => "Live orders are off".into(),
                Ok(_) if blocked => "Blocked by a pre-trade check (see above)".into(),
                Ok(_) => blocker.clone().or(problem.clone()).unwrap_or_else(|| "Working…".into()),
            };
            let tip = if demo_mode {
                "Simulated instantly with virtual cash (Enter)"
            } else {
                "Your broker checks the order and estimates cost; then one confirmation sends it"
            };
            if ui.add_enabled(can, b).on_hover_text(tip).on_disabled_hover_text(reason).clicked() {
                act = Some(TicketAct::Submit { confirmed: warned });
            }
            if busy {
                ui.add(egui::Spinner::new().size(14.0));
            }
        });
        // Enter never confirms a warning: that takes a click on 'Place anyway'.
        if enter && can && !warned {
            act = Some(TicketAct::Submit { confirmed: false });
        }
        if busy {
            ui.label(
                RichText::new(if t.placing {
                    "Sending to your broker…"
                } else {
                    "Your broker is checking the order…"
                })
                .font(ui_font(11.5))
                .color(muted()),
            );
        } else if demo_mode {
            ui.label(
                RichText::new("Demo: fills at the live quote with virtual cash. Nothing is sent to a broker.")
                    .font(ui_font(11.0))
                    .color(muted()),
            );
        }
        act
    }
}
