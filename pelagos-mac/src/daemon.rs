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

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use pelagos_vz::vm::{Vm, VmConfig};

use crate::port_dispatcher::{DispatchCmd, PortDispatcher};
use crate::state::StateDir;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A host→container port forward: host TCP listener relays to a port inside the VM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortForward {
    /// Port to listen on on the host (0.0.0.0).
    pub host_port: u16,
    /// Port to connect to inside the VM (192.168.105.2).
    pub container_port: u16,
}

/// Parse a `"host_port:container_port"` or bare `"port"` spec.
pub fn parse_port_spec(spec: &str) -> Option<PortForward> {
    let parts: Vec<&str> = spec.splitn(2, ':').collect();
    if parts.len() == 2 {
        let host_port = parts[0].parse().ok()?;
        let container_port = parts[1].parse().ok()?;
        Some(PortForward {
            host_port,
            container_port,
        })
    } else {
        let port = spec.parse().ok()?;
        Some(PortForward {
            host_port: port,
            container_port: port,
        })
    }
}

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

/// Commands sent directly to the daemon (not proxied to vsock).
///
/// The daemon peeks at the first JSON line of each connection; if it
/// deserialises as `DaemonCmd` it is handled locally and the connection is
/// closed.  Any other first line is forwarded to the guest via vsock.
#[derive(Debug, Deserialize, Serialize)]
pub enum DaemonCmd {
    /// Start listening on `host_port` and relay connections to `container_port`.
    RegisterPort { host_port: u16, container_port: u16 },
    /// Stop listening on `host_port`.  Active connections are not affected.
    UnregisterPort { host_port: u16 },
    /// Associate a set of host ports with a compose project name so they can
    /// be bulk-deregistered when the project stops.
    TrackComposeProject { project: String, host_ports: Vec<u16> },
    /// Deregister all ports previously associated with a compose project.
    UnregisterComposePorts { project: String },
}

/// One-line JSON response to a `DaemonCmd`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Ok,
    Err { message: String },
}

/// Live port-forward state shared between connection handler threads and (in a
/// future PR) the subscription watcher thread.
struct PortState {
    /// Maps host_port → container_port for every currently active forward.
    active: HashMap<u16, u16>,
    /// Maps compose project name → list of host ports it registered.
    compose_projects: HashMap<String, Vec<u16>>,
}

impl PortState {
    fn new() -> Self {
        Self {
            active: HashMap::new(),
            compose_projects: HashMap::new(),
        }
    }
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
    /// Host→container port forwards (may be empty).
    pub port_forwards: Vec<PortForward>,
    /// VM profile name ("default" or a named profile).
    pub profile: String,
    /// Secondary disk images: first → /dev/vdb, second → /dev/vdc, etc.
    /// Used during build-VM provisioning to avoid virtiofs I/O overhead.
    pub extra_disks: Vec<std::path::PathBuf>,
    /// Loopback TCP port for the NAT relay proxy.  Distinct per profile so
    /// multiple profiles can run simultaneously without relay conflicts.
    pub relay_proxy_port: u16,
}

/// Ensure the daemon is running, starting it if necessary.
/// Returns Ok(()) once vm.sock is connectable.
///
/// If the daemon is already running but was started with a different mount
/// configuration, returns an error asking the user to run `pelagos vm stop`.
pub fn ensure_running(args: &DaemonArgs) -> io::Result<()> {
    let state = StateDir::open_profile(&args.profile)?;

    if state.is_daemon_alive() {
        // Verify that the running daemon was started with the same mounts.
        // (virtiofs shares are part of the VM config and cannot change at runtime.)
        let running_mounts = state.read_mounts()?;
        if running_mounts != args.virtiofs_shares {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "daemon is running with different mount configuration; \
                 run 'pelagos vm stop' first, then retry",
            ));
        }
        // Extra disks are block devices attached at VM boot; they cannot be
        // changed on a running VM.  Verify the running daemon was started with
        // the same set (persisted in vm.extra_disks by daemon::run()).
        let running_extra = state.read_extra_disks()?;
        if running_extra != args.extra_disks {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "daemon is running with different extra-disk configuration; \
                 run 'pelagos vm stop' first, then retry",
            ));
        }
        // Ports are managed dynamically via DaemonCmd::RegisterPort; no
        // static port-list validation is needed here (issue #170).
        return Ok(());
    }

    log::info!("starting persistent VM daemon...");
    state.clear(); // remove stale files from a previous dead daemon

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    for arg in daemon_subprocess_args(args) {
        cmd.arg(arg);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    // Always write daemon stderr to a log file so failures are diagnosable
    // regardless of whether the caller set RUST_LOG.  Default to "info" so
    // lifecycle events are always captured; the caller can override verbosity
    // by setting RUST_LOG before invoking any pelagos command.
    let log_path = state.sock_file.with_file_name("daemon.log");
    let log_file = std::fs::File::create(&log_path)?;
    cmd.stderr(log_file);
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "info");
    } else {
        cmd.env("RUST_LOG", std::env::var("RUST_LOG").unwrap());
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

/// Connect to the running daemon's Unix socket for the given profile.
pub fn connect(profile: &str) -> io::Result<UnixStream> {
    let state = StateDir::open_profile(profile)?;
    UnixStream::connect(&state.sock_file)
        .map_err(|e| io::Error::new(e.kind(), format!("daemon connect: {}", e)))
}

/// Entry point for the `vm-daemon-internal` subcommand.
/// Boots the VM, serves vsock connections, and never returns.
pub fn run(args: DaemonArgs) -> ! {
    let state = StateDir::open_profile(&args.profile).expect("state dir");

    // Guard against two daemons racing.
    if state.is_daemon_alive() {
        log::error!("another daemon is already running");
        std::process::exit(1);
    }
    state.clear();

    let config = build_vm_config(&args);
    log::info!("booting VM...");
    let (vm, console_fd) = Vm::start(config).unwrap_or_else(|e| {
        log::error!("VM start failed: {}", e);
        std::process::exit(1);
    });
    let vm = Arc::new(vm);
    log::info!("VM running");

    let listener = UnixListener::bind(&state.sock_file).unwrap_or_else(|e| {
        log::error!("bind {}: {}", state.sock_file.display(), e);
        std::process::exit(1);
    });

    // Bind the console socket and start the relay thread.
    // Stale socket from a previous daemon is cleaned up by state.clear() above.
    let console_listener = UnixListener::bind(&state.console_sock_file).unwrap_or_else(|e| {
        log::error!("bind {}: {}", state.console_sock_file.display(), e);
        std::process::exit(1);
    });
    std::thread::spawn(move || {
        console_relay_loop(console_listener, console_fd);
    });

    state.write_pid(std::process::id()).unwrap_or_else(|e| {
        log::error!("write pid: {}", e);
    });

    // Persist mount and extra-disk configuration (validated on reconnect).
    // Port forwards are managed dynamically via DaemonCmd — no static list to persist.
    state
        .write_mounts(&args.virtiofs_shares)
        .unwrap_or_else(|e| {
            log::error!("write mounts: {}", e);
        });
    state
        .write_extra_disks(&args.extra_disks)
        .unwrap_or_else(|e| {
            log::error!("write extra_disks: {}", e);
        });

    // Spawn the single-threaded port-forward dispatcher (O(1) listener threads
    // regardless of how many ports or containers are active).
    let dispatcher = Arc::new(PortDispatcher::spawn(args.relay_proxy_port));
    let port_state = Arc::new(Mutex::new(PortState::new()));

    // Pre-register any ports requested at daemon startup via `vm start -p`.
    {
        let mut ps = port_state.lock().unwrap();
        for pf in &args.port_forwards {
            dispatcher.send(DispatchCmd::Add {
                host_port: pf.host_port,
                container_port: pf.container_port,
            });
            ps.active.insert(pf.host_port, pf.container_port);
        }
    }

    log::info!("daemon listening on {}", state.sock_file.display());

    // Spawn the subscription watcher: auto-deregisters ports when containers exit.
    start_subscription_watcher(
        Arc::clone(&vm),
        Arc::clone(&dispatcher),
        Arc::clone(&port_state),
    );

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
            dispatcher.send(DispatchCmd::Shutdown);
            drop(dispatcher);
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
        let disp2 = Arc::clone(&dispatcher);
        let ps2 = Arc::clone(&port_state);
        std::thread::spawn(move || {
            handle_connection(unix, vm2, disp2, ps2);
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
fn proxy(unix: UnixStream, vsock: OwnedFd, conn_id: std::thread::ThreadId) {
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
            let n = std::io::copy(&mut src, &mut dst);
            log::debug!("[{conn_id:?}] unix→vsock closed ({n:?} bytes)");
        }
    });

    // Thread B: vsock → Unix
    let t_b = std::thread::spawn({
        let mut src = vsock_read;
        let mut dst = unix_write;
        move || {
            let n = std::io::copy(&mut src, &mut dst);
            log::debug!("[{conn_id:?}] vsock→unix closed ({n:?} bytes)");
        }
    });

    let _ = t_a.join();
    let _ = t_b.join();
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

/// Handle one accepted Unix socket connection.
///
/// Peeks at the first newline-terminated JSON line:
/// - If it is a `DaemonCmd`: handle locally (port registration / removal) and return.
/// - Otherwise: open a vsock channel to the guest and bidirectionally proxy all bytes.
fn handle_connection(
    mut unix: UnixStream,
    vm: Arc<Vm>,
    dispatcher: Arc<PortDispatcher>,
    port_state: Arc<Mutex<PortState>>,
) {
    let conn_id = std::thread::current().id();
    log::info!("[{conn_id:?}] client connected");

    // Read the first line byte-by-byte so we never consume more than one line
    // into a buffer that could then be lost when we hand the stream to proxy().
    let mut first_line: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match unix.read(&mut byte) {
            Ok(0) | Err(_) => {
                log::debug!("[{conn_id:?}] connection closed before first line");
                return;
            }
            Ok(_) => {
                first_line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if first_line.len() > 64 * 1024 {
                    log::warn!("[{conn_id:?}] first line exceeds 64 KiB — dropping connection");
                    return;
                }
            }
        }
    }

    let trimmed = String::from_utf8_lossy(&first_line);
    let trimmed = trimmed.trim_end();

    // If the first line is a DaemonCmd, handle it locally (no vsock needed).
    if let Ok(cmd) = serde_json::from_str::<DaemonCmd>(trimmed) {
        handle_daemon_cmd(cmd, &mut unix, &dispatcher, &port_state, conn_id);
        return;
    }

    // Not a DaemonCmd — proxy to vsock.  Connect inside this thread so the
    // accept loop is not blocked while waiting for the guest to start.
    let vsock_fd = match vm.connect_vsock() {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("[{conn_id:?}] vsock connect: {}", e);
            return;
        }
    };
    drop(vm);

    // Replay the peeked line to vsock before entering the bidirectional proxy.
    let vsock_raw = vsock_fd.into_raw_fd();
    {
        // SAFETY: vsock_raw is valid.  mem::forget prevents the File from
        // closing it so we can hand it to proxy() below.
        let mut f = unsafe { std::fs::File::from_raw_fd(vsock_raw) };
        let ok = f.write_all(&first_line).is_ok();
        std::mem::forget(f);
        if !ok {
            log::warn!("[{conn_id:?}] failed to replay first line to vsock");
            unsafe { libc::close(vsock_raw) };
            return;
        }
    }
    let vsock_owned = unsafe { OwnedFd::from_raw_fd(vsock_raw) };
    proxy(unix, vsock_owned, conn_id);
    log::info!("[{conn_id:?}] client disconnected");
}

/// Execute a `DaemonCmd` locally and write a `DaemonResponse` back to the caller.
fn handle_daemon_cmd(
    cmd: DaemonCmd,
    unix: &mut UnixStream,
    dispatcher: &PortDispatcher,
    port_state: &Mutex<PortState>,
    conn_id: std::thread::ThreadId,
) {
    let response = match cmd {
        DaemonCmd::RegisterPort {
            host_port,
            container_port,
        } => {
            let mut ps = port_state.lock().unwrap();
            if let std::collections::hash_map::Entry::Vacant(e) = ps.active.entry(host_port) {
                dispatcher.send(DispatchCmd::Add {
                    host_port,
                    container_port,
                });
                e.insert(container_port);
                log::info!(
                    "[{conn_id:?}] registered port {}:{}",
                    host_port,
                    container_port
                );
                DaemonResponse::Ok
            } else {
                let msg = format!("port {} is already registered", host_port);
                log::warn!("[{conn_id:?}] register port: {}", msg);
                DaemonResponse::Err { message: msg }
            }
        }
        DaemonCmd::UnregisterPort { host_port } => {
            let mut ps = port_state.lock().unwrap();
            if ps.active.remove(&host_port).is_some() {
                dispatcher.send(DispatchCmd::Remove { host_port });
                log::info!("[{conn_id:?}] unregistered port {}", host_port);
            }
            DaemonResponse::Ok
        }
        DaemonCmd::TrackComposeProject { project, host_ports } => {
            let mut ps = port_state.lock().unwrap();
            log::info!(
                "[{conn_id:?}] tracking compose project '{}' with {} port(s)",
                project,
                host_ports.len()
            );
            ps.compose_projects.insert(project, host_ports);
            DaemonResponse::Ok
        }
        DaemonCmd::UnregisterComposePorts { project } => {
            let mut ps = port_state.lock().unwrap();
            if let Some(ports) = ps.compose_projects.remove(&project) {
                for host_port in ports {
                    if ps.active.remove(&host_port).is_some() {
                        dispatcher.send(DispatchCmd::Remove { host_port });
                        log::info!(
                            "[{conn_id:?}] compose '{}': unregistered port {}",
                            project,
                            host_port
                        );
                    }
                }
            }
            DaemonResponse::Ok
        }
    };

    let mut resp_str = serde_json::to_string(&response).unwrap_or_default();
    resp_str.push('\n');
    let _ = unix.write_all(resp_str.as_bytes());
}

// ---------------------------------------------------------------------------
// Subscription watcher — auto-deregisters ports when containers exit (#169)
// ---------------------------------------------------------------------------

/// Minimal wire-format types for the `pelagos subscribe` NDJSON stream.
/// Must match `GuestEvent` in pelagos-guest (`#[serde(tag = "type", rename_all = "snake_case")]`).
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WatchEvent {
    Snapshot {
        containers: Vec<WatchContainer>,
    },
    ContainerStarted {
        container: WatchContainer,
    },
    ContainerExited {
        name: String,
    },
    /// Catch-all for heartbeats and any future event types.
    #[serde(other)]
    Unknown,
}

#[derive(serde::Deserialize)]
struct WatchContainer {
    name: String,
    #[serde(default)]
    ports: Vec<String>,
}

/// Spawn the subscription watcher thread.  The thread connects to vsock,
/// subscribes to container lifecycle events, and automatically removes port
/// forwards from `PortDispatcher` / `PortState` when a container exits.
fn start_subscription_watcher(
    vm: Arc<Vm>,
    dispatcher: Arc<PortDispatcher>,
    port_state: Arc<Mutex<PortState>>,
) {
    std::thread::Builder::new()
        .name("port-sub-watcher".into())
        .spawn(move || subscription_watcher_loop(vm, dispatcher, port_state))
        .expect("spawn port-sub-watcher");
}

fn subscription_watcher_loop(
    vm: Arc<Vm>,
    dispatcher: Arc<PortDispatcher>,
    port_state: Arc<Mutex<PortState>>,
) {
    use std::io::BufRead;

    // Local map: container name → port specs (e.g. ["8080:80"]).
    // Updated on every Snapshot and ContainerStarted event so that a later
    // ContainerExited can look up which ports belong to that container.
    let mut container_ports: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        // Connect to vsock (retry until the VM guest is ready).
        let vsock_fd = loop {
            match vm.connect_vsock() {
                Ok(fd) => break fd,
                Err(_) => {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        };

        // Send {"cmd":"subscribe"}\n — the GuestCommand::Subscribe wire format.
        const SUBSCRIBE_LINE: &[u8] = b"{\"cmd\":\"subscribe\"}\n";
        let vsock_raw = vsock_fd.into_raw_fd();
        {
            // SAFETY: vsock_raw is valid; mem::forget prevents double-close.
            let mut f = unsafe { std::fs::File::from_raw_fd(vsock_raw) };
            let ok = f.write_all(SUBSCRIBE_LINE).is_ok();
            std::mem::forget(f);
            if !ok {
                // SAFETY: we prevented the File from closing it; close manually.
                unsafe { libc::close(vsock_raw) };
                log::warn!("port-sub-watcher: failed to send subscribe command");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        }

        // Read NDJSON events until the connection closes.
        let f = unsafe { std::fs::File::from_raw_fd(vsock_raw) };
        let mut reader = std::io::BufReader::new(f);
        let mut line = String::new();
        log::info!("port-sub-watcher: subscribed to container events");

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<WatchEvent>(trimmed) {
                Ok(WatchEvent::Snapshot { containers }) => {
                    container_ports.clear();
                    for c in containers {
                        if !c.ports.is_empty() {
                            container_ports.insert(c.name, c.ports);
                        }
                    }
                    log::debug!(
                        "port-sub-watcher: snapshot — tracking {} containers with ports",
                        container_ports.len()
                    );
                }
                Ok(WatchEvent::ContainerStarted { container }) => {
                    if !container.ports.is_empty() {
                        log::debug!(
                            "port-sub-watcher: {} started with ports {:?}",
                            container.name,
                            container.ports
                        );
                        container_ports.insert(container.name, container.ports);
                    }
                }
                Ok(WatchEvent::ContainerExited { name }) => {
                    if let Some(ports) = container_ports.remove(&name) {
                        let mut ps = port_state.lock().unwrap();
                        for spec in &ports {
                            if let Some(pf) = parse_port_spec(spec) {
                                if ps.active.remove(&pf.host_port).is_some() {
                                    dispatcher.send(DispatchCmd::Remove {
                                        host_port: pf.host_port,
                                    });
                                    log::info!(
                                        "port-sub-watcher: {} exited — unregistered port {}",
                                        name,
                                        pf.host_port
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(WatchEvent::Unknown) => {}
                Err(_) => {
                    log::trace!("port-sub-watcher: unparseable line: {}", trimmed);
                }
            }
        }

        log::info!("port-sub-watcher: subscribe connection closed — reconnecting in 5s");
        std::thread::sleep(Duration::from_secs(5));
    }
}

// ---------------------------------------------------------------------------
// VmConfig from DaemonArgs
// ---------------------------------------------------------------------------

/// Build the argument list for the `vm-daemon-internal` subprocess.
///
/// Returns only the *arguments* (not the executable path).  Extracted as a
/// pure function so the subprocess arg serialization can be unit-tested
/// without spawning a real process.
pub(crate) fn daemon_subprocess_args(args: &DaemonArgs) -> Vec<std::ffi::OsString> {
    let mut v: Vec<std::ffi::OsString> = Vec::new();
    if args.profile != "default" {
        v.push("--profile".into());
        v.push((&args.profile).into());
    }
    v.push("--kernel".into());
    v.push(args.kernel.as_os_str().into());
    v.push("--disk".into());
    v.push(args.disk.as_os_str().into());
    if let Some(ref initrd) = args.initrd {
        v.push("--initrd".into());
        v.push(initrd.as_os_str().into());
    }
    v.push("--cmdline".into());
    v.push(args.cmdline.as_str().into());
    v.push("--memory".into());
    v.push(args.memory_mib.to_string().into());
    v.push("--cpus".into());
    v.push(args.cpus.to_string().into());
    for share in &args.virtiofs_shares {
        let mut spec = format!("{}:{}", share.host_path.display(), share.container_path);
        if share.read_only {
            spec.push_str(":ro");
        }
        v.push("--volume".into());
        v.push(spec.into());
    }
    for pf in &args.port_forwards {
        v.push("--port".into());
        v.push(format!("{}:{}", pf.host_port, pf.container_port).into());
    }
    for path in &args.extra_disks {
        v.push("--extra-disk".into());
        v.push(path.as_os_str().into());
    }
    v.push("vm-daemon-internal".into());
    v
}

fn build_vm_config(args: &DaemonArgs) -> VmConfig {
    let mut b = VmConfig::builder()
        .kernel(&args.kernel)
        .disk(&args.disk)
        .cmdline(build_cmdline(args))
        .memory_mib(args.memory_mib)
        .cpus(args.cpus)
        .relay_proxy_port(args.relay_proxy_port);
    if let Some(ref initrd) = args.initrd {
        b = b.initrd(initrd);
    }
    for share in &args.virtiofs_shares {
        b = b.virtiofs(&share.host_path, &share.tag, share.read_only);
    }
    for path in &args.extra_disks {
        b = b.extra_disk(path);
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
// Serial console relay
// ---------------------------------------------------------------------------

/// Fixed-capacity circular buffer that retains recent console output so that
/// late-connecting clients can replay what they missed.  Lives entirely in the
/// `console_relay_loop` thread; no synchronisation needed.
struct ConsoleRingBuffer {
    buf: Vec<u8>,
    /// Index of the oldest byte.
    head: usize,
    /// Number of valid bytes currently stored (0 ..= buf.len()).
    len: usize,
}

impl ConsoleRingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            head: 0,
            len: 0,
        }
    }

    /// Append `data` to the ring, overwriting the oldest bytes when full.
    fn push_slice(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        // If data exceeds capacity, only the last `cap` bytes matter.
        let data = if data.len() > cap {
            &data[data.len() - cap..]
        } else {
            data
        };
        for &b in data {
            let tail = (self.head + self.len) % cap;
            self.buf[tail] = b;
            if self.len < cap {
                self.len += 1;
            } else {
                // Overwrite oldest — advance head.
                self.head = (self.head + 1) % cap;
            }
        }
    }

    /// Copy all buffered bytes to `fd` in chronological order.
    /// Write errors are logged and silently ignored — a client that disappears
    /// mid-replay is not a fatal condition.
    fn replay_to_fd(&self, fd: RawFd) {
        if self.len == 0 {
            return;
        }
        let cap = self.buf.len();
        // The ring may wrap: first segment is head..min(head+len, cap),
        // second segment (if wrapped) is 0..remainder.
        let first_end = self.head + self.len;
        if first_end <= cap {
            // Contiguous — one write.
            unsafe {
                libc::write(
                    fd,
                    self.buf[self.head..first_end].as_ptr() as *const libc::c_void,
                    self.len,
                )
            };
        } else {
            // Wrapped — two writes.
            let first_len = cap - self.head;
            let second_len = self.len - first_len;
            unsafe {
                libc::write(
                    fd,
                    self.buf[self.head..].as_ptr() as *const libc::c_void,
                    first_len,
                );
                libc::write(
                    fd,
                    self.buf[..second_len].as_ptr() as *const libc::c_void,
                    second_len,
                );
            }
        }
    }

    #[cfg(test)]
    fn contents(&self) -> Vec<u8> {
        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            out.push(self.buf[(self.head + i) % cap]);
        }
        out
    }
}

/// Accept console clients forever.  Each client gets the serial port for its
/// session; when it disconnects we wait for the next one.  The serial port
/// socketpair end (`relay_fd`) is kept alive for the process lifetime.
///
/// IMPORTANT: when no client is connected we must continuously drain console
/// output from `relay_fd`.  If nobody reads, the socketpair buffer fills up
/// (~128 KB on macOS), AVF's virtio-console backend stalls, and the guest
/// kernel's hvc_write() path blocks while holding spinlocks in the printk
/// path.  That prevents CPUs from passing through RCU quiescent states,
/// causing rcu_preempt stalls at every boot.
///
/// Rather than discarding drained bytes we store them in a ring buffer so
/// late-connecting clients can replay everything they missed.
fn console_relay_loop(listener: UnixListener, relay_fd: OwnedFd) {
    let raw = relay_fd.into_raw_fd();
    let listener_fd = listener.as_raw_fd();
    let mut ring = ConsoleRingBuffer::new(256 * 1024);
    let mut drain_buf = vec![0u8; 4096];
    loop {
        // Poll for either a new console client OR data to drain from the VM.
        let mut pfds = [
            libc::pollfd {
                fd: listener_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: raw,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("console poll: {}", err);
            break;
        }

        // New console client connecting — replay buffered output then proxy live.
        if pfds[0].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((client, _)) => {
                    log::info!("console client connected");
                    ring.replay_to_fd(client.as_raw_fd());
                    proxy_console(client, raw);
                    log::info!("console client disconnected");
                }
                Err(e) => log::warn!("console accept: {}", e),
            }
            continue;
        }

        // Console output from VM with no client attached — drain into ring to
        // prevent the guest's hvc write path from blocking.
        if pfds[1].revents & libc::POLLIN != 0 {
            let n = unsafe {
                libc::read(
                    raw,
                    drain_buf.as_mut_ptr() as *mut libc::c_void,
                    drain_buf.len(),
                )
            };
            if n > 0 {
                ring.push_slice(&drain_buf[..n as usize]);
            }
        }
    }
}

/// Bidirectionally proxy between a Unix socket client and the serial console
/// fd.  Uses a single-threaded poll(2) loop so that a client disconnect
/// closes both directions cleanly without leaking the relay fd.
fn proxy_console(client: UnixStream, relay_fd: RawFd) {
    let client_fd = client.as_raw_fd();
    // dup the relay fd so we can close the dups independently when done
    // without closing the original (which must stay open for the next client).
    let r_read = unsafe { libc::dup(relay_fd) };
    let r_write = unsafe { libc::dup(relay_fd) };
    if r_read < 0 || r_write < 0 {
        log::error!("dup relay_fd failed");
        unsafe {
            if r_read >= 0 {
                libc::close(r_read);
            }
            if r_write >= 0 {
                libc::close(r_write);
            }
        }
        return;
    }

    let mut buf = vec![0u8; 4096];
    loop {
        let mut pfds = [
            libc::pollfd {
                fd: client_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: r_read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, 1000) };
        if n < 0 {
            break;
        }

        // Client → relay
        if pfds[0].revents & libc::POLLIN != 0 {
            let n =
                unsafe { libc::read(client_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            unsafe { libc::write(r_write, buf.as_ptr() as *const libc::c_void, n as usize) };
        }
        if pfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }

        // Relay → client
        if pfds[1].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(r_read, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            let w =
                unsafe { libc::write(client_fd, buf.as_ptr() as *const libc::c_void, n as usize) };
            if w < 0 {
                break;
            }
        }
        if pfds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }

    unsafe {
        libc::close(r_read);
        libc::close(r_write);
    }
    // `client` is dropped here, closing client_fd.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        build_cmdline_from_parts, daemon_subprocess_args, mounts_match, parse_port_spec,
        DaemonArgs, PortForward, VirtiofsShare,
    };
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
        assert_eq!(
            build_cmdline_from_parts("console=hvc0", &[]),
            "console=hvc0"
        );
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

    #[test]
    fn parse_port_colon_form() {
        let pf = parse_port_spec("8080:80").unwrap();
        assert_eq!(
            pf,
            PortForward {
                host_port: 8080,
                container_port: 80
            }
        );
    }

    #[test]
    fn parse_port_bare_form() {
        let pf = parse_port_spec("3000").unwrap();
        assert_eq!(
            pf,
            PortForward {
                host_port: 3000,
                container_port: 3000
            }
        );
    }

    #[test]
    fn parse_port_invalid_returns_none() {
        assert!(parse_port_spec("notaport").is_none());
        assert!(parse_port_spec("abc:def").is_none());
        assert!(parse_port_spec("99999:80").is_none()); // u16 overflow
    }

    // -----------------------------------------------------------------------
    // ConsoleRingBuffer
    // -----------------------------------------------------------------------
    use super::ConsoleRingBuffer;

    #[test]
    fn ring_empty_contents() {
        let r = ConsoleRingBuffer::new(8);
        assert_eq!(r.contents(), b"");
    }

    #[test]
    fn ring_push_less_than_capacity() {
        let mut r = ConsoleRingBuffer::new(8);
        r.push_slice(b"hello");
        assert_eq!(r.contents(), b"hello");
    }

    #[test]
    fn ring_push_exactly_capacity() {
        let mut r = ConsoleRingBuffer::new(5);
        r.push_slice(b"hello");
        assert_eq!(r.contents(), b"hello");
    }

    #[test]
    fn ring_push_overflow_overwrites_oldest() {
        let mut r = ConsoleRingBuffer::new(4);
        r.push_slice(b"abcd"); // fills buffer: [a,b,c,d]
        r.push_slice(b"ef"); // overwrites a,b  → [e,f,c,d] with head=2
        assert_eq!(r.contents(), b"cdef");
    }

    #[test]
    fn ring_multiple_pushes_in_order() {
        let mut r = ConsoleRingBuffer::new(8);
        r.push_slice(b"abc");
        r.push_slice(b"def");
        assert_eq!(r.contents(), b"abcdef");
    }

    #[test]
    fn ring_large_push_keeps_last_cap_bytes() {
        let mut r = ConsoleRingBuffer::new(4);
        r.push_slice(b"123456789"); // only last 4 bytes retained
        assert_eq!(r.contents(), b"6789");
    }

    #[test]
    fn ring_overflow_then_more_data() {
        let mut r = ConsoleRingBuffer::new(4);
        r.push_slice(b"abcdefgh"); // only last 4 retained: efgh
        r.push_slice(b"ij"); // overwrites e,f → ghi j
        assert_eq!(r.contents(), b"ghij");
    }

    #[test]
    fn ring_replay_to_fd_matches_contents() {
        let mut r = ConsoleRingBuffer::new(8);
        r.push_slice(b"boot!");

        // socketpair: write end for replay, read end to verify
        let mut fds = [-1i32; 2];
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        let (read_fd, write_fd) = (fds[0], fds[1]);

        r.replay_to_fd(write_fd);
        unsafe { libc::close(write_fd) };

        let mut out = vec![0u8; 16];
        let n = unsafe { libc::read(read_fd, out.as_mut_ptr() as *mut libc::c_void, out.len()) };
        unsafe { libc::close(read_fd) };

        assert!(n > 0);
        assert_eq!(&out[..n as usize], b"boot!");
    }

    #[test]
    fn ring_replay_wrapped_to_fd() {
        // Force a wrap: capacity=4, push "abcdef" → ring holds "cdef" (wrapped)
        let mut r = ConsoleRingBuffer::new(4);
        r.push_slice(b"abcdef"); // last 4: cdef; head=2, buf=[e,f,c,d]

        let mut fds = [-1i32; 2];
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        let (read_fd, write_fd) = (fds[0], fds[1]);

        r.replay_to_fd(write_fd);
        unsafe { libc::close(write_fd) };

        let mut out = vec![0u8; 16];
        let n = unsafe { libc::read(read_fd, out.as_mut_ptr() as *mut libc::c_void, out.len()) };
        unsafe { libc::close(read_fd) };

        assert!(n > 0);
        assert_eq!(&out[..n as usize], b"cdef");
    }

    fn base_args() -> DaemonArgs {
        DaemonArgs {
            kernel: PathBuf::from("/out/vmlinuz"),
            initrd: None,
            disk: PathBuf::from("/out/root.img"),
            cmdline: "console=hvc0".into(),
            memory_mib: 4096,
            cpus: 2,
            virtiofs_shares: vec![],
            port_forwards: vec![],
            profile: "default".into(),
            extra_disks: vec![],
            relay_proxy_port: pelagos_vz::nat_relay::RELAY_PROXY_PORT,
        }
    }

    #[test]
    fn subprocess_args_no_extra_disks() {
        let args = base_args();
        let v = daemon_subprocess_args(&args);
        assert!(!v.contains(&"--extra-disk".into()));
        assert!(v.contains(&"vm-daemon-internal".into()));
    }

    #[test]
    fn subprocess_args_one_extra_disk() {
        let mut args = base_args();
        args.extra_disks.push(PathBuf::from("/out/build.img"));
        let v = daemon_subprocess_args(&args);
        let idx = v
            .iter()
            .position(|a| a == "--extra-disk")
            .expect("--extra-disk missing");
        assert_eq!(v[idx + 1], std::ffi::OsStr::new("/out/build.img"));
    }

    #[test]
    fn subprocess_args_two_extra_disks() {
        let mut args = base_args();
        args.extra_disks.push(PathBuf::from("/out/build.img"));
        args.extra_disks.push(PathBuf::from("/out/data.img"));
        let v = daemon_subprocess_args(&args);
        let count = v.iter().filter(|a| *a == "--extra-disk").count();
        assert_eq!(count, 2);
        // Verify order: build.img before data.img
        let first = v.iter().position(|a| a == "--extra-disk").unwrap();
        assert_eq!(v[first + 1], std::ffi::OsStr::new("/out/build.img"));
    }

    #[test]
    fn subprocess_args_extra_disk_before_subcommand() {
        let mut args = base_args();
        args.extra_disks.push(PathBuf::from("/out/build.img"));
        let v = daemon_subprocess_args(&args);
        let disk_idx = v.iter().position(|a| a == "--extra-disk").unwrap();
        let sub_idx = v.iter().position(|a| a == "vm-daemon-internal").unwrap();
        assert!(disk_idx < sub_idx);
    }

    #[test]
    fn subprocess_args_named_profile_emits_flag() {
        let mut args = base_args();
        args.profile = "build".into();
        let v = daemon_subprocess_args(&args);
        let idx = v
            .iter()
            .position(|a| a == "--profile")
            .expect("--profile missing");
        assert_eq!(v[idx + 1], std::ffi::OsStr::new("build"));
    }

    // -----------------------------------------------------------------------
    // DaemonCmd / DaemonResponse serialization
    // -----------------------------------------------------------------------

    #[test]
    fn daemon_cmd_register_port_roundtrip() {
        use super::{DaemonCmd, DaemonResponse};
        let cmd = DaemonCmd::RegisterPort {
            host_port: 8080,
            container_port: 80,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["RegisterPort"]["host_port"], 8080);
        assert_eq!(v["RegisterPort"]["container_port"], 80);

        let resp_ok = DaemonResponse::Ok;
        let r = serde_json::to_string(&resp_ok).unwrap();
        assert!(r.contains("\"ok\""));

        let resp_err = DaemonResponse::Err {
            message: "port 8080 is already registered".into(),
        };
        let e = serde_json::to_string(&resp_err).unwrap();
        assert!(e.contains("\"err\""));
        assert!(e.contains("already registered"));
    }

    #[test]
    fn daemon_cmd_unregister_port_roundtrip() {
        use super::DaemonCmd;
        let cmd = DaemonCmd::UnregisterPort { host_port: 8080 };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["UnregisterPort"]["host_port"], 8080);
    }

    // -----------------------------------------------------------------------
    // WatchEvent deserialization (validates wire-format match with pelagos-guest)
    // -----------------------------------------------------------------------

    #[test]
    fn watch_event_snapshot_deserializes() {
        use super::WatchEvent;
        let json = r#"{"type":"snapshot","containers":[{"name":"nginx","status":"running","rootfs":"alpine","pid":1,"started_at":"2024-01-01","ports":["8080:80"]}],"vm_running":true}"#;
        match serde_json::from_str::<WatchEvent>(json).unwrap() {
            WatchEvent::Snapshot { containers } => {
                assert_eq!(containers.len(), 1);
                assert_eq!(containers[0].name, "nginx");
                assert_eq!(containers[0].ports, vec!["8080:80"]);
            }
            other => panic!("unexpected: {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn watch_event_container_started_deserializes() {
        use super::WatchEvent;
        let json = r#"{"type":"container_started","container":{"name":"redis","status":"running","rootfs":"redis","pid":2,"started_at":"2024-01-01","ports":["6379:6379"]}}"#;
        match serde_json::from_str::<WatchEvent>(json).unwrap() {
            WatchEvent::ContainerStarted { container } => {
                assert_eq!(container.name, "redis");
                assert_eq!(container.ports, vec!["6379:6379"]);
            }
            other => panic!("unexpected: {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn watch_event_container_exited_deserializes() {
        use super::WatchEvent;
        let json = r#"{"type":"container_exited","name":"nginx","exit_code":0}"#;
        match serde_json::from_str::<WatchEvent>(json).unwrap() {
            WatchEvent::ContainerExited { name } => {
                assert_eq!(name, "nginx");
            }
            other => panic!("unexpected: {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn watch_event_heartbeat_is_unknown() {
        use super::WatchEvent;
        let json = r#"{"type":"heartbeat","ts":1234567890}"#;
        // heartbeats are caught by the Unknown variant
        assert!(matches!(
            serde_json::from_str::<WatchEvent>(json).unwrap(),
            WatchEvent::Unknown
        ));
    }

    #[test]
    fn watch_event_snapshot_no_ports_ignored() {
        use super::WatchEvent;
        // containers without ports → empty ports vec
        let json = r#"{"type":"snapshot","containers":[{"name":"busybox","status":"running","rootfs":"busybox","pid":3,"started_at":"2024-01-01"}],"vm_running":true}"#;
        match serde_json::from_str::<WatchEvent>(json).unwrap() {
            WatchEvent::Snapshot { containers } => {
                assert_eq!(containers[0].ports, Vec::<String>::new());
            }
            _ => panic!("expected Snapshot"),
        }
    }
}
