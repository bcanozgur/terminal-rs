//! Hotkeys, the periodic tick and autosave.

use super::*;

impl TerminalApp {
    // ---- input ---------------------------------------------------------------------------

    /// Global and chart hotkeys. Runs before the command line so it can claim keys; when
    /// the command line holds text, keys belong to it.
    pub(super) fn hotkeys(&mut self, ctx: &egui::Context) {
        let presses: Vec<(Key, Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    Event::Key { key, pressed: true, repeat: false, modifiers, .. } if !prefs::is_modifier(*key) => {
                        Some((*key, *modifiers))
                    }
                    _ => None,
                })
                .collect()
        });
        // Settings is capturing a new binding: the next key press is it.
        if let Some(action) = self.rebinding {
            if let Some(&(key, m)) = presses.first() {
                ctx.input_mut(|i| i.consume_key(m, key));
                self.rebinding = None;
                if key != Key::Escape {
                    let chord = prefs::Chord::from_event(key, m);
                    let displaced = self.prefs.hotkeys.bind(action, chord);
                    self.status = match displaced {
                        Some(d) => {
                            format!("{} bound to {} (removed from {})", action.label(), chord.display(), d.label())
                        }
                        None => format!("{} bound to {}", action.label(), chord.display()),
                    };
                    self.save_prefs();
                }
            }
            return;
        }
        // ⌘K (Ctrl+K) opens and closes the palette from anywhere, even from a text field.
        if let Some(&(key, m)) =
            presses.iter().find(|(k, m)| self.prefs.hotkeys.action_for(*k, *m) == Some(prefs::Action::CommandPalette))
        {
            ctx.input_mut(|i| i.consume_key(m, key));
            self.palette.toggle();
            return;
        }
        // While it is open, the palette owns the keyboard.
        if self.palette.open {
            return;
        }
        let cmd_id = egui::Id::new("command-line");
        let focused = ctx.memory(|m| m.focused());
        let cmd_focused = focused == Some(cmd_id);
        let other_text_focused = focused.is_some() && !cmd_focused;
        if other_text_focused {
            return;
        }
        // Chorded bindings (⌘, Ctrl, Alt) work even while the command line has text.
        let mut swallow_text = false;
        for &(key, m) in &presses {
            let Some(action) = self.prefs.hotkeys.action_for(key, m) else { continue };
            let chorded = m.command || m.ctrl || m.alt;
            if !chorded && !self.cmd.is_empty() {
                continue;
            }
            // ⇧B / ⇧S type letters: never while the command line is being typed into.
            let letter = !chorded && Self::types_text(key);
            if letter && cmd_focused {
                continue;
            }
            if !self.hotkey_applies(action) {
                continue;
            }
            ctx.input_mut(|i| i.consume_key(m, key));
            swallow_text |= letter;
            self.run_hotkey(action);
        }
        if !cmd_focused && !swallow_text {
            // Type-anywhere: capture keystrokes until the command line has focus.
            // Judge modifiers per key event: the Alt release can arrive in the same frame
            // as Alt+T, so the frame-level modifier state is unreliable.
            let typed: String = ctx.input(|i| {
                let mut chord = false;
                let mut out = String::new();
                for e in &i.events {
                    match e {
                        Event::Key { pressed: true, modifiers, .. } => {
                            chord = modifiers.alt || modifiers.ctrl || modifiers.command
                        }
                        Event::Text(t) if !chord => out.push_str(t),
                        _ => {}
                    }
                }
                out
            });
            if !typed.is_empty() {
                let starts = !self.cmd.is_empty()
                    || typed.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '^' || c == '?');
                if starts {
                    self.cmd.push_str(&typed.to_ascii_uppercase());
                    self.focus_cmd = true;
                }
            }
        }
        if !self.cmd.is_empty() || !self.focused_is_chart() {
            return;
        }
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if pressed(Modifiers::NONE, Key::Escape) {
            self.with_chart(|c, _| {
                c.cancel();
            });
        }
        if pressed(Modifiers::NONE, Key::Delete) || pressed(Modifiers::NONE, Key::Backspace) {
            self.with_chart(|c, _| {
                c.delete_selected();
            });
        }
        if pressed(Modifiers::NONE, Key::ArrowLeft) {
            self.with_chart(|c, _| c.pan_bars(-5.0));
        }
        if pressed(Modifiers::NONE, Key::ArrowRight) {
            self.with_chart(|c, _| c.pan_bars(5.0));
        }
        if pressed(Modifiers::COMMAND, Key::ArrowUp)
            || pressed(Modifiers::NONE, Key::Plus)
            || pressed(Modifiers::NONE, Key::Equals)
        {
            self.with_chart(|c, _| c.zoom(1.25));
        }
        if pressed(Modifiers::COMMAND, Key::ArrowDown) || pressed(Modifiers::NONE, Key::Minus) {
            self.with_chart(|c, _| c.zoom(0.8));
        }
    }

    /// Keys that also produce text (a bare or shifted letter, digit or symbol).
    pub(super) fn types_text(key: Key) -> bool {
        let n = key.name();
        n.len() == 1 || matches!(key, Key::Space | Key::Minus | Key::Plus | Key::Equals | Key::Period | Key::Comma)
    }

    pub(super) fn hotkey_applies(&self, a: prefs::Action) -> bool {
        use prefs::Action as A;
        match a {
            A::DrawTrendline
            | A::DrawHorizontal
            | A::DrawVertical
            | A::DrawFib
            | A::DrawRectangle
            | A::DrawRay
            | A::DrawChannel
            | A::DrawText
            | A::DrawMeasure
            | A::DrawLong
            | A::DrawShort
            | A::ResetView
            | A::LogScale
            | A::Magnet => self.focused_is_chart(),
            a => a.layout_slot().is_none_or(|i| i < self.layout_names.len()),
        }
    }

    pub(super) fn run_hotkey(&mut self, a: prefs::Action) {
        use prefs::Action as A;
        match a {
            A::Help => self.apply(Function::Help),
            A::Congress => self.apply(Function::Congress),
            A::Politicians => self.apply(Function::Politician(String::new())),
            A::Alerts => self.apply(Function::Alerts),
            A::Watchlist => self.apply(Function::Watchlist),
            A::Refresh => self.apply(Function::Refresh),
            A::FrameTiming => self.show_perf = !self.show_perf,
            A::Portfolio => self.apply(Function::Portfolio(command::PortView::Summary, String::new())),
            A::Positions => self.apply(Function::Portfolio(command::PortView::Positions, String::new())),
            A::Orders => self.apply(Function::Portfolio(command::PortView::Orders, String::new())),
            A::Buy => self.quick_trade(true),
            A::Sell => self.quick_trade(false),
            A::NextPane => self.cycle_focus(1),
            A::PrevPane => self.cycle_focus(-1),
            A::ToggleSidebar => self.sidebar_open = !self.sidebar_open,
            A::NextTheme => self.cycle_theme(1),
            A::SwapSidebarSide => self.apply(Function::Sidebar("SWAP".into())),
            A::SplitRight => self.apply(Function::Split { down: false, panel: String::new() }),
            A::SplitDown => self.apply(Function::Split { down: true, panel: String::new() }),
            A::NewTab => {
                let target = self.focused_visible();
                let kind = self.panel_from_spec("", None).expect("chart");
                let id = self.new_pane(kind);
                self.dock_pane(id, target, crate::dock::Zone::Center);
            }
            A::ClosePane => self.close_pane(self.focus),
            A::PopOut => self.pop_out(self.focus),
            A::SaveLayout => {
                self.save_layout();
                self.status = format!("Saved layout {:?}", self.layout_name);
            }
            A::Settings => self.apply(Function::Settings),
            A::CommandPalette => self.palette.toggle(),
            A::DrawTrendline => self.with_chart(|c, _| c.start_tool(Tool::Trendline)),
            A::DrawHorizontal => self.with_chart(|c, _| c.start_tool(Tool::Horizontal)),
            A::DrawVertical => self.with_chart(|c, _| c.start_tool(Tool::Vertical)),
            A::DrawFib => self.with_chart(|c, _| c.start_tool(Tool::Fib)),
            A::DrawRectangle => self.with_chart(|c, _| c.start_tool(Tool::Rectangle)),
            A::DrawRay => self.with_chart(|c, _| c.start_tool(Tool::Ray)),
            A::DrawChannel => self.with_chart(|c, _| c.start_tool(Tool::Channel)),
            A::DrawText => self.with_chart(|c, _| c.start_tool(Tool::Text)),
            A::DrawMeasure => self.with_chart(|c, _| c.start_tool(Tool::Measure)),
            A::DrawLong => self.with_chart(|c, _| c.start_tool(Tool::Long)),
            A::DrawShort => self.with_chart(|c, _| c.start_tool(Tool::Short)),
            A::ResetView => self.with_chart(|c, _| c.reset_view()),
            A::LogScale => self.with_chart(|c, _| c.log = !c.log),
            A::Magnet => self.with_chart(|c, _| c.magnet = !c.magnet),
            a => {
                if let Some(spec) = a.panel_spec() {
                    self.apply(Function::NewPanel(spec.into()));
                } else if let Some(name) = a.layout_slot().and_then(|i| self.layout_names.get(i)).cloned() {
                    self.switch_layout(&name);
                }
            }
        }
    }

    pub(super) fn periodic(&mut self) {
        let visible = self.visible_panes();
        let shown = |f: &dyn Fn(&PaneKind) -> bool| visible.iter().any(|&i| f(&self.panes[i].kind));
        let visible_wl = self.sidebar_open || shown(&|k| matches!(k, PaneKind::Watchlist | PaneKind::List(_)));
        if self.last_quotes.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            let mut keys = Vec::new();
            if visible_wl {
                keys = self.watchlist.clone();
                let list_shown = |name: &str| shown(&|k| matches!(k, PaneKind::List(n) if n == name));
                for g in self.groups.iter().filter(|g| (g.open && self.sidebar_open) || list_shown(&g.name)) {
                    keys.extend(g.keys.iter().filter(|k| !keys.contains(k)).cloned().collect::<Vec<_>>());
                }
            }
            // Charted securities get quotes too, for the source and delay tag in the legend.
            for &i in &visible {
                if let PaneKind::Chart(c) = &self.panes[i].kind
                    && !keys.contains(&c.symbol)
                {
                    keys.push(c.symbol.clone());
                }
            }
            if !keys.is_empty() {
                self.worker.send(Request::Quotes(keys));
                self.last_quotes = Some(Instant::now());
            }
        }
        if shown(&|k| matches!(k, PaneKind::Wei)) && self.last_wei.is_none_or(|t| t.elapsed() > Duration::from_secs(10))
        {
            self.worker.send(Request::Quotes(markets_view::overview_keys()));
            self.last_wei = Some(Instant::now());
        }
        if shown(&|k| matches!(k, PaneKind::Curve))
            && self.last_curve.is_none_or(|t| t.elapsed() > Duration::from_secs(600))
        {
            self.worker.send(Request::YieldCurves);
            self.last_curve = Some(Instant::now());
        }
        self.wl_periodic(&visible);
        // News every 5 minutes and order books every second, while visible.
        for &i in &visible {
            let p = &mut self.panes[i];
            match &mut p.kind {
                PaneKind::News(n) if n.fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) => {
                    n.fetched = Some(Instant::now());
                    self.worker.send(Request::News { pane: p.id, symbol: n.symbol.clone() });
                }
                PaneKind::Options(o) if o.fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) => {
                    o.fetched = Some(Instant::now());
                    self.worker.send(Request::Options {
                        pane: p.id,
                        symbol: o.symbol.clone(),
                        expiration: o.next_expiration(),
                    });
                }
                PaneKind::OrderBook(b) if b.fetched.is_none_or(|t| t.elapsed() > Duration::from_millis(1000)) => {
                    b.fetched = Some(Instant::now());
                    self.worker.send(Request::OrderBook { pane: p.id, symbol: b.symbol.clone(), depth: b.depth });
                }
                PaneKind::Edgar(e) => {
                    if e.due(&self.prefs.panels.sec_user_agent) {
                        e.fetched = Some(Instant::now());
                        e.loading = true;
                        if e.bist_code().is_some() {
                            self.worker.send(Request::Bist { pane: p.id, symbol: e.symbol.clone(), usd: e.usd });
                        } else {
                            let user_agent = self.prefs.panels.sec_user_agent.clone();
                            self.worker.send(Request::Edgar { pane: p.id, symbol: e.symbol.clone(), user_agent });
                        }
                    }
                    if e.quote_due() {
                        e.quoted = Some(Instant::now());
                        self.worker.send(Request::Quotes(vec![tr_data::security(&e.symbol).key()]));
                    }
                }
                _ => {}
            }
        }
        self.broker_periodic(&visible);
        self.earnings_periodic(&visible);
        self.eco_periodic(&visible);
        self.etf_periodic(&visible);
        self.trf_periodic(&visible);
        self.top_periodic(&visible);
        self.most_periodic(&visible);
        self.fut_periodic(&visible);
        self.smart_periodic(&visible);
        if let Some((q, at)) = &self.search_pending
            && at.elapsed() > Duration::from_millis(300)
        {
            self.worker.send(Request::Search(q.clone()));
            self.search_pending = None;
        }
        if self.last_feed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            self.worker.send(Request::Latest { limit: 2_000 });
            self.last_feed = Some(Instant::now());
        }
        self.save_dirty_themes(false);
        if self.last_save.elapsed() > Duration::from_secs(10) {
            self.autosave();
            self.last_save = Instant::now();
        }
        self.toasts.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
    }

    /// Save the active layout (only when it changed) and app settings.
    pub(super) fn autosave(&mut self) {
        self.save_dirty_themes(true);
        let json = self.current_doc().to_json();
        if json != self.last_saved {
            self.save_layout();
        }
        self.save_settings();
    }
}
