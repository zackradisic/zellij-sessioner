# zellij-sessioner

A [Zellij](https://zellij.dev) plugin that enhances the built-in session manager
by showing **pane titles** for each session. See at a glance what's running
everywhere — particularly useful with tools like [Claude Code](https://claude.ai/code)
that put their status in the terminal title.

The built-in session manager can show pane titles too, but they have to be
expanded manually one session at a time using keybindings. This plugin shows
them all at once. There is a [feature request](https://github.com/zellij-org/zellij/issues/4765)
to add an "expand all" option to the built-in session manager, which would
make this plugin unnecessary.

![screenshot](session-manager-plus.png)

## Install

### Pre-built wasm (recommended)

Use the plugin directly from a GitHub release URL — Zellij downloads and
caches it automatically:

```
zellij action launch-or-focus-plugin \
  https://github.com/nomeata/zellij-sessioner/releases/latest/download/zellij-sessioner.wasm \
  --floating
```

Or pin a specific version:

```
https://github.com/nomeata/zellij-sessioner/releases/download/v0.1.0/zellij-sessioner.wasm
```

### Keybinding

The built-in session manager is bound to `Ctrl o` then `w`. You can **replace**
it with zellij-sessioner by overriding that binding in your Zellij config
(`~/.config/zellij/config.kdl`):

```kdl
keybinds {
    session {
        bind "w" {
            LaunchOrFocusPlugin "https://github.com/nomeata/zellij-sessioner/releases/latest/download/zellij-sessioner.wasm" {
                floating true
                move_to_focused_tab true
            };
            SwitchToMode "Normal"
        }
    }
}
```

Or if you prefer to keep the built-in and add this alongside it, bind a
different key (e.g. `e`):

```kdl
keybinds {
    session {
        bind "e" {
            LaunchOrFocusPlugin "https://github.com/nomeata/zellij-sessioner/releases/latest/download/zellij-sessioner.wasm" {
                floating true
                move_to_focused_tab true
            };
            SwitchToMode "Normal"
        }
    }
}
```

Then open it with `Ctrl o` then `e`.

### Layout (`zellij -l sessioner`)

To launch the plugin with `zellij -l sessioner` (similar to `zellij -l welcome`),
place a layout file at `~/.config/zellij/layouts/sessioner.kdl`:

```kdl
layout {
    pane borderless=true {
        plugin location="https://github.com/nomeata/zellij-sessioner/releases/latest/download/zellij-sessioner.wasm"
    }
}
show_startup_tips false
```

Zellij looks for layouts by name in `~/.config/zellij/layouts/` (or whatever
`layout_dir` is set to in your config).

### Build from source

Requires Rust with the `wasm32-wasip1` target.

```bash
# with Nix
nix develop && cargo build --release

# without Nix
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

Then use the local build:

```bash
zellij action launch-or-focus-plugin \
  file:target/wasm32-wasip1/release/zellij-sessioner.wasm \
  --floating
```

### Tests

The ANSI/SGR preview parser has unit tests. `.cargo/config.toml` pins the build
to `wasm32-wasip1`, which can't run a test binary, so tests run on the host
target:

```bash
cargo test --target $(rustc -vV | sed -n 's/host: //p')
```

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move selection up |
| `↓` / `j` | Move selection down |
| `/` | Search / filter sessions |
| `a` | Toggle "only sessions waiting on me" (see [Pane annotation bus](#pane-annotation-bus)) |
| `l` | Drill deeper in place: session → tabs → individual panes; `j`/`k` moves at the current depth |
| `h` | Back out one level (pane → tab → session) |
| `m` | Mark the current selection done — clears its annotations entirely, scoped to depth (pane / tab / session) |
| `p` | Toggle a live color preview of the selected session's focused pane |
| `Enter` | Switch to selected session / start new session |
| `r` | Rename the attached session (other sessions can't be renamed via the plugin API) |
| `x` | Kill selected live session (asks to confirm; not the attached one) |
| `d` | Delete selected dead session |
| `D` | Delete all dead sessions |
| `Esc` / `q` | Close the plugin |

### Search

Press `/` to filter the list as you type. A session matches if its **name** or
any of its **pane titles** contains the query (case-insensitive) — so you can
jump to a session by what's running in it, not just its name. While searching:

| Key | Action |
|-----|--------|
| `↑` / `↓` | Move selection (within matches) |
| `Backspace` | Edit the query |
| `Enter` | Attach to the selected match |
| `Esc` | Clear the query, then exit search |

Dead sessions match on their name only.

## How it works

Subscribes to Zellij's `SessionUpdate` event which provides the full session
list with pane manifests. For each session it shows:

- **Session name** with status (attached / connected / exited)
- **Pane titles** indented underneath (plugin panes excluded)

Dead (resurrectable) sessions appear at the bottom with their age.

## Pane annotation bus

Any external process can **annotate a pane** — flag it as needing attention or
attach state to it — by piping a message to the plugin. The session list then
shows a badge next to the pane and an aggregate bell next to its session, so you
can see at a glance which panes want you (and filter to just those with `a`).

The motivating case: surfacing which Claude Code sessions are idle waiting on
your input. But the bus is generic — a build, a test watcher, or any script can
use it.

### Concepts

- **State** — a durable, producer-namespaced label (`claude=waiting`,
  `tests=failing`). Overwritten by the next event from that producer. Stored as a
  typed value (string / list / counter), so producers can keep more than strings.
- **Attention** — a transient "look at me" bell, raised by an event and cleared
  when *you* see the pane. State persists when attention clears, so a pane can
  stay `waiting` in the list without nagging.

A pane is marked **seen** (its bell cleared) when you either attach to its
session through the sessioner or manually focus the pane in the attached session.

### `pane-notify`

[`scripts/pane-notify`](scripts/pane-notify) wraps `zellij pipe`, filling in the
session/pane from the environment. It sends an **operation**, not a whole value —
the plugin (the single owner of the state) applies it, so appends and counters
can't race:

```sh
pane-notify set    key=claude level=info -- waiting   # set state + ring the bell
pane-notify set    key=claude            -- working   # update state, no bell
pane-notify append key=history max=50    -- "ran tests"  # push onto a capped list
pane-notify incr   key=builds                          # bump a counter
pane-notify notify level=warn            -- "needs input"  # bell only
pane-notify clear  key=claude                          # drop a key (or the whole pane)
```

| op | effect |
|----|--------|
| `set key=<k> [level=…] -- <v>` | `states[k] = <v>`; with `level`, also rings the bell |
| `append key=<k> [max=<n>] -- <item>` | push onto a list, trimmed to the last `n` |
| `remove key=<k> [-- <item>]` | drop one list item, or the whole key |
| `incr key=<k> [by=<n>]` | bump a counter (default +1) |
| `notify [level=…] -- <label>` | ring the attention bell only |
| `clear [key=<k>]` | drop a key, or the whole pane's annotations |

**Row tint:** there's no special color op — a pane is tinted by setting the
reserved `color` state key to a hex value with the ordinary `set` op:
`set key=color -- d97757` (no `#` — it's a shell comment). Clear it with
`clear key=color`; `m` (mark done) clears it along with the rest of the pane's
state. The `Text` UI is **theme-palette only (no RGB)**, so the tint uses a theme
palette level (`COLORED_PANE_LEVEL`, currently `3`) — the stored hex is parsed to
decide *whether* to tint, not for an exact color.

Free-form text goes after `--` (so it may contain commas/spaces); structured
fields go as `key=value` args.

### Claude Code hooks

Wire `pane-notify` into `~/.claude/settings.json` so Claude sessions surface
automatically:

```jsonc
"SessionStart":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "/path/to/pane-notify set key=claude -- idle" }]}],
"UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "/path/to/pane-notify set key=claude -- working" }]}],
"Stop":             [{ "matcher": "", "hooks": [{ "type": "command", "command": "/path/to/pane-notify set key=claude level=info -- waiting" }]}],
"Notification":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "/path/to/pane-notify set key=claude level=warn -- waiting" }]}],
"SessionEnd":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "/path/to/pane-notify clear key=claude" }]}]
```

The badge glyphs the plugin renders for the `claude` state: `○` idle, `◐`
working, `●` waiting (yellow), `✗` error — overridden by a bell-colored `●`
while attention is unacknowledged.

To tint Claude panes (orange), set the `color` key on the activity hooks and
clear it on `SessionEnd` (alongside the commands above):

```jsonc
"SessionStart": ... "/path/to/pane-notify set key=color -- d97757"   // bare hex — no '#' (shell comment)
"SessionEnd":   ... "/path/to/pane-notify clear key=color"
```

### Routing

A message must be **targeted at the plugin's URL** (`zellij pipe --plugin <url>`):
broadcasting (no `--plugin`) does *not* reach the plugin in practice. `pane-notify`
resolves the URL in this order:

1. `$SESSIONER_PLUGIN`, if set (e.g. `file:/abs/path/to/zellij-sessioner.wasm`).
2. Otherwise, the `LaunchOrFocusPlugin "...sessioner....wasm"` entry in your zellij
   config (`$ZELLIJ_CONFIG_FILE`, else `$XDG_CONFIG_HOME/zellij/config.kdl`, else
   `~/.config/zellij/config.kdl`) — so your **keybind URL is the single source of
   truth** and you don't have to duplicate it.

The URL must match the one your keybind uses, or `--plugin` launches a *separate*
instance (zellij keys plugins by URL + configuration). If no URL can be resolved,
`pane-notify` falls back to a (best-effort) broadcast; outside zellij it's a no-op.

### Keeping it resident

The annotation store lives only in the running plugin's memory (its lifetime is
the zellij session's lifetime — by design, since closing zellij also kills every
pane being tracked). For annotations to accumulate while you're away from the
sessioner, the plugin must be **running** when the events arrive. Targeting by URL
helps here: `pane-notify` launches the plugin if it isn't already running. Note a
freshly launched instance starts empty, so loading it once per session (via your
keybind or layout) keeps state continuous.

## Preview

Pressing `p` expands the plugin to full screen width and shows a live, in-color
preview of the selected session's focused pane beside the list. Since the plugin
API can only read pane content for its *own* session, the preview shells out (via
the `RunCommands` permission) to:

```
ZELLIJ_SESSION_NAME=<session> zellij action dump-screen --pane-id terminal_<id> --ansi
```

The dump runs asynchronously; its output arrives back as a `RunCommandResult`
event, is cached per pane, and is re-fetched on the refresh timer to stay
current. The `--ansi` output is rendered as raw escape codes so terminal colors
are preserved (the `Text` UI API only supports theme-palette colors). Toggling
the preview off restores the plugin's original floating-pane size.

## Pane title freshness

Zellij only pushes `SessionUpdate` events (the only source of cross-session pane
data) on session-level changes — not when a pane title changes within a session.
The plugin API does not provide a way to request fresh session data on demand.
This is the same limitation the built-in session manager has; see
[zellij-org/zellij#4765](https://github.com/zellij-org/zellij/issues/4765).

As a workaround, the plugin periodically renames its own pane title (cycling a
spinner), which counts as a pane change and nudges Zellij into pushing a fresh
`SessionUpdate`. This keeps the displayed titles reasonably up to date.

## Releasing

1. Bump the version in `Cargo.toml`
2. Run `cargo build` to update `Cargo.lock`
3. Commit: `git commit -am "Release vX.Y.Z"`
4. Tag: `git tag vX.Y.Z`
5. Push: `git push && git push --tags`

GitHub Actions will build the `.wasm` and create a release automatically.

## What about the name?

The name follows the German convention of deriving a profession from the task
performed: a *Sessioner* is one who manages sessions, just as a *Fensterputzer*
is one who cleans windows.

## Credits

This plugin was vibe-coded with [Claude Code](https://claude.ai/code).
