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

    /// Memory in MiB (default 2048)
    #[arg(long, default_value = "2048")]
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
        /// Label KEY=VALUE (repeatable; forwarded to pelagos run --label)
        #[arg(short = 'l', long = "label")]
        labels: Vec<String>,
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
    /// Exec a command inside an already-running container (enters its namespaces).
    ExecInto {
        /// Running container name
        container: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Allocate a pseudo-TTY (default: auto-detect from stdout)
        #[arg(short = 't', long)]
        tty: bool,
        /// User to run command as (passed to guest, runs as this user inside container).
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Working directory inside the container.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
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
    /// Print low-level JSON information about a container (delegates to `pelagos container inspect`)
    Inspect {
        /// Container name
        name: String,
    },
    /// Restart a stopped container with its original parameters
    Start {
        /// Container name
        name: String,
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
    /// Build an OCI image from a Dockerfile (Remfile) inside the VM
    Build {
        /// Image tag (e.g. myapp:latest)
        #[arg(short = 't', long)]
        tag: String,
        /// Path to the Dockerfile/Remfile inside the build context (default: Dockerfile)
        #[arg(short = 'f', long, default_value = "Dockerfile")]
        file: String,
        /// Build argument (KEY=VALUE); may be repeated
        #[arg(long = "build-arg")]
        build_args: Vec<String>,
        /// Do not use the cache
        #[arg(long)]
        no_cache: bool,
        /// Target build stage (accepted for compatibility; pelagos builds the final stage)
        #[arg(long)]
        target: Option<String>,
        /// Build context path (default: .)
        #[arg(default_value = ".")]
        context: String,
    },
    /// Manage named volumes inside the VM
    Volume {
        /// Subcommand: create, ls, rm
        sub: String,
        /// Volume name (for create/rm)
        name: Option<String>,
    },
    /// Manage named networks inside the VM
    Network {
        /// Subcommand: create, ls, rm, inspect
        sub: String,
        /// Remaining args forwarded verbatim (name, flags like --subnet, etc.)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Copy files between the host and a running container.
    /// Use `container:path` syntax to denote a path inside a container.
    /// Examples:
    ///   pelagos cp mycontainer:/etc/os-release /tmp/os-release
    ///   pelagos cp /tmp/myfile mycontainer:/tmp/
    Cp {
        /// Source: either `container:path` or a local path
        src: String,
        /// Destination: either `container:path` or a local path
        dst: String,
    },
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
    /// Relative path within the virtiofs mount to bind (empty = root of the share).
    /// Used when a single broad share (e.g. $HOME as share0) covers multiple bind mounts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subpath: String,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        labels: Vec<String>,
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        tty: bool,
    },
    ExecInto {
        container: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        tty: bool,
        /// Working directory inside the container (Docker exec -w).
        #[serde(default)]
        workdir: Option<String>,
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
    ContainerInspect {
        name: String,
    },
    Start {
        name: String,
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
    Build {
        tag: String,
        dockerfile: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        build_args: Vec<String>,
        #[serde(skip_serializing_if = "is_false")]
        no_cache: bool,
        context_size: u64,
    },
    Volume {
        sub: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Network {
        sub: String,
        /// All remaining args forwarded verbatim (name, --subnet, etc.).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Copy a path out of a running container.
    CpFrom {
        container: String,
        src: String,
    },
    /// Copy a tar payload into a running container.
    /// `data_size` raw tar bytes follow immediately after the JSON command line.
    CpTo {
        container: String,
        dst: String,
        data_size: u64,
    },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum GuestResponse {
    Stream {
        stream: String,
        data: String,
    },
    Exit {
        exit: i32,
    },
    Pong {
        pong: bool,
    },
    Error {
        error: String,
    },
    Ready {
        ready: bool,
    },
    /// Precedes `size` raw bytes written directly to the socket.
    RawBytes {
        size: u64,
    },
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
            ref labels,
        } => {
            let image = image.clone();
            let args = args.clone();
            let name = name.clone();
            let labels = labels.clone();
            let env_map: std::collections::HashMap<String, String> = env
                .iter()
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect();
            let daemon_args = daemon_args_from_cli(&cli);
            // Build the guest-side mount list from the user's -v flags.
            // share0 is always $HOME; paths under $HOME use it via a subpath.
            // Paths outside $HOME have their own shareN entry.
            let home = std::env::var("HOME").unwrap_or_default();
            let mounts: Vec<GuestMount> = cli
                .volumes
                .iter()
                .filter_map(|spec| {
                    let parts: Vec<&str> = spec.splitn(3, ':').collect();
                    if parts.len() < 2 {
                        return None;
                    }
                    let host_path = parts[0];
                    let container_path = parts[1].to_string();
                    if !home.is_empty()
                        && (host_path == home || host_path.starts_with(&format!("{}/", home)))
                    {
                        // Path is under $HOME — use share0 with a subpath.
                        let subpath = host_path
                            .strip_prefix(&format!("{}/", home))
                            .unwrap_or("")
                            .to_string();
                        Some(GuestMount {
                            tag: "share0".to_string(),
                            subpath,
                            container_path,
                        })
                    } else {
                        // Path outside $HOME — find its own share by host path.
                        let share = daemon_args
                            .virtiofs_shares
                            .iter()
                            .find(|s| s.host_path == std::path::Path::new(host_path))?;
                        Some(GuestMount {
                            tag: share.tag.clone(),
                            subpath: String::new(),
                            container_path,
                        })
                    }
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
                    labels,
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

        Commands::ExecInto {
            ref container,
            ref args,
            tty,
            user: _,
            ref workdir,
        } => {
            let container = container.clone();
            let args = args.clone();
            let workdir = workdir.clone();
            let tty = tty || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
            // Fix A ensures all commands start the daemon with the same virtiofs
            // shares ($HOME as share0), so ensure_running is safe here — no
            // mount-mismatch risk. Auto-start the VM if it has shut down since
            // the last container run (e.g. between probe exit and docker exec).
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(exec_command(
                stream,
                GuestCommand::ExecInto {
                    container,
                    args,
                    env: std::collections::HashMap::new(),
                    tty,
                    workdir,
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
            // `ps` must not start the daemon: if no daemon is running, there are
            // no containers.  If the daemon is alive (possibly with different
            // mounts), just connect and ask.  This allows `docker ps` (called by
            // the devcontainer CLI) to return empty before the container is started
            // without triggering a "different mount configuration" error.
            let state = match state::StateDir::open() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("failed to open state dir: {}", e);
                    process::exit(1);
                }
            };
            if !state.is_daemon_alive() {
                // No daemon = no containers.
                process::exit(0);
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

        Commands::Inspect { ref name } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(
                stream,
                GuestCommand::ContainerInspect { name },
            ));
        }

        Commands::Start { ref name } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(stream, GuestCommand::Start { name }));
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

        Commands::Build {
            ref tag,
            ref file,
            ref build_args,
            no_cache,
            target: _,
            ref context,
        } => {
            let tag = tag.clone();
            let file = file.clone();
            let build_args = build_args.clone();
            let context = std::path::PathBuf::from(context);
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(build_command(
                stream,
                &tag,
                &file,
                &build_args,
                no_cache,
                &context,
            ));
        }

        Commands::Volume { ref sub, ref name } => {
            let sub = sub.clone();
            let name = name.clone();
            // If no daemon is running there are no volumes.  Return immediately
            // so devcontainer pre-flight checks don't trigger a full VM boot.
            let state = match state::StateDir::open() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("failed to open state dir: {}", e);
                    process::exit(1);
                }
            };
            if !state.is_daemon_alive() {
                match sub.as_str() {
                    "ls" => process::exit(0),
                    "create" => {
                        if let Some(n) = &name {
                            println!("{}", n);
                        }
                        process::exit(0);
                    }
                    "rm" => process::exit(0),
                    _ => process::exit(1), // inspect etc. require a running daemon
                }
            }
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(
                stream,
                GuestCommand::Volume { sub, name },
            ));
        }

        Commands::Network { ref sub, ref args } => {
            let sub = sub.clone();
            let args = args.clone();
            // Same early-return pattern as Volume: pre-flight network checks
            // should not boot the VM when no daemon is running.
            let state = match state::StateDir::open() {
                Ok(s) => s,
                Err(e) => {
                    log::error!("failed to open state dir: {}", e);
                    process::exit(1);
                }
            };
            if !state.is_daemon_alive() {
                match sub.as_str() {
                    "ls" => process::exit(0),
                    "create" => {
                        if let Some(n) = args.last() {
                            println!("{}", n);
                        }
                        process::exit(0);
                    }
                    "rm" => process::exit(0),
                    _ => process::exit(1),
                }
            }
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            process::exit(passthrough_command(
                stream,
                GuestCommand::Network { sub, args },
            ));
        }

        Commands::Cp { ref src, ref dst } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit();
            // One of src/dst must be `container:path`; the other is a local path.
            if let Some((container, src_path)) = parse_container_path(src) {
                let local_dst = dst.as_str();
                process::exit(cp_from_command(stream, &container, src_path, local_dst));
            } else if let Some((container, dst_path)) = parse_container_path(dst) {
                let local_src = src.as_str();
                process::exit(cp_to_command(stream, local_src, &container, dst_path));
            } else {
                log::error!("cp: one of src or dst must be container:path");
                process::exit(1);
            }
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

    // Always-on volumes share: ~/.local/share/pelagos/volumes → /var/lib/pelagos/volumes in VM.
    // This makes named pelagos volumes persistent across VM restarts (virtiofs-backed on host).
    let volumes_host = pelagos_volumes_host_path();
    if let Err(e) = std::fs::create_dir_all(&volumes_host) {
        log::warn!(
            "could not create volumes directory {}: {}",
            volumes_host.display(),
            e
        );
    }
    // pelagos-volumes is share0 (fixed tag, handled specially by the init script).
    // build_virtiofs_shares then injects $HOME as the next share, ensuring every
    // invocation (run, ps, exec-into, volume ls, etc.) starts the daemon with the
    // same mount configuration — preventing mount-mismatch errors on daemon reuse.
    let mut virtiofs_shares = vec![daemon::VirtiofsShare {
        host_path: volumes_host,
        tag: "pelagos-volumes".to_string(),
        read_only: false,
        container_path: "/var/lib/pelagos/volumes".to_string(),
    }];
    virtiofs_shares.extend(build_virtiofs_shares(&cli.volumes));

    let port_forwards = parse_ports(&cli.ports);

    // Embed the current host UTC time so the guest init can set the system clock
    // instantly without NTP (avoids TLS cert failures on first-boot).
    // Passed as clock.utc=YYYY-MM-DDTHH:MM:SS (ISO 8601, no spaces — cmdline safe).
    // init reads it and calls: busybox date -s "YYYY-MM-DD HH:MM:SS".
    // Skip injection if clock.utc is already present (e.g., inside vm-daemon-internal
    // subprocess which receives the cmdline forwarded from the parent process).
    let cmdline = if cli.cmdline.contains("clock.utc=") {
        cli.cmdline.clone()
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let secs = now % 60;
        let mins = (now / 60) % 60;
        let hours = (now / 3600) % 24;
        let days_since_epoch = now / 86400;
        // Compute year/month/day from days_since_epoch using proleptic Gregorian calendar.
        let (year, month, day) = {
            let z = days_since_epoch as i64 + 719468;
            let era = if z >= 0 { z } else { z - 146096 } / 146097;
            let doe = z - era * 146097;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            (y as u64, m as u64, d as u64)
        };
        let clock_utc = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            year, month, day, hours, mins, secs
        );
        format!("{} clock.utc={}", cli.cmdline, clock_utc)
    };

    daemon::DaemonArgs {
        kernel,
        initrd: cli.initrd.clone(),
        disk,
        cmdline,
        memory_mib: cli.memory,
        cpus: cli.cpus,
        virtiofs_shares,
        port_forwards,
    }
}

/// Returns `~/.local/share/pelagos/volumes`, the host-side backing directory for
/// the always-on `pelagos-volumes` virtiofs share.
fn pelagos_volumes_host_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("pelagos")
        .join("volumes")
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

/// Build the virtiofs share list from the user's `-v` flags.
///
/// `$HOME` is always injected as the first share (`share0`).  Any user volume
/// whose host path falls under `$HOME` is covered by this share and does not
/// get its own entry; paths outside `$HOME` get `share1`, `share2`, etc.
///
/// This ensures that every invocation — `ps`, `volume ls`, `run -v ~/x:/y` —
/// produces the same virtiofs share set, eliminating the "different mount
/// configuration" error that would otherwise occur when pre-flight commands
/// start the daemon before `run` does.
fn build_virtiofs_shares(volumes: &[String]) -> Vec<daemon::VirtiofsShare> {
    let home = std::env::var("HOME").ok();
    let home_spec = home.as_deref().map(|h| format!("{}:", h));

    // Determine whether the home share is already the first entry in volumes.
    // This is true when we are re-invoked as vm-daemon-internal: the daemon
    // start code passes `--volume $HOME:` first, so we must not add it again.
    let home_already_present = home_spec
        .as_ref()
        .map(|hs| volumes.first().map(|v| v == hs).unwrap_or(false))
        .unwrap_or(false);

    let mut effective: Vec<String> = Vec::new();

    if home_already_present {
        // We are vm-daemon-internal: the home share is volumes[0] already.
        // Keep it verbatim so parse_volumes assigns it share0.
        effective.push(volumes[0].clone());
        // Only add paths outside $HOME from the remaining volumes.
        for v in &volumes[1..] {
            let host = v.split(':').next().unwrap_or("");
            let under_home = home
                .as_deref()
                .map(|h| host == h || host.starts_with(&format!("{}/", h)))
                .unwrap_or(false);
            if !under_home {
                effective.push(v.clone());
            }
        }
    } else {
        // Normal invocation: inject the home share as share0 first.
        if let Some(ref hs) = home_spec {
            effective.push(hs.clone());
        }
        // Add user volumes that are outside $HOME as per-path shares.
        for v in volumes {
            let host = v.split(':').next().unwrap_or("");
            let under_home = home
                .as_deref()
                .map(|h| host == h || host.starts_with(&format!("{}/", h)))
                .unwrap_or(false);
            if !under_home {
                effective.push(v.clone());
            }
        }
    }

    parse_volumes(&effective)
}

/// Parse a slice of `/host:/container[:ro]` strings into `VirtiofsShare`s.
/// Tags are assigned as `share0`, `share1`, etc. based on index.
/// For home-aware share building, call `build_virtiofs_shares` instead.
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

/// Tar up the build context, send it to the guest, and relay build output.
fn build_command(
    stream: UnixStream,
    tag: &str,
    dockerfile: &str,
    build_args: &[String],
    no_cache: bool,
    context: &std::path::Path,
) -> i32 {
    // Determine the Dockerfile path relative to the context.
    // If -f is an absolute path outside the context dir (e.g. a temp file generated
    // by the devcontainer CLI), copy it into a scratch directory alongside the context
    // contents so the guest can find it by name.
    let dockerfile_path = std::path::Path::new(dockerfile);
    let (effective_context, dockerfile_name) =
        if dockerfile_path.is_absolute() && !dockerfile_path.starts_with(context) {
            // Build a temp dir with the context contents + the external Dockerfile.
            let ts_prep = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros();
            let scratch = std::env::temp_dir().join(format!("pelagos-ctx-prep-{}", ts_prep));
            if let Err(e) = std::fs::create_dir_all(&scratch) {
                log::error!("build: create scratch dir: {}", e);
                return 1;
            }
            // Copy context into scratch.
            let cp_status = std::process::Command::new("cp")
                .arg("-a")
                .arg(format!("{}/.", context.display()))
                .arg(&scratch)
                .status();
            match cp_status {
                Err(e) => {
                    log::error!("build: cp context: {}", e);
                    let _ = std::fs::remove_dir_all(&scratch);
                    return 1;
                }
                Ok(s) if !s.success() => {
                    log::error!("build: cp context failed");
                    let _ = std::fs::remove_dir_all(&scratch);
                    return 1;
                }
                Ok(_) => {}
            }
            // Copy the external Dockerfile into scratch using its basename.
            let name = dockerfile_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Dockerfile");
            if let Err(e) = std::fs::copy(dockerfile_path, scratch.join(name)) {
                log::error!("build: copy external Dockerfile: {}", e);
                let _ = std::fs::remove_dir_all(&scratch);
                return 1;
            }
            (scratch, name.to_string())
        } else {
            // Dockerfile is inside (or relative to) the context; just use its name/rel path.
            let name = dockerfile_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(dockerfile);
            (context.to_path_buf(), name.to_string())
        };

    // Write a gzipped tar of the (effective) context to a temp file so we know its size.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let tar_path = std::env::temp_dir().join(format!("pelagos-ctx-{}.tar.gz", ts));

    let tar_status = std::process::Command::new("tar")
        .arg("czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&effective_context)
        .arg(".")
        .status();
    // Clean up scratch dir if we created one.
    if effective_context != context {
        let _ = std::fs::remove_dir_all(&effective_context);
    }
    match tar_status {
        Err(e) => {
            log::error!("tar: {}", e);
            return 1;
        }
        Ok(s) if !s.success() => {
            log::error!("tar failed (exit {})", s.code().unwrap_or(-1));
            return 1;
        }
        Ok(_) => {}
    }

    let context_size = match std::fs::metadata(&tar_path) {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("tar metadata: {}", e);
            let _ = std::fs::remove_file(&tar_path);
            return 1;
        }
    };

    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    // Send JSON header with context_size.
    let cmd = GuestCommand::Build {
        tag: tag.to_string(),
        dockerfile: dockerfile_name,
        build_args: build_args.to_vec(),
        no_cache,
        context_size,
    };
    let mut msg = serde_json::to_string(&cmd).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("build: write header: {}", e);
        let _ = std::fs::remove_file(&tar_path);
        return 1;
    }

    // Stream the tar bytes immediately after the header.
    let result =
        std::fs::File::open(&tar_path).and_then(|mut f| std::io::copy(&mut f, &mut writer));
    let _ = std::fs::remove_file(&tar_path);
    if let Err(e) = result {
        log::error!("build: send context: {}", e);
        return 1;
    }

    // Read streaming build output.
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

// ---------------------------------------------------------------------------
// docker cp helpers
// ---------------------------------------------------------------------------

/// Parse `container:path` notation. Returns `(container, path)` if the input
/// matches, otherwise `None`.
fn parse_container_path(s: &str) -> Option<(String, &str)> {
    // A bare path starts with `/`, `.`, or is just `-` (stdin/stdout).
    // Anything with a `:` that doesn't start with those is container:path.
    if s.starts_with('/') || s.starts_with('.') || s == "-" {
        return None;
    }
    let colon = s.find(':')?;
    let container = s[..colon].to_string();
    let path = &s[colon + 1..];
    if container.is_empty() || path.is_empty() {
        return None;
    }
    Some((container, path))
}

/// Copy a path out of a container to a local destination.
/// Receives `GuestResponse::RawBytes { size }` then raw tar bytes from the guest.
fn cp_from_command(stream: UnixStream, container: &str, src: &str, local_dst: &str) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    let mut msg = serde_json::to_string(&GuestCommand::CpFrom {
        container: container.to_string(),
        src: src.to_string(),
    })
    .unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("cp: write error: {}", e);
        return 1;
    }

    // First response must be RawBytes with the tar size.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                log::error!("cp: connection closed before response");
                return 1;
            }
            Ok(_) => {}
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<GuestResponse>(trimmed) {
            Ok(GuestResponse::RawBytes { size }) => {
                // Read exactly `size` raw bytes and pipe through `tar xf -`.
                let dst_path = std::path::Path::new(local_dst);
                let dst_dir = if dst_path.is_dir() {
                    local_dst.to_string()
                } else {
                    dst_path
                        .parent()
                        .map(|p| p.to_str().unwrap_or("."))
                        .unwrap_or(".")
                        .to_string()
                };

                let mut tar_proc = match std::process::Command::new("tar")
                    .arg("xf")
                    .arg("-")
                    .arg("-C")
                    .arg(&dst_dir)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(p) => p,
                    Err(e) => {
                        log::error!("cp: tar spawn: {}", e);
                        return 1;
                    }
                };

                let copy_result = {
                    let mut sink = tar_proc.stdin.take().unwrap();
                    let mut limited = (&mut reader).take(size);
                    std::io::copy(&mut limited, &mut sink)
                };
                let tar_status = tar_proc.wait();
                if copy_result.is_err() || tar_status.map(|s| !s.success()).unwrap_or(true) {
                    log::error!("cp: tar extract failed");
                    return 1;
                }
            }
            Ok(GuestResponse::Error { error }) => {
                log::error!("cp: {}", error);
                return 1;
            }
            Ok(GuestResponse::Exit { exit }) => return exit,
            Ok(_) => continue,
            Err(e) => {
                log::error!("cp: parse error: {}", e);
                return 1;
            }
        }
    }
}

/// Copy a local path into a container at `dst`.
/// Tars the local source and streams it via `GuestCommand::CpTo`.
fn cp_to_command(stream: UnixStream, local_src: &str, container: &str, dst: &str) -> i32 {
    // Tar the local source into a temp file.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let tar_path = std::env::temp_dir().join(format!("pelagos-cp-{}.tar", ts));

    let src_path = std::path::Path::new(local_src);
    let (tar_dir, tar_name) = if src_path.is_dir() {
        (local_src, ".".to_string())
    } else {
        let parent = src_path
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        let name = src_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(local_src);
        (parent, name.to_string())
    };

    let tar_status = std::process::Command::new("tar")
        .arg("cf")
        .arg(&tar_path)
        .arg("-C")
        .arg(tar_dir)
        .arg(&tar_name)
        .status();
    match tar_status {
        Err(e) => {
            log::error!("cp: tar: {}", e);
            return 1;
        }
        Ok(s) if !s.success() => {
            log::error!("cp: tar failed (exit {})", s.code().unwrap_or(-1));
            return 1;
        }
        Ok(_) => {}
    }

    let data_size = match std::fs::metadata(&tar_path) {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("cp: tar metadata: {}", e);
            let _ = std::fs::remove_file(&tar_path);
            return 1;
        }
    };

    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    // Send JSON header.
    let mut msg = serde_json::to_string(&GuestCommand::CpTo {
        container: container.to_string(),
        dst: dst.to_string(),
        data_size,
    })
    .unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("cp: write error: {}", e);
        let _ = std::fs::remove_file(&tar_path);
        return 1;
    }

    // Stream raw tar bytes.
    let mut tar_file = match std::fs::File::open(&tar_path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("cp: open tar: {}", e);
            let _ = std::fs::remove_file(&tar_path);
            return 1;
        }
    };
    if let Err(e) = std::io::copy(&mut tar_file, &mut writer) {
        log::error!("cp: stream tar: {}", e);
        let _ = std::fs::remove_file(&tar_path);
        return 1;
    }
    let _ = std::fs::remove_file(&tar_path);

    // Read streaming response from guest.
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
                log::error!("cp: {}", error);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                log::error!("cp: parse: {}", e);
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
            // Use libc::read directly — NOT io::stdin() which wraps a BufReader<StdinRaw>
            // with an 8192-byte internal buffer. When the caller buf is 4096 bytes,
            // BufReader pre-fills 8192 bytes from the kernel fd but only returns 4096,
            // leaving up to 4096 bytes stranded in its internal buffer. poll() then
            // sees no POLLIN (kernel fd is empty) and those bytes are never forwarded.
            // Direct libc::read reads exactly what poll() knows about — no over-read.
            // (Fixes pelagos#119 / pelagos-mac#103)
            if fds[0].revents & libc::POLLIN != 0 {
                let n = unsafe {
                    libc::read(
                        libc::STDIN_FILENO,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                match n {
                    -1 => {
                        if unsafe { *libc::__error() } == libc::EINTR {
                            continue; // interrupted by signal, retry
                        }
                        // Other error — send EOF frame and stop.
                        let mut w = writer_stdin.lock().unwrap();
                        let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                        break;
                    }
                    0 => {
                        // EOF — send a zero-length Stdin frame so the guest
                        // knows to close the child's stdin pipe.
                        let mut w = writer_stdin.lock().unwrap();
                        let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                        break;
                    }
                    n => {
                        let mut w = writer_stdin.lock().unwrap();
                        if send_frame(&mut *w, FRAME_STDIN, &buf[..n as usize]).is_err() {
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
        recv_frame, send_frame, Cli, Commands, GuestCommand, GuestMount, GuestResponse, FRAME_EXIT,
        FRAME_RESIZE, FRAME_STDIN, FRAME_STDOUT,
    };
    use clap::Parser as _;
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
            labels: vec![],
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
            labels: vec![],
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
                subpath: String::new(),
                container_path: "/data".into(),
            }],
            name: None,
            detach: false,
            env: std::collections::HashMap::new(),
            labels: vec![],
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

    #[test]
    fn build_command_serializes() {
        let cmd = GuestCommand::Build {
            tag: "myapp:latest".into(),
            dockerfile: "Dockerfile".into(),
            build_args: vec!["KEY=VAL".into()],
            no_cache: true,
            context_size: 4096,
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "build");
        assert_eq!(v["tag"], "myapp:latest");
        assert_eq!(v["dockerfile"], "Dockerfile");
        assert_eq!(v["build_args"][0], "KEY=VAL");
        assert_eq!(v["no_cache"], true);
        assert_eq!(v["context_size"], 4096);
    }

    #[test]
    fn build_command_omits_defaults() {
        let cmd = GuestCommand::Build {
            tag: "x".into(),
            dockerfile: "Dockerfile".into(),
            build_args: vec![],
            no_cache: false,
            context_size: 0,
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // build_args omitted when empty; no_cache omitted when false
        assert!(v["build_args"].is_null());
        assert!(v["no_cache"].is_null());
    }

    #[test]
    fn volume_command_serializes() {
        let cmd = GuestCommand::Volume {
            sub: "create".into(),
            name: Some("myvol".into()),
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "volume");
        assert_eq!(v["sub"], "create");
        assert_eq!(v["name"], "myvol");
    }

    #[test]
    fn network_command_serializes() {
        let cmd = GuestCommand::Network {
            sub: "ls".into(),
            args: vec![],
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "network");
        assert_eq!(v["sub"], "ls");
        // args omitted when empty
        assert!(v["args"].is_null());
    }

    #[test]
    fn network_clap_parses_subnet_flag() {
        // Verify that clap's trailing_var_arg actually captures --subnet into args
        let cli = Cli::try_parse_from([
            "pelagos",
            "--kernel",
            "/dev/null",
            "--initrd",
            "/dev/null",
            "--disk",
            "/dev/null",
            "--cmdline",
            "console=hvc0",
            "network",
            "create",
            "--subnet",
            "10.88.1.0/24",
            "testnet",
        ])
        .expect("parse failed");
        match cli.command {
            Commands::Network { sub, args } => {
                assert_eq!(sub, "create");
                assert_eq!(args, vec!["--subnet", "10.88.1.0/24", "testnet"]);
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn network_command_with_args_serializes() {
        let cmd = GuestCommand::Network {
            sub: "create".into(),
            args: vec!["--subnet".into(), "10.88.1.0/24".into(), "mynet".into()],
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "network");
        assert_eq!(v["sub"], "create");
        assert_eq!(v["args"][0], "--subnet");
        assert_eq!(v["args"][2], "mynet");
    }

    #[test]
    fn cp_from_serializes() {
        let cmd = GuestCommand::CpFrom {
            container: "mybox".into(),
            src: "/etc/os-release".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "cp_from");
        assert_eq!(v["container"], "mybox");
        assert_eq!(v["src"], "/etc/os-release");
    }

    #[test]
    fn cp_to_serializes() {
        let cmd = GuestCommand::CpTo {
            container: "mybox".into(),
            dst: "/tmp/".into(),
            data_size: 4096,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "cp_to");
        assert_eq!(v["container"], "mybox");
        assert_eq!(v["dst"], "/tmp/");
        assert_eq!(v["data_size"], 4096);
    }

    #[test]
    fn parse_container_path_detects_container() {
        let (c, p) = super::parse_container_path("mybox:/etc/os-release").unwrap();
        assert_eq!(c, "mybox");
        assert_eq!(p, "/etc/os-release");
    }

    #[test]
    fn parse_container_path_rejects_absolute() {
        assert!(super::parse_container_path("/tmp/foo").is_none());
    }

    #[test]
    fn parse_container_path_rejects_relative() {
        assert!(super::parse_container_path("./foo/bar").is_none());
    }

    #[test]
    fn parse_container_path_rejects_dash() {
        assert!(super::parse_container_path("-").is_none());
    }
}
