use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;
use zellij_tile::prelude::*;

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Re-dump previews only every Nth timer tick (each tick is 2s). Dumping spawns
/// a process per pane, so we throttle the periodic refresh.
const PREVIEW_REFRESH_EVERY: usize = 3;
/// Cap parsed preview rows/columns so a pathological pane can't blow up memory.
/// We can never display more than this anyway.
const PREVIEW_MAX_ROWS: usize = 200;
const PREVIEW_MAX_COLS: usize = 512;

#[derive(Default)]
struct State {
    sessions: Vec<SessionInfo>,
    resurrectable: Vec<(String, Duration)>,
    selected: usize,
    scroll_offset: usize,
    permissions_granted: bool,
    spinner_idx: usize,
    searching: bool,
    query: String,
    /// Name of a live session awaiting kill confirmation. When `Some`, the UI
    /// shows a confirm prompt and swallows other keys until y/n.
    pending_kill: Option<String>,
    /// New-name buffer while renaming. When `Some`, the rename input is shown
    /// and editing keys are routed to it. Only the attached session can be
    /// renamed (the plugin API's RenameSession targets the current session).
    rename_input: Option<String>,
    /// Transient one-shot status line shown in the footer; cleared on the next
    /// key handled in normal mode.
    notice: Option<String>,
    /// Whether the side preview panel is shown.
    preview_on: bool,
    /// Our floating pane's geometry (x, y, cols, rows) captured before
    /// expanding to full width, so we can restore it when preview is closed.
    saved_geom: Option<(usize, usize, usize, usize)>,
    /// When true, j/k scroll through the selected session's tabs (and the
    /// preview follows) instead of through the session list. Entered with `l`,
    /// left with `h`.
    tab_focus: bool,
    /// Index (into the selected session's tab positions) being previewed while
    /// in tab focus.
    selected_tab: usize,
    /// Cached pane-screen dumps, keyed by (session name, terminal pane id).
    /// Populated asynchronously from `dump-screen` via `RunCommandResult`.
    previews: BTreeMap<(String, u32), Vec<String>>,
    /// Dumps currently in flight, so we don't fire duplicate commands.
    preview_pending: BTreeSet<(String, u32)>,
}

/// One row group in the (possibly filtered) list. Selection, activation and
/// rendering all index into the same `Vec<Entry>` so they can never disagree.
enum Entry {
    /// The "New session" item.
    New,
    /// A live session, by index into `sessions`.
    Live(usize),
    /// A dead/resurrectable session, by index into `resurrectable`.
    Dead(usize),
}

/// A pane's geometry within a tab, normalized so the tab's top-left is (0, 0).
/// Used to composite the preview at 1:1, tmux-style.
#[derive(Clone)]
struct PreviewPane {
    id: u32,
    /// Pane frame top-left, relative to the tab origin.
    rx: usize,
    ry: usize,
    /// Pane frame size (including border).
    cols: usize,
    rows: usize,
    /// Content top-left, relative to the tab origin.
    cx: usize,
    cy: usize,
    /// Content size (inside the border).
    ccols: usize,
    crows: usize,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            // Needed to shell out to `zellij action dump-screen` for previews.
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::SessionUpdate,
            EventType::Key,
            EventType::Timer,
            EventType::PermissionRequestResult,
            EventType::RunCommandResult,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permissions_granted = true;
                // Populate the list immediately rather than waiting for the
                // first timer tick.
                self.refresh_sessions();
                set_timeout(2.0);
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.permissions_granted = false;
                true
            }
            Event::SessionUpdate(sessions, resurrectable) => {
                self.sessions = sessions;
                self.resurrectable = resurrectable;
                self.clamp_selection();
                true
            }
            Event::Timer(_) => {
                // zellij only populates the full peer-session list when a
                // plugin explicitly asks for it via get_session_list(); the
                // passive SessionUpdate event otherwise only ever contains the
                // current session. Poll it on a timer to keep the list fresh.
                let changed = self.refresh_sessions();
                let frame = SPINNER[self.spinner_idx % SPINNER.len()];
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                let plugin_id = get_plugin_ids().plugin_id;
                rename_plugin_pane(plugin_id, format!("Sessioner {}", frame));
                set_timeout(2.0);
                // Keep the visible preview live by re-dumping it — but only
                // every few ticks, since each pane dump spawns a process.
                if self.preview_on && self.spinner_idx.is_multiple_of(PREVIEW_REFRESH_EVERY) {
                    self.request_preview(true);
                }
                changed
            }
            Event::Key(key) => {
                let rerender = self.handle_key(key);
                // After a selection change, lazily fetch the preview if needed.
                if self.preview_on {
                    self.request_preview(false);
                }
                rerender
            }
            Event::RunCommandResult(_exit, stdout, _stderr, context) => {
                self.handle_command_result(stdout, context)
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if !self.permissions_granted {
            print_text_with_coordinates(
                Text::new("Waiting for permissions..."),
                0, 0, Some(cols), None,
            );
            return;
        }

        // Header ribbon
        print_ribbon_with_coordinates(
            Text::new(" Sessioner ").selected(),
            0, 0, Some(cols), Some(1),
        );

        // Prompt drawn to the right of the ribbon. Rename takes precedence
        // over the search prompt (they can't both be active).
        if let Some(buf) = &self.rename_input {
            let label = "rename: ";
            let display = format!("{}{}\u{2588}", label, buf);
            print_text_with_coordinates(
                Text::new(&display).color_range(2, 0..label.chars().count()),
                13, 0, Some(cols.saturating_sub(13)), Some(1),
            );
        } else if self.searching || !self.query.is_empty() {
            // Shown while searching, or whenever a filter is active.
            let prompt = format!("/{}", self.query);
            // Trailing block acts as a cursor while actively typing.
            let display = if self.searching {
                format!("{}\u{2588}", prompt)
            } else {
                prompt
            };
            print_text_with_coordinates(
                Text::new(&display).color_range(2, 0..1),
                13, 0, Some(cols.saturating_sub(13)), Some(1),
            );
        }

        // Footer
        let footer_y = rows.saturating_sub(1);
        let footer = if let Some(name) = &self.pending_kill {
            // Confirmation prompt takes over the footer line.
            let text = format!("Kill session '{}'?  y/n", name);
            let name_start = "Kill session '".chars().count();
            let name_end = name_start + name.chars().count();
            let yn_start = text.chars().count() - 3;
            Text::new(&text)
                .color_range(0, name_start..name_end)
                .color_range(3, yn_start..)
        } else if self.rename_input.is_some() {
            keyhints(&[("Enter", "save"), ("Esc", "cancel")])
        } else if let Some(msg) = &self.notice {
            Text::new(msg).color_range(3, ..)
        } else if self.searching {
            keyhints(&[
                ("type", "filter"),
                ("\u{2191}\u{2193}", "navigate"),
                ("Enter", "attach/new"),
                ("Esc", "clear"),
            ])
        } else if self.tab_focus {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "tab"),
                ("h", "back to sessions"),
                ("Enter", "attach"),
                ("Esc", "quit"),
            ])
        } else {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "navigate"),
                ("/", "search"),
                ("p", "preview"),
                ("l", "tabs"),
                ("Enter", "attach/new"),
                ("x", "kill"),
                ("d", "kill dead"),
                ("Esc", "quit"),
            ])
        };
        print_text_with_coordinates(footer, 0, footer_y, Some(cols), Some(1));

        // Body
        let body_height = rows.saturating_sub(2);
        if body_height == 0 {
            return;
        }

        let entries = self.visible_entries();
        if entries.is_empty() {
            print_text_with_coordinates(
                Text::new("  No matching sessions").color_range(1, ..),
                0, 1, Some(cols), Some(1),
            );
            return;
        }

        // When the preview is on, split the body: a compact list on the left,
        // the pane dump filling the rest, with a separator column between them.
        let list_cols = if self.preview_on {
            (cols / 3).clamp(24, 48).min(cols.saturating_sub(4))
        } else {
            cols
        };

        // In tab focus the left panel lists the session's tabs; otherwise the
        // session list.
        let lines = if self.tab_focus {
            self.build_tab_lines(body_height)
        } else {
            self.build_list_lines(&entries, body_height)
        };
        for (i, text) in lines.into_iter().enumerate() {
            print_text_with_coordinates(text, 0, 1 + i, Some(list_cols), Some(1));
        }

        if self.preview_on {
            for row in 0..body_height {
                print_text_with_coordinates(
                    Text::new("\u{2502}"),
                    list_cols, 1 + row, Some(1), Some(1),
                );
            }
            let px = list_cols + 1;
            let pw = cols.saturating_sub(px);
            self.render_preview(px, 1, pw, body_height);
        }
    }
}

impl State {
    /// The entries currently visible, in display order, after applying the
    /// search filter. When the query is empty this is "New session" + every
    /// live session + every dead session. Selection, activation and rendering
    /// all index into this list.
    fn visible_entries(&self) -> Vec<Entry> {
        let q = self.query.trim().to_lowercase();

        if q.is_empty() {
            let mut entries = Vec::with_capacity(self.entry_count());
            entries.push(Entry::New);
            entries.extend((0..self.sessions.len()).map(Entry::Live));
            entries.extend((0..self.resurrectable.len()).map(Entry::Dead));
            return entries;
        }

        let mut entries = Vec::new();
        // Live sessions match on their name or any of their pane titles.
        for (i, session) in self.sessions.iter().enumerate() {
            let name_match = session.name.to_lowercase().contains(&q);
            let pane_match = Self::pane_titles(session)
                .iter()
                .any(|t| t.to_lowercase().contains(&q));
            if name_match || pane_match {
                entries.push(Entry::Live(i));
            }
        }
        // Dead sessions match on their name only.
        for (i, (name, _)) in self.resurrectable.iter().enumerate() {
            if name.to_lowercase().contains(&q) {
                entries.push(Entry::Dead(i));
            }
        }
        entries
    }

    /// Total unfiltered entries: 1 ("New session") + live + dead sessions.
    fn entry_count(&self) -> usize {
        1 + self.sessions.len() + self.resurrectable.len()
    }

    /// Pull the full session list from zellij. This is the only way to learn
    /// about peer sessions: the server only fills its peer-session cache in
    /// response to this call, so a plugin that merely subscribes to
    /// `SessionUpdate` sees nothing but its own session. Returns whether the
    /// list was refreshed (i.e. the plugin should re-render).
    fn refresh_sessions(&mut self) -> bool {
        match get_session_list() {
            Ok(snapshot) => {
                self.sessions = snapshot.live_sessions;
                self.resurrectable = snapshot.resurrectable_sessions;
                self.clamp_selection();
                true
            }
            Err(_) => false,
        }
    }

    fn clamp_selection(&mut self) {
        let total = self.visible_entries().len();
        if total == 0 {
            self.selected = 0;
        } else if self.selected >= total {
            self.selected = total - 1;
        }
    }

    /// Collect pane titles for a session, excluding plugin panes.
    fn pane_titles(session: &SessionInfo) -> Vec<String> {
        let mut titles = Vec::new();
        let mut tab_indices: Vec<&usize> = session.panes.panes.keys().collect();
        tab_indices.sort();
        for tab_idx in tab_indices {
            if let Some(panes) = session.panes.panes.get(tab_idx) {
                for pane in panes {
                    if pane.is_plugin {
                        continue;
                    }
                    titles.push(pane.title.clone());
                }
            }
        }
        titles
    }

    /// Number of body lines an entry occupies (header + any detail lines).
    fn entry_lines(&self, entry: &Entry) -> usize {
        match entry {
            Entry::New => 1,
            Entry::Live(i) => 1 + Self::pane_titles(&self.sessions[*i]).len(),
            Entry::Dead(_) => 2,
        }
    }

    /// Build text lines for the body with scroll support. `entries` is the
    /// filtered, display-ordered list; `self.selected` indexes into it.
    fn build_list_lines(&mut self, entries: &[Entry], visible_rows: usize) -> Vec<Text> {
        // Line offset of each entry's header, and the total line count.
        let mut headers = Vec::with_capacity(entries.len());
        let mut line = 0usize;
        for entry in entries {
            headers.push(line);
            line += self.entry_lines(entry);
        }
        let total_lines = line;

        // Scroll to keep the selected entry visible.
        let sel_header = headers.get(self.selected).copied().unwrap_or(0);
        let sel_size = entries
            .get(self.selected)
            .map(|e| self.entry_lines(e))
            .unwrap_or(1);

        if sel_header < self.scroll_offset {
            self.scroll_offset = sel_header;
        } else if sel_header + sel_size > self.scroll_offset + visible_rows {
            self.scroll_offset = (sel_header + sel_size).saturating_sub(visible_rows);
        }
        if total_lines <= visible_rows {
            self.scroll_offset = 0;
        } else if self.scroll_offset > total_lines.saturating_sub(visible_rows) {
            self.scroll_offset = total_lines.saturating_sub(visible_rows);
        }

        // Emit the visible Text lines.
        let mut items = Vec::new();
        let mut cur = 0usize;
        let vis_end = self.scroll_offset + visible_rows;
        let push_visible = |items: &mut Vec<Text>, cur: usize, text: Text| {
            if cur >= self.scroll_offset && cur < vis_end {
                items.push(text);
            }
        };

        for (pos, entry) in entries.iter().enumerate() {
            let is_selected = self.selected == pos;
            let prefix = if is_selected { "▸ " } else { "  " };
            let prefix_chars = prefix.chars().count();

            match entry {
                Entry::New => {
                    let label = format!("{}New session", prefix);
                    let mut item = Text::new(&label).color_range(2, prefix_chars..);
                    if is_selected {
                        item = item.selected();
                    }
                    push_visible(&mut items, cur, item);
                    cur += 1;
                }
                Entry::Live(i) => {
                    let session = &self.sessions[*i];
                    let suffix = if session.is_current_session {
                        " (attached)"
                    } else if session.connected_clients > 0 {
                        " (connected)"
                    } else {
                        ""
                    };

                    let text = format!("{}{}{}", prefix, session.name, suffix);
                    let name_end = prefix_chars + session.name.len();
                    let mut item = Text::new(&text).color_range(0, prefix_chars..name_end);
                    if !suffix.is_empty() {
                        item = item.color_range(2, name_end..text.len());
                    }
                    if is_selected {
                        item = item.selected();
                    }
                    push_visible(&mut items, cur, item);
                    cur += 1;

                    for title in &Self::pane_titles(session) {
                        let line_text = format!("    {}", title);
                        let mut item = Text::new(&line_text).color_range(1, 4..);
                        if is_selected {
                            item = item.selected();
                        }
                        push_visible(&mut items, cur, item);
                        cur += 1;
                    }
                }
                Entry::Dead(i) => {
                    let (name, age) = &self.resurrectable[*i];
                    let text = format!("{}{} (exited)", prefix, name);
                    let name_end = prefix_chars + name.len();
                    let mut item = Text::new(&text)
                        .color_range(0, prefix_chars..name_end)
                        .color_range(2, name_end..text.len());
                    if is_selected {
                        item = item.selected();
                    }
                    push_visible(&mut items, cur, item);
                    cur += 1;

                    let info = format!("    exited {}", format_duration(*age));
                    let mut item = Text::new(&info).color_range(1, 4..);
                    if is_selected {
                        item = item.selected();
                    }
                    push_visible(&mut items, cur, item);
                    cur += 1;
                }
            }
        }

        items
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.pending_kill.is_some() {
            return self.handle_confirm_key(key);
        }
        if self.rename_input.is_some() {
            return self.handle_rename_key(key);
        }
        if self.searching {
            return self.handle_search_key(key);
        }
        self.handle_normal_key(key)
    }

    /// Key handling while the rename input is showing. Enter commits, Esc
    /// cancels, Backspace/printable chars edit the new name.
    fn handle_rename_key(&mut self, key: KeyWithModifier) -> bool {
        if key.is_key_without_modifier(BareKey::Enter) {
            if let Some(name) = self.rename_input.take() {
                let name = name.trim();
                if !name.is_empty() {
                    rename_session(name);
                    self.refresh_sessions();
                }
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Esc) {
            self.rename_input = None;
            return true;
        }

        if key.is_key_without_modifier(BareKey::Backspace) {
            if let Some(buf) = self.rename_input.as_mut() {
                buf.pop();
            }
            return true;
        }

        if key.key_modifiers.is_empty() {
            if let BareKey::Char(c) = key.bare_key {
                if let Some(buf) = self.rename_input.as_mut() {
                    buf.push(c);
                }
                return true;
            }
        }

        false
    }

    /// Key handling while a kill confirmation prompt is showing. `y`/Enter
    /// confirms, anything else (notably `n`/Esc) cancels.
    fn handle_confirm_key(&mut self, key: KeyWithModifier) -> bool {
        if key.is_key_without_modifier(BareKey::Char('y'))
            || key.is_key_without_modifier(BareKey::Enter)
        {
            if let Some(name) = self.pending_kill.take() {
                let _ = kill_sessions(&[name]);
                // Reflect the kill immediately rather than waiting for the
                // next timer-driven refresh.
                self.refresh_sessions();
            }
        } else {
            self.pending_kill = None;
        }
        true
    }

    /// Key handling in the default (navigation) mode.
    fn handle_normal_key(&mut self, key: KeyWithModifier) -> bool {
        // Any keypress dismisses a one-shot notice.
        self.notice = None;

        // `l` drills into the selected session's tabs; `h` pops back out.
        if key.is_key_without_modifier(BareKey::Char('l')) {
            self.enter_tab_focus();
            return true;
        }
        if key.is_key_without_modifier(BareKey::Char('h')) {
            if self.tab_focus {
                self.tab_focus = false;
                return true;
            }
            return false;
        }

        let up = key.is_key_without_modifier(BareKey::Up)
            || key.is_key_without_modifier(BareKey::Char('k'));
        let down = key.is_key_without_modifier(BareKey::Down)
            || key.is_key_without_modifier(BareKey::Char('j'));

        if self.tab_focus {
            // j/k cycle the previewed tab.
            let count = self.selected_tab_count();
            if up && self.selected_tab > 0 {
                self.selected_tab -= 1;
            } else if down && count > 0 && self.selected_tab < count - 1 {
                self.selected_tab += 1;
            }
            if up || down {
                return true;
            }
        } else {
            // j/k move through the session list.
            let total = self.visible_entries().len();
            if up {
                self.selected = self.selected.saturating_sub(1);
                return true;
            }
            if down {
                if total > 0 && self.selected < total - 1 {
                    self.selected += 1;
                }
                return true;
            }
        }

        if key.is_key_without_modifier(BareKey::Char('/')) {
            self.searching = true;
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('p')) {
            self.toggle_preview();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Enter) {
            self.activate_selected();
            return false;
        }

        if key.is_key_without_modifier(BareKey::Char('d')) {
            self.delete_selected_dead();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('D')) {
            let _ = delete_all_dead_sessions();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('x')) {
            self.kill_selected_live();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('r')) {
            self.start_rename();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('q'))
            || key.is_key_without_modifier(BareKey::Esc)
        {
            close_self();
            return false;
        }

        false
    }

    /// Key handling while the search field is focused. Printable characters
    /// edit the query; arrows navigate; Enter attaches; Esc clears/exits.
    fn handle_search_key(&mut self, key: KeyWithModifier) -> bool {
        // Arrow keys still navigate (j/k are reserved for typing here).
        if key.is_key_without_modifier(BareKey::Up) {
            if self.selected > 0 {
                self.selected -= 1;
            }
            return true;
        }
        if key.is_key_without_modifier(BareKey::Down) {
            let total = self.visible_entries().len();
            if total > 0 && self.selected < total - 1 {
                self.selected += 1;
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Enter) {
            self.activate_selected();
            return false;
        }

        if key.is_key_without_modifier(BareKey::Backspace) {
            self.query.pop();
            self.selected = 0;
            self.clamp_selection();
            return true;
        }

        if key.is_key_without_modifier(BareKey::Esc) {
            // First Esc clears an active query; a second leaves search mode.
            if self.query.is_empty() {
                self.searching = false;
            } else {
                self.query.clear();
            }
            self.selected = 0;
            return true;
        }

        // Printable character with no modifiers -> append to the query.
        if key.key_modifiers.is_empty() {
            if let BareKey::Char(c) = key.bare_key {
                self.query.push(c);
                self.selected = 0;
                self.clamp_selection();
                return true;
            }
        }

        false
    }

    fn activate_selected(&self) {
        let entries = self.visible_entries();
        let Some(entry) = entries.get(self.selected) else {
            return;
        };
        match entry {
            Entry::New => {
                switch_session(None);
                close_self();
            }
            Entry::Live(i) => {
                let session = &self.sessions[*i];
                if session.is_current_session {
                    close_self();
                    return;
                }
                switch_session(Some(&session.name));
                close_self();
            }
            Entry::Dead(i) => {
                if let Some((name, _)) = self.resurrectable.get(*i) {
                    switch_session(Some(name));
                    close_self();
                }
            }
        }
    }

    fn delete_selected_dead(&self) {
        let entries = self.visible_entries();
        if let Some(Entry::Dead(i)) = entries.get(self.selected) {
            if let Some((name, _)) = self.resurrectable.get(*i) {
                let _ = delete_dead_session(name);
            }
        }
    }

    /// Arm a kill confirmation for the selected live session. The actual kill
    /// happens in `handle_confirm_key` once the user confirms. The
    /// currently-attached session is left alone — killing the session this
    /// plugin runs in would pull the rug out from under it.
    fn kill_selected_live(&mut self) {
        let entries = self.visible_entries();
        if let Some(Entry::Live(i)) = entries.get(self.selected) {
            if let Some(session) = self.sessions.get(*i) {
                if session.is_current_session {
                    return;
                }
                self.pending_kill = Some(session.name.clone());
            }
        }
    }

    /// Begin renaming the selected session. The plugin API can only rename the
    /// currently-attached session, so anything else surfaces a notice instead.
    fn start_rename(&mut self) {
        let entries = self.visible_entries();
        if let Some(Entry::Live(i)) = entries.get(self.selected) {
            if let Some(session) = self.sessions.get(*i) {
                if session.is_current_session {
                    self.rename_input = Some(session.name.clone());
                    return;
                }
            }
        }
        self.notice = Some("Can only rename the attached session".to_string());
    }

    /// Geometry (x, y, cols, rows) of our own floating plugin pane, read from
    /// the current session's manifest.
    fn my_geom(&self, plugin_id: u32) -> Option<(usize, usize, usize, usize)> {
        let session = self.sessions.iter().find(|s| s.is_current_session)?;
        for panes in session.panes.panes.values() {
            for pane in panes {
                if pane.is_plugin && pane.id == plugin_id {
                    return Some((pane.pane_x, pane.pane_y, pane.pane_columns, pane.pane_rows));
                }
            }
        }
        None
    }

    /// Toggle the preview panel. When enabling, the floating pane is expanded
    /// to full screen width (keeping its vertical position/size); when
    /// disabling, it is restored to the geometry captured on enable.
    fn toggle_preview(&mut self) {
        self.preview_on = !self.preview_on;
        if !self.preview_on {
            // No preview, no tab focus.
            self.tab_focus = false;
        }
        let plugin_id = get_plugin_ids().plugin_id;

        if self.preview_on {
            if self.saved_geom.is_none() {
                self.saved_geom = self.my_geom(plugin_id);
            }
            let mut coords = FloatingPaneCoordinates::default()
                .with_x_percent(0)
                .with_width_percent(100);
            // Keep the original vertical position/height if we know it.
            if let Some((_, y, _, h)) = self.saved_geom {
                coords = coords.with_y_fixed(y).with_height_fixed(h);
            }
            change_floating_panes_coordinates(vec![(PaneId::Plugin(plugin_id), coords)]);
        } else if let Some((x, y, w, h)) = self.saved_geom.take() {
            let coords = FloatingPaneCoordinates::default()
                .with_x_fixed(x)
                .with_y_fixed(y)
                .with_width_fixed(w)
                .with_height_fixed(h);
            change_floating_panes_coordinates(vec![(PaneId::Plugin(plugin_id), coords)]);
        }
    }

    /// The live session under the current selection, if any.
    fn selected_session(&self) -> Option<&SessionInfo> {
        let entries = self.visible_entries();
        if let Some(Entry::Live(i)) = entries.get(self.selected) {
            return self.sessions.get(*i);
        }
        None
    }

    /// Tab positions present in a session's pane manifest, sorted. This is the
    /// authoritative tab list for previewing — `session.tabs` metadata is
    /// client-relative, but the manifest is populated for peer sessions too.
    fn sorted_tab_positions(session: &SessionInfo) -> Vec<usize> {
        let mut keys: Vec<usize> = session.panes.panes.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Number of tabs in the selected session (0 if none selected).
    fn selected_tab_count(&self) -> usize {
        self.selected_session()
            .map(|s| Self::sorted_tab_positions(s).len())
            .unwrap_or(0)
    }

    /// The tab position to preview: the `selected_tab`-th tab while in tab
    /// focus, otherwise the active tab (falling back to the first).
    fn target_tab_position(&self, session: &SessionInfo) -> Option<usize> {
        let positions = Self::sorted_tab_positions(session);
        if positions.is_empty() {
            return None;
        }
        if self.tab_focus {
            let idx = self.selected_tab.min(positions.len() - 1);
            return positions.get(idx).copied();
        }
        session
            .tabs
            .iter()
            .find(|t| t.active)
            .map(|t| t.position)
            .filter(|p| positions.contains(p))
            .or_else(|| positions.first().copied())
    }

    /// Begin tab focus on the selected live session, starting at its active tab
    /// (or the first). Turns the preview on if it wasn't already.
    fn enter_tab_focus(&mut self) {
        let active_idx = {
            let Some(session) = self.selected_session() else {
                self.notice = Some("Select a live session to preview tabs".to_string());
                return;
            };
            let positions = Self::sorted_tab_positions(session);
            if positions.is_empty() {
                return;
            }
            session
                .tabs
                .iter()
                .find(|t| t.active)
                .and_then(|t| positions.iter().position(|p| *p == t.position))
                .unwrap_or(0)
        };
        self.selected_tab = active_idx;
        self.tab_focus = true;
        if !self.preview_on {
            self.toggle_preview();
        }
    }

    /// Build the left-panel tab list for the selected session (tab focus mode).
    fn build_tab_lines(&self, visible_rows: usize) -> Vec<Text> {
        let mut items = Vec::new();
        let Some(session) = self.selected_session() else {
            return items;
        };
        let positions = Self::sorted_tab_positions(session);
        for (i, pos) in positions.iter().enumerate().take(visible_rows) {
            let name = session
                .tabs
                .iter()
                .find(|t| t.position == *pos)
                .map(|t| t.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("tab {}", pos + 1));
            let is_selected = i == self.selected_tab;
            let prefix = if is_selected { "\u{25b8} " } else { "  " };
            let text = format!("{}{}: {}", prefix, i + 1, name);
            let prefix_chars = prefix.chars().count();
            let num_end = prefix_chars + (i + 1).to_string().len();
            let mut item = Text::new(&text)
                .color_range(3, prefix_chars..num_end)
                .color_range(0, num_end..);
            if is_selected {
                item = item.selected();
            }
            items.push(item);
        }
        items
    }

    /// A short "tab i/n[: name]" indicator for the previewed tab.
    fn tab_indicator(&self, session: &SessionInfo) -> Option<String> {
        let positions = Self::sorted_tab_positions(session);
        let pos = self.target_tab_position(session)?;
        let idx = positions.iter().position(|p| *p == pos).unwrap_or(0);
        let name = session
            .tabs
            .iter()
            .find(|t| t.position == pos)
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty());
        Some(match name {
            Some(n) => format!("tab {}/{}: {}", idx + 1, positions.len(), n),
            None => format!("tab {}/{}", idx + 1, positions.len()),
        })
    }

    /// The selected session's name plus the geometry of every non-plugin pane
    /// in the target tab, normalized to a (0, 0) origin. This is the layout we
    /// composite the preview from. `None` when the selection isn't a live
    /// session or the target tab has no previewable panes.
    fn preview_layout(&self) -> Option<(String, Vec<PreviewPane>)> {
        let session = self.selected_session()?;
        let tab_pos = self.target_tab_position(session)?;
        let panes = session.panes.panes.get(&tab_pos)?;

        // Drop plugin panes (UI bars etc.) and suppressed (background) panes.
        // `is_selectable` is client-relative and unreliable for peer sessions,
        // so we don't use it.
        let mut visible: Vec<&PaneInfo> = panes
            .iter()
            .filter(|p| !p.is_plugin && !p.is_suppressed)
            .collect();
        if visible.is_empty() {
            return None;
        }

        // Collapse overlapping panes (e.g. a `stacked` group, where every
        // member reports the full stack rectangle) down to one — otherwise we'd
        // draw them on top of each other. Keep the focused pane, then the
        // largest, and drop anything that overlaps something already kept.
        visible.sort_by(|a, b| {
            b.is_focused.cmp(&a.is_focused).then_with(|| {
                let area = |p: &PaneInfo| p.pane_columns.saturating_mul(p.pane_rows);
                area(b).cmp(&area(a))
            })
        });
        let mut kept: Vec<&PaneInfo> = Vec::new();
        for pane in visible {
            if !kept.iter().any(|k| panes_overlap(k, pane)) {
                kept.push(pane);
            }
        }

        // The tab's top-left corner: panes are in absolute screen coordinates
        // (offset by the tab/status bars), so normalize against the minimums.
        let ox = kept.iter().map(|p| p.pane_x).min().unwrap_or(0);
        let oy = kept.iter().map(|p| p.pane_y).min().unwrap_or(0);

        let layout = kept
            .iter()
            .map(|p| PreviewPane {
                id: p.id,
                rx: p.pane_x.saturating_sub(ox),
                ry: p.pane_y.saturating_sub(oy),
                cols: p.pane_columns,
                rows: p.pane_rows,
                cx: p.pane_content_x.saturating_sub(ox),
                cy: p.pane_content_y.saturating_sub(oy),
                ccols: p.pane_content_columns,
                crows: p.pane_content_rows,
            })
            .collect();

        Some((session.name.clone(), layout))
    }

    /// Fire an async `dump-screen` for each pane in the selected session's
    /// active tab that we don't already have (or `force` to refresh all). Each
    /// result lands in `update` as a `RunCommandResult`, correlated by `context`.
    fn request_preview(&mut self, force: bool) {
        let Some((session, panes)) = self.preview_layout() else {
            return;
        };
        for pane in panes {
            let key = (session.clone(), pane.id);
            if self.preview_pending.contains(&key) {
                continue;
            }
            if !force && self.previews.contains_key(&key) {
                continue;
            }
            self.preview_pending.insert(key);

            let pane_arg = format!("terminal_{}", pane.id);
            let mut env = BTreeMap::new();
            env.insert("ZELLIJ_SESSION_NAME".to_string(), session.clone());
            let mut context = BTreeMap::new();
            context.insert("kind".to_string(), "preview".to_string());
            context.insert("sid".to_string(), session.clone());
            context.insert("pid".to_string(), pane.id.to_string());

            run_command_with_env_variables_and_cwd(
                &[
                    "zellij", "action", "dump-screen",
                    "--pane-id", pane_arg.as_str(),
                    "--ansi", // keep SGR color codes so the preview renders in color
                ],
                env,
                PathBuf::from("/"),
                context,
            );
        }
    }

    /// Store the result of a preview `dump-screen`, keyed back to its pane via
    /// the `context` map we sent. Returns whether to re-render.
    fn handle_command_result(
        &mut self,
        stdout: Vec<u8>,
        context: BTreeMap<String, String>,
    ) -> bool {
        if context.get("kind").map(String::as_str) != Some("preview") {
            return false;
        }
        let (Some(sid), Some(pid)) = (context.get("sid"), context.get("pid")) else {
            return false;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            return false;
        };
        let key = (sid.clone(), pid);
        let rows = parse_ansi_rows(&stdout);
        self.previews.insert(key.clone(), rows);
        self.preview_pending.remove(&key);

        // Coalesce: only re-render if this result belongs to the preview that's
        // actually on screen. Stale/off-screen results just warm the cache.
        if !self.preview_on {
            return false;
        }
        self.preview_layout()
            .map(|(session, panes)| session == key.0 && panes.iter().any(|p| p.id == key.1))
            .unwrap_or(false)
    }

    /// Draw the preview panel in the region (x, y, w, h).
    ///
    /// Composites the active tab's panes at their real positions and sizes
    /// (1:1, clipped to the region) — like tmux's choose-tree preview, which
    /// preserves the split layout rather than scaling the text. The header uses
    /// the `Text` API (theme colors); pane content is emitted as raw ANSI,
    /// since the `--ansi` dump carries real SGR colors that `Text` (theme-
    /// palette only) can't express.
    fn render_preview(&self, x: usize, y: usize, w: usize, h: usize) {
        if w == 0 || h == 0 {
            return;
        }
        let session_name = self.selected_session().map(|s| s.name.clone());
        let header = match self.selected_session() {
            Some(s) => match self.tab_indicator(s) {
                Some(t) => format!("preview: {} \u{b7} {}", s.name, t),
                None => format!("preview: {}", s.name),
            },
            None => "preview".to_string(),
        };
        print_text_with_coordinates(
            Text::new(&header).color_range(2, ..),
            x, y, Some(w), Some(1),
        );

        let cy0 = y + 1;
        let ch = h.saturating_sub(1);
        if ch == 0 {
            return;
        }

        // Clear the content region first (erase-to-EOL from the preview's left
        // edge leaves the list and separator untouched, since they sit to the
        // left of column `x`).
        for row in 0..ch {
            print!("\u{1b}[{};{}H\u{1b}[K", cy0 + row + 1, x + 1);
        }

        let Some((session, panes)) = self.preview_layout() else {
            let msg = if session_name.is_some() {
                "  (no previewable panes)"
            } else {
                "  (select a live session)"
            };
            print_text_with_coordinates(
                Text::new(msg).color_range(1, ..),
                x, cy0, Some(w), Some(1),
            );
            return;
        };

        let region = (x, cy0, w, ch);
        let region_right = x + w;
        let region_bottom = cy0 + ch;

        for pane in &panes {
            // Pane frame, clipped to the region.
            self.draw_box(region, x + pane.rx, cy0 + pane.ry, pane.cols, pane.rows);

            // Pane content (raw ANSI), positioned and clipped to the region.
            let Some(lines) = self.previews.get(&(session.clone(), pane.id)) else {
                continue;
            };
            for (i, line) in lines.iter().take(pane.crows).enumerate() {
                let col0 = x + pane.cx;
                let row0 = cy0 + pane.cy + i;
                if row0 >= region_bottom {
                    break;
                }
                if col0 >= region_right {
                    continue;
                }
                let max_w = pane.ccols.min(region_right - col0);
                if max_w == 0 {
                    continue;
                }
                print!("\u{1b}[{};{}H{}", row0 + 1, col0 + 1, clip_ansi(line, max_w));
            }
        }
    }

    /// Draw a box (border only) at absolute cell rect (bx, by, bw, bh), drawing
    /// only the cells that fall inside `region` = (rx, ry, rw, rh).
    fn draw_box(&self, region: (usize, usize, usize, usize), bx: usize, by: usize, bw: usize, bh: usize) {
        if bw == 0 || bh == 0 {
            return;
        }
        let (rx, ry, rw, rh) = region;
        let put = |col: usize, row: usize, ch: &str| {
            if col >= rx && col < rx + rw && row >= ry && row < ry + rh {
                print!("\u{1b}[{};{}H{}", row + 1, col + 1, ch);
            }
        };
        let right = bx + bw - 1;
        let bottom = by + bh - 1;
        for col in bx..bx + bw {
            put(col, by, "\u{2500}");
            put(col, bottom, "\u{2500}");
        }
        for row in by..by + bh {
            put(bx, row, "\u{2502}");
            put(right, row, "\u{2502}");
        }
        put(bx, by, "\u{250c}");
        put(right, by, "\u{2510}");
        put(bx, bottom, "\u{2514}");
        put(right, bottom, "\u{2518}");
    }
}

/// Whether two panes' rectangles overlap (touching edges don't count). Used to
/// collapse stacked/overlapping panes in the preview composite.
fn panes_overlap(a: &PaneInfo, b: &PaneInfo) -> bool {
    let ax2 = a.pane_x + a.pane_columns;
    let ay2 = a.pane_y + a.pane_rows;
    let bx2 = b.pane_x + b.pane_columns;
    let by2 = b.pane_y + b.pane_rows;
    a.pane_x < bx2 && b.pane_x < ax2 && a.pane_y < by2 && b.pane_y < ay2
}

/// Truncate an ANSI-styled line to `max_visible` printable columns, copying
/// escape sequences through verbatim (they don't consume columns) and
/// appending a reset so styling can't bleed past the clip.
fn clip_ansi(line: &str, max_visible: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut visible = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            match chars.peek() {
                // CSI (e.g. SGR `ESC [ … m`): copy until the final byte 0x40–0x7E.
                Some('[') => {
                    out.push(chars.next().unwrap());
                    while let Some(&pc) = chars.peek() {
                        out.push(chars.next().unwrap());
                        if ('@'..='~').contains(&pc) {
                            break;
                        }
                    }
                }
                // OSC (`ESC ] … BEL`): copy until BEL.
                Some(']') => {
                    out.push(chars.next().unwrap());
                    while let Some(&pc) = chars.peek() {
                        out.push(chars.next().unwrap());
                        if pc == '\u{7}' {
                            break;
                        }
                    }
                }
                // Other two-byte escapes: copy the following byte.
                Some(_) => out.push(chars.next().unwrap()),
                None => {}
            }
            continue;
        }
        if visible >= max_visible {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out.push_str("\u{1b}[0m");
    out
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The SGR (color/attribute) state of the terminal at a point in the stream.
/// Tracked as a fixed struct (not an accumulating string) so the escape we emit
/// to re-assert it is always minimal and bounded.
#[derive(Clone, Default, PartialEq, Debug)]
struct Sgr {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
    strike: bool,
    blink: bool,
}

impl Sgr {
    /// Apply one SGR escape's parameters to the state.
    fn apply(&mut self, params: &[u16]) {
        if params.is_empty() {
            // `ESC[m` is `ESC[0m`.
            *self = Sgr::default();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => *self = Sgr::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 | 6 => self.blink = true,
                7 => self.reverse = true,
                9 => self.strike = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.reverse = false,
                29 => self.strike = false,
                30..=37 => self.fg = Color::Indexed((params[i] - 30) as u8),
                90..=97 => self.fg = Color::Indexed((params[i] - 90 + 8) as u8),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Indexed((params[i] - 40) as u8),
                100..=107 => self.bg = Color::Indexed((params[i] - 100 + 8) as u8),
                49 => self.bg = Color::Default,
                38 => {
                    if let Some((c, adv)) = read_ext_color(&params[i + 1..]) {
                        self.fg = c;
                        i += adv;
                    }
                }
                48 => {
                    if let Some((c, adv)) = read_ext_color(&params[i + 1..]) {
                        self.bg = c;
                        i += adv;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Emit a minimal, absolute escape that reproduces this state from scratch.
    /// Empty for the default state (we reset at the end of every row anyway).
    fn open(&self) -> String {
        if *self == Sgr::default() {
            return String::new();
        }
        let mut s = String::from("\u{1b}[0m");
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.reverse, "7"),
            (self.strike, "9"),
        ] {
            if on {
                s.push_str("\u{1b}[");
                s.push_str(code);
                s.push('m');
            }
        }
        s.push_str(&color_escape(self.fg, true));
        s.push_str(&color_escape(self.bg, false));
        s
    }
}

/// Parse the extended-color tail of an SGR `38`/`48` code, returning the color
/// and how many extra params it consumed.
fn read_ext_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first()? {
        5 => rest.get(1).map(|&n| (Color::Indexed(n as u8), 2)),
        2 => {
            let r = *rest.get(1)? as u8;
            let g = *rest.get(2)? as u8;
            let b = *rest.get(3)? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

fn color_escape(c: Color, fg: bool) -> String {
    let lead = if fg { 38 } else { 48 };
    match c {
        Color::Default => String::new(),
        Color::Indexed(n) => format!("\u{1b}[{};5;{}m", lead, n),
        Color::Rgb(r, g, b) => format!("\u{1b}[{};2;{};{};{}m", lead, r, g, b),
    }
}

/// Consume one escape sequence starting at the ESC already seen. Returns the
/// raw sequence, whether it's an SGR (`…m`), and its parsed numeric params.
fn read_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (String, bool, Vec<u16>) {
    let mut seq = String::from("\u{1b}");
    match chars.peek() {
        Some('[') => {
            seq.push(chars.next().unwrap());
            let mut params = String::new();
            let mut is_sgr = false;
            while let Some(&pc) = chars.peek() {
                seq.push(chars.next().unwrap());
                if ('@'..='~').contains(&pc) {
                    is_sgr = pc == 'm';
                    break;
                }
                params.push(pc);
            }
            let parsed = if is_sgr && !params.is_empty() {
                params
                    .split(';')
                    .map(|s| s.parse::<u16>().unwrap_or(0))
                    .collect()
            } else {
                Vec::new()
            };
            (seq, is_sgr, parsed)
        }
        // OSC: consume to BEL.
        Some(']') => {
            seq.push(chars.next().unwrap());
            while let Some(&pc) = chars.peek() {
                seq.push(chars.next().unwrap());
                if pc == '\u{7}' {
                    break;
                }
            }
            (seq, false, Vec::new())
        }
        Some(_) => {
            seq.push(chars.next().unwrap());
            (seq, false, Vec::new())
        }
        None => (seq, false, Vec::new()),
    }
}

/// Parse a `dump-screen --ansi` blob into self-contained rows: each row is
/// prefixed with the SGR state active at its start (so it can be drawn at any
/// position without color bleeding between lines) and bounded in size.
fn parse_ansi_rows(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut rows = Vec::new();
    let mut sgr = Sgr::default();
    let mut row_open = String::new();
    let mut body = String::new();
    let mut visible = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                rows.push(format!("{}{}\u{1b}[0m", row_open, body));
                if rows.len() >= PREVIEW_MAX_ROWS {
                    return rows;
                }
                body.clear();
                visible = 0;
                row_open = sgr.open();
            }
            '\r' => {}
            '\u{1b}' => {
                let (seq, is_sgr, params) = read_escape(&mut chars);
                if is_sgr {
                    sgr.apply(&params);
                    // Keep in-line changes, but only while there's room to show them.
                    if visible < PREVIEW_MAX_COLS {
                        body.push_str(&seq);
                    }
                }
            }
            _ => {
                if visible < PREVIEW_MAX_COLS {
                    body.push(c);
                    visible += 1;
                }
            }
        }
    }
    if !body.is_empty() || !row_open.is_empty() {
        rows.push(format!("{}{}\u{1b}[0m", row_open, body));
    }
    rows
}

/// Build a Text with key names highlighted (color 3) and labels plain.
fn keyhints(pairs: &[(&str, &str)]) -> Text {
    let mut s = String::new();
    let mut ranges = Vec::new();
    let mut char_pos = 0usize;
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str("  ");
            char_pos += 2;
        }
        let start = char_pos;
        s.push_str(key);
        char_pos += key.chars().count();
        ranges.push((start, char_pos));
        s.push(' ');
        char_pos += 1;
        s.push_str(label);
        char_pos += label.chars().count();
    }
    let mut text = Text::new(&s);
    for (start, end) in ranges {
        text = text.color_range(3, start..end);
    }
    text
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI escapes the same way a terminal would consume them, so tests
    /// can assert on visible content/width.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        while let Some(&pc) = chars.peek() {
                            chars.next();
                            if ('@'..='~').contains(&pc) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(&pc) = chars.peek() {
                            chars.next();
                            if pc == '\u{7}' {
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn visible_len(s: &str) -> usize {
        strip_ansi(s).chars().count()
    }

    // ---- read_ext_color ----

    #[test]
    fn ext_color_256() {
        assert_eq!(read_ext_color(&[5, 200]), Some((Color::Indexed(200), 2)));
    }

    #[test]
    fn ext_color_rgb() {
        assert_eq!(read_ext_color(&[2, 10, 20, 30]), Some((Color::Rgb(10, 20, 30), 4)));
    }

    #[test]
    fn ext_color_truncated_or_unknown() {
        assert_eq!(read_ext_color(&[5]), None);
        assert_eq!(read_ext_color(&[2, 1, 2]), None);
        assert_eq!(read_ext_color(&[]), None);
        assert_eq!(read_ext_color(&[3]), None);
    }

    // ---- Sgr::apply / open ----

    #[test]
    fn sgr_default_open_is_empty() {
        assert_eq!(Sgr::default().open(), "");
    }

    #[test]
    fn sgr_bold() {
        let mut s = Sgr::default();
        s.apply(&[1]);
        let mut expected = Sgr::default();
        expected.bold = true;
        assert_eq!(s, expected);
        let open = s.open();
        assert!(open.starts_with("\u{1b}[0m"));
        assert!(open.contains("\u{1b}[1m"));
    }

    #[test]
    fn sgr_reset_code_clears() {
        let mut s = Sgr::default();
        s.apply(&[1, 31]);
        s.apply(&[0]);
        assert_eq!(s, Sgr::default());
        assert_eq!(s.open(), "");
    }

    #[test]
    fn sgr_empty_params_is_reset() {
        let mut s = Sgr::default();
        s.apply(&[1]);
        s.apply(&[]);
        assert_eq!(s, Sgr::default());
    }

    #[test]
    fn sgr_fg_indexed_and_bright() {
        let mut s = Sgr::default();
        s.apply(&[31]);
        assert_eq!(s.fg, Color::Indexed(1));
        let mut bright = Sgr::default();
        bright.apply(&[91]);
        assert_eq!(bright.fg, Color::Indexed(9));
    }

    #[test]
    fn sgr_fg_rgb() {
        let mut s = Sgr::default();
        s.apply(&[38, 2, 255, 0, 0]);
        assert_eq!(s.fg, Color::Rgb(255, 0, 0));
        assert!(s.open().contains("38;2;255;0;0"));
    }

    #[test]
    fn sgr_fg_256() {
        let mut s = Sgr::default();
        s.apply(&[38, 5, 123]);
        assert_eq!(s.fg, Color::Indexed(123));
        assert!(s.open().contains("38;5;123"));
    }

    #[test]
    fn sgr_bg_set_and_default() {
        let mut s = Sgr::default();
        s.apply(&[44]);
        assert_eq!(s.bg, Color::Indexed(4));
        s.apply(&[49]);
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_unbold() {
        let mut s = Sgr::default();
        s.apply(&[1]);
        s.apply(&[22]);
        assert!(!s.bold);
    }

    #[test]
    fn sgr_multi_param_in_one_apply() {
        let mut s = Sgr::default();
        s.apply(&[1, 32]);
        assert!(s.bold);
        assert_eq!(s.fg, Color::Indexed(2));
    }

    #[test]
    fn sgr_rgb_then_trailing_param() {
        // `38;2;r;g;b` consumes its args, then `1` (bold) still applies.
        let mut s = Sgr::default();
        s.apply(&[38, 2, 1, 2, 3, 1]);
        assert_eq!(s.fg, Color::Rgb(1, 2, 3));
        assert!(s.bold);
    }

    // ---- clip_ansi ----

    #[test]
    fn clip_plain() {
        assert_eq!(clip_ansi("hello", 3), "hel\u{1b}[0m");
    }

    #[test]
    fn clip_no_clip_needed() {
        assert_eq!(clip_ansi("hi", 10), "hi\u{1b}[0m");
    }

    #[test]
    fn clip_preserves_leading_escape() {
        assert_eq!(clip_ansi("\u{1b}[31mhello", 3), "\u{1b}[31mhel\u{1b}[0m");
    }

    #[test]
    fn clip_escapes_dont_count_as_width() {
        let out = clip_ansi("\u{1b}[1m\u{1b}[31mX", 1);
        assert_eq!(visible_len(&out), 1);
        assert!(out.contains("\u{1b}[1m"));
        assert!(out.contains("\u{1b}[31m"));
    }

    #[test]
    fn clip_zero_width() {
        assert_eq!(clip_ansi("abc", 0), "\u{1b}[0m");
    }

    // ---- read_escape ----

    #[test]
    fn read_escape_sgr_with_params() {
        let mut it = "[1;31mrest".chars().peekable();
        let (seq, is_sgr, params) = read_escape(&mut it);
        assert_eq!(seq, "\u{1b}[1;31m");
        assert!(is_sgr);
        assert_eq!(params, vec![1, 31]);
        assert_eq!(it.collect::<String>(), "rest");
    }

    #[test]
    fn read_escape_empty_sgr() {
        let mut it = "mX".chars().peekable();
        let (seq, is_sgr, params) = read_escape(&mut it);
        assert_eq!(seq, "\u{1b}m"); // not a CSI; treated as a 2-byte escape
        assert!(!is_sgr);
        assert!(params.is_empty());
        assert_eq!(it.collect::<String>(), "X");
    }

    #[test]
    fn read_escape_bare_csi_sgr() {
        let mut it = "[mX".chars().peekable();
        let (seq, is_sgr, params) = read_escape(&mut it);
        assert_eq!(seq, "\u{1b}[m");
        assert!(is_sgr);
        assert!(params.is_empty());
        assert_eq!(it.collect::<String>(), "X");
    }

    #[test]
    fn read_escape_non_sgr_csi() {
        let mut it = "[2JX".chars().peekable();
        let (seq, is_sgr, params) = read_escape(&mut it);
        assert_eq!(seq, "\u{1b}[2J");
        assert!(!is_sgr);
        assert!(params.is_empty());
        assert_eq!(it.collect::<String>(), "X");
    }

    #[test]
    fn read_escape_osc_to_bel() {
        let mut it = "]8;;http://x\u{7}Y".chars().peekable();
        let (seq, is_sgr, _) = read_escape(&mut it);
        assert!(!is_sgr);
        assert_eq!(seq, "\u{1b}]8;;http://x\u{7}");
        assert_eq!(it.collect::<String>(), "Y");
    }

    // ---- parse_ansi_rows ----

    #[test]
    fn parse_plain_rows() {
        let rows = parse_ansi_rows(b"abc\ndef");
        assert_eq!(rows, vec!["abc\u{1b}[0m".to_string(), "def\u{1b}[0m".to_string()]);
    }

    #[test]
    fn parse_carries_style_across_lines() {
        let rows = parse_ansi_rows(b"\x1b[31mred\nmore");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\u{1b}[31m"));
        assert!(rows[0].ends_with("\u{1b}[0m"));
        // The fix: row 2 re-asserts the carried foreground at its start.
        assert!(rows[1].starts_with("\u{1b}[0m"));
        assert!(rows[1].contains("38;5;1"));
        assert!(rows[1].contains("more"));
    }

    #[test]
    fn parse_reset_stops_carry() {
        let rows = parse_ansi_rows(b"\x1b[31mA\x1b[0mB\nC");
        assert_eq!(rows[1], "C\u{1b}[0m");
    }

    #[test]
    fn parse_ignores_carriage_return() {
        let rows = parse_ansi_rows(b"a\rb\n");
        assert_eq!(rows, vec!["ab\u{1b}[0m".to_string()]);
    }

    #[test]
    fn parse_drops_non_sgr_escapes() {
        let rows = parse_ansi_rows(b"a\x1b[2Jb\n");
        assert_eq!(rows, vec!["ab\u{1b}[0m".to_string()]);
    }

    #[test]
    fn parse_keeps_inline_sgr() {
        let rows = parse_ansi_rows(b"\x1b[1mX\x1b[0mY\n");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("\u{1b}[1mX"));
        assert!(rows[0].contains("Y"));
    }

    #[test]
    fn parse_caps_width() {
        let input = "a".repeat(600);
        let rows = parse_ansi_rows(input.as_bytes());
        assert_eq!(rows.len(), 1);
        assert_eq!(visible_len(&rows[0]), PREVIEW_MAX_COLS);
    }

    #[test]
    fn parse_caps_rows() {
        let input = "x\n".repeat(250);
        let rows = parse_ansi_rows(input.as_bytes());
        assert_eq!(rows.len(), PREVIEW_MAX_ROWS);
    }

    // ---- panes_overlap ----

    fn pane(x: usize, y: usize, cols: usize, rows: usize) -> PaneInfo {
        PaneInfo {
            pane_x: x,
            pane_y: y,
            pane_columns: cols,
            pane_rows: rows,
            ..Default::default()
        }
    }

    #[test]
    fn overlap_identical_rects() {
        assert!(panes_overlap(&pane(0, 0, 100, 40), &pane(0, 0, 100, 40)));
    }

    #[test]
    fn overlap_side_by_side_touching_is_false() {
        assert!(!panes_overlap(&pane(0, 0, 50, 40), &pane(50, 0, 50, 40)));
    }

    #[test]
    fn overlap_stacked_vertically_touching_is_false() {
        assert!(!panes_overlap(&pane(0, 0, 100, 20), &pane(0, 20, 100, 20)));
    }

    #[test]
    fn overlap_partial_is_true() {
        assert!(panes_overlap(&pane(0, 0, 100, 40), &pane(50, 20, 100, 40)));
    }
}
