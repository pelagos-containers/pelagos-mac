//! pelagos — macOS CLI for the pelagos container runtime.
//!
//! Boots a Linux VM via Apple Virtualization Framework (pelagos-vz), then
//! forwards subcommands to the pelagos-guest daemon inside the VM over vsock.
//! The VM is kept alive between invocations via a background daemon process
//! that owns the VZVirtualMachine and proxies vsock connections over a Unix socket.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

mod daemon;
mod state;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "pelagos", about = "pelagos container runtime for macOS")]
struct Cli {
    /// Path to the VM kernel image
    #[arg(long, env = "PELAGOS_KERNEL")]
    kernel: Option<PathBuf>,

    /// Path to the initrd image
    #[arg(long, env = "PELAGOS_INITRD")]
    initrd: Option<PathBuf>,

    /// Path to the root disk image
    #[arg(long, env = "PELAGOS_DISK")]
    disk: Option<PathBuf>,

    /// Kernel command-line arguments
    #[arg(long, env = "PELAGOS_CMDLINE", default_value = "console=hvc0")]
    cmdline: String,

    /// Memory in MiB (default 1024)
    #[arg(long, default_value = "1024")]
    memory: usize,

    /// Number of vCPUs (default 2)
    #[arg(long, default_value = "2")]
    cpus: usize,

    /// Bind-mount a host directory into the container: /host/path:/container/path[:ro]
    /// May be specified multiple times.
    #[arg(short = 'v', long = "volume", global = true)]
    volumes: Vec<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a container image inside the VM
    Run {
        /// Container image name (e.g. alpine)
        image: String,
        /// Arguments to pass to the container (use -- before flags, e.g. -- -c "cmd")
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command interactively in a container (stdin forwarded, optional TTY)
    Exec {
        /// Container image name
        image: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Allocate a pseudo-TTY (default: auto-detect from stdin)
        #[arg(short = 't', long)]
        tty: bool,
    },
    /// Ping the guest daemon (readiness check)
    Ping,
    /// Persistent VM management
    Vm {
        #[command(subcommand)]
        sub: VmCommands,
    },
    /// Internal: run as the persistent VM daemon. Not for direct use.
    #[command(hide = true)]
    VmDaemonInternal,
}

#[derive(Subcommand)]
enum VmCommands {
    /// Stop the persistent VM daemon
    Stop,
    /// Show persistent VM daemon status
    Status,
}

// ---------------------------------------------------------------------------
// Guest protocol types (mirrors pelagos-guest)
// ---------------------------------------------------------------------------

/// A mount to pass to the guest for bind-mounting inside the container.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GuestMount {
    /// virtiofs tag (e.g. "share0") — already mounted at /mnt/<tag> in the guest.
    pub tag: String,
    /// Absolute path inside the container.
    pub container_path: String,
}

#[derive(Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum GuestCommand {
    Run {
        image: String,
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<GuestMount>,
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        tty: bool,
    },
    Ping,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum GuestResponse {
    Stream { stream: String, data: String },
    Exit { exit: i32 },
    Pong { pong: bool },
    Error { error: String },
    Ready { ready: bool },
}

// ---------------------------------------------------------------------------
// Framed binary protocol constants
// ---------------------------------------------------------------------------

const FRAME_STDIN: u8 = 0;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const FRAME_EXIT: u8 = 3;
const FRAME_RESIZE: u8 = 4;

fn send_frame(w: &mut impl Write, frame_type: u8, data: &[u8]) -> io::Result<()> {
    w.write_all(&[frame_type])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn recv_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut type_buf = [0u8; 1];
    r.read_exact(&mut type_buf)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok((type_buf[0], data))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::VmDaemonInternal => {
            let args = daemon_args_from_cli(&cli);
            daemon::run(args); // -> !
        }

        Commands::Vm {
            sub: VmCommands::Stop,
        } => vm_stop(),
        Commands::Vm {
            sub: VmCommands::Status,
        } => vm_status(),

        Commands::Run {
            ref image,
            ref args,
        } => {
            let image = image.clone();
            let args = args.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            // Build the guest-side mount list from the parsed shares.
            let mounts: Vec<GuestMount> = daemon_args
                .virtiofs_shares
                .iter()
                .map(|s| GuestMount {
                    tag: s.tag.clone(),
                    container_path: s.container_path.clone(),
                })
                .collect();
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(run_command(stream, image, args, mounts));
        }

        Commands::Exec {
            ref image,
            ref args,
            tty,
        } => {
            let image = image.clone();
            let args = args.clone();
            let tty = tty || unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(exec_command(stream, image, args, tty));
        }

        Commands::Ping => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(ping_command(stream));
        }
    }
}

fn daemon_args_from_cli(cli: &Cli) -> daemon::DaemonArgs {
    let kernel = cli.kernel.clone().unwrap_or_else(|| {
        log::error!("--kernel / PELAGOS_KERNEL is required");
        process::exit(1);
    });
    let disk = cli.disk.clone().unwrap_or_else(|| {
        log::error!("--disk / PELAGOS_DISK is required");
        process::exit(1);
    });

    let virtiofs_shares = parse_volumes(&cli.volumes);

    daemon::DaemonArgs {
        kernel,
        initrd: cli.initrd.clone(),
        disk,
        cmdline: cli.cmdline.clone(),
        memory_mib: cli.memory,
        cpus: cli.cpus,
        virtiofs_shares,
    }
}

/// Parse `-v /host/path:/container/path[:ro]` strings into `VirtiofsShare`s.
/// Tags are assigned as `share0`, `share1`, etc.
fn parse_volumes(volumes: &[String]) -> Vec<daemon::VirtiofsShare> {
    volumes
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let parts: Vec<&str> = spec.splitn(3, ':').collect();
            if parts.len() < 2 {
                log::error!(
                    "invalid volume spec {:?}: expected /host:/container[:ro]",
                    spec
                );
                process::exit(1);
            }
            let host_path = PathBuf::from(parts[0]);
            let container_path = parts[1].to_string();
            let read_only = parts.get(2).is_some_and(|s| *s == "ro");
            daemon::VirtiofsShare {
                host_path,
                tag: format!("share{}", i),
                read_only,
                container_path,
            }
        })
        .collect()
}

fn connect_or_exit() -> UnixStream {
    daemon::connect().unwrap_or_else(|e| {
        log::error!("connect to VM daemon: {}", e);
        process::exit(1);
    })
}

// ---------------------------------------------------------------------------
// VM management commands
// ---------------------------------------------------------------------------

fn vm_stop() {
    let state = state::StateDir::open().unwrap_or_else(|e| {
        log::error!("state dir: {}", e);
        process::exit(1);
    });
    match state.running_pid() {
        None => {
            println!("no VM running");
        }
        Some(pid) => {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            println!("sent SIGTERM to daemon (pid {})", pid);
            // Wait for the daemon to fully exit before returning.  Without
            // this wait a caller that immediately re-invokes pelagos (e.g.
            // the e2e test restarting with different mounts) sees the still-
            // alive daemon and gets a "different mount configuration" error.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                if state.running_pid().is_none() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            log::warn!("daemon (pid {}) did not exit within 15 s", pid);
        }
    }
}

fn vm_status() {
    let state = state::StateDir::open().unwrap_or_else(|e| {
        log::error!("state dir: {}", e);
        process::exit(1);
    });
    match state.running_pid() {
        None => {
            println!("stopped");
            process::exit(1);
        }
        Some(pid) => {
            println!("running (pid {})", pid);
        }
    }
}

// ---------------------------------------------------------------------------
// Command handlers — operate on a UnixStream connected to the daemon
// ---------------------------------------------------------------------------

fn run_command(
    stream: UnixStream,
    image: String,
    args: Vec<String>,
    mounts: Vec<GuestMount>,
) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    let cmd = GuestCommand::Run {
        image,
        args,
        mounts,
    };
    let mut msg = serde_json::to_string(&cmd).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("write error: {}", e);
        return 1;
    }

    let mut exit_code = 1;
    let mut line = String::new();
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
        match serde_json::from_str::<GuestResponse>(trimmed) {
            Ok(GuestResponse::Stream { stream, data }) => {
                if stream == "stderr" {
                    eprint!("{}", data);
                } else {
                    print!("{}", data);
                }
            }
            Ok(GuestResponse::Exit { exit }) => {
                exit_code = exit;
                break;
            }
            Ok(GuestResponse::Error { error }) => {
                log::error!("guest error: {}", error);
                break;
            }
            Ok(resp) => {
                log::warn!("unexpected response: {:?}", resp);
            }
            Err(e) => {
                log::error!("parse error: {} (line: {:?})", e, trimmed);
                break;
            }
        }
    }
    exit_code
}

fn ping_command(stream: UnixStream) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    let mut msg = serde_json::to_string(&GuestCommand::Ping).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("write error: {}", e);
        return 1;
    }

    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            log::error!("no response from guest");
            return 1;
        }
        Ok(_) => {}
    }
    match serde_json::from_str::<GuestResponse>(line.trim_end()) {
        Ok(GuestResponse::Pong { pong: true }) => {
            println!("pong");
            0
        }
        other => {
            log::error!("unexpected ping response: {:?}", other);
            1
        }
    }
}

/// Run an exec command: send the exec JSON handshake, read ready ack, then
/// switch to framed binary protocol forwarding stdin/stdout/stderr.
fn exec_command(stream: UnixStream, image: String, args: Vec<String>, tty: bool) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream.try_clone().expect("clone stream");

    // Send exec handshake.
    let cmd = GuestCommand::Exec {
        image,
        args,
        env: std::collections::HashMap::new(),
        tty,
    };
    let mut msg = serde_json::to_string(&cmd).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("exec: write handshake: {}", e);
        return 1;
    }

    // Read JSON lines until we get ready:true or an error.
    // The guest may send pull progress (Stream/stderr) before the ready ack.
    let ready = loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                log::error!("exec: no ready ack from guest");
                return 1;
            }
            Ok(_) => {}
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<GuestResponse>(trimmed) {
            Ok(GuestResponse::Ready { ready: true }) => break true,
            Ok(GuestResponse::Error { error }) => {
                log::error!("exec: guest error: {}", error);
                return 1;
            }
            Ok(GuestResponse::Stream { stream, data }) => {
                // Pull progress — relay to stderr and continue waiting.
                if stream == "stderr" {
                    eprint!("{}", data);
                } else {
                    print!("{}", data);
                }
            }
            Ok(resp) => {
                log::warn!("exec: unexpected pre-ready response: {:?}", resp);
            }
            Err(e) => {
                log::error!(
                    "exec: parse error waiting for ready: {} (line: {:?})",
                    e,
                    trimmed
                );
                return 1;
            }
        }
    };
    if !ready {
        return 1;
    }

    // Optionally put the terminal in raw mode.
    let saved_termios: Option<libc::termios> = if tty { Some(enter_raw_mode()) } else { None };

    // Spawn stdin-forwarding thread.
    // Uses a pipe to also signal resize events.
    let (resize_r, resize_w) = create_pipe();

    // Install SIGWINCH handler writing to resize_w.
    install_sigwinch_handler(resize_w);

    let writer_arc = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let writer_stdin = std::sync::Arc::clone(&writer_arc);
    let writer_resize = std::sync::Arc::clone(&writer_arc);

    // Stdin thread: read raw bytes from stdin, send as FRAME_STDIN.
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            // Use poll so we can interleave resize pipe reads with stdin.
            let mut fds = [
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: resize_r,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ret = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ret <= 0 {
                break;
            }
            // Handle resize pipe first.
            if fds[1].revents & libc::POLLIN != 0 {
                // Drain the pipe byte.
                let mut byte = [0u8; 1];
                unsafe {
                    libc::read(resize_r, byte.as_mut_ptr() as *mut libc::c_void, 1);
                }
                // Read terminal size and send Resize frame.
                if let Some(resize_data) = read_winsize() {
                    let mut w = writer_resize.lock().unwrap();
                    let _ = send_frame(&mut *w, FRAME_RESIZE, &resize_data);
                }
            }
            // Handle stdin.
            if fds[0].revents & libc::POLLIN != 0 {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        // EOF — send a zero-length Stdin frame so the guest
                        // knows to close the child's stdin pipe.
                        let mut w = writer_stdin.lock().unwrap();
                        let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                        break;
                    }
                    Ok(n) => {
                        let mut w = writer_stdin.lock().unwrap();
                        if send_frame(&mut *w, FRAME_STDIN, &buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            // If stdin got HUP, stop.
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                let mut w = writer_stdin.lock().unwrap();
                let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                break;
            }
        }
    });

    // Main thread: read frames from the guest.
    let exit_code = read_guest_frames(&mut reader, saved_termios.is_some());

    // Restore terminal if we changed it.
    if let Some(saved) = saved_termios {
        restore_terminal(saved);
    }

    exit_code
}

/// Read frames from the guest until an Exit frame is received.
fn read_guest_frames(reader: &mut impl Read, _tty: bool) -> i32 {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    loop {
        match recv_frame(reader) {
            Ok((FRAME_STDOUT, data)) => {
                let _ = stdout.write_all(&data);
                let _ = stdout.flush();
            }
            Ok((FRAME_STDERR, data)) => {
                let _ = stderr.write_all(&data);
                let _ = stderr.flush();
            }
            Ok((FRAME_EXIT, data)) => {
                if data.len() == 4 {
                    let code = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    return code;
                }
                return 0;
            }
            Ok((frame_type, _)) => {
                log::warn!("exec: unexpected frame type {}", frame_type);
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::UnexpectedEof
                    && e.kind() != io::ErrorKind::ConnectionReset
                {
                    log::error!("exec: frame read error: {}", e);
                }
                return 1;
            }
        }
    }
}

/// Put stdin into raw mode. Returns the saved termios to restore later.
fn enter_raw_mode() -> libc::termios {
    unsafe {
        let mut termios = std::mem::zeroed::<libc::termios>();
        libc::tcgetattr(libc::STDIN_FILENO, &mut termios);
        let saved = termios;
        libc::cfmakeraw(&mut termios);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
        saved
    }
}

/// Restore the terminal to a saved state.
fn restore_terminal(saved: libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
    }
}

/// Create a pipe, return (read_fd, write_fd).
fn create_pipe() -> (libc::c_int, libc::c_int) {
    let mut fds = [0i32; 2];
    unsafe { libc::pipe(fds.as_mut_ptr()) };
    (fds[0], fds[1])
}

// Global write end of the SIGWINCH pipe.
static SIGWINCH_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn sigwinch_handler(_: libc::c_int) {
    let fd = SIGWINCH_PIPE_W.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let byte = [0u8; 1];
        unsafe { libc::write(fd, byte.as_ptr() as *const libc::c_void, 1) };
    }
}

fn install_sigwinch_handler(write_fd: libc::c_int) {
    SIGWINCH_PIPE_W.store(write_fd, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            sigwinch_handler as *const () as libc::sighandler_t,
        );
    }
}

/// Read current terminal window size. Returns 4 bytes: u16 rows + u16 cols, big-endian.
fn read_winsize() -> Option<Vec<u8>> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret < 0 {
        return None;
    }
    let mut data = Vec::with_capacity(4);
    data.extend_from_slice(&ws.ws_row.to_be_bytes());
    data.extend_from_slice(&ws.ws_col.to_be_bytes());
    Some(data)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        recv_frame, send_frame, GuestCommand, GuestMount, GuestResponse, FRAME_EXIT, FRAME_RESIZE,
        FRAME_STDIN, FRAME_STDOUT,
    };
    use std::io::Cursor;

    #[test]
    fn pong_deserializes() {
        let json = r#"{"pong":{"pong":true}}"#;
        let resp: GuestResponse = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(resp, GuestResponse::Pong { pong: true }));
    }

    #[test]
    fn stream_stdout_deserializes() {
        let json = r#"{"stream":{"stream":"stdout","data":"hello\n"}}"#;
        let resp: GuestResponse = serde_json::from_str(json).expect("parse failed");
        match resp {
            GuestResponse::Stream { stream, data } => {
                assert_eq!(stream, "stdout");
                assert_eq!(data, "hello\n");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn exit_deserializes() {
        let json = r#"{"exit":{"exit":0}}"#;
        let resp: GuestResponse = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(resp, GuestResponse::Exit { exit: 0 }));
    }

    #[test]
    fn run_command_serializes() {
        let cmd = GuestCommand::Run {
            image: "alpine".into(),
            args: vec!["/bin/echo".into(), "hello".into()],
            mounts: vec![],
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["image"], "alpine");
        assert_eq!(v["args"][0], "/bin/echo");
    }

    #[test]
    fn run_command_with_mounts_serializes() {
        let cmd = GuestCommand::Run {
            image: "alpine".into(),
            args: vec!["cat".into(), "/data/hello.txt".into()],
            mounts: vec![GuestMount {
                tag: "share0".into(),
                container_path: "/data".into(),
            }],
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["mounts"][0]["tag"], "share0");
        assert_eq!(v["mounts"][0]["container_path"], "/data");
    }

    #[test]
    fn ping_command_serializes() {
        let cmd = GuestCommand::Ping;
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "ping");
    }

    #[test]
    fn exec_command_serializes() {
        let cmd = GuestCommand::Exec {
            image: "alpine".into(),
            args: vec!["sh".into()],
            env: std::collections::HashMap::new(),
            tty: true,
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "exec");
        assert_eq!(v["image"], "alpine");
        assert_eq!(v["tty"], true);
    }

    #[test]
    fn ready_response_deserializes() {
        let json = r#"{"ready":{"ready":true}}"#;
        let resp: GuestResponse = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(resp, GuestResponse::Ready { ready: true }));
    }

    #[test]
    fn parse_volumes_basic() {
        use super::parse_volumes;
        let specs = vec!["/host/foo:/container/bar".to_string()];
        let shares = parse_volumes(&specs);
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].tag, "share0");
        assert_eq!(shares[0].container_path, "/container/bar");
        assert!(!shares[0].read_only);
    }

    #[test]
    fn parse_volumes_readonly() {
        use super::parse_volumes;
        let specs = vec![
            "/host/a:/ctr/a:ro".to_string(),
            "/host/b:/ctr/b".to_string(),
        ];
        let shares = parse_volumes(&specs);
        assert!(shares[0].read_only);
        assert!(!shares[1].read_only);
        assert_eq!(shares[0].tag, "share0");
        assert_eq!(shares[1].tag, "share1");
    }

    // ---------------------------------------------------------------------------
    // Frame encode/decode tests
    // ---------------------------------------------------------------------------

    #[test]
    fn frame_encode_decode_roundtrip() {
        let payload = b"hello world";
        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_STDOUT, payload).unwrap();

        let mut cursor = Cursor::new(buf);
        let (frame_type, data) = recv_frame(&mut cursor).unwrap();
        assert_eq!(frame_type, FRAME_STDOUT);
        assert_eq!(data, payload);
    }

    #[test]
    fn frame_exit_decode() {
        // Exit frame: type=3, length=4, data = i32 big-endian exit code
        let exit_code: i32 = 42;
        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_EXIT, &exit_code.to_be_bytes()).unwrap();

        let mut cursor = Cursor::new(buf);
        let (frame_type, data) = recv_frame(&mut cursor).unwrap();
        assert_eq!(frame_type, FRAME_EXIT);
        assert_eq!(data.len(), 4);
        let decoded = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        assert_eq!(decoded, 42);
    }

    #[test]
    fn frame_resize_encode() {
        // Resize frame: 4 bytes = u16 rows + u16 cols, big-endian
        let rows: u16 = 24;
        let cols: u16 = 80;
        let mut data = Vec::with_capacity(4);
        data.extend_from_slice(&rows.to_be_bytes());
        data.extend_from_slice(&cols.to_be_bytes());

        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_RESIZE, &data).unwrap();

        let mut cursor = Cursor::new(buf);
        let (frame_type, received) = recv_frame(&mut cursor).unwrap();
        assert_eq!(frame_type, FRAME_RESIZE);
        assert_eq!(received.len(), 4);
        let r = u16::from_be_bytes([received[0], received[1]]);
        let c = u16::from_be_bytes([received[2], received[3]]);
        assert_eq!(r, 24);
        assert_eq!(c, 80);
    }

    #[test]
    fn frame_stdin_roundtrip() {
        let payload = b"\x03\x04"; // Ctrl-C, Ctrl-D
        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_STDIN, payload).unwrap();

        let mut cursor = Cursor::new(buf);
        let (frame_type, data) = recv_frame(&mut cursor).unwrap();
        assert_eq!(frame_type, FRAME_STDIN);
        assert_eq!(data, payload);
    }

    #[test]
    fn frame_empty_payload() {
        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_STDOUT, b"").unwrap();

        let mut cursor = Cursor::new(buf);
        let (frame_type, data) = recv_frame(&mut cursor).unwrap();
        assert_eq!(frame_type, FRAME_STDOUT);
        assert!(data.is_empty());
    }
}
