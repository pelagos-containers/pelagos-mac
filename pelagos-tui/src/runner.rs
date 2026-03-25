//! Runner trait and platform implementations.
//!
//! The Runner trait abstracts over the underlying pelagos binary invocation so
//! that M5 can add a `LinuxRunner` without touching app or ui code.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Subset of `SpawnConfig` from pelagos/src/cli/mod.rs that we surface in the
/// inspect overlay.  Fields are all `#[serde(default)]` so old state files
/// (without a `spawn_config` key) deserialise cleanly.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SpawnConfigView {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub bind: Vec<String>,
    #[serde(default)]
    pub bind_ro: Vec<String>,
    #[serde(default)]
    pub volume: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

/// Mirrors the JSON shape emitted by `pelagos ps --json`.
///
/// Field names match `ContainerState` in pelagos/src/cli/mod.rs exactly.
/// Optional/collection fields use `#[serde(default)]` for forward/backward
/// compatibility — absent keys deserialise to empty/None.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Container {
    pub name: String,
    pub rootfs: String,
    pub status: String, // "running" | "exited"
    pub pid: i32,
    pub started_at: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub command: Vec<String>,
    /// Port mappings from `pelagos run -p HOST:CONTAINER` (e.g. `["8080:80"]`).
    #[serde(default)]
    pub ports: Vec<String>,
    // ---- fields present in ContainerState but absent from ContainerSnapshot ----
    /// Bridge IP address (bridge networking).
    #[serde(default)]
    pub bridge_ip: Option<String>,
    /// Per-network IP map: network_name → IP.
    #[serde(default)]
    pub network_ips: HashMap<String, String>,
    /// Key-value labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Captured stdout log path (detached containers).
    #[serde(default)]
    pub stdout_log: Option<String>,
    /// Captured stderr log path (detached containers).
    #[serde(default)]
    pub stderr_log: Option<String>,
    /// Original spawn configuration (present after first `pelagos run`).
    #[serde(default)]
    pub spawn_config: Option<SpawnConfigView>,
}

// ---------------------------------------------------------------------------
// Runner trait
// ---------------------------------------------------------------------------

// ps and vm_status will be used by LinuxRunner (M5); kept here for the trait contract.
#[allow(dead_code)]
pub trait Runner {
    /// List containers.  `all` maps to `--all` flag.
    fn ps(&self, all: bool) -> anyhow::Result<Vec<Container>>;
    /// Return true when the VM daemon is alive.
    fn vm_status(&self) -> bool;
    /// Enumerate available profiles from the on-disk state directory.
    fn profiles(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// MacOsRunner
// ---------------------------------------------------------------------------

pub struct MacOsRunner {
    #[allow(dead_code)]
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

        let out = cmd.output()?;

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
        let ok = Command::new("pelagos")
            .arg("--profile")
            .arg(&self.profile)
            .arg("vm")
            .arg("status")
            .output()
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
