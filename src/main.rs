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
    /// Navigation depth into the selected session, controlled by `l` (deeper)
    /// and `h` (shallower): `Session` (move between sessions) → `Tab` (the
    /// session expands and j/k moves between its tabs) → `Pane` (j/k moves
    /// between the selected tab's panes).
    focus: Focus,
    /// Index (into the selected session's tab positions) of the selected tab,
    /// while expanded. The preview follows it.
    selected_tab: usize,
    /// Index (into the selected tab's panes) of the selected pane, at `Pane` depth.
    selected_pane: usize,
    /// Cached pane-screen dumps, keyed by (session name, terminal pane id).
    /// Populated asynchronously from `dump-screen` via `RunCommandResult`.
    previews: BTreeMap<(String, u32), Vec<String>>,
    /// Dumps currently in flight, so we don't fire duplicate commands.
    preview_pending: BTreeSet<(String, u32)>,
    /// Annotations attached to panes by external producers over `zellij pipe`,
    /// keyed by (session name, terminal pane id) — the same shape as `previews`.
    /// This is the sole source of truth for pane state/attention; it lives only
    /// in memory, scoped to this zellij session's lifetime.
    annotations: BTreeMap<(String, u32), PaneAnno>,
    /// When true, the list is filtered to only sessions with an unacknowledged
    /// attention bell ("show me only what's waiting"). Toggled with `a`.
    only_attention: bool,
}

/// How deep navigation has drilled into the selected session. `l` deepens,
/// `h` shallows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Focus {
    /// Move between sessions (the default).
    #[default]
    Session,
    /// The selected session is expanded; move between its tabs.
    Tab,
    /// Move between the selected tab's individual panes.
    Pane,
}

/// A typed annotation value. Producers manipulate these with operations
/// (`set`/`append`/`incr`/…) rather than assigning whole values, so the plugin —
/// the single owner — applies the logic and there's no read-modify-write race.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    List(Vec<String>),
    Counter(i64),
}

/// Severity of an attention bell, governing its display precedence and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Info,
    Warn,
    Error,
}

/// A transient "look at me" bell raised by a producer event. Unlike state
/// (which is overwritten by the next event), attention is cleared by the *user*
/// seeing the pane — see `mark_session_seen` / the `PaneUpdate` handler.
#[derive(Clone, Debug, PartialEq)]
struct Attention {
    level: Level,
    label: String,
    producer: String,
}

/// Everything annotated onto a single pane.
#[derive(Clone, Default, Debug, PartialEq)]
struct PaneAnno {
    /// Producer-namespaced state values (e.g. `"claude" -> Str("waiting")`).
    states: BTreeMap<String, Value>,
    /// The attention bell; `None` once acknowledged (seen).
    attention: Option<Attention>,
}

impl PaneAnno {
    /// True once there's nothing left to display, so the entry can be dropped.
    fn is_empty(&self) -> bool {
        self.states.is_empty() && self.attention.is_none()
    }
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
            // Needed to receive annotations piped in via `zellij pipe`.
            PermissionType::ReadCliPipes,
        ]);
        subscribe(&[
            EventType::SessionUpdate,
            EventType::Key,
            EventType::Timer,
            EventType::PermissionRequestResult,
            EventType::RunCommandResult,
            // Drives the manual-focus "seen" transition: focusing a pane with an
            // attention bell acknowledges it.
            EventType::PaneUpdate,
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
            // Manual-focus "seen": focusing a pane in the attached session that
            // carries an attention bell acknowledges it.
            Event::PaneUpdate(manifest) => self.handle_pane_focus(&manifest),
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
        } else if self.focus == Focus::Pane {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "pane"),
                ("h", "tabs"),
                ("m", "done"),
                ("Enter", "attach"),
                ("Esc", "quit"),
            ])
        } else if self.focus == Focus::Tab {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "tab"),
                ("l", "panes"),
                ("h", "sessions"),
                ("m", "done"),
                ("Enter", "attach"),
                ("Esc", "quit"),
            ])
        } else {
            keyhints(&[
                ("\u{2191}\u{2193}/jk", "navigate"),
                ("/", "search"),
                ("a", if self.only_attention { "all" } else { "waiting" }),
                ("m", "done"),
                ("p", "preview"),
                ("l", "tabs"),
                ("Enter", "attach/new"),
                ("x", "kill"),
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

        // The session list is the only view; in tab focus the selected session
        // expands into its tabs in place (handled inside build_list_lines).
        let lines = self.build_list_lines(&entries, body_height);
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

    /// Receive an annotation from `zellij pipe`. The message carries an op in
    /// `name`, structured fields in `args` (session/pane/key/producer/level),
    /// and free-form text in `payload` (so values may contain commas/spaces).
    /// The producer sends an *operation*, not a value, so all the logic lives
    /// here in the single owner — appends and counters can't race.
    fn pipe(&mut self, msg: PipeMessage) -> bool {
        // A CLI pipe blocks the `zellij pipe` caller until it's unblocked.
        // Producers are fire-and-forget, so release the caller immediately —
        // before any early return below — so a hook can never hang on us.
        // (wasm-only: the shim routes through a host import absent in native
        // test builds, and there's no pipe to unblock off-wasm anyway.)
        #[cfg(target_family = "wasm")]
        if let PipeSource::Cli(pipe_id) = &msg.source {
            unblock_cli_pipe_input(pipe_id);
        }

        // Addressing: session + pane id are required. Bail (no re-render) if the
        // message isn't a well-formed pane annotation.
        let Some(session) = msg.args.get("session").cloned() else {
            return false;
        };
        let Some(pane) = msg.args.get("pane").and_then(|p| p.parse::<u32>().ok()) else {
            return false;
        };
        let key = msg.args.get("key").cloned();
        let value = msg
            .payload
            .clone()
            .or_else(|| msg.args.get("value").cloned());
        let producer = msg
            .args
            .get("producer")
            .cloned()
            .unwrap_or_else(|| "anon".to_string());
        let level = msg.args.get("level").map(|l| match l.as_str() {
            "error" => Level::Error,
            "warn" => Level::Warn,
            _ => Level::Info,
        });

        let map_key = (session, pane);

        match msg.name.as_str() {
            "set" => {
                let (Some(k), Some(v)) = (key, value.clone()) else {
                    return false;
                };
                let anno = self.annotations.entry(map_key).or_default();
                anno.states.insert(k, Value::Str(v.clone()));
                if let Some(level) = level {
                    anno.attention = Some(Attention { level, label: v, producer });
                }
            }
            "append" => {
                let (Some(k), Some(v)) = (key, value) else {
                    return false;
                };
                let max = msg.args.get("max").and_then(|m| m.parse::<usize>().ok());
                let anno = self.annotations.entry(map_key).or_default();
                let list = match anno.states.entry(k).or_insert_with(|| Value::List(Vec::new())) {
                    Value::List(l) => l,
                    // Coerce a mismatched value into a fresh list.
                    slot => {
                        *slot = Value::List(Vec::new());
                        let Value::List(l) = slot else { unreachable!() };
                        l
                    }
                };
                list.push(v);
                if let Some(max) = max {
                    let overflow = list.len().saturating_sub(max);
                    if overflow > 0 {
                        list.drain(0..overflow);
                    }
                }
            }
            "remove" => {
                let Some(k) = key else { return false };
                if let Some(anno) = self.annotations.get_mut(&map_key) {
                    match (value, anno.states.get_mut(&k)) {
                        // Remove a single item from a list.
                        (Some(v), Some(Value::List(l))) => l.retain(|x| x != &v),
                        // No value, or a non-list value: drop the whole key.
                        _ => {
                            anno.states.remove(&k);
                        }
                    }
                    if anno.is_empty() {
                        self.annotations.remove(&map_key);
                    }
                }
            }
            "incr" => {
                let Some(k) = key else { return false };
                let by = msg
                    .args
                    .get("by")
                    .and_then(|b| b.parse::<i64>().ok())
                    .unwrap_or(1);
                let anno = self.annotations.entry(map_key).or_default();
                match anno.states.entry(k).or_insert(Value::Counter(0)) {
                    Value::Counter(n) => *n += by,
                    slot => *slot = Value::Counter(by),
                }
            }
            "notify" => {
                let anno = self.annotations.entry(map_key).or_default();
                anno.attention = Some(Attention {
                    level: level.unwrap_or(Level::Info),
                    label: value.unwrap_or_default(),
                    producer,
                });
            }
            "clear" => {
                match key {
                    // Clear one key; drop the pane entry if nothing's left.
                    Some(k) => {
                        if let Some(anno) = self.annotations.get_mut(&map_key) {
                            anno.states.remove(&k);
                            if anno.is_empty() {
                                self.annotations.remove(&map_key);
                            }
                        }
                    }
                    // Clear the whole pane.
                    None => {
                        self.annotations.remove(&map_key);
                    }
                }
            }
            _ => return false,
        }
        true
    }
}

impl State {
    /// The entries currently visible, in display order, after applying the
    /// search filter. When the query is empty this is "New session" + every
    /// live session + every dead session. Selection, activation and rendering
    /// all index into this list.
    fn visible_entries(&self) -> Vec<Entry> {
        let q = self.query.trim().to_lowercase();

        let mut entries = Vec::new();
        if q.is_empty() {
            entries.reserve(self.entry_count());
            entries.push(Entry::New);
            entries.extend((0..self.sessions.len()).map(Entry::Live));
            entries.extend((0..self.resurrectable.len()).map(Entry::Dead));
        } else {
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
        }

        // "Show only what's waiting": keep just live sessions whose panes carry
        // an unacknowledged attention bell.
        if self.only_attention {
            entries.retain(|e| match e {
                Entry::Live(i) => self.session_attention(&self.sessions[*i].name).is_some(),
                _ => false,
            });
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

    /// (pane id, title) for each non-plugin pane in a session, in tab order.
    /// The id lets us join against `annotations` for per-pane badges.
    fn visible_panes(session: &SessionInfo) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        let mut tab_indices: Vec<&usize> = session.panes.panes.keys().collect();
        tab_indices.sort();
        for tab_idx in tab_indices {
            if let Some(panes) = session.panes.panes.get(tab_idx) {
                for pane in panes {
                    if pane.is_plugin {
                        continue;
                    }
                    out.push((pane.id, pane.title.clone()));
                }
            }
        }
        out
    }

    /// Collect pane titles for a session, excluding plugin panes.
    fn pane_titles(session: &SessionInfo) -> Vec<String> {
        Self::visible_panes(session)
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }

    /// The worst (highest-precedence) unacknowledged attention level across a
    /// session's panes, if any — drives the session-header aggregate badge.
    fn session_attention(&self, name: &str) -> Option<Level> {
        self.annotations
            .iter()
            .filter(|((s, _), _)| s == name)
            .filter_map(|(_, a)| a.attention.as_ref().map(|att| att.level))
            .max()
    }

    /// A session's tabs as (tab name, its pane (id, title) list), in tab order —
    /// used to render the inline tab expansion when a session is opened with `l`.
    fn session_tabs(session: &SessionInfo) -> Vec<(String, Vec<(u32, String)>)> {
        Self::sorted_tab_positions(session)
            .iter()
            .map(|pos| {
                let name = session
                    .tabs
                    .iter()
                    .find(|t| t.position == *pos)
                    .map(|t| t.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("tab {}", pos + 1));
                let panes = session
                    .panes
                    .panes
                    .get(pos)
                    .map(|ps| {
                        ps.iter()
                            .filter(|p| !p.is_plugin)
                            .map(|p| (p.id, p.title.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                (name, panes)
            })
            .collect()
    }

    /// One indented pane-title line with its annotation badge. `indent` is the
    /// left padding in columns; `selected` highlights it. Shared by the session
    /// list and the per-tab view so the badge layout lives in one place.
    fn pane_line(&self, session: &str, id: u32, title: &str, indent: usize, selected: bool) -> Text {
        let pad = " ".repeat(indent);
        let badge = self
            .annotations
            .get(&(session.to_string(), id))
            .and_then(anno_badge);
        let (text, title_start, glyph) = match badge {
            Some((g, color)) => {
                let g_len = g.chars().count();
                (format!("{}{} {}", pad, g, title), indent + g_len + 1, Some((g_len, color)))
            }
            None => (format!("{}{}", pad, title), indent, None),
        };
        let mut item = Text::new(&text);
        if let Some((g_len, color)) = glyph {
            item = item.color_range(color, indent..indent + g_len);
        }
        item = item.color_range(1, title_start..);
        if selected {
            item = item.selected();
        }
        item
    }

    /// Build text lines for the body with scroll support. `entries` is the
    /// filtered, display-ordered list; `self.selected` indexes into it.
    ///
    /// When the selected session is expanded (`Focus::Tab`/`Pane` via `l`), it
    /// shows its tabs in place with panes grouped under each — the rest of the
    /// list stays put. The cursor is `selected_tab` at `Tab` depth, or
    /// `selected_pane` within that tab at `Pane` depth. Otherwise each live
    /// session shows its panes flat.
    ///
    /// Builds every line, recording the "anchor" range that must stay visible
    /// (the selected session / tab block / pane), then returns the scrolled
    /// window.
    fn build_list_lines(&mut self, entries: &[Entry], visible_rows: usize) -> Vec<Text> {
        let mut lines: Vec<Text> = Vec::new();
        let mut anchor_start = 0usize;
        let mut anchor_size = 1usize;

        for (pos, entry) in entries.iter().enumerate() {
            let is_selected = self.selected == pos;
            let prefix = if is_selected { "\u{25b8} " } else { "  " };
            let prefix_chars = prefix.chars().count();

            match entry {
                Entry::New => {
                    let label = format!("{}New session", prefix);
                    let mut item = Text::new(&label).color_range(2, prefix_chars..);
                    if is_selected {
                        item = item.selected();
                        anchor_start = lines.len();
                        anchor_size = 1;
                    }
                    lines.push(item);
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

                    // Aggregate attention bell, prefixed before the session name.
                    let bell = self.session_attention(&session.name);
                    let badge = if bell.is_some() { "● " } else { "" };
                    let badge_chars = badge.chars().count();
                    let text = format!("{}{}{}{}", prefix, badge, session.name, suffix);
                    let name_start = prefix_chars + badge_chars;
                    let name_end = name_start + session.name.chars().count();
                    let mut item = Text::new(&text).color_range(0, name_start..name_end);
                    if let Some(level) = bell {
                        item = item.color_range(level_color(level), prefix_chars..name_start);
                    }
                    if !suffix.is_empty() {
                        item = item.color_range(2, name_end..text.chars().count());
                    }

                    let expanded = is_selected && self.focus != Focus::Session;
                    // When expanded the cursor lives deeper (a tab or a pane), so
                    // the session header keeps just its ▸ marker, not the full
                    // highlight.
                    if is_selected && !expanded {
                        item = item.selected();
                        anchor_start = lines.len();
                        anchor_size = 1 + Self::visible_panes(session).len();
                    }
                    lines.push(item);

                    if expanded {
                        let pane_depth = self.focus == Focus::Pane;
                        let tabs = Self::session_tabs(session);
                        let sel_tab = self.selected_tab.min(tabs.len().saturating_sub(1));
                        for (ti, (name, panes)) in tabs.iter().enumerate() {
                            let tsel = ti == sel_tab;
                            let tprefix = if tsel { "\u{25b8} " } else { "  " };
                            let tprefix_chars = 4 + tprefix.chars().count();
                            let num = ti + 1;
                            let line_text = format!("    {}{}: {}", tprefix, num, name);
                            let num_end = tprefix_chars + num.to_string().len();
                            let mut header = Text::new(&line_text)
                                .color_range(3, tprefix_chars..num_end)
                                .color_range(0, num_end..);
                            // At Tab depth the selected tab's whole block is the
                            // highlight/anchor; at Pane depth the header only marks
                            // the tab and the highlight moves to the selected pane.
                            if tsel && !pane_depth {
                                header = header.selected();
                                anchor_start = lines.len();
                                anchor_size = 1 + panes.len();
                            }
                            lines.push(header);

                            let sel_pane = self.selected_pane.min(panes.len().saturating_sub(1));
                            for (pi, (id, title)) in panes.iter().enumerate() {
                                let psel = pane_depth && tsel && pi == sel_pane;
                                let selected = if pane_depth { psel } else { tsel };
                                let idx = lines.len();
                                lines.push(self.pane_line(&session.name, *id, title, 8, selected));
                                if psel {
                                    anchor_start = idx;
                                    anchor_size = 1;
                                }
                            }
                        }
                    } else {
                        for (id, title) in Self::visible_panes(session) {
                            lines.push(self.pane_line(&session.name, id, &title, 4, is_selected));
                        }
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
                        anchor_start = lines.len();
                        anchor_size = 2;
                    }
                    lines.push(item);

                    let info = format!("    exited {}", format_duration(*age));
                    let mut info_item = Text::new(&info).color_range(1, 4..);
                    if is_selected {
                        info_item = info_item.selected();
                    }
                    lines.push(info_item);
                }
            }
        }

        // Scroll to keep the anchor block visible.
        let total = lines.len();
        if anchor_start < self.scroll_offset {
            self.scroll_offset = anchor_start;
        } else if anchor_start + anchor_size > self.scroll_offset + visible_rows {
            self.scroll_offset = (anchor_start + anchor_size).saturating_sub(visible_rows);
        }
        if total <= visible_rows {
            self.scroll_offset = 0;
        } else if self.scroll_offset > total.saturating_sub(visible_rows) {
            self.scroll_offset = total.saturating_sub(visible_rows);
        }

        let end = (self.scroll_offset + visible_rows).min(total);
        lines
            .into_iter()
            .skip(self.scroll_offset)
            .take(end.saturating_sub(self.scroll_offset))
            .collect()
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

        // `l` drills deeper (session → tab → pane); `h` pops back out.
        if key.is_key_without_modifier(BareKey::Char('l')) {
            match self.focus {
                Focus::Session => self.enter_tab_focus(),
                Focus::Tab => {
                    // Only descend into panes if the selected tab has any.
                    if self.selected_tab_pane_count() > 0 {
                        self.focus = Focus::Pane;
                        self.selected_pane = 0;
                    }
                }
                Focus::Pane => {}
            }
            return true;
        }
        if key.is_key_without_modifier(BareKey::Char('h')) {
            match self.focus {
                Focus::Pane => {
                    self.focus = Focus::Tab;
                    return true;
                }
                Focus::Tab => {
                    self.focus = Focus::Session;
                    return true;
                }
                Focus::Session => return false,
            }
        }

        let up = key.is_key_without_modifier(BareKey::Up)
            || key.is_key_without_modifier(BareKey::Char('k'));
        let down = key.is_key_without_modifier(BareKey::Down)
            || key.is_key_without_modifier(BareKey::Char('j'));

        if self.focus == Focus::Pane {
            // j/k move between the selected tab's panes.
            let count = self.selected_tab_pane_count();
            if up && self.selected_pane > 0 {
                self.selected_pane -= 1;
            } else if down && count > 0 && self.selected_pane < count - 1 {
                self.selected_pane += 1;
            }
            if up || down {
                return true;
            }
        } else if self.focus == Focus::Tab {
            // j/k cycle the selected tab (preview follows).
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

        // `a` toggles the "only sessions waiting on me" filter.
        if key.is_key_without_modifier(BareKey::Char('a')) {
            self.only_attention = !self.only_attention;
            self.selected = 0;
            self.clamp_selection();
            return true;
        }

        // `m` manually marks the current selection's notification done (scoped to
        // the navigation depth: pane / tab / session).
        if key.is_key_without_modifier(BareKey::Char('m')) {
            return self.mark_selected_done();
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

    fn activate_selected(&mut self) {
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
                // Attaching drops us on the session's *active tab*, whose panes
                // are all tiled into view — so only that tab's bells are "seen".
                // Other tabs are unseen and keep their bells.
                let name = session.name.clone();
                let ids = Self::active_tab_pane_ids(session);
                self.mark_panes_seen(&name, &ids);
                switch_session(Some(&name));
                close_self();
            }
            Entry::Dead(i) => {
                // A dead session has no live panes — nothing to mark seen.
                if let Some((name, _)) = self.resurrectable.get(*i) {
                    let name = name.clone();
                    switch_session(Some(&name));
                    close_self();
                }
            }
        }
    }

    /// The attached session's name, if known.
    fn current_session_name(&self) -> Option<String> {
        self.sessions
            .iter()
            .find(|s| s.is_current_session)
            .map(|s| s.name.clone())
    }

    /// Manual-focus "seen": when a pane in the attached session is focused and it
    /// carries an attention bell, acknowledge it (state is left intact). Returns
    /// whether anything changed, so we only re-render on a real transition.
    fn handle_pane_focus(&mut self, manifest: &PaneManifest) -> bool {
        let Some(session) = self.current_session_name() else {
            return false;
        };
        let mut changed = false;
        for panes in manifest.panes.values() {
            for pane in panes {
                if pane.is_plugin || !pane.is_focused {
                    continue;
                }
                let k = (session.clone(), pane.id);
                let cleared = self
                    .annotations
                    .get_mut(&k)
                    .map(|a| a.attention.take().is_some())
                    .unwrap_or(false);
                if cleared {
                    changed = true;
                    if self.annotations.get(&k).map(PaneAnno::is_empty).unwrap_or(false) {
                        self.annotations.remove(&k);
                    }
                }
            }
        }
        changed
    }

    /// Pane ids of a session's active tab (the one you land on when attaching),
    /// excluding plugin panes.
    fn active_tab_pane_ids(session: &SessionInfo) -> Vec<u32> {
        let Some(pos) = session.tabs.iter().find(|t| t.active).map(|t| t.position) else {
            return Vec::new();
        };
        session
            .panes
            .panes
            .get(&pos)
            .map(|ps| {
                ps.iter()
                    .filter(|p| !p.is_plugin)
                    .map(|p| p.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Jump "seen": acknowledge the attention bell on the given panes of a
    /// session (state values are preserved), dropping any entry left empty.
    fn mark_panes_seen(&mut self, session: &str, ids: &[u32]) {
        for &id in ids {
            let k = (session.to_string(), id);
            if let Some(anno) = self.annotations.get_mut(&k) {
                anno.attention = None;
            }
            if self.annotations.get(&k).map(PaneAnno::is_empty).unwrap_or(false) {
                self.annotations.remove(&k);
            }
        }
    }

    /// Remove every annotation (state *and* attention — any glyph) on the given
    /// panes. Unlike `mark_panes_seen`, which only silences the bell, this fully
    /// clears them. Returns whether anything was removed.
    fn clear_panes(&mut self, session: &str, ids: &[u32]) -> bool {
        let mut changed = false;
        for &id in ids {
            if self.annotations.remove(&(session.to_string(), id)).is_some() {
                changed = true;
            }
        }
        changed
    }

    /// Manually "mark done" the current selection — clears any glyph entirely
    /// (state and bell), scoped to the navigation depth: the selected pane at
    /// `Pane` depth, the selected tab's panes at `Tab` depth, or the whole
    /// session at `Session` depth. Returns whether to re-render.
    fn mark_selected_done(&mut self) -> bool {
        let Some(session) = self.selected_session() else {
            return false;
        };
        let name = session.name.clone();
        let ids: Vec<u32> = match self.focus {
            Focus::Session => Self::visible_panes(session)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            Focus::Tab | Focus::Pane => {
                let tabs = Self::session_tabs(session);
                let sel = self.selected_tab.min(tabs.len().saturating_sub(1));
                let panes = tabs.get(sel).map(|(_, p)| p.clone()).unwrap_or_default();
                if self.focus == Focus::Pane {
                    let pi = self.selected_pane.min(panes.len().saturating_sub(1));
                    panes.get(pi).map(|(id, _)| vec![*id]).unwrap_or_default()
                } else {
                    panes.into_iter().map(|(id, _)| id).collect()
                }
            }
        };
        if ids.is_empty() {
            return false;
        }
        self.clear_panes(&name, &ids)
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

    /// Number of panes in the selected session's selected tab (0 if none).
    fn selected_tab_pane_count(&self) -> usize {
        let Some(session) = self.selected_session() else {
            return 0;
        };
        let tabs = Self::session_tabs(session);
        let sel = self.selected_tab.min(tabs.len().saturating_sub(1));
        tabs.get(sel).map(|(_, p)| p.len()).unwrap_or(0)
    }

    /// The tab position to preview: the `selected_tab`-th tab while in tab
    /// focus, otherwise the active tab (falling back to the first).
    fn target_tab_position(&self, session: &SessionInfo) -> Option<usize> {
        let positions = Self::sorted_tab_positions(session);
        if positions.is_empty() {
            return None;
        }
        if self.focus != Focus::Session {
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
                self.notice = Some("Select a live session to open its tabs".to_string());
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
        self.selected_pane = 0;
        self.focus = Focus::Tab;
        // The selected session expands in place within the list; build_list_lines
        // re-derives scroll to keep the selected tab visible. Preview is left to
        // `p` — opening tabs no longer forces it on.
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

/// Palette color index (0–3, theme-relative) for an attention level.
fn level_color(level: Level) -> usize {
    match level {
        Level::Error => 0,
        Level::Warn => 3,
        Level::Info => 2,
    }
}

/// A status badge for a pane annotation: (glyph, palette color index).
/// Attention takes visual precedence over state; once acknowledged, the
/// underlying state glyph shows through (so a seen `waiting` still reads as
/// waiting, just without the bell color).
fn anno_badge(anno: &PaneAnno) -> Option<(&'static str, usize)> {
    if let Some(att) = &anno.attention {
        return Some(("●", level_color(att.level)));
    }
    // Prefer Claude's state, else any producer's.
    let state = anno
        .states
        .get("claude")
        .or_else(|| anno.states.values().next())?;
    if let Value::Str(s) = state {
        return Some(match s.as_str() {
            "waiting" | "needs-attention" => ("●", 3),
            "working" => ("◐", 1),
            "idle" => ("○", 1),
            "error" => ("✗", 0),
            _ => ("•", 2),
        });
    }
    None
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

    // ---- pipe() annotation ops ----

    fn pipe_msg(name: &str, args: &[(&str, &str)], payload: Option<&str>) -> PipeMessage {
        PipeMessage {
            source: PipeSource::Cli("test".into()),
            name: name.into(),
            payload: payload.map(String::from),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            is_private: false,
        }
    }

    fn anno(s: &State, session: &str, pane: u32) -> PaneAnno {
        s.annotations
            .get(&(session.to_string(), pane))
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn pipe_set_writes_state() {
        let mut s = State::default();
        let r = s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "3"), ("key", "claude")],
            Some("waiting"),
        ));
        assert!(r);
        assert_eq!(
            anno(&s, "a", 3).states.get("claude"),
            Some(&Value::Str("waiting".into()))
        );
    }

    #[test]
    fn pipe_set_with_level_raises_attention() {
        let mut s = State::default();
        s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "3"), ("key", "claude"), ("level", "info")],
            Some("waiting"),
        ));
        let a = anno(&s, "a", 3);
        assert_eq!(a.states.get("claude"), Some(&Value::Str("waiting".into())));
        assert_eq!(a.attention.as_ref().map(|x| x.level), Some(Level::Info));
    }

    #[test]
    fn pipe_set_without_level_leaves_attention_unset() {
        let mut s = State::default();
        s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "3"), ("key", "claude")],
            Some("working"),
        ));
        assert!(anno(&s, "a", 3).attention.is_none());
    }

    #[test]
    fn pipe_missing_address_is_ignored() {
        let mut s = State::default();
        // No pane id.
        assert!(!s.pipe(pipe_msg("set", &[("session", "a"), ("key", "k")], Some("v"))));
        // Non-numeric pane id.
        assert!(!s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "x"), ("key", "k")],
            Some("v"),
        )));
        assert!(s.annotations.is_empty());
    }

    #[test]
    fn pipe_append_builds_list_and_caps() {
        let mut s = State::default();
        for v in ["one", "two", "three"] {
            s.pipe(pipe_msg(
                "append",
                &[("session", "a"), ("pane", "1"), ("key", "history"), ("max", "2")],
                Some(v),
            ));
        }
        assert_eq!(
            anno(&s, "a", 1).states.get("history"),
            Some(&Value::List(vec!["two".into(), "three".into()]))
        );
    }

    #[test]
    fn pipe_append_coerces_non_list() {
        let mut s = State::default();
        s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "1"), ("key", "k")],
            Some("scalar"),
        ));
        s.pipe(pipe_msg(
            "append",
            &[("session", "a"), ("pane", "1"), ("key", "k")],
            Some("item"),
        ));
        assert_eq!(
            anno(&s, "a", 1).states.get("k"),
            Some(&Value::List(vec!["item".into()]))
        );
    }

    #[test]
    fn pipe_incr_counts() {
        let mut s = State::default();
        s.pipe(pipe_msg("incr", &[("session", "a"), ("pane", "1"), ("key", "n")], None));
        s.pipe(pipe_msg(
            "incr",
            &[("session", "a"), ("pane", "1"), ("key", "n"), ("by", "5")],
            None,
        ));
        assert_eq!(anno(&s, "a", 1).states.get("n"), Some(&Value::Counter(6)));
    }

    #[test]
    fn pipe_remove_item_then_key() {
        let mut s = State::default();
        for v in ["x", "y"] {
            s.pipe(pipe_msg(
                "append",
                &[("session", "a"), ("pane", "1"), ("key", "l")],
                Some(v),
            ));
        }
        // Remove a single item.
        s.pipe(pipe_msg(
            "remove",
            &[("session", "a"), ("pane", "1"), ("key", "l")],
            Some("x"),
        ));
        assert_eq!(
            anno(&s, "a", 1).states.get("l"),
            Some(&Value::List(vec!["y".into()]))
        );
        // Remove with no value drops the key; the now-empty pane entry is GC'd.
        s.pipe(pipe_msg("remove", &[("session", "a"), ("pane", "1"), ("key", "l")], None));
        assert!(s.annotations.is_empty());
    }

    #[test]
    fn pipe_clear_key_and_pane() {
        let mut s = State::default();
        s.pipe(pipe_msg("set", &[("session", "a"), ("pane", "1"), ("key", "a")], Some("1")));
        s.pipe(pipe_msg("set", &[("session", "a"), ("pane", "1"), ("key", "b")], Some("2")));
        s.pipe(pipe_msg("clear", &[("session", "a"), ("pane", "1"), ("key", "a")], None));
        assert_eq!(anno(&s, "a", 1).states.len(), 1);
        s.pipe(pipe_msg("clear", &[("session", "a"), ("pane", "1")], None));
        assert!(s.annotations.is_empty());
    }

    #[test]
    fn pipe_unknown_op_is_ignored() {
        let mut s = State::default();
        assert!(!s.pipe(pipe_msg(
            "bogus",
            &[("session", "a"), ("pane", "1"), ("key", "k")],
            Some("v"),
        )));
    }

    // ---- "seen" transitions ----

    #[test]
    fn mark_panes_seen_clears_attention_keeps_state() {
        let mut s = State::default();
        s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "1"), ("key", "claude"), ("level", "info")],
            Some("waiting"),
        ));
        s.mark_panes_seen("a", &[1]);
        let a = anno(&s, "a", 1);
        assert!(a.attention.is_none(), "bell cleared");
        assert_eq!(
            a.states.get("claude"),
            Some(&Value::Str("waiting".into())),
            "state preserved"
        );
    }

    #[test]
    fn mark_panes_seen_gcs_attention_only_entry() {
        let mut s = State::default();
        // notify sets attention with no state.
        s.pipe(pipe_msg(
            "notify",
            &[("session", "a"), ("pane", "1"), ("level", "warn")],
            Some("look"),
        ));
        s.mark_panes_seen("a", &[1]);
        assert!(s.annotations.is_empty(), "empty entry dropped");
    }

    #[test]
    fn mark_panes_seen_only_named_panes() {
        let mut s = State::default();
        // Two panes in session "a" (different tabs), one in "b".
        for (sess, pane) in [("a", "1"), ("a", "2"), ("b", "1")] {
            s.pipe(pipe_msg(
                "set",
                &[("session", sess), ("pane", pane), ("key", "claude"), ("level", "info")],
                Some("waiting"),
            ));
        }
        // Land on the tab holding only pane 1.
        s.mark_panes_seen("a", &[1]);
        assert!(anno(&s, "a", 1).attention.is_none(), "seen pane cleared");
        assert!(anno(&s, "a", 2).attention.is_some(), "other tab's pane untouched");
        assert!(anno(&s, "b", 1).attention.is_some(), "other session untouched");
    }

    #[test]
    fn clear_panes_removes_state_and_attention() {
        let mut s = State::default();
        // A pane with both state and a bell, plus a sibling that must survive.
        s.pipe(pipe_msg(
            "set",
            &[("session", "a"), ("pane", "1"), ("key", "claude"), ("level", "info")],
            Some("waiting"),
        ));
        s.pipe(pipe_msg("append", &[("session", "a"), ("pane", "1"), ("key", "h")], Some("x")));
        s.pipe(pipe_msg("set", &[("session", "a"), ("pane", "2"), ("key", "claude")], Some("working")));

        assert!(s.clear_panes("a", &[1]), "something removed");
        assert!(
            s.annotations.get(&("a".into(), 1)).is_none(),
            "pane 1 fully gone (no glyph)"
        );
        assert!(anno(&s, "a", 2).states.contains_key("claude"), "pane 2 untouched");
        // Nothing to remove → no change.
        assert!(!s.clear_panes("a", &[1]));
    }

    #[test]
    fn session_attention_reports_worst_level() {
        let mut s = State::default();
        s.pipe(pipe_msg(
            "notify",
            &[("session", "a"), ("pane", "1"), ("level", "info")],
            Some(""),
        ));
        s.pipe(pipe_msg(
            "notify",
            &[("session", "a"), ("pane", "2"), ("level", "error")],
            Some(""),
        ));
        assert_eq!(s.session_attention("a"), Some(Level::Error));
        assert_eq!(s.session_attention("nope"), None);
    }
}
