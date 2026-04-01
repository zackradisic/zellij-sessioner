use std::collections::BTreeMap;
use std::time::Duration;
use zellij_tile::prelude::*;

const NEW_SESSION_IDX: usize = 0;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Default)]
struct State {
    sessions: Vec<SessionInfo>,
    resurrectable: Vec<(String, Duration)>,
    selected: usize,
    scroll_offset: usize,
    permissions_granted: bool,
    spinner_idx: usize,
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
                // Cycle the plugin pane title to trigger a fresh
                // SessionUpdate with up-to-date pane titles.
                let frame = SPINNER[self.spinner_idx % SPINNER.len()];
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                let plugin_id = get_plugin_ids().plugin_id;
                rename_plugin_pane(plugin_id, &format!("Sessioner {}", frame));
                set_timeout(2.0);
                false
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

        // Footer
        let footer_y = rows.saturating_sub(1);
        print_text_with_coordinates(
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "navigate"),
                ("Enter", "attach/new"),
                ("d", "kill dead"),
                ("D", "kill all dead"),
                ("Esc", "quit"),
            ]),
            0, footer_y, Some(cols), Some(1),
        );

        // Body
        let body_height = rows.saturating_sub(2);
        if body_height == 0 {
            return;
        }

        let items = self.build_list_items(body_height);
        print_nested_list_with_coordinates(items, 0, 1, Some(cols), Some(body_height));
    }
}

impl State {
    /// Total entries: 1 ("New session") + live sessions + dead sessions.
    fn entry_count(&self) -> usize {
        1 + self.sessions.len() + self.resurrectable.len()
    }

    fn clamp_selection(&mut self) {
        let total = self.entry_count();
        if self.selected >= total {
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

    /// Build the nested list items with scroll support.
    fn build_list_items(&mut self, visible_rows: usize) -> Vec<NestedListItem> {
        struct Block {
            header_line: usize,
            lines: usize, // total lines this block occupies
            entry_idx: usize,
        }

        let mut blocks = Vec::new();
        let mut line = 0usize;

        // "New session" entry
        blocks.push(Block {
            header_line: line,
            lines: 1,
            entry_idx: NEW_SESSION_IDX,
        });
        line += 1;

        // Live sessions
        for (i, session) in self.sessions.iter().enumerate() {
            let n_panes = Self::pane_titles(session).len();
            let block_lines = 1 + n_panes;
            blocks.push(Block {
                header_line: line,
                lines: block_lines,
                entry_idx: 1 + i,
            });
            line += block_lines;
        }

        // Dead sessions
        for (i, _) in self.resurrectable.iter().enumerate() {
            blocks.push(Block {
                header_line: line,
                lines: 2,
                entry_idx: 1 + self.sessions.len() + i,
            });
            line += 2;
        }

        let total_lines = line;

        // Scroll to keep selected block visible
        let selected_block = blocks.iter().find(|b| b.entry_idx == self.selected);
        let sel_header = selected_block.map(|b| b.header_line).unwrap_or(0);
        let sel_size = selected_block.map(|b| b.lines).unwrap_or(1);

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

        // Emit visible NestedListItems
        let mut items = Vec::new();
        let mut cur = 0usize;
        let vis_end = self.scroll_offset + visible_rows;

        // "New session" item
        if cur >= self.scroll_offset && cur < vis_end {
            let is_selected = self.selected == NEW_SESSION_IDX;
            let label = if is_selected {
                "▸ New session".to_string()
            } else {
                "  New session".to_string()
            };
            let mut item = NestedListItem::new(&label).color_range(2, ..);
            if is_selected {
                item = item.selected();
            }
            items.push(item);
        }
        cur += 1;

        // Live sessions
        for (i, session) in self.sessions.iter().enumerate() {
            let entry_idx = 1 + i;
            let is_selected = self.selected == entry_idx;

            if cur >= self.scroll_offset && cur < vis_end {
                let suffix = if session.is_current_session {
                    " (attached)"
                } else if session.connected_clients > 0 {
                    " (connected)"
                } else {
                    ""
                };

                let prefix = if is_selected { "▸ " } else { "  " };
                let header_text = format!("{}{}{}", prefix, session.name, suffix);
                let prefix_len = prefix.chars().count();
                let name_end = prefix_len + session.name.len();
                let mut item =
                    NestedListItem::new(&header_text).color_range(0, prefix_len..name_end);
                if !suffix.is_empty() {
                    item = item.color_range(2, name_end..header_text.len());
                }
                if is_selected {
                    item = item.selected();
                }
                items.push(item);
            }
            cur += 1;

            for title in &Self::pane_titles(session) {
                if cur >= self.scroll_offset && cur < vis_end {
                    let mut item =
                        NestedListItem::new(title).indent(1).color_range(1, ..);
                    if is_selected {
                        item = item.selected();
                    }
                    items.push(item);
                }
                cur += 1;
            }
        }

        // Dead sessions
        for (i, (name, age)) in self.resurrectable.iter().enumerate() {
            let entry_idx = 1 + self.sessions.len() + i;
            let is_selected = self.selected == entry_idx;

            if cur >= self.scroll_offset && cur < vis_end {
                let prefix = if is_selected { "▸ " } else { "  " };
                let header_text = format!("{}{} (exited)", prefix, name);
                let prefix_len = prefix.chars().count();
                let name_end = prefix_len + name.len();
                let mut item =
                    NestedListItem::new(&header_text).color_range(0, prefix_len..name_end);
                item = item.color_range(2, name_end..header_text.len());
                if is_selected {
                    item = item.selected();
                }
                items.push(item);
            }
            cur += 1;

            if cur >= self.scroll_offset && cur < vis_end {
                let info = format!("exited {}", format_duration(*age));
                let mut item =
                    NestedListItem::new(&info).indent(1).color_range(1, ..);
                if is_selected {
                    item = item.selected();
                }
                items.push(item);
            }
            cur += 1;
        }

        items
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        let total = self.entry_count();

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
            if self.selected < total - 1 {
                self.selected += 1;
            }
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
            delete_all_dead_sessions();
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

    fn activate_selected(&self) {
        if self.selected == NEW_SESSION_IDX {
            switch_session(None);
            close_self();
            return;
        }

        let session_idx = self.selected - 1;
        if session_idx < self.sessions.len() {
            let session = &self.sessions[session_idx];
            if session.is_current_session {
                close_self();
                return;
            }
            switch_session(Some(&session.name));
            close_self();
        } else {
            let dead_idx = session_idx - self.sessions.len();
            if let Some((name, _)) = self.resurrectable.get(dead_idx) {
                switch_session(Some(name));
                close_self();
            }
        }
    }

    fn delete_selected_dead(&self) {
        let session_idx = self.selected.saturating_sub(1);
        if session_idx >= self.sessions.len() {
            let dead_idx = session_idx - self.sessions.len();
            if let Some((name, _)) = self.resurrectable.get(dead_idx) {
                delete_dead_session(name);
            }
        }
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
