# pelagos-tui — Implementation Plan

**Epic:** [#149](https://github.com/pelagos-containers/pelagos-mac/issues/149)
**Approach:** Alternative A — Rust + ratatui + crossterm
**Status:** M1 in progress

---

## Design Decisions (locked)

| Decision | Choice | Rationale |
|---|---|---|
| Library | ratatui + crossterm | Pure Rust, fits workspace, same stack |
| Data source | `pelagos ps --json` subprocess | Works identically on macOS and Linux |
| Interaction model | Monitoring-first, operational later | M1–M2 read-only; M3+ adds write ops |
| Run/create UX | Command palette (not modal form) | Developers know the flags; lower design cost |
| Profile UX | Modeline shows current; `p` opens picker overlay | Emacs-modeline inspired; close at hand, out of main panel |
| Exec into container | Not in scope (any milestone) | Dropped by design |
| Platform | macOS first; Linux via Runner trait swap | Runner abstraction makes Linux a single new struct |

---

## Layout

```
┌─ pelagos ─────────────────────────────────────────────────┐
│                                                           │
│  NAME          STATUS    IMAGE                  UPTIME    │
│ ▶ webserver    running   nginx:alpine            2m 14s   │
│   worker       running   myapp:latest            18m 02s  │
│   dbmigrate    exited    postgres:16             1h 04m   │
│                                                           │
│                                                           │
│  [q]quit  [a]all  [j/k]nav  [p]profile  [?]help          │
├───────────────────────────────────────────────────────────┤
│  default │ VM: running │ 3 containers │ ↻ 1s ago          │
└───────────────────────────────────────────────────────────┘
```

- **Main panel:** container table, fills available height
- **Hint bar:** one line above modeline, static keybind legend
- **Modeline:** always-visible bottom bar (emacs-inspired): profile | VM status | container count | last refresh age
- **Overlays:** profile picker, help — centered popups rendered over main panel

---

## Crate Structure

New crate `pelagos-tui` added to the workspace at `pelagos-tui/`.

```
pelagos-tui/
  src/
    main.rs       — terminal setup/teardown, top-level event loop
    app.rs        — App state struct, update logic, Mode enum
    ui.rs         — ratatui layout and rendering (panel + modeline + overlays)
    runner.rs     — Runner trait + MacOsRunner impl
  Cargo.toml
```

Add to workspace `Cargo.toml` members list: `"pelagos-tui"`.

### Cargo.toml dependencies

```toml
[package]
name = "pelagos-tui"
version.workspace = true
edition.workspace = true

[dependencies]
ratatui = "0.29"
crossterm = "0.28"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
log.workspace = true
env_logger.workspace = true
anyhow = "1"
```

---

## Data Model

### `runner.rs`

```rust
pub trait Runner {
    fn ps(&self, all: bool) -> anyhow::Result<Vec<Container>>;
    fn vm_status(&self) -> bool;
    fn profiles(&self) -> Vec<String>;
    // M2+:
    // fn stop(&self, name: &str) -> anyhow::Result<()>;
    // fn rm(&self, name: &str, force: bool) -> anyhow::Result<()>;
    // fn logs(&self, name: &str) -> anyhow::Result<impl BufRead>;
}

pub struct MacOsRunner {
    pub profile: String,
}

impl Runner for MacOsRunner {
    fn ps(&self, all: bool) -> anyhow::Result<Vec<Container>> {
        // spawn: pelagos --profile <profile> ps --json [--all]
        // parse stdout as serde_json::from_str::<Vec<Container>>
    }

    fn vm_status(&self) -> bool {
        // spawn: pelagos --profile <profile> vm status
        // exit code 0 = running, non-zero = stopped
    }

    fn profiles(&self) -> Vec<String> {
        // read dir: ~/.local/share/pelagos/profiles/
        // return subdirectory names; always include "default"
        // see pelagos-mac/src/state.rs profile_dir() for the path
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Container {
    pub name: String,
    pub status: String,          // "running" | "exited"
    pub pid: Option<u32>,
    pub rootfs: String,          // image reference
    pub started_at: Option<String>,
}
```

Note: `Container` fields match `pelagos ps --json` output. Check `pelagos/src/cli/ps.rs`
and `pelagos/src/cli/mod.rs` (`ContainerState`) for exact field names and types.

### `app.rs`

```rust
pub enum Mode {
    Normal,
    ProfilePicker,
    // M3: CommandPalette { input: String },
    // M2: Help,
}

pub struct App {
    pub mode: Mode,
    pub containers: Vec<Container>,
    pub selected: usize,
    pub show_all: bool,
    pub profile: String,
    pub profiles: Vec<String>,
    pub vm_running: bool,
    pub last_refresh: std::time::Instant,
    pub refresh_interval: std::time::Duration,
    pub profile_picker_selected: usize,
}

impl App {
    pub fn new(profile: String, profiles: Vec<String>) -> Self { ... }
    pub fn refresh(&mut self, runner: &impl Runner) { ... }
    pub fn on_key(&mut self, key: KeyEvent, runner: &impl Runner) { ... }
    pub fn selected_container(&self) -> Option<&Container> { ... }
}
```

---

## Event Loop (`main.rs`)

```
setup terminal (raw mode, alternate screen)

loop:
  if tick elapsed (2s):
    app.refresh(runner)        // ps --json + vm status

  if crossterm event available:
    match event:
      KeyEvent → app.on_key(key, runner)
      Resize   → terminal.autoresize()

  terminal.draw(|f| ui::render(f, &app))

  if app.should_quit: break

restore terminal
```

---

## Keybindings — M1

| Key | Mode | Action |
|---|---|---|
| `j` / `↓` | Normal | select next container |
| `k` / `↑` | Normal | select previous container |
| `a` | Normal | toggle `--all`, immediate refresh |
| `p` | Normal | open profile picker overlay |
| `j` / `↓` | ProfilePicker | next profile |
| `k` / `↑` | ProfilePicker | previous profile |
| `Enter` | ProfilePicker | switch to selected profile |
| `Esc` / `p` | ProfilePicker | close without switching |
| `q` / `Ctrl-C` | Normal | quit |

Keys shown in hint bar but not yet wired (M2+): `s` stop, `d` delete, `l` logs, `r` run, `?` help.

---

## Rendering (`ui.rs`)

Three layers composed with ratatui:

1. **Main layout** — vertical split: `[table][hint_bar][modeline]`
2. **Table** — ratatui `Table` widget, columns: NAME / STATUS / IMAGE / UPTIME
   - Running rows: green status text
   - Exited rows: dim/red status text
   - Selected row: highlighted (reversed)
3. **Modeline** — single `Paragraph` line:
   `  {profile}  │  VM: {running|stopped}  │  {n} containers  │  ↻ {age}  `
4. **Profile picker overlay** (when `mode == ProfilePicker`):
   - Centered `Block` with border, lists profiles, highlights selected
   - Rendered last so it appears on top

---

## Milestones

| Milestone | Scope |
|---|---|
| **M1** (current) | Container list, modeline, profile picker, `j/k/a/p/q` |
| **M2** | Log viewer (bottom split pane, follow mode), `s` stop, `d` rm |
| **M3** | Command palette — `r` turns modeline into input for `pelagos run ...` |
| **M4** | Image list tab (`i`), volume list tab (`v`) — needs pelagos JSON output |
| **M5** | Linux runner (`LinuxRunner`), cross-platform build + test |

---

## M1 Definition of Done

- [ ] `pelagos-tui` crate builds cleanly in workspace (`cargo build -p pelagos-tui`)
- [ ] Container list renders and live-refreshes every 2s
- [ ] `j`/`k` navigate, `a` toggles `--all`, `q` quits
- [ ] Modeline shows profile, VM status, container count, refresh age
- [ ] Profile picker opens with `p`, switches profile on `Enter`, closes on `Esc`
- [ ] Switching profile immediately refreshes container list
- [ ] Empty state (VM stopped, no containers) renders gracefully — no crash
- [ ] Terminal fully restored on quit (no leftover artifacts, no raw mode leak)
- [ ] `cargo clippy -p pelagos-tui -- -D warnings` clean
- [ ] Tested live with VM running and at least one container

---

## Key References in Codebase

| What | Where |
|---|---|
| Profile directory path | `pelagos-mac/src/state.rs` → `profile_dir()`, `pelagos_base()` |
| `pelagos ps --json` output shape | `pelagos/src/cli/mod.rs` → `ContainerState` struct |
| `pelagos ps` JSON flag wire format | `pelagos-mac/pelagos-guest/src/main.rs` → `GuestCommand::Ps` |
| VM status check | `pelagos-mac/src/state.rs` → `StateDir::is_daemon_alive()` |
| Workspace Cargo.toml | `/Users/christopherbrown/Projects/pelagos-mac/Cargo.toml` |

---

## Notes

- Do not use `eprintln!`/`println!` for diagnostics — use `log::` macros (project rule).
- `pelagos-tui` is macOS-only for M1–M4; add `[target.'cfg(target_os = "macos")']`
  if needed, but the Runner abstraction means the crate itself is platform-agnostic.
- The TUI must handle the case where the pelagos binary is not in PATH gracefully.
- Refresh should not block the UI — consider spawning refresh in a thread and
  communicating via a channel, or accept the brief block for M1 (ps is fast).
