//! Drawing the docked layout: tab groups, drag and drop, and group commands.

use super::*;

impl TerminalApp {
    pub(in crate::app) fn render_dock(&mut self, ui: &mut Ui, area: Rect, gap: f32) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut cmds = Vec::new();
        if self.focused_is_chart() {
            self.last_chart = Some(self.focus);
        }
        if self.dock.is_empty() {
            self.empty_state(ui, area, &mut cmds);
        }
        let (groups, dividers) = self.dock.layout(area, gap);
        for g in &groups {
            self.render_group(ui, g, &mut actions, &mut cmds);
        }
        for d in &dividers {
            let hit = d.rect.expand(2.0);
            let r = ui.interact(hit, Id::new(("divider", &d.path, d.index)), Sense::click_and_drag());
            let icon = match d.dir {
                Dir::Row => CursorIcon::ResizeHorizontal,
                Dir::Column => CursorIcon::ResizeVertical,
            };
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(icon);
                let c = d.rect.center();
                let (a, b) = match d.dir {
                    Dir::Row => (pos2(c.x, d.rect.top() + 6.0), pos2(c.x, d.rect.bottom() - 6.0)),
                    Dir::Column => (pos2(d.rect.left() + 6.0, c.y), pos2(d.rect.right() - 6.0, c.y)),
                };
                ui.painter().line_segment([a, b], Stroke::new(2.0, theme::accent().gamma_multiply(0.7)));
            }
            if r.dragged() {
                let delta = match d.dir {
                    Dir::Row => r.drag_delta().x,
                    Dir::Column => r.drag_delta().y,
                };
                self.dock.resize(&d.path, d.index, delta, d.span);
            }
            if r.double_clicked() {
                self.dock.equalize(&d.path);
            }
        }
        self.drag_and_drop(ui, area, &groups);
        self.apply_group_cmds(cmds);
        actions
    }

    fn empty_state(&mut self, ui: &mut Ui, area: Rect, cmds: &mut Vec<GroupCmd>) {
        let p = ui.painter();
        p.text(area.center() - vec2(0.0, 28.0), Align2::CENTER_CENTER, "No panels", ui_font(16.0), text());
        p.text(
            area.center() - vec2(0.0, 6.0),
            Align2::CENTER_CENTER,
            "Add one below, type a ticker, or switch layouts (LAYOUT <name>)",
            ui_font(12.0),
            muted(),
        );
        let r = Rect::from_center_size(area.center() + vec2(0.0, 28.0), vec2(140.0, 28.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(r));
        child.menu_button("＋ Add Panel", |ui| {
            for (label, spec) in PANELS {
                if ui.button(label).clicked() {
                    cmds.push(GroupCmd::Add { target: 0, zone: Zone::Center, spec });
                    ui.close();
                }
            }
        });
    }

    fn render_group(
        &mut self,
        ui: &mut Ui,
        g: &crate::dock::Group,
        actions: &mut Vec<Action>,
        cmds: &mut Vec<GroupCmd>,
    ) {
        let Some(active) = g.active_pane() else { return };
        let focused = active == self.focus;
        let focus_t = anim::spring_bool(ui.ctx(), Id::new(("group-focus", active)), focused);
        let inner = widgets::card(ui, g.rect, focus_t);
        let th = tab_h();
        let strip = Rect::from_min_size(inner.min, vec2(inner.width(), th));
        let r = theme::CARD_RADIUS.saturating_sub(1);
        let p = ui.painter_at(g.rect);
        p.rect_filled(strip, CornerRadius { nw: r, ne: r, sw: 0, se: 0 }, theme::header());
        p.hline(strip.x_range(), strip.bottom() - 0.5, Stroke::new(1.0, theme::border()));

        // Trailing buttons: add panel (as tab) and the group menu.
        let btn =
            |i: f32| Rect::from_min_size(pos2(strip.right() - 26.0 * i - 2.0, strip.top() + 2.0), vec2(24.0, th - 4.0));
        let searchable = self.idx(active).is_some_and(|i| self.panes[i].kind.search_hint().is_some());
        let info_slot = if searchable { 4.0 } else { 3.0 };
        let syncable = self
            .idx(active)
            .is_some_and(|i| self.panes[i].kind.follows_symbol() && !matches!(self.panes[i].kind, PaneKind::Chart(_)));
        let sync_w = if syncable { 44.0 } else { 0.0 };
        let tabs_right = btn(info_slot).left() - 4.0 - sync_w;
        // A chart's interval and type controls share the tab strip when there is room,
        // which saves the chart a whole header row.
        let chart_idx = self.idx(active).filter(|&i| matches!(self.panes[i].kind, PaneKind::Chart(_)));
        let ctrl_w = if chart_idx.is_some() { crate::chart_view::ChartState::controls_width(ui) } else { 0.0 };
        let min_tabs = 120.0 * g.panes.len().clamp(1, 3) as f32;
        let controls_fit = chart_idx.is_some() && tabs_right - strip.left() - ctrl_w - 10.0 >= min_tabs;
        let controls_left = tabs_right - ctrl_w;
        let tabs_right = if controls_fit { controls_left - 10.0 } else { tabs_right };
        let press = ui.input(|i| if i.pointer.primary_pressed() { i.pointer.press_origin() } else { None });
        let mut x = strip.left() + 4.0;
        let n = g.panes.len().max(1) as f32;
        let max_w = ((tabs_right - x) / n - 2.0).clamp(54.0, 220.0);
        for (ti, &pid) in g.panes.iter().enumerate() {
            let Some(i) = self.idx(pid) else { continue };
            let pane = &self.panes[i];
            let (title, code, link) = (pane.display_title(), pane.kind.code(), pane.link);
            let is_active = ti == g.active;
            let code_g = p.layout_no_wrap(code.to_string(), mono(10.0), amber());
            let title_g = p.layout_no_wrap(title.clone(), ui_font(12.0), text());
            let want = 10.0 + link.map_or(0.0, |_| 10.0) + code_g.size().x + 6.0 + title_g.size().x + 24.0;
            let w = want.min(max_w);
            if x + w > tabs_right + 1.0 && ti > 0 {
                // Out of room: the overflow is reachable from the group menu.
                break;
            }
            let tr = Rect::from_min_size(pos2(x, strip.top() + 3.0), vec2(w, th - 3.0));
            let resp = ui.interact(tr, Id::new(("tab", pid)), Sense::click_and_drag());
            let hover = anim::spring_bool(ui.ctx(), Id::new(("tab-hover", pid)), resp.hovered());
            let tp = ui.painter_at(tr);
            if is_active {
                tp.rect_filled(tr, CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, theme::card());
                if focused {
                    tp.hline(tr.shrink2(vec2(6.0, 0.0)).x_range(), tr.top() + 1.0, Stroke::new(2.0, theme::accent()));
                }
            } else if hover > 0.01 {
                tp.rect_filled(tr.shrink(1.0), 6, theme::control().gamma_multiply(hover * 1.5));
            }
            let mut tx = tr.left() + 8.0;
            if let Some(l) = link {
                tp.circle_filled(pos2(tx + 3.0, tr.center().y), 3.5, l.color());
                tx += 10.0;
            }
            tp.galley(pos2(tx, tr.center().y - code_g.size().y / 2.0), code_g.clone(), amber());
            tx += code_g.size().x + 6.0;
            let fg = if is_active { text() } else { muted() };
            let renaming = self.renaming_tab.as_ref().is_some_and(|(r, _)| *r == pid);
            // Close button.
            let close = Rect::from_center_size(pos2(tr.right() - 11.0, tr.center().y), vec2(16.0, 16.0));
            if !renaming {
                let clip = Rect::from_min_max(pos2(tx, tr.top()), pos2(close.left() - 2.0, tr.bottom()));
                tp.with_clip_rect(clip.intersect(tr)).text(
                    pos2(tx, tr.center().y),
                    Align2::LEFT_CENTER,
                    &title,
                    ui_font(12.0),
                    fg,
                );
            }
            if is_active || resp.hovered() {
                let cr = ui.interact(close, Id::new(("tab-close", pid)), Sense::click());
                if cr.hovered() {
                    tp.rect_filled(close, 4, theme::control_hover());
                }
                let c = close.center();
                let s = Stroke::new(1.3, if cr.hovered() { text() } else { muted() });
                tp.line_segment([c - vec2(3.5, 3.5), c + vec2(3.5, 3.5)], s);
                tp.line_segment([c + vec2(-3.5, 3.5), c + vec2(3.5, -3.5)], s);
                if cr.clicked() {
                    cmds.push(GroupCmd::Close(pid));
                }
            }
            if renaming {
                let edit =
                    Rect::from_min_max(pos2(tx - 2.0, tr.top() + 3.0), pos2(close.left() - 2.0, tr.bottom() - 3.0));
                let mut child = ui.new_child(UiBuilder::new().max_rect(edit));
                if let Some((_, buf)) = &mut self.renaming_tab {
                    let te = child.add(egui::TextEdit::singleline(buf).font(ui_font(12.0)).desired_width(edit.width()));
                    te.request_focus();
                    if te.lost_focus() {
                        let commit = !child.input(|i| i.key_pressed(egui::Key::Escape));
                        cmds.push(GroupCmd::Rename(pid, if commit { buf.clone() } else { "\u{0}".into() }));
                    }
                }
            }
            if resp.clicked() {
                cmds.push(GroupCmd::Activate(pid));
            }
            if resp.middle_clicked() {
                cmds.push(GroupCmd::Close(pid));
            }
            if resp.double_clicked() {
                cmds.push(GroupCmd::StartRename(pid));
            }
            // Drags are tracked from the press itself (see `drag_and_drop`) rather than
            // egui's widget drag state, so quick trackpad gestures whose press and first
            // movement land in the same frame still pick the tab up.
            if !renaming
                && let Some(p) = press
                && tr.contains(p)
                && !close.contains(p)
            {
                self.drag = Some(crate::app::Drag { pane: pid, origin: p, active: false });
            }
            resp.on_hover_text(format!(
                "{title}\nDrag to move or dock · double-click to rename · middle-click to close"
            ))
            .context_menu(|ui| tab_menu(ui, pid, link, cmds));
            x += w + 2.0;
        }

        // The empty part of the header strip drags the active pane too.
        if let Some(p) = press
            && self.drag.is_none()
            && strip.contains(p)
            && p.x < tabs_right
            && self.renaming_tab.is_none()
        {
            self.drag = Some(crate::app::Drag { pane: active, origin: p, active: false });
        }
        let strip_hover = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| strip.contains(p) && p.x < tabs_right);
        if strip_hover && self.drag.is_none() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }

        // Group buttons.
        if syncable {
            let r = Rect::from_min_size(
                pos2(btn(info_slot).left() - 4.0 - sync_w + 4.0, strip.top() + 3.0),
                vec2(sync_w - 4.0, th - 6.0),
            );
            let has_chart = self.active_chart().is_some();
            let mut b =
                ui.new_child(UiBuilder::new().max_rect(r).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let resp = b.add_enabled(has_chart, egui::Button::new(egui::RichText::new("Sync").font(ui_font(11.0))));
            let tip = if has_chart { "Show the active chart's symbol here" } else { "Open a chart to sync from" };
            if resp.on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                cmds.push(GroupCmd::Sync(active));
            }
        }
        if searchable {
            let mut b = ui.new_child(UiBuilder::new().max_rect(btn(3.0)));
            let open = self.pane_search.contains_key(&active);
            let tip = if open { "Close filter (Esc)" } else { "Filter this panel" };
            if widgets::icon_button_tinted(&mut b, Icon::Search, tip, open, theme::amber(), theme::amber_wash())
                .clicked()
            {
                if open {
                    self.pane_search.remove(&active);
                } else {
                    self.pane_search.insert(active, String::new());
                    self.search_focus = Some(active);
                    self.focus = active;
                }
            }
        }
        if let Some(i) = self.idx(active) {
            let mut b = ui.new_child(UiBuilder::new().max_rect(btn(info_slot)));
            let resp = widgets::icon_button(&mut b, Icon::Info, "About this panel", false);
            let about = self.panes[i].kind.about();
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_max_width(320.0);
                ui.label(egui::RichText::new(about).font(ui_font(12.0)).color(text()));
            });
        }
        let plus = btn(2.0);
        let mut b = ui.new_child(UiBuilder::new().max_rect(plus));
        let resp = widgets::icon_button(&mut b, Icon::Plus, "Add Panel as Tab", false);
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Add as Tab").font(ui_font(11.0)).color(muted()));
            for (label, spec) in PANELS {
                if ui.button(label).clicked() {
                    cmds.push(GroupCmd::Add { target: active, zone: Zone::Center, spec });
                    ui.close();
                }
            }
        });
        let more = btn(1.0);
        let mut b = ui.new_child(UiBuilder::new().max_rect(more));
        let resp = widgets::icon_button(&mut b, Icon::More, "Pane Options", false);
        let link = self.idx(active).and_then(|i| self.panes[i].link);
        let tabs: Vec<(PaneId, String)> =
            g.panes.iter().filter_map(|&id| self.idx(id).map(|i| (id, self.panes[i].display_title()))).collect();
        egui::Popup::menu(&resp).show(|ui| {
            if tabs.len() > 1 {
                ui.menu_button("Tabs", |ui| {
                    for (id, t) in &tabs {
                        if ui.radio(*id == active, t).clicked() {
                            cmds.push(GroupCmd::Activate(*id));
                            ui.close();
                        }
                    }
                });
            }
            tab_menu(ui, active, link, cmds);
        });

        if let Some(ci) = chart_idx {
            let worker = &self.worker;
            if let PaneKind::Chart(c) = &mut self.panes[ci].kind {
                c.controls_in_strip = controls_fit;
                if controls_fit {
                    let r = Rect::from_min_size(
                        pos2(controls_left, strip.center().y - crate::chart_view::CONTROL_H / 2.0),
                        vec2(ctrl_w, crate::chart_view::CONTROL_H),
                    );
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(r)
                            .layout(egui::Layout::left_to_right(egui::Align::Center))
                            .id_salt(("strip-controls", active)),
                    );
                    if c.controls(&mut child, worker) {
                        self.focus = active;
                    }
                }
            }
        }

        // Content.
        let content = Rect::from_min_max(pos2(inner.left(), strip.bottom()), inner.max);
        let Some(i) = self.idx(active) else { return };
        let (mut acts, clicked) = self.render_content(ui, i, content, focused);
        // Clicks in a linked list panel drive its link group.
        if let Some(l) = self.panes[i].link
            && !self.panes[i].kind.follows_symbol()
            && self.panes.iter().any(|p| p.link == Some(l) && p.kind.follows_symbol())
        {
            for a in &mut acts {
                if let Action::OpenChart(s) = a {
                    *a = Action::LinkedSymbol(l, std::mem::take(s));
                }
            }
        }
        actions.extend(acts);
        let pressed_here = ui.input(|inp| {
            inp.pointer.primary_pressed() && inp.pointer.interact_pos().is_some_and(|p| g.rect.contains(p))
        });
        if (clicked || pressed_here) && self.drag.is_none_or(|d| !d.active) {
            self.focus = active;
        }
    }

    /// Pane drag and drop. A press on a tab or header starts a pending drag; past a small
    /// threshold it becomes active: a ghost of the pane follows the pointer, the hovered
    /// pane shows a five-way dock compass, and the landing area is highlighted and labelled.
    /// Release drops; Esc or releasing over nothing cancels.
    fn drag_and_drop(&mut self, ui: &mut Ui, area: Rect, groups: &[crate::dock::Group]) {
        let Some(mut drag) = self.drag else { return };
        let (pos, down, esc) =
            ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down(), i.key_pressed(egui::Key::Escape)));
        if esc {
            self.drag = None;
            return;
        }
        if !drag.active {
            // Released before moving: that was a click.
            if !down {
                self.drag = None;
                return;
            }
            if pos.is_some_and(|p| p.distance(drag.origin) > DRAG_THRESHOLD) {
                drag.active = true;
                self.drag = Some(drag);
            } else {
                return;
            }
        }
        let Some(pos) = pos else {
            if !down {
                self.drag = None;
            }
            return;
        };
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let target = drop_target(area, groups, pos, drag.pane);
        let layer = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("dock-drop")));
        let accent = theme::accent();
        // Every pane is a potential target: outline them faintly.
        for g in groups {
            layer.rect_stroke(
                g.rect.shrink(1.0),
                theme::CARD_RADIUS,
                Stroke::new(1.0, accent.gamma_multiply(0.35)),
                StrokeKind::Inside,
            );
        }
        if let Some(t) = &target {
            if let Some(g) = t.group.and_then(|i| groups.get(i)) {
                draw_compass(&layer, g.rect, t.zone, t.noop);
            }
            if !t.noop {
                let r = t.preview.shrink(4.0);
                layer.rect_filled(r, theme::CARD_RADIUS, accent.gamma_multiply(0.20));
                layer.rect_stroke(r, theme::CARD_RADIUS, Stroke::new(2.0, accent), StrokeKind::Inside);
                let label = t.label();
                let lg = layer.layout_no_wrap(label.into(), ui_font(12.5), Color32::WHITE);
                let pill = Rect::from_center_size(pos2(r.center().x, r.top() + 22.0), lg.size() + vec2(18.0, 8.0));
                layer.rect_filled(pill, 8.0, accent);
                layer.galley(pill.min + vec2(9.0, 4.0), lg, Color32::WHITE);
            }
        }
        self.draw_ghost(&layer, drag.pane, pos);
        ui.ctx().request_repaint();
        if !down {
            self.drag = None;
            if let Some(t) = target.filter(|t| !t.noop) {
                self.dock.insert_at(&t.path, t.zone, drag.pane);
                self.focus = drag.pane;
                self.preset = None;
                self.status = format!(
                    "Moved {}: {}",
                    self.idx(drag.pane).map(|i| self.panes[i].display_title()).unwrap_or_default(),
                    t.label()
                );
            }
        }
    }

    /// A translucent miniature of the dragged pane following the pointer.
    fn draw_ghost(&self, layer: &egui::Painter, pane: PaneId, pos: egui::Pos2) {
        let Some(i) = self.idx(pane) else { return };
        let p = &self.panes[i];
        let r = Rect::from_min_size(pos + vec2(22.0, 20.0), vec2(168.0, 96.0));
        layer.rect_filled(r.translate(vec2(0.0, 4.0)), theme::CARD_RADIUS, Color32::from_black_alpha(60));
        layer.rect_filled(r, theme::CARD_RADIUS, theme::card().gamma_multiply(0.92));
        layer.rect_stroke(r, theme::CARD_RADIUS, Stroke::new(1.5, theme::accent()), StrokeKind::Inside);
        let strip = Rect::from_min_size(r.min, vec2(r.width(), 24.0));
        layer.rect_filled(
            strip,
            CornerRadius { nw: theme::CARD_RADIUS, ne: theme::CARD_RADIUS, sw: 0, se: 0 },
            theme::header(),
        );
        let mut x = strip.left() + 8.0;
        if let Some(l) = p.link {
            layer.circle_filled(pos2(x + 3.0, strip.center().y), 3.5, l.color());
            x += 10.0;
        }
        let code = layer.layout_no_wrap(p.kind.code().into(), mono(10.0), amber());
        let cw = code.size().x;
        layer.galley(pos2(x, strip.center().y - code.size().y / 2.0), code, amber());
        layer.with_clip_rect(strip.shrink(2.0)).text(
            pos2(x + cw + 6.0, strip.center().y),
            Align2::LEFT_CENTER,
            p.display_title(),
            ui_font(12.0),
            text(),
        );
        // Placeholder content lines.
        for k in 0..4 {
            let y = strip.bottom() + 12.0 + k as f32 * 15.0;
            let w = [130.0, 100.0, 140.0, 80.0][k];
            layer.rect_filled(
                Rect::from_min_size(pos2(r.left() + 10.0, y), vec2(w, 6.0)),
                3.0,
                theme::muted().gamma_multiply(0.25),
            );
        }
    }

    fn apply_group_cmds(&mut self, cmds: Vec<GroupCmd>) {
        for c in cmds {
            match c {
                GroupCmd::Activate(id) => {
                    self.dock.activate(id);
                    self.focus = id;
                }
                GroupCmd::Close(id) => self.close_pane(id),
                GroupCmd::CloseOthers(id) => {
                    if let Some(path) = self.dock.find(id)
                        && let Some(Node::Tabs { panes, .. }) = self.dock.get(&path)
                    {
                        for other in panes.clone().into_iter().filter(|&p| p != id) {
                            self.close_pane(other);
                        }
                    }
                }
                GroupCmd::PopOut(id) => self.pop_out(id),
                GroupCmd::Add { target, zone, spec } => {
                    if let Some(kind) = self.panel_from_spec(spec, None) {
                        let new = self.new_pane(kind);
                        let t = (target != 0).then_some(target);
                        self.dock_pane(new, t, zone);
                    }
                }
                GroupCmd::Link(id, l) => self.set_link(id, l),
                GroupCmd::Sync(id) => self.sync_to_chart(id),
                GroupCmd::StartRename(id) => {
                    let t = self.idx(id).map(|i| self.panes[i].display_title()).unwrap_or_default();
                    self.renaming_tab = Some((id, t));
                }
                GroupCmd::Rename(id, t) => {
                    self.renaming_tab = None;
                    if t != "\u{0}"
                        && let Some(p) = self.pane_mut(id)
                    {
                        let t = t.trim();
                        p.title = (!t.is_empty() && *t != p.kind.tab_title()).then(|| t.to_string());
                    }
                }
            }
        }
    }
}
