//! Runner trait and platform implementations.
//!
//! The Runner trait abstracts over the underlying pelagos binary invocation so
//! that M5 can add a `LinuxRunner` without touching app or ui code.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Mirrors the JSON shape emitted by `pelagos ps --format json`.
///
/// Field names match `ContainerState` in pelagos/src/cli/mod.rs exactly.
/// Optional fields use `#[serde(default)]` for forward/backward compatibility.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Container {
    pub name: String,
    pub rootfs: String,
    pub status: String, // "running" | "exited"
    // pid, exit_code, and command are part of the wire format and may be used in M2+.
    #[allow(dead_code)]
    pub pid: i32,
    pub started_at: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    #[allow(dead_code)]
    pub command: Vec<String>,
}

// ---------------------------------------------------------------------------
// Runner trait
// ---------------------------------------------------------------------------

pub trait Runner {
    /// List containers.  `all` maps to `--all` flag.
    fn ps(&self, all: bool) -> anyhow::Result<Vec<Container>>;
    /// Return true when the VM daemon is alive.
    fn vm_status(&self) -> bool;
    /// Enumerate available profiles from the on-disk state directory.
    fn profiles(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// Timeout helper
// ---------------------------------------------------------------------------

/// Run a `Command` and return its output, or `None` if it does not complete
/// within `timeout`.
///
/// The subprocess is spawned on a background thread.  If the timeout fires the
/// thread is abandoned — the subprocess continues running but the caller
/// receives `None` and can proceed without blocking.  This keeps the TUI
/// responsive when the guest daemon is occupied (e.g. serving an interactive
/// container over vsock).
fn output_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Some(out),
        Ok(Err(e)) => {
            log::debug!("output_timeout: command error: {}", e);
            None
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            log::debug!("output_timeout: timed out after {:?}", timeout);
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

// How long to wait for a pelagos subprocess before giving up.
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// MacOsRunner
// ---------------------------------------------------------------------------

pub struct MacOsRunner {
    pub profile: String,
}

impl MacOsRunner {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
        }
    }
}

impl Runner for MacOsRunner {
    fn ps(&self, all: bool) -> anyhow::Result<Vec<Container>> {
        let mut cmd = Command::new("pelagos");
        cmd.arg("--profile").arg(&self.profile);
        cmd.arg("ps").arg("--json");
        if all {
            cmd.arg("--all");
        }

        let Some(out) = output_timeout(cmd, CMD_TIMEOUT) else {
            log::debug!("pelagos ps timed out — VM daemon busy");
            return Ok(Vec::new());
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            log::debug!("pelagos ps failed: {}", stderr.trim());
            // VM likely stopped — return empty list rather than error.
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim();

        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        // pelagos ps --format json outputs a JSON array.
        match serde_json::from_str::<Vec<Container>>(trimmed) {
            Ok(v) => Ok(v),
            Err(e) => {
                log::debug!(
                    "pelagos ps JSON parse error: {} — output was: {}",
                    e,
                    trimmed
                );
                Ok(Vec::new())
            }
        }
    }

    fn vm_status(&self) -> bool {
        // `pelagos vm status` exits 0 when running, 1 when stopped.
        let mut cmd = Command::new("pelagos");
        cmd.arg("--profile")
            .arg(&self.profile)
            .arg("vm")
            .arg("status");

        let ok = output_timeout(cmd, CMD_TIMEOUT)
            .map(|o| o.status.success())
            .unwrap_or(false);
        log::trace!("vm_status profile={} running={}", self.profile, ok);
        ok
    }

    fn profiles(&self) -> Vec<String> {
        let mut result = vec!["default".to_string()];

        // Replicate pelagos_base() / profile_dir() from state.rs using std only.
        let base = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            PathBuf::from(xdg).join("pelagos")
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".local/share/pelagos")
        } else {
            log::warn!("profiles: neither XDG_DATA_HOME nor HOME is set");
            return result;
        };

        let profiles_dir = base.join("profiles");
        let Ok(entries) = std::fs::read_dir(&profiles_dir) else {
            // profiles/ dir simply doesn't exist yet — only "default" is available.
            return result;
        };

        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    let name = name.to_string();
                    if name != "default" {
                        result.push(name);
                    }
                }
            }
        }

        result.sort();
        result
    }
}
