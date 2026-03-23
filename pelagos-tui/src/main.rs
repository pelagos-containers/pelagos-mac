//! pelagos-tui — terminal UI for the pelagos container runtime.
//!
//! Entry point: sets up the terminal, runs the event loop, restores on exit.
//!
//! # Architecture
//!
//! A background thread runs `pelagos --profile <p> subscribe` and reads NDJSON
//! events from its stdout.  Events are forwarded to the main event loop via an
//! `mpsc::Receiver<SubscriptionMsg>`.  The main loop never calls blocking
//! runner methods, so it stays responsive regardless of what the guest daemon
//! is doing (including serving interactive containers).
//!
//! One-shot operations (run, profile list) use short-lived `pelagos` subprocesses
//! spawned in a background thread so the event loop never blocks on them either.

mod app;
mod runner;
mod ui;

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, SubscriptionMsg};
use runner::{MacOsRunner, Runner};

/// Shared state that the subscription thread reads before each (re)connect.
#[derive(Default)]
struct SubConfig {
    profile: String,
    /// Bumped on profile switch or show_all toggle to force a reconnect.
    generation: u64,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let profile = resolve_profile();

    // Collect profile list (quick filesystem read — doesn't hit the daemon).
    let runner = MacOsRunner::new(&profile);
    let profiles = runner.profiles();

    let mut app = App::new(profile.clone(), profiles);

    // Start the subscription background thread.
    let (sub_tx, sub_rx) = mpsc::channel::<SubscriptionMsg>();
    let sub_config = std::sync::Arc::new(std::sync::Mutex::new(SubConfig {
        profile: profile.clone(),
        generation: 0,
    }));
    start_subscription_thread(sub_config.clone(), sub_tx);
    app.sub_config = Some(sub_config);

    // Install a panic hook that restores the terminal before printing the panic
    // message. Without this, a panic leaves raw mode + alternate screen active.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app, sub_rx);

    restore_terminal()?;

    result
}

fn restore_terminal() -> anyhow::Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(io::stdout(), LeaveAlternateScreen)?;
    crossterm::execute!(io::stdout(), crossterm::cursor::Show)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscription background thread
// ---------------------------------------------------------------------------

/// Spawn a thread that runs `pelagos --profile <p> subscribe` and forwards
/// NDJSON events to `tx`.  Reconnects automatically with exponential backoff
/// (1s → 2s → 4s … capped at 30s) when the connection drops.
fn start_subscription_thread(
    config: std::sync::Arc<std::sync::Mutex<SubConfig>>,
    tx: mpsc::Sender<SubscriptionMsg>,
) {
    std::thread::Builder::new()
        .name("subscription".into())
        .spawn(move || {
            let mut last_gen: u64 = u64::MAX; // force first connect
            let mut backoff = Duration::from_secs(1);
            loop {
                let (profile, gen) = {
                    let c = config.lock().unwrap();
                    (c.profile.clone(), c.generation)
                };
                let reconnect_forced = gen != last_gen;
                last_gen = gen;

                if reconnect_forced {
                    backoff = Duration::from_millis(0); // reconnect immediately on forced switch
                }

                match run_subscription(&profile, &tx, &config, gen) {
                    Ok(()) => {
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        log::debug!("subscription ended: {}", e);
                    }
                }
                let _ = tx.send(SubscriptionMsg::Disconnected);
                if backoff > Duration::ZERO {
                    log::debug!("subscription: reconnecting in {:?}", backoff);
                    std::thread::sleep(backoff);
                }
                backoff = (backoff * 2)
                    .max(Duration::from_secs(1))
                    .min(Duration::from_secs(30));
            }
        })
        .expect("failed to spawn subscription thread");
}

/// Run `pelagos subscribe` and forward each parsed event to `tx`.
/// Returns when the subprocess exits, the pipe breaks, or the generation changes.
fn run_subscription(
    profile: &str,
    tx: &mpsc::Sender<SubscriptionMsg>,
    config: &std::sync::Arc<std::sync::Mutex<SubConfig>>,
    gen: u64,
) -> anyhow::Result<()> {
    use std::io::BufRead;

    let mut child = std::process::Command::new("pelagos")
        .arg("--profile")
        .arg(profile)
        .arg("subscribe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::io::BufReader::new(stdout);

    for line in reader.lines() {
        // Abort early if profile/show_all changed while we were reading.
        if config.lock().unwrap().generation != gen {
            break;
        }
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<SubscriptionMsg>(&line) {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    break; // main thread gone
                }
            }
            Err(e) => {
                log::debug!("subscription: parse error: {} (line: {:?})", e, line);
            }
        }
    }

    let _ = child.wait();
    Ok(())
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    sub_rx: mpsc::Receiver<SubscriptionMsg>,
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(250);

    loop {
        // Drain all pending subscription events (non-blocking).
        loop {
            match sub_rx.try_recv() {
                Ok(msg) => app.apply_subscription(msg),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        terminal.draw(|f| ui::render(f, app))?;

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else {
                        app.on_key(key);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        // Command palette: execute pending run in a background thread so the
        // event loop never blocks.  The subscription thread will deliver the
        // ContainerStarted event when the container appears.
        if let Some(input) = app.pending_run.take() {
            let profile = app.profile.clone();
            let status_tx = app.status_tx.clone();
            std::thread::spawn(move || {
                execute_run_bg(&profile, &input, status_tx);
            });
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Run command execution (background thread — never blocks event loop)
// ---------------------------------------------------------------------------

/// Execute `pelagos run <args>` in a background thread.  On error, sends the
/// message back to the main loop via `status_tx` for display in the modeline.
/// On success, the subscription thread will deliver a ContainerStarted event.
fn execute_run_bg(profile: &str, input: &str, status_tx: Option<mpsc::SyncSender<String>>) {
    let args: Vec<&str> = input.split_whitespace().collect();
    log::info!("palette run: profile={} args={:?}", profile, args);

    // Interactive flags: open in a new terminal window so the TUI is unaffected.
    let interactive = args
        .iter()
        .any(|a| *a == "-i" || *a == "--interactive" || *a == "-it" || *a == "-ti");
    if interactive {
        if let Err(e) = open_in_terminal(profile, input) {
            send_status(&status_tx, format!("terminal launch: {}", e));
        }
        return;
    }

    let result = std::process::Command::new("pelagos")
        .arg("--profile")
        .arg(profile)
        .arg("run")
        .args(&args)
        .output();

    match result {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if msg.is_empty() {
                format!("run failed (exit {})", out.status)
            } else {
                format!("run: {}", msg)
            };
            log::warn!("{}", msg);
            send_status(&status_tx, msg);
        }
        Err(e) => {
            send_status(&status_tx, format!("run: {}", e));
        }
    }
}

fn send_status(tx: &Option<mpsc::SyncSender<String>>, msg: String) {
    if let Some(tx) = tx {
        let _ = tx.try_send(msg);
    }
}

// ---------------------------------------------------------------------------
// Terminal launcher (for interactive -i runs)
// ---------------------------------------------------------------------------

fn open_in_terminal(profile: &str, input: &str) -> anyhow::Result<()> {
    let cmd = format!("pelagos --profile {} run {}", shell_escape(profile), input);

    if let Ok(term_bin) = std::env::var("PELAGOS_TERMINAL") {
        return spawn_generic(&term_bin, &cmd);
    }

    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    match term_program.as_str() {
        "Apple_Terminal" => osascript_apple_terminal(&cmd),
        "iTerm.app" => osascript_iterm(&cmd),
        "ghostty" => spawn_generic("ghostty", &cmd),
        "WarpTerminal" => osascript_apple_terminal(&cmd),
        "kitty" => spawn_generic("kitty", &cmd),
        "alacritty" => spawn_generic("alacritty", &cmd),
        _ => osascript_apple_terminal(&cmd),
    }
}

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

fn spawn_generic(term_bin: &str, cmd: &str) -> anyhow::Result<()> {
    std::process::Command::new(term_bin)
        .args(["-e", "sh", "-c", cmd])
        .spawn()?;
    Ok(())
}

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Profile resolution
// ---------------------------------------------------------------------------

fn resolve_profile() -> String {
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
    std::env::var("PELAGOS_PROFILE").unwrap_or_else(|_| "default".to_string())
}
