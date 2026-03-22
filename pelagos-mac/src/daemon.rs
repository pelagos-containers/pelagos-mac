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
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
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
        // Verify that any explicitly requested port forwards are active.
        // Requesting no ports (-p not given) always succeeds even if the daemon
        // has active forwards (the proxies are already running).
        if !args.port_forwards.is_empty() {
            let running_ports = state.read_ports()?;
            for pf in &args.port_forwards {
                if !running_ports.contains(pf) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "port {}:{} is not forwarded by the running daemon; \
                             run 'pelagos vm stop' first, then retry",
                            pf.host_port, pf.container_port
                        ),
                    ));
                }
            }
        }
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

    // Persist mount, port, and extra-disk configuration.
    state
        .write_mounts(&args.virtiofs_shares)
        .unwrap_or_else(|e| {
            log::error!("write mounts: {}", e);
        });
    state.write_ports(&args.port_forwards).unwrap_or_else(|e| {
        log::error!("write ports: {}", e);
    });
    state
        .write_extra_disks(&args.extra_disks)
        .unwrap_or_else(|e| {
            log::error!("write extra_disks: {}", e);
        });

    // Start a TCP proxy thread for each requested port forward.
    for pf in &args.port_forwards {
        let host_port = pf.host_port;
        let container_port = pf.container_port;
        std::thread::spawn(move || {
            port_forward_loop(host_port, container_port);
        });
    }

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
            let conn_id = std::thread::current().id();
            log::info!("[{conn_id:?}] client connected");
            let vsock_fd = match vm2.connect_vsock() {
                Ok(fd) => fd,
                Err(e) => {
                    log::error!("[{conn_id:?}] vsock connect: {}", e);
                    return; // unix stream dropped → EOF on CLI side
                }
            };
            drop(vm2); // release Arc before entering the proxy loop
            proxy(unix, vsock_fd, conn_id);
            log::info!("[{conn_id:?}] client disconnected");
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
// Port forwarding
// ---------------------------------------------------------------------------

/// Accept TCP connections on `host_port` and proxy each one to the VM container
/// port via the smoltcp NAT relay proxy (127.0.0.1:RELAY_PROXY_PORT).
///
/// Protocol: connect to relay, send 2-byte big-endian container_port, then
/// use the socket as a bidirectional stream into the VM.  This avoids needing
/// a macOS route to 192.168.105.2 (which no longer exists without vmnet).
pub(crate) fn port_forward_loop(host_port: u16, container_port: u16) {
    use std::io::Write;
    const RELAY_PROXY_PORT: u16 = pelagos_vz::nat_relay::RELAY_PROXY_PORT;

    let listener = match TcpListener::bind(("0.0.0.0", host_port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("port forward bind 0.0.0.0:{}: {}", host_port, e);
            return;
        }
    };
    log::info!(
        "port forward active: 0.0.0.0:{} → VM:{}  (via relay :{})",
        host_port,
        container_port,
        RELAY_PROXY_PORT
    );
    for incoming in listener.incoming() {
        let client = match incoming {
            Ok(s) => s,
            Err(e) => {
                log::warn!("port forward accept: {}", e);
                continue;
            }
        };
        std::thread::spawn(move || {
            let relay_addr = std::net::SocketAddr::from(([127, 0, 0, 1], RELAY_PROXY_PORT));
            let mut server =
                match TcpStream::connect_timeout(&relay_addr, std::time::Duration::from_secs(5)) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("port forward connect relay:{}: {}", RELAY_PROXY_PORT, e);
                        return;
                    }
                };
            // Send 2-byte big-endian container port.
            if server.write_all(&container_port.to_be_bytes()).is_err() {
                return;
            }
            tcp_proxy(client, server);
        });
    }
}

/// Bidirectionally proxy two TCP streams.  Returns when either side closes.
fn tcp_proxy(client: TcpStream, server: TcpStream) {
    let mut client_read = client;
    let mut server_read = server;
    let mut client_write = client_read.try_clone().expect("tcp clone");
    let mut server_write = server_read.try_clone().expect("tcp clone");

    // client → server
    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut server_write);
        // Signal server that client is done sending.
        let _ = server_write.shutdown(std::net::Shutdown::Write);
    });

    // server → client
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut server_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Write);
    });

    let _ = t1.join();
    let _ = t2.join();
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
        .cpus(args.cpus);
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
        r.push_slice(b"ef");   // overwrites a,b  → [e,f,c,d] with head=2
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
        r.push_slice(b"ij");       // overwrites e,f → ghi j
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
}
