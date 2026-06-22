use std::collections::BTreeMap;
use std::time::Duration;
use zellij_tile::prelude::*;

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);
        subscribe(&[
            EventType::SessionUpdate,
            EventType::Key,
            EventType::Timer,
            EventType::PermissionRequestResult,
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
                changed
            }
            Event::Key(key) => self.handle_key(key),
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
        } else {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "navigate"),
                ("/", "search"),
                ("Enter", "attach/new"),
                ("r", "rename"),
                ("x", "kill"),
                ("d", "kill dead"),
                ("D", "kill all dead"),
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

        let lines = self.build_list_lines(&entries, body_height);
        for (i, text) in lines.into_iter().enumerate() {
            print_text_with_coordinates(text, 0, 1 + i, Some(cols), Some(1));
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

        let total = self.visible_entries().len();

        if key.is_key_without_modifier(BareKey::Up)
            || key.is_key_without_modifier(BareKey::Char('k'))
        {
            if self.selected > 0 {
                self.selected -= 1;
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Down)
            || key.is_key_without_modifier(BareKey::Char('j'))
        {
            if total > 0 && self.selected < total - 1 {
                self.selected += 1;
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Char('/')) {
            self.searching = true;
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
