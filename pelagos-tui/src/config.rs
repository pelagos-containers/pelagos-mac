//! Per-profile TUI configuration loaded from `tui.conf`.
//!
//! File location:
//!   default profile → `~/.local/share/pelagos/tui.conf`
//!   named profile   → `~/.local/share/pelagos/profiles/<name>/tui.conf`
//!
//! Respects `$XDG_DATA_HOME` (falls back to `$HOME/.local/share`).
//!
//! ```text
//! # tui.conf
//! default_image  = alpine
//! default_it_cmd = /bin/sh
//! ```

use std::io;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// TuiConfig
// ---------------------------------------------------------------------------

/// Per-profile TUI defaults.  Missing keys fall back to the built-in defaults.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// Image used when preseeding an interactive (`r`) run. Default: `alpine`.
    pub default_image: String,
    /// Command used when preseeding an interactive (`r`) run. Default: `/bin/sh`.
    pub default_it_cmd: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            default_image: "alpine".to_string(),
            default_it_cmd: "/bin/sh".to_string(),
        }
    }
}

impl TuiConfig {
    /// Load `tui.conf` for the given profile.  Returns built-in defaults if
    /// the file does not exist.
    pub fn load(profile: &str) -> Self {
        let path = match profile_dir(profile) {
            Ok(d) => d.join("tui.conf"),
            Err(e) => {
                log::warn!("tui config: cannot resolve profile dir: {}", e);
                return Self::default();
            }
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!("tui config: failed to read {:?}: {}", path, e);
                return Self::default();
            }
        };

        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let val = val.trim();
            match key {
                "default_image" => cfg.default_image = val.to_string(),
                "default_it_cmd" => cfg.default_it_cmd = val.to_string(),
                _ => {}
            }
        }
        log::debug!("tui config loaded from {:?}: {:?}", path, cfg);
        cfg
    }
}

// ---------------------------------------------------------------------------
// Path helpers — mirrors the logic in pelagos-mac/src/state.rs
// ---------------------------------------------------------------------------

fn pelagos_base() -> io::Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/share")
        });
    Ok(base.join("pelagos"))
}

fn profile_dir(name: &str) -> io::Result<PathBuf> {
    let base = pelagos_base()?;
    if name == "default" {
        Ok(base)
    } else {
        Ok(base.join("profiles").join(name))
    }
}
