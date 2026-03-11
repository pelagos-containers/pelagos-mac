//! Persistent VM daemon: holds the VM alive and proxies vsock connections
//! over a Unix socket so multiple CLI invocations can share one VM.
//!
//! Lifecycle:
//!   1. `ensure_running()` is called by `pelagos run` / `pelagos ping`.
//!      If no daemon is alive it spawns the current binary with the hidden
//!      `vm-daemon-internal` subcommand and waits up to 30 s for vm.sock.
//!   2. The daemon boots the VM, binds vm.sock, writes vm.pid, then loops
//!      accepting Unix socket connections.
//!   3. For each connection the daemon calls `vm.connect_vsock()`, then
//!      bidirectionally proxies bytes between the Unix stream and the vsock fd.
//!   4. On SIGTERM the daemon stops the VM, removes state files, and exits.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use pelagos_vz::vm::{Vm, VmConfig};

use crate::state::StateDir;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single virtiofs host→guest mount.
///
/// Carried in `DaemonArgs` and persisted in the state dir so that subsequent
/// CLI invocations can verify they are compatible with the running daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VirtiofsShare {
    /// Host directory to expose.
    pub host_path: PathBuf,
    /// virtiofs mount tag (`share0`, `share1`, …).
    pub tag: String,
    /// Mount the share read-only inside the guest.
    pub read_only: bool,
    /// Absolute path inside the container where the share is mounted.
    pub container_path: String,
}

// ---------------------------------------------------------------------------
// Public API used by main.rs
// ---------------------------------------------------------------------------

/// Configuration forwarded from the CLI to the daemon subprocess.
pub struct DaemonArgs {
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub disk: PathBuf,
    pub cmdline: String,
    pub memory_mib: usize,
    pub cpus: usize,
    /// virtiofs shares requested for this invocation (may be empty).
    pub virtiofs_shares: Vec<VirtiofsShare>,
}

/// Ensure the daemon is running, starting it if necessary.
/// Returns Ok(()) once vm.sock is connectable.
///
/// If the daemon is already running but was started with a different mount
/// configuration, returns an error asking the user to run `pelagos vm stop`.
pub fn ensure_running(args: &DaemonArgs) -> io::Result<()> {
    let state = StateDir::open()?;

    if state.is_daemon_alive() {
        // Verify that the running daemon was started with the same mounts.
        let running_mounts = state.read_mounts()?;
        if running_mounts != args.virtiofs_shares {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "daemon is running with different mount configuration; \
                 run 'pelagos vm stop' first, then retry",
            ));
        }
        return Ok(());
    }

    log::info!("starting persistent VM daemon...");
    state.clear(); // remove stale files from a previous dead daemon

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--kernel").arg(&args.kernel);
    cmd.arg("--disk").arg(&args.disk);
    if let Some(ref initrd) = args.initrd {
        cmd.arg("--initrd").arg(initrd);
    }
    cmd.arg("--cmdline").arg(&args.cmdline);
    cmd.arg("--memory").arg(args.memory_mib.to_string());
    cmd.arg("--cpus").arg(args.cpus.to_string());
    // Forward virtiofs shares as repeated --volume flags to the daemon subcommand.
    for share in &args.virtiofs_shares {
        let mut spec = format!(
            "{}:{}",
            share.host_path.display(),
            share.container_path
        );
        if share.read_only {
            spec.push_str(":ro");
        }
        cmd.arg("--volume").arg(&spec);
    }
    cmd.arg("vm-daemon-internal");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    // If RUST_LOG is set, send daemon stderr to a log file for debugging.
    // Otherwise discard it.
    if std::env::var_os("RUST_LOG").is_some() {
        let log_path = state.sock_file.with_file_name("daemon.log");
        let log_file = std::fs::File::create(&log_path)?;
        cmd.stderr(log_file);
        cmd.env("RUST_LOG", std::env::var("RUST_LOG").unwrap());
    } else {
        cmd.stderr(std::process::Stdio::null());
    }
    cmd.spawn()?;

    // Poll until vm.sock exists (daemon bound its UnixListener and is ready).
    // We intentionally do NOT connect here: a test connection would be accepted
    // by the daemon and get proxied to the guest, blocking the guest's
    // single-threaded accept loop and preventing the real command from landing.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if state.sock_file.exists() {
            log::info!("daemon ready");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon did not start within 60s",
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Connect to the running daemon's Unix socket.
pub fn connect() -> io::Result<UnixStream> {
    let state = StateDir::open()?;
    UnixStream::connect(&state.sock_file)
        .map_err(|e| io::Error::new(e.kind(), format!("daemon connect: {}", e)))
}

/// Entry point for the `vm-daemon-internal` subcommand.
/// Boots the VM, serves vsock connections, and never returns.
pub fn run(args: DaemonArgs) -> ! {
    let state = StateDir::open().expect("state dir");

    // Guard against two daemons racing.
    if state.is_daemon_alive() {
        log::error!("another daemon is already running");
        std::process::exit(1);
    }
    state.clear();

    let config = build_vm_config(&args);
    log::info!("booting VM...");
    let vm = Arc::new(Vm::start(config).unwrap_or_else(|e| {
        log::error!("VM start failed: {}", e);
        std::process::exit(1);
    }));
    log::info!("VM running");

    let listener = UnixListener::bind(&state.sock_file).unwrap_or_else(|e| {
        log::error!("bind {}: {}", state.sock_file.display(), e);
        std::process::exit(1);
    });

    state.write_pid(std::process::id()).unwrap_or_else(|e| {
        log::error!("write pid: {}", e);
    });

    // Persist mount configuration so subsequent invocations can verify compatibility.
    state.write_mounts(&args.virtiofs_shares).unwrap_or_else(|e| {
        log::error!("write mounts: {}", e);
    });

    log::info!("daemon listening on {}", state.sock_file.display());

    // Install SIGTERM handler: sets flag, SIGINT terminates immediately.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        unsafe {
            // Store flag pointer globally for the C-level signal handler.
            SHUTDOWN_FLAG = Arc::into_raw(flag);
            libc::signal(
                libc::SIGTERM,
                sigterm_handler as *const () as libc::sighandler_t,
            );
        }
    }

    // Accept loop: use poll(2) with 1-second timeout so SIGTERM is checked promptly.
    loop {
        if shutdown.load(Ordering::Relaxed) {
            log::info!("shutdown requested, stopping VM...");
            // Drop the Arc. If no proxy threads are active, Vm::drop runs stop().
            // If threads still hold clones the VM will stop when the last clone
            // drops. Either way the process exits immediately after cleanup.
            drop(vm);
            state.clear();
            std::process::exit(0);
        }

        // poll the listener fd for an incoming connection.
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, 1000) }; // 1 s timeout
        if n <= 0 {
            continue;
        }
        if pfd.revents & libc::POLLIN == 0 {
            continue;
        }

        let unix = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) => {
                log::error!("accept: {}", e);
                continue;
            }
        };

        // Connect vsock inside the spawned thread so the accept loop is not
        // blocked while waiting for the guest daemon to start (which can take
        // up to ~45 s during the ping-gate phase).
        let vm2 = Arc::clone(&vm);
        std::thread::spawn(move || {
            let vsock_fd = match vm2.connect_vsock() {
                Ok(fd) => fd,
                Err(e) => {
                    log::error!("vsock connect: {}", e);
                    return; // unix stream dropped → EOF on CLI side
                }
            };
            drop(vm2); // release Arc before entering the proxy loop
            proxy(unix, vsock_fd);
        });
    }
}

// ---------------------------------------------------------------------------
// SIGTERM handler
// ---------------------------------------------------------------------------

static mut SHUTDOWN_FLAG: *const AtomicBool = std::ptr::null();

extern "C" fn sigterm_handler(_: libc::c_int) {
    // Safety: SHUTDOWN_FLAG is set once before this handler is installed.
    if let Some(flag) = unsafe { SHUTDOWN_FLAG.as_ref() } {
        flag.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Bidirectional proxy
// ---------------------------------------------------------------------------

/// Proxy bytes between a Unix socket (CLI side) and a vsock fd (guest side).
/// Runs two threads: Unix→vsock and vsock→Unix. Returns when either side closes.
fn proxy(unix: UnixStream, vsock: OwnedFd) {
    // dup the vsock fd so each thread owns one end.
    let vsock_raw = vsock.into_raw_fd();
    let vsock_read_fd = unsafe { libc::dup(vsock_raw) };
    // vsock_raw is now the write end (consumed by vsock_write below)
    let vsock_write: std::fs::File = unsafe { std::fs::File::from_raw_fd(vsock_raw) };
    let vsock_read: std::fs::File = unsafe { std::fs::File::from_raw_fd(vsock_read_fd) };

    let unix_write = unix.try_clone().expect("clone unix stream");
    // unix is the read end; unix_write is the write end.

    // Thread A: Unix → vsock
    let t_a = std::thread::spawn({
        let mut src = unix;
        let mut dst = vsock_write;
        move || {
            let _ = std::io::copy(&mut src, &mut dst);
        }
    });

    // Thread B: vsock → Unix
    let t_b = std::thread::spawn({
        let mut src = vsock_read;
        let mut dst = unix_write;
        move || {
            let _ = std::io::copy(&mut src, &mut dst);
        }
    });

    let _ = t_a.join();
    let _ = t_b.join();
}

// ---------------------------------------------------------------------------
// VmConfig from DaemonArgs
// ---------------------------------------------------------------------------

fn build_vm_config(args: &DaemonArgs) -> VmConfig {
    let mut b = VmConfig::builder()
        .kernel(&args.kernel)
        .disk(&args.disk)
        .cmdline(build_cmdline(args))
        .memory_mib(args.memory_mib)
        .cpus(args.cpus);
    if let Some(ref initrd) = args.initrd {
        b = b.initrd(initrd);
    }
    for share in &args.virtiofs_shares {
        b = b.virtiofs(&share.host_path, &share.tag, share.read_only);
    }
    b.build().expect("vm config")
}

/// Build the kernel cmdline from DaemonArgs.
///
/// Delegates to `build_cmdline_from_parts` so the core logic is unit-testable
/// without constructing a full DaemonArgs.
fn build_cmdline(args: &DaemonArgs) -> String {
    build_cmdline_from_parts(&args.cmdline, &args.virtiofs_shares)
}

/// Append `virtiofs.tags=tag0,tag1,...` to `base` when shares are present.
///
/// The guest init script reads this parameter to mount each virtiofs share
/// before exec'ing pelagos-guest.  Extracted as a pure function for testability.
pub(crate) fn build_cmdline_from_parts(base: &str, shares: &[VirtiofsShare]) -> String {
    let mut cmdline = base.to_owned();
    if !shares.is_empty() {
        let tags: Vec<&str> = shares.iter().map(|s| s.tag.as_str()).collect();
        cmdline.push_str(" virtiofs.tags=");
        cmdline.push_str(&tags.join(","));
    }
    cmdline
}

/// Return true when two share lists are configuration-equivalent.
#[cfg(test)]
pub(crate) fn mounts_match(a: &[VirtiofsShare], b: &[VirtiofsShare]) -> bool {
    a == b
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{build_cmdline_from_parts, mounts_match, VirtiofsShare};
    use std::path::PathBuf;

    fn share(tag: &str, host: &str, container: &str, ro: bool) -> VirtiofsShare {
        VirtiofsShare {
            host_path: PathBuf::from(host),
            tag: tag.to_owned(),
            read_only: ro,
            container_path: container.to_owned(),
        }
    }

    #[test]
    fn cmdline_no_shares() {
        assert_eq!(build_cmdline_from_parts("console=hvc0", &[]), "console=hvc0");
    }

    #[test]
    fn cmdline_one_share() {
        let shares = vec![share("share0", "/host/data", "/data", false)];
        assert_eq!(
            build_cmdline_from_parts("console=hvc0", &shares),
            "console=hvc0 virtiofs.tags=share0"
        );
    }

    #[test]
    fn cmdline_two_shares() {
        let shares = vec![
            share("share0", "/host/data", "/data", false),
            share("share1", "/host/cfg", "/etc/cfg", true),
        ];
        assert_eq!(
            build_cmdline_from_parts("console=hvc0", &shares),
            "console=hvc0 virtiofs.tags=share0,share1"
        );
    }

    #[test]
    fn cmdline_preserves_existing_params() {
        let shares = vec![share("share0", "/host/x", "/x", false)];
        assert_eq!(
            build_cmdline_from_parts("console=hvc0 quiet", &shares),
            "console=hvc0 quiet virtiofs.tags=share0"
        );
    }

    #[test]
    fn mounts_match_empty() {
        assert!(mounts_match(&[], &[]));
    }

    #[test]
    fn mounts_match_identical() {
        let a = vec![share("share0", "/host/a", "/a", false)];
        assert!(mounts_match(&a, &a.clone()));
    }

    #[test]
    fn mounts_mismatch_different_path() {
        let a = vec![share("share0", "/host/a", "/a", false)];
        let b = vec![share("share0", "/host/b", "/a", false)];
        assert!(!mounts_match(&a, &b));
    }

    #[test]
    fn mounts_mismatch_different_length() {
        let a = vec![share("share0", "/host/a", "/a", false)];
        assert!(!mounts_match(&a, &[]));
    }

    #[test]
    fn mounts_mismatch_readonly_flag() {
        let a = vec![share("share0", "/host/a", "/a", false)];
        let b = vec![share("share0", "/host/a", "/a", true)];
        assert!(!mounts_match(&a, &b));
    }
}
