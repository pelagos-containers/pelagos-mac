//! Configuration: locate the pelagos binary and VM image artifacts.
//!
//! Resolution order:
//!   1. `$XDG_CONFIG_HOME/pelagos/config.toml` (or `~/.config/pelagos/config.toml`)
//!   2. Environment variables: PELAGOS_BIN, PELAGOS_KERNEL, PELAGOS_INITRD,
//!      PELAGOS_DISK, PELAGOS_CMDLINE, PELAGOS_MEMORY_MIB
//!   3. Artifacts adjacent to the `pelagos` binary found on PATH
//!   4. `./out/` relative to CWD (dev layout after `make image`)

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub pelagos_bin: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub disk: PathBuf,
    pub cmdline: String,
    /// VM memory in MiB.  Default 4096; override via `PELAGOS_MEMORY_MIB` env var
    /// or `memory_mib = "4096"` in config.toml.
    pub memory_mib: usize,
}

/// Minimal TOML parser — only handles `key = "value"` lines.
fn parse_toml_str(src: &str, key: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('=') {
                let val = rest.trim().trim_matches('"');
                return Some(val.to_string());
            }
        }
    }
    None
}

impl Config {
    pub fn load() -> Result<Self, String> {
        // 1. Config file
        if let Some(cfg) = Self::from_file() {
            return Ok(cfg);
        }
        // 2. Environment variables
        if let Some(cfg) = Self::from_env() {
            return Ok(cfg);
        }
        // 3. Adjacent to pelagos on PATH
        if let Some(cfg) = Self::from_path_sibling() {
            return Ok(cfg);
        }
        // 4. ./out/ dev layout
        if let Some(cfg) = Self::from_dev_out() {
            return Ok(cfg);
        }
        Err(
            "Cannot find VM image artifacts. Create ~/.config/pelagos/config.toml or run from \
             the repo root after 'make image'. See 'pelagos-docker --help' for details."
                .into(),
        )
    }

    fn from_file() -> Option<Self> {
        let path = config_file_path()?;
        let src = std::fs::read_to_string(&path).ok()?;
        let pelagos_bin = parse_toml_str(&src, "pelagos_bin")
            .map(PathBuf::from)
            .or_else(find_pelagos_on_path)?;
        let kernel = PathBuf::from(parse_toml_str(&src, "kernel")?);
        let initrd = PathBuf::from(parse_toml_str(&src, "initrd")?);
        let disk = PathBuf::from(parse_toml_str(&src, "disk")?);
        let cmdline = parse_toml_str(&src, "cmdline").unwrap_or_else(|| "console=hvc0".into());
        let memory_mib = parse_toml_str(&src, "memory_mib")
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        if kernel.exists() && initrd.exists() && disk.exists() {
            Some(Self {
                pelagos_bin,
                kernel,
                initrd,
                disk,
                cmdline,
                memory_mib,
            })
        } else {
            None
        }
    }

    fn from_env() -> Option<Self> {
        let pelagos_bin = std::env::var("PELAGOS_BIN")
            .ok()
            .map(PathBuf::from)
            .or_else(find_pelagos_on_path)?;
        let kernel = PathBuf::from(std::env::var("PELAGOS_KERNEL").ok()?);
        let initrd = PathBuf::from(std::env::var("PELAGOS_INITRD").ok()?);
        let disk = PathBuf::from(std::env::var("PELAGOS_DISK").ok()?);
        let cmdline = std::env::var("PELAGOS_CMDLINE").unwrap_or_else(|_| "console=hvc0".into());
        let memory_mib = std::env::var("PELAGOS_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        if kernel.exists() && initrd.exists() && disk.exists() {
            Some(Self {
                pelagos_bin,
                kernel,
                initrd,
                disk,
                cmdline,
                memory_mib,
            })
        } else {
            None
        }
    }

    fn from_path_sibling() -> Option<Self> {
        let bin = find_pelagos_on_path()?;
        let dir = bin.parent()?;
        let kernel = dir.join("vmlinuz");
        let initrd = dir.join("initramfs-custom.gz");
        let disk = dir.join("root.img");
        let memory_mib = std::env::var("PELAGOS_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        if kernel.exists() && initrd.exists() && disk.exists() {
            Some(Self {
                pelagos_bin: bin,
                kernel,
                initrd,
                disk,
                cmdline: "console=hvc0".into(),
                memory_mib,
            })
        } else {
            None
        }
    }

    fn from_dev_out() -> Option<Self> {
        // In dev, the binaries live next to each other in the same target dir.
        // Try to find `pelagos` adjacent to this binary, then fall back to PATH.
        let self_exe = std::env::current_exe().ok()?;
        let bin_dir = self_exe.parent()?;
        let sibling = bin_dir.join("pelagos");
        let bin = if sibling.is_file() {
            sibling
        } else {
            find_pelagos_on_path()?
        };
        let out = PathBuf::from("out");
        let kernel = out.join("vmlinuz");
        let initrd = out.join("initramfs-custom.gz");
        let disk = out.join("root.img");
        let memory_mib = std::env::var("PELAGOS_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096);
        if kernel.exists() && initrd.exists() && disk.exists() {
            Some(Self {
                pelagos_bin: bin,
                kernel,
                initrd,
                disk,
                cmdline: "console=hvc0".into(),
                memory_mib,
            })
        } else {
            None
        }
    }

    /// Build the common prefix args that every `pelagos` invocation requires.
    pub fn pelagos_prefix_args(&self) -> Vec<std::ffi::OsString> {
        vec![
            "--kernel".into(),
            self.kernel.as_os_str().to_owned(),
            "--initrd".into(),
            self.initrd.as_os_str().to_owned(),
            "--disk".into(),
            self.disk.as_os_str().to_owned(),
            "--cmdline".into(),
            self.cmdline.as_str().into(),
            "--memory".into(),
            self.memory_mib.to_string().into(),
        ]
    }
}

fn config_file_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("pelagos").join("config.toml"))
}

fn find_pelagos_on_path() -> Option<PathBuf> {
    std::env::var("PATH").ok()?.split(':').find_map(|dir| {
        let p = PathBuf::from(dir).join("pelagos");
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::parse_toml_str;

    #[test]
    fn parse_toml_str_basic() {
        let src = r#"
kernel  = "/usr/local/share/pelagos/vmlinuz"
initrd  = "/usr/local/share/pelagos/initramfs-custom.gz"
cmdline = "console=hvc0"
"#;
        assert_eq!(
            parse_toml_str(src, "kernel"),
            Some("/usr/local/share/pelagos/vmlinuz".into())
        );
        assert_eq!(parse_toml_str(src, "cmdline"), Some("console=hvc0".into()));
        assert_eq!(parse_toml_str(src, "missing"), None);
    }
}
