//! Persistent VM state: PID file, Unix socket path, and mounts config
//! stored in ~/.local/share/pelagos/.

use std::io;
use std::path::PathBuf;

pub struct StateDir {
    pub pid_file: PathBuf,
    pub sock_file: PathBuf,
    /// Unix socket for the serial console relay (pelagos vm console).
    pub console_sock_file: PathBuf,
    pub mounts_file: PathBuf,
}

impl StateDir {
    pub fn open() -> io::Result<Self> {
        let base = base_dir()?;
        std::fs::create_dir_all(&base)?;
        Ok(Self {
            pid_file: base.join("vm.pid"),
            sock_file: base.join("vm.sock"),
            console_sock_file: base.join("console.sock"),
            mounts_file: base.join("vm.mounts"),
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

    /// Remove PID, socket, console socket, and mounts files. Best-effort; ignores errors.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.pid_file);
        let _ = std::fs::remove_file(&self.sock_file);
        let _ = std::fs::remove_file(&self.console_sock_file);
        let _ = std::fs::remove_file(&self.mounts_file);
    }
}

fn base_dir() -> io::Result<PathBuf> {
    // Respect XDG_DATA_HOME if set, otherwise ~/.local/share/pelagos
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("pelagos"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "$HOME not set"))?;
    Ok(PathBuf::from(home).join(".local/share/pelagos"))
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
    }
}
