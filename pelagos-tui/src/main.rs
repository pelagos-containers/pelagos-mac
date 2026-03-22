//! pelagos-tui — terminal UI for the pelagos container runtime.
//!
//! Entry point: sets up the terminal, runs the event loop, restores on exit.

mod app;
mod runner;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use runner::{MacOsRunner, Runner};

fn main() -> anyhow::Result<()> {
    // Initialise env_logger.  RUST_LOG controls verbosity; default is warn so
    // the TUI screen is not polluted by log output.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Determine the initial profile.  Accept `--profile <name>` or
    // `PELAGOS_PROFILE` env var; fall back to "default".
    let profile = resolve_profile();

    // Build runner and collect initial profile list.
    let runner = MacOsRunner::new(&profile);
    let profiles = runner.profiles();

    // Initialise app state.
    let mut app = App::new(profile, profiles);

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the event loop; capture the result so we can restore the terminal
    // even if the loop returns an error.
    let result = run_loop(&mut terminal, &mut app);

    // Restore terminal unconditionally.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    // Do an immediate refresh so the screen is populated on first draw.
    let runner = MacOsRunner::new(&app.profile);
    app.refresh(&runner);

    let tick = Duration::from_millis(250); // poll interval

    loop {
        // Rebuild runner from current profile (may have changed via picker).
        let runner = MacOsRunner::new(&app.profile);

        // Auto-refresh when the interval has elapsed.
        if app.last_refresh.elapsed() >= app.refresh_interval {
            app.refresh(&runner);
        }

        // Draw.
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with a short timeout so we keep refreshing.
        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Ctrl-C is handled inside on_key, but also intercept here
                    // as a safety net in case the app struct misses it.
                    if key.code == KeyCode::Char('c')
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else {
                        app.on_key(key, &runner);
                    }
                }
                Event::Resize(_, _) => {
                    // crossterm handles resize automatically; we just need to
                    // redraw on the next tick, which we always do.
                }
                _ => {}
            }
        }

        // Command palette: execute pending run command.
        if let Some(input) = app.pending_run.take() {
            execute_run(app, &runner, &input);
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Run command execution
// ---------------------------------------------------------------------------

/// Dispatch a palette `run` command.
///
/// - Non-interactive: run silently via `.output()`; TUI never leaves alternate
///   screen.  On success the container list refreshes immediately.
/// - Interactive (`-i` / `--interactive` / combined flags): open the command
///   in a new terminal window so the TUI keeps running alongside.  The terminal
///   is chosen by `PELAGOS_TERMINAL` env var, then `$TERM_PROGRAM` auto-detect,
///   then Apple Terminal as a fallback.
fn execute_run(app: &mut App, runner: &impl Runner, input: &str) {
    let args: Vec<&str> = input.split_whitespace().collect();
    log::info!("palette run: profile={} args={:?}", app.profile, args);

    let interactive = args
        .iter()
        .any(|a| *a == "-i" || *a == "--interactive" || *a == "-it" || *a == "-ti");

    if interactive {
        let full_cmd = format!(
            "pelagos --profile {} run {}",
            shell_escape(&app.profile),
            input
        );
        match open_in_terminal(&full_cmd) {
            Ok(()) => {
                // Refresh after a brief moment so the new container appears.
                app.refresh(runner);
            }
            Err(e) => {
                let msg = format!("terminal launch: {}", e);
                log::warn!("{}", msg);
                app.status_message = Some(msg);
            }
        }
        return;
    }

    // Non-interactive: run silently, capture output.
    let result = std::process::Command::new("pelagos")
        .arg("--profile")
        .arg(&app.profile)
        .arg("run")
        .args(&args)
        .output();

    match result {
        Ok(out) if out.status.success() => {
            app.refresh(runner);
        }
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if msg.is_empty() {
                format!("run failed (exit {})", out.status)
            } else {
                format!("run: {}", msg)
            };
            log::warn!("{}", msg);
            app.status_message = Some(msg);
        }
        Err(e) => {
            let msg = format!("run: {}", e);
            log::warn!("{}", msg);
            app.status_message = Some(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal launcher
// ---------------------------------------------------------------------------

/// Open `cmd` in a new terminal window/tab.
///
/// Resolution order:
/// 1. `PELAGOS_TERMINAL` env var — treated as the terminal binary name.
///    The command is appended as `-e sh -c "<cmd>"` (works for most emulators).
/// 2. `$TERM_PROGRAM` auto-detect — handles Apple Terminal, iTerm2, Ghostty,
///    WarpTerminal, kitty, and Alacritty.
/// 3. Apple Terminal via `osascript` as a last resort.
fn open_in_terminal(cmd: &str) -> anyhow::Result<()> {
    // 1. User override.
    if let Ok(term_bin) = std::env::var("PELAGOS_TERMINAL") {
        return spawn_generic(&term_bin, cmd);
    }

    // 2. Auto-detect.
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    log::debug!("open_in_terminal: TERM_PROGRAM={:?}", term_program);

    match term_program.as_str() {
        "Apple_Terminal" => osascript_apple_terminal(cmd),
        "iTerm.app" => osascript_iterm(cmd),
        "ghostty" => spawn_generic("ghostty", cmd),
        "WarpTerminal" => osascript_apple_terminal(cmd), // Warp has no CLI launcher; fall back
        "kitty" => spawn_generic("kitty", cmd),
        "alacritty" => spawn_generic("alacritty", cmd),
        _ => {
            log::debug!(
                "open_in_terminal: unrecognised TERM_PROGRAM {:?}, falling back to Apple Terminal",
                term_program
            );
            osascript_apple_terminal(cmd)
        }
    }
}

/// Open a new Terminal.app window/tab running `cmd`.
fn osascript_apple_terminal(cmd: &str) -> anyhow::Result<()> {
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        escape_applescript(cmd)
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn()?;
    Ok(())
}

/// Open a new iTerm2 window running `cmd`.
fn osascript_iterm(cmd: &str) -> anyhow::Result<()> {
    let script = format!(
        "tell application \"iTerm\" to create window with default profile command \"{}\"",
        escape_applescript(cmd)
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn()?;
    Ok(())
}

/// Open `cmd` in a generic terminal that accepts `-e sh -c <cmd>`.
///
/// Works for: ghostty, kitty, alacritty, and most X11/Wayland emulators.
fn spawn_generic(term_bin: &str, cmd: &str) -> anyhow::Result<()> {
    std::process::Command::new(term_bin)
        .args(["-e", "sh", "-c", cmd])
        .spawn()?;
    Ok(())
}

/// Escape a string for embedding inside an AppleScript string literal.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Minimal shell-escape: wrap in single quotes, escaping any single quotes.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Profile resolution
// ---------------------------------------------------------------------------

fn resolve_profile() -> String {
    // Simple arg parse: look for --profile <name> in argv.
    let args: Vec<String> = std::env::args().collect();
    let mut iter = args.iter().skip(1).peekable();
    while let Some(arg) = iter.next() {
        if arg == "--profile" || arg == "-p" {
            if let Some(val) = iter.next() {
                return val.clone();
            }
        } else if let Some(val) = arg.strip_prefix("--profile=") {
            return val.to_string();
        }
    }

    // Fall back to env var, then "default".
    std::env::var("PELAGOS_PROFILE").unwrap_or_else(|_| "default".to_string())
}
