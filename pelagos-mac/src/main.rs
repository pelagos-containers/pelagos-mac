//! pelagos — macOS CLI for the pelagos container runtime.
//!
//! Boots a Linux VM via Apple Virtualization Framework (pelagos-vz), then
//! forwards subcommands to the pelagos-guest daemon inside the VM over vsock.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::io::IntoRawFd;
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use pelagos_vz::vm::{Vm, VmConfig};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "pelagos", about = "pelagos container runtime for macOS")]
struct Cli {
    /// Path to the VM kernel image
    #[arg(long, env = "PELAGOS_KERNEL")]
    kernel: PathBuf,

    /// Path to the initrd image
    #[arg(long, env = "PELAGOS_INITRD")]
    initrd: Option<PathBuf>,

    /// Path to the root disk image
    #[arg(long, env = "PELAGOS_DISK")]
    disk: PathBuf,

    /// Memory in MiB (default 1024)
    #[arg(long, default_value = "1024")]
    memory: usize,

    /// Number of vCPUs (default 2)
    #[arg(long, default_value = "2")]
    cpus: usize,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a container image inside the VM
    Run {
        /// Container image name (e.g. alpine)
        image: String,
        /// Arguments to pass to the container
        args: Vec<String>,
    },
    /// Ping the guest daemon (readiness check)
    Ping,
}

// ---------------------------------------------------------------------------
// Guest protocol types (mirrors pelagos-guest)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum GuestCommand {
    Run { image: String, args: Vec<String> },
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

    let mut builder = VmConfig::builder()
        .kernel(&cli.kernel)
        .disk(&cli.disk)
        .memory_mib(cli.memory)
        .cpus(cli.cpus);
    if let Some(ref initrd) = cli.initrd {
        builder = builder.initrd(initrd);
    }
    let config = builder.build().unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        process::exit(1);
    });

    eprintln!("Booting VM...");
    let vm = Vm::start(config).unwrap_or_else(|e| {
        eprintln!("VM start failed: {}", e);
        process::exit(1);
    });
    eprintln!("VM running.");

    let exit_code = match cli.command {
        Commands::Run { image, args } => run_command(&vm, image, args),
        Commands::Ping => ping_command(&vm),
    };

    process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn run_command(vm: &Vm, image: String, args: Vec<String>) -> i32 {
    let fd = vm.connect_vsock().unwrap_or_else(|e| {
        eprintln!("vsock connect failed: {}", e);
        process::exit(1);
    });

    let raw = fd.into_raw_fd();
    let write_fd = unsafe { libc::dup(raw) };
    let mut reader = BufReader::new(unsafe { std::fs::File::from_raw_fd(raw) });
    let mut writer = unsafe { std::fs::File::from_raw_fd(write_fd) };

    let cmd = GuestCommand::Run { image, args };
    let mut msg = serde_json::to_string(&cmd).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        eprintln!("write error: {}", e);
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
                eprintln!("guest error: {}", error);
                break;
            }
            Ok(resp) => {
                eprintln!("unexpected response: {:?}", resp);
            }
            Err(e) => {
                eprintln!("parse error: {} (line: {:?})", e, trimmed);
                break;
            }
        }
    }
    exit_code
}

fn ping_command(vm: &Vm) -> i32 {
    let fd = vm.connect_vsock().unwrap_or_else(|e| {
        eprintln!("vsock connect failed: {}", e);
        process::exit(1);
    });

    let raw = fd.into_raw_fd();
    let write_fd = unsafe { libc::dup(raw) };
    let mut reader = BufReader::new(unsafe { std::fs::File::from_raw_fd(raw) });
    let mut writer = unsafe { std::fs::File::from_raw_fd(write_fd) };

    let mut msg = serde_json::to_string(&GuestCommand::Ping).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        eprintln!("write error: {}", e);
        return 1;
    }

    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            eprintln!("no response from guest");
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
            eprintln!("unexpected ping response: {:?}", other);
            1
        }
    }
}
