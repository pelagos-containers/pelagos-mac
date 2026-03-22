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

        if app.should_quit {
            break;
        }
    }

    Ok(())
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
