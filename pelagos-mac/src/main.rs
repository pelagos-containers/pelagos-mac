//! pelagos — macOS CLI for the pelagos container runtime.
//!
//! Boots a Linux VM via Apple Virtualization Framework (pelagos-vz), then
//! forwards subcommands to the pelagos-guest daemon inside the VM over vsock.
//! The VM is kept alive between invocations via a background daemon process
//! that owns the VZVirtualMachine and proxies vsock connections over a Unix socket.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
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

    /// Forward a host port to a container port: host_port:container_port
    /// May be specified multiple times.
    #[arg(short = 'p', long = "port", global = true)]
    ports: Vec<String>,

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
        /// Assign a name to the container
        #[arg(long)]
        name: Option<String>,
        /// Run in background; print container name and exit
        #[arg(short = 'd', long)]
        detach: bool,
        /// Set an environment variable KEY=VALUE inside the container (repeatable)
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
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
    /// List containers (running by default; use -a for all)
    Ps {
        /// Show all containers, including exited
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Print container logs
    Logs {
        /// Container name
        name: String,
        /// Follow log output
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Stop a running container
    Stop {
        /// Container name
        name: String,
    },
    /// Remove a container
    Rm {
        /// Container name
        name: String,
        /// Force remove even if running
        #[arg(short = 'f', long)]
        force: bool,
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
    /// Open an interactive shell directly in the VM (not in a container)
    Shell,
    /// Attach to the VM's hvc0 serial console (Ctrl-] to detach)
    Console,
    /// Open an SSH session to the VM (key-based, no password)
    Ssh {
        /// Extra arguments forwarded to ssh (e.g. -- uname -s  or  -- -L 8080:localhost:8080)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
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

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum GuestCommand {
    Run {
        image: String,
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<GuestMount>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "is_false")]
        detach: bool,
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        env: std::collections::HashMap<String, String>,
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        tty: bool,
    },
    /// Open a shell directly in the VM (no container, no namespaces).
    Shell {
        #[serde(skip_serializing_if = "is_false")]
        tty: bool,
    },
    Ps {
        #[serde(skip_serializing_if = "is_false")]
        all: bool,
    },
    Logs {
        name: String,
        #[serde(skip_serializing_if = "is_false")]
        follow: bool,
    },
    Stop {
        name: String,
    },
    Rm {
        name: String,
        #[serde(skip_serializing_if = "is_false")]
        force: bool,
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
        Commands::Vm {
            sub: VmCommands::Shell,
        } => {
            let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(exec_command(stream, GuestCommand::Shell { tty }, tty));
        }

        Commands::Vm {
            sub: VmCommands::Console,
        } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let state = match state::StateDir::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            };
            if !state.console_sock_file.exists() {
                eprintln!("error: console socket not found (daemon may still be starting)");
                process::exit(1);
            }
            let stream = match UnixStream::connect(&state.console_sock_file) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("console connect: {}", e);
                    process::exit(1);
                }
            };
            eprintln!("[pelagos] connected to VM console (hvc0). Press Ctrl-] to detach.");
            let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
            let saved = if is_tty { Some(enter_raw_mode()) } else { None };
            let exit_code = console_proxy(stream);
            if let Some(t) = saved {
                restore_terminal(t);
            }
            process::exit(exit_code);
        }

        Commands::Vm {
            sub: VmCommands::Ssh { ref extra },
        } => {
            let extra = extra.clone();
            let state = match state::StateDir::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            };
            if !state.is_daemon_alive() {
                let daemon_args = daemon_args_from_cli(&cli);
                if let Err(e) = daemon::ensure_running(&daemon_args) {
                    log::error!("failed to start VM daemon: {}", e);
                    process::exit(1);
                }
            }
            if !state.ssh_key_file.exists() {
                eprintln!(
                    "error: SSH key not found at {}. Rebuild the VM image with 'make image'.",
                    state.ssh_key_file.display()
                );
                process::exit(1);
            }
            let mut cmd = std::process::Command::new("ssh");
            cmd.arg("-i")
                .arg(&state.ssh_key_file)
                .arg("-o")
                .arg("StrictHostKeyChecking=no")
                .arg("-o")
                .arg("UserKnownHostsFile=/dev/null")
                .arg("-o")
                .arg("LogLevel=ERROR")
                .arg("root@192.168.105.2");
            for arg in &extra {
                cmd.arg(arg);
            }
            let status = cmd.status().unwrap_or_else(|e| {
                eprintln!("ssh: {}", e);
                process::exit(1);
            });
            process::exit(status.code().unwrap_or(1));
        }

        Commands::Run {
            ref image,
            ref args,
            ref name,
            detach,
            ref env,
        } => {
            let image = image.clone();
            let args = args.clone();
            let name = name.clone();
            let env_map: std::collections::HashMap<String, String> = env
                .iter()
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect();
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
            process::exit(passthrough_command(
                stream,
                GuestCommand::Run {
                    image,
                    args,
                    mounts,
                    name,
                    detach,
                    env: env_map,
                },
            ));
        }

        Commands::Exec {
            ref image,
            ref args,
            tty,
        } => {
            let image = image.clone();
            let args = args.clone();
            // Auto-detect: enable TTY only when stdout is a real terminal.
            // Checking STDOUT (not STDIN) correctly handles `OUT=$(pelagos exec ...)`:
            // stdout is a pipe, so TTY is skipped even when stdin is a terminal.
            let tty = tty || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(exec_command(
                stream,
                GuestCommand::Exec {
                    image,
                    args,
                    env: std::collections::HashMap::new(),
                    tty,
                },
                tty,
            ));
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

        Commands::Ps { all } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(stream, GuestCommand::Ps { all }));
        }

        Commands::Logs { ref name, follow } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(
                stream,
                GuestCommand::Logs { name, follow },
            ));
        }

        Commands::Stop { ref name } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(stream, GuestCommand::Stop { name }));
        }

        Commands::Rm { ref name, force } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(
                stream,
                GuestCommand::Rm { name, force },
            ));
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
    let port_forwards = parse_ports(&cli.ports);

    daemon::DaemonArgs {
        kernel,
        initrd: cli.initrd.clone(),
        disk,
        cmdline: cli.cmdline.clone(),
        memory_mib: cli.memory,
        cpus: cli.cpus,
        virtiofs_shares,
        port_forwards,
    }
}

/// Parse `-p host_port:container_port` strings into `PortForward`s.
fn parse_ports(ports: &[String]) -> Vec<daemon::PortForward> {
    ports
        .iter()
        .map(|spec| {
            daemon::parse_port_spec(spec).unwrap_or_else(|| {
                log::error!(
                    "invalid port spec {:?}: expected host_port:container_port",
                    spec
                );
                process::exit(1);
            })
        })
        .collect()
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

/// Send `cmd` to the guest and relay streaming output to stdout/stderr.
/// Returns the container exit code (or 1 on protocol error).
fn passthrough_command(stream: UnixStream, cmd: GuestCommand) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

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
/// Send an exec-style command (Exec or Shell) and handle the binary frame protocol.
/// `tty` controls whether the host terminal is put into raw mode.
fn exec_command(stream: UnixStream, cmd: GuestCommand, tty: bool) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream.try_clone().expect("clone stream");

    // Send handshake.
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

/// Proxy stdin/stdout ↔ a Unix socket connected to the VM serial console.
///
/// - In TTY mode: Ctrl-] (0x1D) detaches cleanly; stdin EOF also exits.
/// - In piped mode: after stdin EOF, continues draining console output for up
///   to 2 seconds so that command output arrives before the process exits.
///   This makes `printf 'cmd\n' | pelagos vm console` work correctly.
///
/// Returns exit code (always 0).
fn console_proxy(stream: UnixStream) -> i32 {
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
    let stream_fd = stream.as_raw_fd();
    let mut buf = vec![0u8; 4096];
    let mut stdin_done = false;
    let mut drain_until: Option<std::time::Instant> = None;

    loop {
        // In piped mode, after stdin is exhausted, drain console output for
        // up to 2 seconds, then exit.  In TTY mode, exit immediately on EOF.
        let timeout_ms: i32 = if stdin_done {
            if is_tty {
                break;
            }
            let deadline = drain_until.get_or_insert_with(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(2)
            });
            let rem = deadline.saturating_duration_since(std::time::Instant::now());
            if rem.is_zero() {
                break;
            }
            rem.as_millis() as i32
        } else {
            -1
        };

        let mut pfds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: if stdin_done { 0 } else { libc::POLLIN },
                revents: 0,
            },
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout_ms) };
        if n < 0 {
            break;
        }
        if n == 0 {
            break; // drain timeout expired
        }

        // stdin → console
        if !stdin_done && pfds[0].revents & libc::POLLIN != 0 {
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                stdin_done = true;
                continue; // enter drain mode; don't break yet
            }
            let chunk = &buf[..n as usize];
            // Ctrl-] (ASCII 0x1D) detaches.
            if chunk.contains(&0x1D) {
                break;
            }
            unsafe {
                libc::write(
                    stream_fd,
                    chunk.as_ptr() as *const libc::c_void,
                    chunk.len(),
                )
            };
        }
        if pfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            stdin_done = true;
        }

        // console → stdout
        if pfds[1].revents & libc::POLLIN != 0 {
            let n =
                unsafe { libc::read(stream_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            let w = unsafe {
                libc::write(
                    libc::STDOUT_FILENO,
                    buf.as_ptr() as *const libc::c_void,
                    n as usize,
                )
            };
            if w < 0 {
                break; // stdout closed (e.g. head -c N reached limit)
            }
        }
        if pfds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            break;
        }
    }
    0
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
            name: None,
            detach: false,
            env: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["image"], "alpine");
        assert_eq!(v["args"][0], "/bin/echo");
        // name and detach omitted when None/false
        assert!(v.get("name").is_none() || v["name"].is_null());
        assert!(v.get("detach").is_none() || v["detach"] == false);
    }

    #[test]
    fn run_command_with_name_detach_serializes() {
        let cmd = GuestCommand::Run {
            image: "alpine".into(),
            args: vec!["sleep".into(), "30".into()],
            mounts: vec![],
            name: Some("mybox".into()),
            detach: true,
            env: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["name"], "mybox");
        assert_eq!(v["detach"], true);
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
            name: None,
            detach: false,
            env: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["mounts"][0]["tag"], "share0");
        assert_eq!(v["mounts"][0]["container_path"], "/data");
    }

    #[test]
    fn ps_command_serializes() {
        let cmd = GuestCommand::Ps { all: true };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "ps");
        assert_eq!(v["all"], true);
    }

    #[test]
    fn logs_command_serializes() {
        let cmd = GuestCommand::Logs {
            name: "mybox".into(),
            follow: false,
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "logs");
        assert_eq!(v["name"], "mybox");
        // follow omitted when false
        assert!(v.get("follow").is_none() || v["follow"] == false);
    }

    #[test]
    fn stop_command_serializes() {
        let cmd = GuestCommand::Stop {
            name: "mybox".into(),
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "stop");
        assert_eq!(v["name"], "mybox");
    }

    #[test]
    fn rm_command_serializes() {
        let cmd = GuestCommand::Rm {
            name: "mybox".into(),
            force: true,
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "rm");
        assert_eq!(v["name"], "mybox");
        assert_eq!(v["force"], true);
    }

    #[test]
    fn shell_command_serializes() {
        let cmd = GuestCommand::Shell { tty: true };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "shell");
        assert_eq!(v["tty"], true);
    }

    #[test]
    fn shell_command_omits_tty_when_false() {
        let cmd = GuestCommand::Shell { tty: false };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "shell");
        assert!(v["tty"].is_null(), "tty should be omitted when false");
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
