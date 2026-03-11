//! pelagos — macOS CLI for the pelagos container runtime.
//!
//! Boots a Linux VM via Apple Virtualization Framework (pelagos-vz), then
//! forwards subcommands to the pelagos-guest daemon inside the VM over vsock.
//! The VM is kept alive between invocations via a background daemon process
//! that owns the VZVirtualMachine and proxies vsock connections over a Unix socket.

use std::io::{BufRead, BufReader, Write};
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
    Ping,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum GuestResponse {
    Stream { stream: String, data: String },
    Exit { exit: i32 },
    Pong { pong: bool },
    Error { error: String },
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

    let cmd = GuestCommand::Run { image, args, mounts };
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{GuestCommand, GuestMount, GuestResponse};

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
}
