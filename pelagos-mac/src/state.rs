//! Persistent VM state: PID file, Unix socket path, and mounts config
//! stored in ~/.local/share/pelagos/ (default profile) or
//! ~/.local/share/pelagos/profiles/<name>/ (named profiles).

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;

pub struct StateDir {
    pub pid_file: PathBuf,
    pub sock_file: PathBuf,
    /// Unix socket for the serial console relay (pelagos vm console).
    pub console_sock_file: PathBuf,
    pub mounts_file: PathBuf,
    /// Active port forwards for the running daemon (JSON).
    pub ports_file: PathBuf,
    /// Extra block device paths attached at boot (JSON).
    pub extra_disks_file: PathBuf,
}

// ---------------------------------------------------------------------------
// Per-profile VM configuration
// ---------------------------------------------------------------------------

/// Per-profile VM configuration loaded from `vm.conf` in the profile state
/// directory.  All fields are optional; absent fields fall back to CLI flags
/// (or their own defaults).
///
/// File format: simple `key = value` lines; `#` comments; blank lines ignored.
///
/// ```text
/// # vm.conf — written by build-build-image.sh
/// disk   = /path/to/build.img
/// kernel = /path/to/vmlinuz
/// initrd = /path/to/initramfs.gz
/// memory = 4096
/// cpus   = 4
/// ```
/// How `pelagos ping` checks VM readiness.
#[derive(Debug, Default, PartialEq, Clone)]
pub enum PingMode {
    /// Send a vsock ping to pelagos-guest (Alpine / container VM). Default.
    #[default]
    Vsock,
    /// Wait for SSH to be available (Ubuntu / non-pelagos OS profiles).
    Ssh,
}

#[derive(Debug, Default)]
pub struct VmProfileConfig {
    pub disk: Option<PathBuf>,
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub memory: Option<usize>,
    pub cpus: Option<usize>,
    /// How `pelagos ping` checks VM readiness. Default: `vsock`.
    pub ping_mode: PingMode,
    /// Override the kernel cmdline. When set, replaces the `--cmdline` default
    /// (`console=hvc0`). The `clock.utc=` token is still appended by the daemon.
    pub cmdline: Option<String>,
    /// Per-profile guest IPv4 for the VM's virtio-net interface.
    /// When set, the host-side utun gets `.1` of the same /24, eliminating
    /// subnet conflicts when multiple profiles run simultaneously.
    /// Absent → `192.168.105.2` (backward-compatible default).
    pub vm_ip: Option<Ipv4Addr>,
}

impl VmProfileConfig {
    /// Load `vm.conf` from the given profile's state directory.
    /// Returns a zeroed config (all `None`) if the file does not exist.
    pub fn load(profile: &str) -> io::Result<Self> {
        let path = profile_dir(profile)?.join("vm.conf");
        Self::load_path(&path)
    }

    fn load_path(path: &std::path::Path) -> io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
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
                "disk" => cfg.disk = Some(PathBuf::from(val)),
                "kernel" => cfg.kernel = Some(PathBuf::from(val)),
                "initrd" => cfg.initrd = Some(PathBuf::from(val)),
                "memory" => cfg.memory = val.parse::<usize>().ok(),
                "cpus" => cfg.cpus = val.parse::<usize>().ok(),
                "ping_mode" => {
                    cfg.ping_mode = match val {
                        "ssh" => PingMode::Ssh,
                        _ => PingMode::Vsock,
                    }
                }
                "cmdline" => cfg.cmdline = Some(val.to_string()),
                "vm_ip" => cfg.vm_ip = val.parse::<Ipv4Addr>().ok(),
                _ => {}
            }
        }
        Ok(cfg)
    }
}

impl StateDir {
    /// Open the default profile state directory (~/.local/share/pelagos/).
    #[allow(dead_code)]
    pub fn open() -> io::Result<Self> {
        Self::open_profile("default")
    }

    /// Open a named profile state directory.
    ///
    /// `"default"` maps to `~/.local/share/pelagos/` (backwards-compatible).
    /// Any other name maps to `~/.local/share/pelagos/profiles/<name>/`.
    pub fn open_profile(name: &str) -> io::Result<Self> {
        let base = profile_dir(name)?;
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            pid_file: base.join("vm.pid"),
            sock_file: base.join("vm.sock"),
            console_sock_file: base.join("console.sock"),
            mounts_file: base.join("vm.mounts"),
            ports_file: base.join("vm.ports"),
            extra_disks_file: base.join("vm.extra_disks"),
        })
    }

    /// Returns the PID of the running daemon, or None if the PID file is
    /// absent, unparseable, or the process no longer exists.
    pub fn running_pid(&self) -> Option<u32> {
        let s = std::fs::read_to_string(&self.pid_file).ok()?;
        let pid: u32 = s.trim().parse().ok()?;
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if alive {
            Some(pid)
        } else {
            None
        }
    }

    pub fn is_daemon_alive(&self) -> bool {
        self.running_pid().is_some()
    }

    /// Atomically write the PID file using rename so two racing daemons
    /// cannot both think they own the state.
    pub fn write_pid(&self, pid: u32) -> io::Result<()> {
        let tmp = self.pid_file.with_extension("pid.tmp");
        std::fs::write(&tmp, pid.to_string())?;
        std::fs::rename(&tmp, &self.pid_file)
    }

    /// Write the current daemon's mount configuration as JSON.
    pub fn write_mounts(&self, mounts: &[crate::daemon::VirtiofsShare]) -> io::Result<()> {
        let json = serde_json::to_string(mounts)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.mounts_file.with_extension("mounts.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.mounts_file)
    }

    /// Read the running daemon's mount configuration.  Returns an empty Vec
    /// if the file does not exist.
    pub fn read_mounts(&self) -> io::Result<Vec<crate::daemon::VirtiofsShare>> {
        match std::fs::read_to_string(&self.mounts_file) {
            Ok(s) => {
                serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Write the extra block device paths for the running daemon as JSON.
    pub fn write_extra_disks(&self, paths: &[PathBuf]) -> io::Result<()> {
        let strs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let json = serde_json::to_string(&strs)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.extra_disks_file.with_extension("extra_disks.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.extra_disks_file)
    }

    /// Read the running daemon's extra disk paths.  Returns an empty Vec if
    /// the file does not exist.
    pub fn read_extra_disks(&self) -> io::Result<Vec<PathBuf>> {
        match std::fs::read_to_string(&self.extra_disks_file) {
            Ok(s) => {
                let strs: Vec<String> = serde_json::from_str(&s)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(strs.into_iter().map(PathBuf::from).collect())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Remove PID, socket, console socket, mounts, ports, and extra_disks files. Best-effort.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.pid_file);
        let _ = std::fs::remove_file(&self.sock_file);
        let _ = std::fs::remove_file(&self.console_sock_file);
        let _ = std::fs::remove_file(&self.mounts_file);
        let _ = std::fs::remove_file(&self.ports_file);
        let _ = std::fs::remove_file(&self.extra_disks_file);
    }
}

/// Returns the path to the SSH private key used for `pelagos vm ssh`.
///
/// The key is generated once by `build-vm-image.sh` and baked into the VM
/// initramfs as the only authorised key.  It is a global artifact — all
/// profiles boot the same image and therefore share the same key.  It always
/// lives in the default (root) pelagos data dir, regardless of the active profile.
pub fn global_ssh_key_file() -> io::Result<PathBuf> {
    Ok(pelagos_base()?.join("vm_key"))
}

/// Returns the base pelagos data dir (respects XDG_DATA_HOME).
pub fn pelagos_base() -> io::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("pelagos"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "$HOME not set"))?;
    Ok(PathBuf::from(home).join(".local/share/pelagos"))
}

/// Returns the state directory for a given profile name.
///
/// "default" → `~/.local/share/pelagos/`
/// other     → `~/.local/share/pelagos/profiles/<name>/`
pub fn profile_dir(name: &str) -> io::Result<PathBuf> {
    let base = pelagos_base()?;
    if name == "default" {
        Ok(base)
    } else {
        Ok(base.join("profiles").join(name))
    }
}

// ---------------------------------------------------------------------------
// Per-profile subnet allocation
// ---------------------------------------------------------------------------

/// Default guest IP for the "default" profile (backward-compatible).
pub const DEFAULT_GUEST_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 105, 2);

/// Assign (and persist) a per-profile guest IP if one is not already set.
///
/// The "default" profile always receives `192.168.105.2`.  Other profiles
/// receive the lowest available `192.168.N.2` (N ∈ 106–254) not in use by
/// any existing profile's `vm.conf`.  The chosen IP is written to the
/// profile's `vm.conf` so subsequent daemon starts are stable.
///
/// Returns the guest IP that should be used for this boot.
pub fn ensure_vm_ip(profile: &str) -> io::Result<Ipv4Addr> {
    // Fast path: already configured.
    let conf = VmProfileConfig::load(profile)?;
    if let Some(ip) = conf.vm_ip {
        return Ok(ip);
    }

    // The default profile is always 192.168.105.2 — no allocation needed.
    if profile == "default" {
        append_vm_ip_to_conf(profile, DEFAULT_GUEST_IP)?;
        return Ok(DEFAULT_GUEST_IP);
    }

    // Collect IPs already in use by all profiles (including default).
    let base = pelagos_base()?;
    let mut taken: std::collections::HashSet<u8> = std::collections::HashSet::new();

    // Default profile.
    if let Ok(c) = VmProfileConfig::load("default") {
        if let Some(ip) = c.vm_ip {
            taken.insert(ip.octets()[2]);
        } else {
            taken.insert(105);
        }
    } else {
        taken.insert(105);
    }

    // Named profiles.
    let profiles_dir = base.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            let pname = entry_name.to_string_lossy();
            if pname == profile {
                continue; // skip ourselves
            }
            if let Ok(c) = VmProfileConfig::load(&pname) {
                if let Some(ip) = c.vm_ip {
                    taken.insert(ip.octets()[2]);
                }
            }
        }
    }

    // Pick the lowest unused N in 106–254.
    let n = (106u8..=254)
        .find(|n| !taken.contains(n))
        .ok_or_else(|| io::Error::other("no available VM subnet (all 192.168.106–254 in use)"))?;

    let ip = Ipv4Addr::new(192, 168, n, 2);
    append_vm_ip_to_conf(profile, ip)?;
    log::info!("subnet: assigned {} to profile '{}'", ip, profile);
    Ok(ip)
}

/// Append `vm_ip = <ip>` to the profile's `vm.conf`, creating the file and
/// directory if necessary.
fn append_vm_ip_to_conf(profile: &str, ip: Ipv4Addr) -> io::Result<()> {
    let dir = profile_dir(profile)?;
    std::fs::create_dir_all(&dir)?;
    let conf_path = dir.join("vm.conf");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&conf_path)?;
    use std::io::Write;
    writeln!(f, "vm_ip = {}", ip)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::StateDir;
    use std::path::PathBuf;

    /// Build a StateDir rooted in a unique temp directory so tests never touch
    /// the real ~/.local/share/pelagos/ and never collide with each other.
    fn temp_state() -> StateDir {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let base = std::env::temp_dir().join(format!("pelagos-test-{}", ns));
        std::fs::create_dir_all(&base).expect("create temp dir");
        StateDir {
            pid_file: base.join("vm.pid"),
            sock_file: base.join("vm.sock"),
            console_sock_file: base.join("console.sock"),
            mounts_file: base.join("vm.mounts"),
            ports_file: base.join("vm.ports"),
            extra_disks_file: base.join("vm.extra_disks"),
        }
    }

    #[test]
    fn write_and_read_pid() {
        let s = temp_state();
        s.write_pid(12345).expect("write_pid");
        let contents = std::fs::read_to_string(&s.pid_file).expect("read pid file");
        assert_eq!(contents.trim(), "12345");
    }

    #[test]
    fn running_pid_current_process() {
        let s = temp_state();
        let my_pid = std::process::id();
        s.write_pid(my_pid).expect("write_pid");
        assert_eq!(s.running_pid(), Some(my_pid));
    }

    #[test]
    fn running_pid_dead_process() {
        let s = temp_state();
        // Spawn a short-lived child, capture its PID, wait for it to exit,
        // then verify that running_pid() returns None for the now-dead PID.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let dead_pid = child.id();
        child.wait().expect("wait");
        // The child has exited; its PID should no longer be alive.
        s.write_pid(dead_pid).expect("write_pid");
        assert_eq!(s.running_pid(), None);
    }

    #[test]
    fn clear_removes_files() {
        let s = temp_state();
        s.write_pid(99).expect("write_pid");
        // Plant a fake sock file so clear() has something to remove.
        std::fs::write(&s.sock_file, b"").expect("write sock");
        assert!(s.pid_file.exists());
        assert!(s.sock_file.exists());

        s.clear();

        assert!(
            !s.pid_file.exists(),
            "pid_file should be gone after clear()"
        );
        assert!(
            !s.sock_file.exists(),
            "sock_file should be gone after clear()"
        );
    }

    #[test]
    fn running_pid_absent_file() {
        let s = temp_state();
        // No pid file written — should return None without panicking.
        assert_eq!(s.running_pid(), None);
    }

    #[test]
    fn running_pid_garbage_content() {
        let s = temp_state();
        std::fs::write(&s.pid_file, b"not-a-pid").expect("write garbage");
        assert_eq!(s.running_pid(), None);
    }

    /// Verify that the field paths are computed relative to the supplied base.
    #[test]
    fn paths_are_inside_base() {
        let base = PathBuf::from("/tmp/pelagos-path-test");
        let s = StateDir {
            pid_file: base.join("vm.pid"),
            sock_file: base.join("vm.sock"),
            console_sock_file: base.join("console.sock"),
            mounts_file: base.join("vm.mounts"),
            ports_file: base.join("vm.ports"),
            extra_disks_file: base.join("vm.extra_disks"),
        };
        assert_eq!(s.pid_file, PathBuf::from("/tmp/pelagos-path-test/vm.pid"));
        assert_eq!(s.sock_file, PathBuf::from("/tmp/pelagos-path-test/vm.sock"));
        assert_eq!(
            s.console_sock_file,
            PathBuf::from("/tmp/pelagos-path-test/console.sock")
        );
        assert_eq!(
            s.mounts_file,
            PathBuf::from("/tmp/pelagos-path-test/vm.mounts")
        );
        assert_eq!(
            s.ports_file,
            PathBuf::from("/tmp/pelagos-path-test/vm.ports")
        );
    }

    /// default profile maps to the root pelagos data dir (backward compat).
    #[test]
    fn profile_dir_default_stays_at_root() {
        std::env::set_var("XDG_DATA_HOME", "/tmp/pelagos-xdg-test");
        let d = super::profile_dir("default").unwrap();
        assert_eq!(d, PathBuf::from("/tmp/pelagos-xdg-test/pelagos"));
        std::env::remove_var("XDG_DATA_HOME");
    }

    /// named profile maps to profiles/<name>/ subdir.
    #[test]
    fn profile_dir_named_uses_subdirectory() {
        std::env::set_var("XDG_DATA_HOME", "/tmp/pelagos-xdg-test");
        let d = super::profile_dir("build").unwrap();
        assert_eq!(
            d,
            PathBuf::from("/tmp/pelagos-xdg-test/pelagos/profiles/build")
        );
        std::env::remove_var("XDG_DATA_HOME");
    }

    /// VmProfileConfig parses a vm.conf file correctly.
    #[test]
    fn vm_profile_config_parse() {
        use std::io::Write;
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!("pelagos-conf-test-{}", ns));
        std::fs::create_dir_all(&dir).unwrap();
        let conf_path = dir.join("vm.conf");
        let mut f = std::fs::File::create(&conf_path).unwrap();
        writeln!(f, "# comment").unwrap();
        writeln!(f, "disk   = /data/build.img").unwrap();
        writeln!(f, "kernel = /boot/vmlinuz").unwrap();
        writeln!(f, "initrd = /boot/initrd.gz").unwrap();
        writeln!(f, "memory = 8192").unwrap();
        writeln!(f, "cpus   = 4").unwrap();
        writeln!(f, "unknown = ignored").unwrap();
        drop(f);

        let cfg = super::VmProfileConfig::load_path(&conf_path).unwrap();
        assert_eq!(cfg.disk, Some(PathBuf::from("/data/build.img")));
        assert_eq!(cfg.kernel, Some(PathBuf::from("/boot/vmlinuz")));
        assert_eq!(cfg.initrd, Some(PathBuf::from("/boot/initrd.gz")));
        assert_eq!(cfg.memory, Some(8192));
        assert_eq!(cfg.cpus, Some(4));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VmProfileConfig returns all-None when vm.conf is absent.
    #[test]
    fn vm_profile_config_missing_returns_default() {
        let absent = PathBuf::from("/tmp/pelagos-no-such-conf-file.conf");
        let cfg = super::VmProfileConfig::load_path(&absent).unwrap();
        assert!(cfg.disk.is_none());
        assert!(cfg.memory.is_none());
    }

    /// default and named profiles use distinct state directories.
    #[test]
    fn profiles_are_isolated() {
        std::env::set_var("XDG_DATA_HOME", "/tmp/pelagos-isolation-test");
        let default_dir = super::profile_dir("default").unwrap();
        let named_dir = super::profile_dir("myprofile").unwrap();
        // Paths must differ.
        assert_ne!(default_dir, named_dir);
        // Named profile lives under profiles/ inside the base dir.
        assert!(named_dir.starts_with(default_dir.join("profiles")));
        std::env::remove_var("XDG_DATA_HOME");
    }
}
