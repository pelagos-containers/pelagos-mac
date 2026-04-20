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

mod bless;
mod daemon;
mod state;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "pelagos",
    about = "pelagos container runtime for macOS",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH")),
)]
struct Cli {
    /// VM profile name (default: "default"). Named profiles use isolated state
    /// directories at ~/.local/share/pelagos/profiles/<name>/.
    #[arg(long, default_value = "default", global = true)]
    profile: String,

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
    #[arg(
        long,
        env = "PELAGOS_CMDLINE",
        default_value = "console=hvc0 random.trust_cpu=on"
    )]
    cmdline: String,

    /// Memory in MiB (default 4096; overridden by vm.conf memory= in profile)
    #[arg(long)]
    memory: Option<usize>,

    /// Number of vCPUs (default 2; overridden by vm.conf cpus= in profile)
    #[arg(long)]
    cpus: Option<usize>,

    /// Bind-mount a host directory into the container: /host/path:/container/path[:ro]
    /// May be specified multiple times.
    #[arg(short = 'v', long = "volume", global = true)]
    volumes: Vec<String>,

    /// Forward a host port to a container port: host_port:container_port
    /// May be specified multiple times.
    #[arg(short = 'p', long = "port", global = true)]
    ports: Vec<String>,

    /// Attach an extra disk image as an additional virtio-blk device.
    /// First extra disk → /dev/vdb, second → /dev/vdc, etc.
    /// Used by build-build-image.sh for provisioning without virtiofs I/O overhead.
    /// May be specified multiple times.
    #[arg(long = "extra-disk", global = true)]
    extra_disks: Vec<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a container image inside the VM.
    /// Add -it for an interactive shell: pelagos run -it alpine /bin/sh
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
        /// Keep stdin open (interactive mode)
        #[arg(short = 'i', long)]
        interactive: bool,
        /// Allocate a pseudo-TTY
        #[arg(short = 't', long)]
        tty: bool,
        /// Network mode: none, loopback, bridge, or pasta (repeatable; first is primary)
        #[arg(long = "network", short = 'n')]
        network: Vec<String>,
        /// DNS server inside the container (repeatable; requires bridge or pasta)
        #[arg(long)]
        dns: Vec<String>,
    },
    /// Run a command inside an already-running container (enters its namespaces).
    /// Equivalent to `docker exec`.
    Exec {
        /// Running container name
        #[arg(value_name = "CONTAINER")]
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
        /// Output as JSON array (one object per container)
        #[arg(long)]
        json: bool,
    },
    /// Print container logs
    Logs {
        /// Name of the container
        #[arg(value_name = "CONTAINER")]
        name: String,
        /// Follow log output
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Show low-level JSON details for a container
    Inspect {
        /// Name of the container to inspect
        #[arg(value_name = "CONTAINER")]
        name: String,
    },
    /// Manage containers (subcommands: prune)
    Container {
        #[command(subcommand)]
        sub: ContainerCommands,
    },
    /// Restart a stopped container with its original parameters
    Start {
        /// Name of the container to start
        #[arg(value_name = "CONTAINER")]
        name: String,
        /// Run interactively with a PTY (like `pelagos run -it`)
        #[arg(long, short = 'i')]
        interactive: bool,
        /// Override the command for this run only; pass after --
        /// Example: pelagos start -i myapp -- /bin/sh
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
    /// Stop a running container
    Stop {
        /// Name of the container to stop
        #[arg(value_name = "CONTAINER")]
        name: String,
    },
    /// Stop then start a container (running or exited)
    Restart {
        /// Name of the container
        #[arg(value_name = "CONTAINER")]
        name: String,
        /// Seconds to wait for clean exit before sending SIGKILL (default: 10)
        #[arg(long, short = 't', default_value = "10")]
        time: u64,
    },
    /// Remove a container
    Rm {
        /// Name of the container to remove
        #[arg(value_name = "CONTAINER")]
        name: String,
        /// Force remove even if running
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Ping the guest daemon (readiness check)
    Ping,
    /// Print version information for pelagos-mac and (if the VM is running) pelagos-guest
    Version,
    /// Subscribe to container lifecycle events (NDJSON stream on stdout).
    ///
    /// Sends an initial snapshot then streams ContainerStarted / ContainerExited
    /// events until disconnected.  Intended for the TUI and other tooling.
    Subscribe,
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
    /// Manage OCI images stored inside the VM
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
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
    /// Start, stop, and manage multi-container applications defined in a .reml compose file.
    ///
    /// The compose file must be under $HOME (virtiofs share0).  Relative paths
    /// inside the file are resolved against the file's parent directory in the VM.
    ///
    /// Use -p HOST:CONTAINER (global flag) to register macOS-side port listeners
    /// before the stack starts.
    ///
    /// Examples:
    ///   pelagos -p 9090:9090 -p 3000:3000 compose up -f ~/Projects/monitoring/compose.reml
    ///   pelagos compose ps -f ~/Projects/monitoring/compose.reml
    ///   pelagos compose logs -f ~/Projects/monitoring/compose.reml --follow grafana
    ///   pelagos compose down -f ~/Projects/monitoring/compose.reml
    Compose {
        #[command(subcommand)]
        sub: ComposeSubcmd,
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

#[derive(Subcommand, Debug)]
enum ComposeSubcmd {
    /// Start all services defined in a compose file (supervisor daemonises by default).
    Up {
        /// Path to the .reml compose file
        #[arg(short = 'f', long = "file", default_value = "compose.reml")]
        file: PathBuf,
        /// Override the project name (default: parent directory name)
        #[arg(long)]
        project: Option<String>,
        /// Run supervisor in foreground; stream output until the stack stops
        #[arg(long)]
        foreground: bool,
    },
    /// Stop and remove all containers in a compose stack.
    Down {
        /// Path to the .reml compose file
        #[arg(short = 'f', long = "file", default_value = "compose.reml")]
        file: PathBuf,
        /// Override the project name (default: parent directory name)
        #[arg(long)]
        project: Option<String>,
        /// Also remove named volumes declared in the compose file
        #[arg(long = "volumes")]
        remove_volumes: bool,
    },
    /// List services and their status in a compose stack.
    Ps {
        /// Path to the .reml compose file
        #[arg(short = 'f', long = "file", default_value = "compose.reml")]
        file: PathBuf,
        /// Override the project name (default: parent directory name)
        #[arg(long)]
        project: Option<String>,
    },
    /// View logs from services in a compose stack.
    Logs {
        /// Path to the .reml compose file
        #[arg(short = 'f', long = "file", default_value = "compose.reml")]
        file: PathBuf,
        /// Override the project name (default: parent directory name)
        #[arg(long)]
        project: Option<String>,
        /// Follow log output (stream continuously)
        #[arg(long)]
        follow: bool,
        /// Show logs for this service only
        service: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// List locally stored OCI images
    Ls {
        /// Output as JSON instead of a human-readable table
        #[arg(long)]
        json: bool,
    },
    /// Pull an OCI image from a registry
    Pull {
        /// Image reference (e.g. alpine:latest, registry.io/org/repo:tag)
        reference: String,
    },
    /// Remove a locally stored OCI image
    Rm {
        /// Image reference to remove
        reference: String,
    },
    /// Tag a local OCI image with a new reference
    Tag {
        /// Source image reference
        source: String,
        /// New tag to apply
        target: String,
    },
    /// Show details of a locally stored OCI image (JSON)
    Inspect {
        /// Image reference
        reference: String,
    },
    /// Authenticate with a registry and store credentials in the VM
    Login {
        /// Registry hostname (e.g. public.ecr.aws, ghcr.io)
        registry: String,
        /// Registry username
        #[arg(long, short = 'u')]
        username: Option<String>,
        /// Read password from stdin (recommended; avoids shell history)
        #[arg(long)]
        password_stdin: bool,
    },
    /// Remove stored credentials for a registry
    Logout {
        /// Registry hostname
        registry: String,
    },
}

#[derive(Subcommand)]
enum VmCommands {
    /// Start the persistent VM daemon (no-op if already running)
    Start,
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
    /// List all configured VM profiles and their running state
    Ls,
    /// Initialise the VM configuration from installed artifacts (run once after install)
    Init {
        /// Directory containing vmlinuz, initramfs.gz, and root.img.
        /// Defaults to the Homebrew share dir ($(brew --prefix)/share/pelagos-mac)
        /// then ~/.local/share/pelagos/.
        #[arg(long)]
        vm_data: Option<PathBuf>,
        /// Overwrite an existing vm.conf without prompting
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ContainerCommands {
    /// Remove all exited containers
    Prune,
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
        /// Port mappings HOST:CONTAINER forwarded to `pelagos run --publish`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        publish: Vec<String>,
        /// Network mode forwarded to `pelagos run --network`.
        #[serde(skip_serializing_if = "Option::is_none")]
        network: Option<String>,
        /// DNS servers forwarded to `pelagos run --dns`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dns: Vec<String>,
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        tty: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<GuestMount>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        labels: Vec<String>,
        /// Port mappings HOST:CONTAINER forwarded to `pelagos run --publish`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        publish: Vec<String>,
        /// Network mode forwarded to `pelagos run --network`.
        #[serde(skip_serializing_if = "Option::is_none")]
        network: Option<String>,
        /// DNS servers forwarded to `pelagos run --dns`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dns: Vec<String>,
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
        #[serde(skip_serializing_if = "is_false")]
        json: bool,
    },
    Logs {
        name: String,
        #[serde(skip_serializing_if = "is_false")]
        follow: bool,
    },
    ContainerInspect {
        name: String,
    },
    /// Remove all exited containers in one shot.
    ContainerPrune,
    Start {
        name: String,
        #[serde(skip_serializing_if = "is_false")]
        interactive: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cmd_override: Vec<String>,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
        time: u64,
    },
    Rm {
        name: String,
        #[serde(skip_serializing_if = "is_false")]
        force: bool,
    },
    Ping,
    Version,
    Subscribe,
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
    /// List locally stored OCI images; maps to `pelagos image ls [--format json]`.
    ImageLs {
        #[serde(skip_serializing_if = "is_false")]
        json: bool,
    },
    /// Pull an OCI image from a registry; maps to `pelagos image pull <reference>`.
    ImagePull {
        reference: String,
    },
    /// Remove a locally stored OCI image; maps to `pelagos image rm <reference>`.
    ImageRm {
        reference: String,
    },
    /// Tag a local OCI image with a new reference; maps to `pelagos image tag <source> <target>`.
    ImageTag {
        source: String,
        target: String,
    },
    /// Inspect a local OCI image; maps to `pelagos image ls --format json` filtered by reference.
    ImageInspect {
        reference: String,
    },
    /// Authenticate with a registry; maps to `pelagos image login`.
    ImageLogin {
        registry: String,
        username: String,
        password: String,
    },
    /// Remove stored credentials for a registry; maps to `pelagos image logout`.
    ImageLogout {
        registry: String,
    },
    /// Proxy a `pelagos compose` subcommand to the VM guest.
    Compose {
        subcommand: String,
        file: String,
        working_dir: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Host environment variables forwarded so that REML `(env ...)` calls
        /// in the compose file resolve secrets from the caller's shell environment.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        env: std::collections::HashMap<String, String>,
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
    /// Version information returned by the guest daemon.
    /// Fields are additive — new fields arrive as Option so older guests remain
    /// wire-compatible.  Planned future field: `vm_image: Option<String>` for
    /// a version token baked into the VM disk image at build time (independent
    /// of pelagos-guest, kernel, and runtime release cadences).
    VersionInfo {
        /// pelagos-guest version (always present).
        #[serde(default)]
        guest: String,
        /// pelagos runtime version (`pelagos --version` inside the VM).
        #[serde(default)]
        runtime: Option<String>,
        /// VM kernel version (`uname -r`).
        #[serde(default)]
        kernel: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Framed binary protocol constants
// ---------------------------------------------------------------------------

const VM_SSH_PORT: u16 = 22;

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
    let profile = cli.profile.clone();

    match cli.command {
        Commands::VmDaemonInternal => {
            let args = daemon_args_from_cli(&cli);
            daemon::run(args); // -> !
        }

        Commands::Vm {
            sub: VmCommands::Start,
        } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            println!("running");
        }
        Commands::Vm {
            sub: VmCommands::Stop,
        } => vm_stop(&profile),
        Commands::Vm {
            sub: VmCommands::Status,
        } => vm_status(&profile),
        Commands::Vm {
            sub: VmCommands::Ls,
        } => vm_ls(),
        Commands::Vm {
            sub: VmCommands::Init { vm_data, force },
        } => {
            if let Err(e) = vm_init(&profile, vm_data.as_deref(), force) {
                eprintln!("error: {}", e);
                process::exit(1);
            }
        }
        Commands::Vm {
            sub: VmCommands::Shell,
        } => {
            let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
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
            let state = match state::StateDir::open_profile(&profile) {
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
            let state = match state::StateDir::open_profile(&profile) {
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
            // The SSH key is baked into the VM image and lives in the default
            // state dir regardless of the active profile.
            let ssh_key = match state::global_ssh_key_file() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {}", e);
                    process::exit(1);
                }
            };
            if !ssh_key.exists() {
                eprintln!(
                    "error: SSH key not found at {}. Rebuild the VM image with 'make image'.",
                    ssh_key.display()
                );
                process::exit(1);
            }
            let mut cmd = std::process::Command::new("ssh");
            cmd.arg("-i")
                .arg(&ssh_key)
                .arg("-o")
                .arg("StrictHostKeyChecking=no")
                .arg("-o")
                .arg("UserKnownHostsFile=/dev/null")
                .arg("-o")
                .arg("LogLevel=ERROR");

            // utun relay: the VM is directly routable at its per-profile guest IP.
            let guest_ip = state::VmProfileConfig::load(&profile)
                .ok()
                .and_then(|c| c.vm_ip)
                .unwrap_or(state::DEFAULT_GUEST_IP);
            cmd.arg(format!("root@{}", guest_ip));

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
            interactive,
            tty,
            ref network,
            ref dns,
        } => {
            let image = image.clone();
            let args = args.clone();
            let name = name.clone();
            let labels = labels.clone();

            // --- Shared setup for both interactive and detached paths ---
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
            // Register port forwards with the daemon.  The daemon installs pf RDR
            // rules via pelagos-pfctl; conflict detection returns an error immediately.
            for spec in &cli.ports {
                if let Some(pf) = daemon::parse_port_spec(spec) {
                    if let Err(e) = send_daemon_cmd(
                        &profile,
                        daemon::DaemonCmd::RegisterPort {
                            host_port: pf.host_port,
                            container_port: pf.container_port,
                        },
                    ) {
                        log::error!("port registration failed: {}", e);
                        process::exit(1);
                    }
                }
            }

            // --- Branch: interactive (PTY/Exec) vs foreground/detached (Run) ---
            if interactive || tty {
                let tty = tty || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
                let stream = connect_or_exit(&profile);
                process::exit(exec_command(
                    stream,
                    GuestCommand::Exec {
                        image,
                        args,
                        env: env_map,
                        tty,
                        name,
                        mounts,
                        labels,
                        publish: cli.ports.clone(),
                        network: network.first().cloned(),
                        dns: dns.clone(),
                    },
                    tty,
                ));
            }
            let stream = connect_or_exit(&profile);
            let exit_code = passthrough_command(
                stream,
                GuestCommand::Run {
                    image,
                    args,
                    mounts,
                    name,
                    detach,
                    env: env_map,
                    labels,
                    publish: cli.ports.clone(),
                    network: network.first().cloned(),
                    dns: dns.clone(),
                },
            );
            // For foreground containers the CLI process stays alive until the
            // container exits; deregister ports now.  For detached containers
            // the caller has already returned (passthrough exits immediately)
            // and deregistration will be handled by the subscription watcher
            // (issue #169).
            if !detach {
                for spec in &cli.ports {
                    if let Some(pf) = daemon::parse_port_spec(spec) {
                        let _ = send_daemon_cmd(
                            &profile,
                            daemon::DaemonCmd::UnregisterPort {
                                host_port: pf.host_port,
                            },
                        );
                    }
                }
            }
            process::exit(exit_code);
        }

        Commands::Exec {
            ref container,
            ref args,
            tty,
            user: _,
            ref workdir,
        } => {
            let container = container.clone();
            let args = args.clone();
            let workdir = workdir.clone();
            // Auto-detect: enable TTY only when stdout is a real terminal.
            let tty = tty || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
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

        Commands::Subscribe => {
            // Subscribe does not start the daemon — if the VM is not running
            // there are no containers to report.  Just exit cleanly.
            let alive = state::StateDir::open_profile(&profile)
                .map(|s| s.is_daemon_alive())
                .unwrap_or(false);
            if !alive {
                log::warn!("subscribe: VM daemon not running");
                process::exit(0);
            }
            let stream = connect_or_exit(&profile);
            subscribe_command(stream);
        }

        Commands::Ping => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let profile_cfg = state::VmProfileConfig::load(&profile).unwrap_or_default();
            if profile_cfg.ping_mode == state::PingMode::Ssh {
                process::exit(ping_ssh(&cli));
            }
            let stream = connect_or_exit(&profile);
            process::exit(ping_command(stream));
        }

        Commands::Version => {
            let mac_version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH"));
            println!("pelagos-mac  {}", mac_version);
            // Query the guest for its version if the daemon is already running.
            let daemon_alive = state::StateDir::open_profile(&profile)
                .map(|s| s.is_daemon_alive())
                .unwrap_or(false);
            if daemon_alive {
                if let Ok(stream) = daemon::connect(&profile) {
                    version_command(stream);
                }
            }
        }

        Commands::Ps { all, json } => {
            // `ps` must not start the daemon: if no daemon is running, there are
            // no containers.  If the daemon is alive (possibly with different
            // mounts), just connect and ask.  This allows `docker ps` (called by
            // the devcontainer CLI) to return empty before the container is started
            // without triggering a "different mount configuration" error.
            let state = match state::StateDir::open_profile(&profile) {
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
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(stream, GuestCommand::Ps { all, json }));
        }

        Commands::Logs { ref name, follow } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
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
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(
                stream,
                GuestCommand::ContainerInspect { name },
            ));
        }

        Commands::Container {
            sub: ContainerCommands::Prune,
        } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(stream, GuestCommand::ContainerPrune));
        }

        Commands::Start {
            ref name,
            interactive,
            ref cmd,
        } => {
            let name = name.clone();
            let cmd_override = cmd.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
            let guest_cmd = GuestCommand::Start {
                name,
                interactive,
                cmd_override,
            };
            if interactive {
                let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
                process::exit(exec_command(stream, guest_cmd, tty));
            } else {
                process::exit(passthrough_command(stream, guest_cmd));
            }
        }

        Commands::Stop { ref name } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(stream, GuestCommand::Stop { name }));
        }

        Commands::Restart { ref name, time } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(
                stream,
                GuestCommand::Restart { name, time },
            ));
        }

        Commands::Rm { ref name, force } => {
            let name = name.clone();
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
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
            let stream = connect_or_exit(&profile);
            process::exit(build_command(
                stream,
                &tag,
                &file,
                &build_args,
                no_cache,
                &context,
            ));
        }

        Commands::Image { ref cmd } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
            let guest_cmd = match cmd {
                ImageCmd::Ls { json } => GuestCommand::ImageLs { json: *json },
                ImageCmd::Pull { reference } => GuestCommand::ImagePull {
                    reference: reference.clone(),
                },
                ImageCmd::Rm { reference } => GuestCommand::ImageRm {
                    reference: reference.clone(),
                },
                ImageCmd::Tag { source, target } => GuestCommand::ImageTag {
                    source: source.clone(),
                    target: target.clone(),
                },
                ImageCmd::Inspect { reference } => {
                    process::exit(inspect_image_command(stream, reference));
                }
                ImageCmd::Login {
                    registry,
                    username,
                    password_stdin,
                } => {
                    process::exit(image_login_command(
                        stream,
                        registry,
                        username.as_deref(),
                        *password_stdin,
                    ));
                }
                ImageCmd::Logout { registry } => {
                    process::exit(passthrough_command(
                        stream,
                        GuestCommand::ImageLogout {
                            registry: registry.clone(),
                        },
                    ));
                }
            };
            process::exit(passthrough_command(stream, guest_cmd));
        }

        Commands::Volume { ref sub, ref name } => {
            let sub = sub.clone();
            let name = name.clone();
            // If no daemon is running there are no volumes.  Return immediately
            // so devcontainer pre-flight checks don't trigger a full VM boot.
            let state = match state::StateDir::open_profile(&profile) {
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
            let stream = connect_or_exit(&profile);
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
            let state = match state::StateDir::open_profile(&profile) {
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
            let stream = connect_or_exit(&profile);
            process::exit(passthrough_command(
                stream,
                GuestCommand::Network { sub, args },
            ));
        }

        Commands::Compose { ref sub } => {
            let (file, project, subcommand_name, extra_args) = match sub {
                ComposeSubcmd::Up {
                    file,
                    project,
                    foreground,
                } => {
                    let mut args = vec![];
                    if *foreground {
                        args.push("--foreground".to_string());
                    }
                    (file.clone(), project.clone(), "up", args)
                }
                ComposeSubcmd::Down {
                    file,
                    project,
                    remove_volumes,
                } => {
                    let mut args = vec![];
                    if *remove_volumes {
                        args.push("--volumes".to_string());
                    }
                    (file.clone(), project.clone(), "down", args)
                }
                ComposeSubcmd::Ps { file, project } => {
                    (file.clone(), project.clone(), "ps", vec![])
                }
                ComposeSubcmd::Logs {
                    file,
                    project,
                    follow,
                    service,
                } => {
                    let mut args = vec![];
                    if *follow {
                        args.push("--follow".to_string());
                    }
                    if let Some(s) = service {
                        args.push(s.clone());
                    }
                    (file.clone(), project.clone(), "logs", args)
                }
            };

            // Resolve the compose file to an absolute path.
            let abs_file = if file.is_absolute() {
                file.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&file)
            };

            // Translate the macOS host path to a VM virtiofs path.
            // $HOME is always share0, mounted at /mnt/share0 in the VM.
            let home = std::env::var("HOME").unwrap_or_default();
            if home.is_empty() || !abs_file.starts_with(&home) {
                eprintln!(
                    "error: compose file {} is outside $HOME — only paths under $HOME are \
                     accessible in the VM without a restart",
                    abs_file.display()
                );
                process::exit(1);
            }
            let subpath = abs_file
                .strip_prefix(&home)
                .unwrap_or(&abs_file)
                .to_string_lossy()
                .trim_start_matches('/')
                .to_string();
            let vm_file = format!("/mnt/share0/{}", subpath);
            let working_dir = std::path::Path::new(&vm_file)
                .parent()
                .unwrap_or(std::path::Path::new("/mnt/share0"))
                .to_string_lossy()
                .into_owned();

            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }

            // Derive the compose project name (used for port tracking).
            let project_name = compose_project_name(&abs_file, project.as_deref());

            // Register macOS-side port listeners before the stack starts.
            // Track which ports we successfully register so we can roll back on failure.
            let mut registered_ports: Vec<u16> = vec![];
            for spec in &cli.ports {
                if let Some(pf) = daemon::parse_port_spec(spec) {
                    if let Err(e) = send_daemon_cmd(
                        &profile,
                        daemon::DaemonCmd::RegisterPort {
                            host_port: pf.host_port,
                            container_port: pf.container_port,
                        },
                    ) {
                        // Roll back any ports we already registered.
                        for &hp in &registered_ports {
                            let _ = send_daemon_cmd(
                                &profile,
                                daemon::DaemonCmd::UnregisterPort { host_port: hp },
                            );
                        }
                        log::error!("port registration failed: {}", e);
                        process::exit(1);
                    }
                    registered_ports.push(pf.host_port);
                }
            }

            // Associate registered ports with this project so they can be
            // bulk-deregistered on exit (issue #161).
            if !registered_ports.is_empty() {
                let _ = send_daemon_cmd(
                    &profile,
                    daemon::DaemonCmd::TrackComposeProject {
                        project: project_name.clone(),
                        host_ports: registered_ports,
                    },
                );
            }

            // Forward only the env vars explicitly referenced in the compose file
            // via (env "NAME") calls.  Forwarding the full host environment would
            // expose unrelated shell secrets over vsock.
            let compose_env = collect_compose_env(&abs_file);

            let stream = connect_or_exit(&profile);
            let exit_code = passthrough_command(
                stream,
                GuestCommand::Compose {
                    subcommand: subcommand_name.to_string(),
                    file: vm_file,
                    working_dir,
                    project,
                    args: extra_args,
                    env: compose_env,
                },
            );

            // Deregister ports on any exit (success or failure).
            // For `compose down` this is a no-op if `up` already cleaned up.
            let _ = send_daemon_cmd(
                &profile,
                daemon::DaemonCmd::UnregisterComposePorts {
                    project: project_name,
                },
            );

            process::exit(exit_code);
        }

        Commands::Cp { ref src, ref dst } => {
            let daemon_args = daemon_args_from_cli(&cli);
            if let Err(e) = daemon::ensure_running(&daemon_args) {
                log::error!("failed to start VM daemon: {}", e);
                process::exit(1);
            }
            let stream = connect_or_exit(&profile);
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
    // Load per-profile vm.conf as a fallback layer below CLI flags.
    let profile_cfg = state::VmProfileConfig::load(&cli.profile).unwrap_or_default();

    // Auto-discover VM artifacts when neither CLI flags nor vm.conf supply them.
    // This fires on a fresh install where no vm.conf has been written yet.
    let discovered = if cli.kernel.is_none() && profile_cfg.kernel.is_none()
        || cli.disk.is_none() && profile_cfg.disk.is_none()
    {
        discover_vm_artifacts()
    } else {
        None
    };

    let kernel = cli
        .kernel
        .clone()
        .or(profile_cfg.kernel)
        .or_else(|| discovered.as_ref().map(|d| d.kernel.clone()))
        .unwrap_or_else(|| {
            log::error!(
                "no kernel image found; install pelagos-mac via Homebrew \
                 or pass --kernel / set kernel= in vm.conf"
            );
            process::exit(1);
        });
    let disk = cli
        .disk
        .clone()
        .or(profile_cfg.disk)
        .or_else(|| discovered.as_ref().map(|d| d.disk.clone()))
        .unwrap_or_else(|| {
            log::error!(
                "no disk image found; install pelagos-mac via Homebrew \
                 or pass --disk / set disk= in vm.conf"
            );
            process::exit(1);
        });

    // Always-on volumes share: <profile-dir>/volumes → /var/lib/pelagos/volumes in VM.
    // This makes named pelagos volumes persistent across VM restarts (virtiofs-backed on host).
    let volumes_host = pelagos_volumes_host_path(&cli.profile);
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
    // vm.conf cmdline overrides the CLI default when set.
    let base_cmdline = profile_cfg.cmdline.as_deref().unwrap_or(&cli.cmdline);
    let cmdline = if base_cmdline.contains("clock.utc=") {
        base_cmdline.to_string()
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
        format!("{} clock.utc={}", base_cmdline, clock_utc)
    };

    daemon::DaemonArgs {
        kernel,
        initrd: cli
            .initrd
            .clone()
            .or(profile_cfg.initrd)
            .or_else(|| discovered.and_then(|d| d.initrd)),
        disk,
        cmdline,
        memory_mib: cli.memory.or(profile_cfg.memory).unwrap_or(4096),
        cpus: cli.cpus.or(profile_cfg.cpus).unwrap_or(2),
        virtiofs_shares,
        port_forwards,
        profile: cli.profile.clone(),
        extra_disks: cli.extra_disks.clone(),
    }
}

/// Returns the host-side backing directory for the always-on `pelagos-volumes`
/// virtiofs share.  For the default profile this is `~/.local/share/pelagos/volumes`;
/// for named profiles it is `~/.local/share/pelagos/profiles/<name>/volumes`.
fn pelagos_volumes_host_path(profile: &str) -> PathBuf {
    state::profile_dir(profile)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/share/pelagos")
        })
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

fn connect_or_exit(profile: &str) -> UnixStream {
    daemon::connect(profile).unwrap_or_else(|e| {
        log::error!("connect to VM daemon: {}", e);
        process::exit(1);
    })
}

/// Send a `DaemonCmd` to the running daemon and return the response.
///
/// Connects to `vm.sock`, writes one JSON line, reads one JSON response line,
/// then closes.  Returns `Err` if the daemon reports a port conflict or the
/// connection fails.
fn send_daemon_cmd(profile: &str, cmd: daemon::DaemonCmd) -> io::Result<()> {
    let mut stream = daemon::connect(profile)?;
    let mut msg =
        serde_json::to_string(&cmd).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    msg.push('\n');
    stream.write_all(msg.as_bytes())?;

    // Read the one-line response.
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;

    match serde_json::from_str::<daemon::DaemonResponse>(line.trim()) {
        Ok(daemon::DaemonResponse::Ok) => Ok(()),
        Ok(daemon::DaemonResponse::Err { message }) => {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, message))
        }
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

// ---------------------------------------------------------------------------
// VM management commands
// ---------------------------------------------------------------------------

fn vm_stop(profile: &str) {
    let state = state::StateDir::open_profile(profile).unwrap_or_else(|e| {
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

fn vm_status(profile: &str) {
    let state = state::StateDir::open_profile(profile).unwrap_or_else(|e| {
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
// vm ls — enumerate all profiles and their running state
// ---------------------------------------------------------------------------

fn vm_ls() {
    let base = match state::pelagos_base() {
        Ok(b) => b,
        Err(e) => {
            log::error!("state dir: {}", e);
            process::exit(1);
        }
    };

    // Collect (profile_name, VmProfileConfig, Option<u32 pid>) for every profile.
    // Always start with "default" (the root data dir), then add named profiles.
    let mut profiles: Vec<(String, state::VmProfileConfig, Option<u32>)> = Vec::new();

    let default_state = state::StateDir::open_profile("default").ok();
    let default_pid = default_state.as_ref().and_then(|s| s.running_pid());
    let default_cfg = state::VmProfileConfig::load("default").unwrap_or_default();
    profiles.push(("default".to_string(), default_cfg, default_pid));

    // Scan ~/.local/share/pelagos/profiles/ for named profiles.
    let profiles_dir = base.join("profiles");
    if let Ok(rd) = std::fs::read_dir(&profiles_dir) {
        let mut named: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        named.sort();
        for name in named {
            let s = state::StateDir::open_profile(&name).ok();
            let pid = s.as_ref().and_then(|s| s.running_pid());
            let cfg = state::VmProfileConfig::load(&name).unwrap_or_default();
            profiles.push((name, cfg, pid));
        }
    }

    // Column widths.
    let col_profile = 10usize;
    let col_access = 10usize;
    let col_mem = 7usize;
    let col_cpu = 5usize;
    let col_status = 20usize;

    println!(
        "{:<col_profile$}  {:<col_access$}  {:>col_mem$}  {:>col_cpu$}  {:<col_status$}",
        "PROFILE",
        "ACCESS",
        "MEMORY",
        "CPUS",
        "STATUS",
        col_profile = col_profile,
        col_access = col_access,
        col_mem = col_mem,
        col_cpu = col_cpu,
        col_status = col_status,
    );
    println!(
        "{:-<col_profile$}  {:-<col_access$}  {:->col_mem$}  {:->col_cpu$}  {:-<col_status$}",
        "",
        "",
        "",
        "",
        "",
        col_profile = col_profile,
        col_access = col_access,
        col_mem = col_mem,
        col_cpu = col_cpu,
        col_status = col_status,
    );

    for (name, cfg, pid) in &profiles {
        let access = match cfg.ping_mode {
            state::PingMode::Ssh => "ssh",
            state::PingMode::Vsock => "vsock/shell",
        };
        let mem = cfg
            .memory
            .map(|m| format!("{} MB", m))
            .unwrap_or_else(|| "-".to_string());
        let cpus = cfg
            .cpus
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        let status = match pid {
            Some(p) => format!("running (pid {})", p),
            None => "stopped".to_string(),
        };
        println!(
            "{:<col_profile$}  {:<col_access$}  {:>col_mem$}  {:>col_cpu$}  {:<col_status$}",
            name,
            access,
            mem,
            cpus,
            status,
            col_profile = col_profile,
            col_access = col_access,
            col_mem = col_mem,
            col_cpu = col_cpu,
            col_status = col_status,
        );
    }
}

// ---------------------------------------------------------------------------
// vm init
// ---------------------------------------------------------------------------

/// Initialise the VM configuration for the given profile.
///
/// Searches for VM artifacts in:
///   1. `vm_data` if provided
///   2. `$(binary)/../share/pelagos-mac/`  — Homebrew pkgshare
///   3. `$(binary)/../../../out/`            — dev build tree
///
/// The kernel and initrd can stay in the (possibly read-only) source directory.
/// `root.img` is copied to the profile state dir so it is always writable.
/// A `vm.conf` is written to the profile state dir pointing at these paths.
fn vm_init(profile: &str, vm_data: Option<&std::path::Path>, force: bool) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};

    // ── 0. running-VM guard (bug 1) ───────────────────────────────────────
    // Refuse to overwrite root.img while the VM is running without --force.
    // Without this guard, if root.img has been unlinked while the VM daemon
    // holds it open (Unix unlink-while-open), dst_disk.exists() returns false
    // and vm init silently overwrites the user's OCI image cache.
    if !force {
        if let Ok(st) = state::StateDir::open_profile(profile) {
            if let Some(pid) = st.running_pid() {
                return Err(Error::other(format!(
                    "VM is running (pid {pid}). \
                     Stop it first with 'pelagos vm stop', then re-run \
                     'pelagos vm init', or pass --force to stop it automatically."
                )));
            }
        }
    }

    // ── 1. locate source directory ────────────────────────────────────────
    let src_dir: Option<PathBuf> = if let Some(p) = vm_data {
        let canonical = p.canonicalize().map_err(|e| {
            Error::new(
                ErrorKind::NotFound,
                format!("vm-data directory {}: {}", p.display(), e),
            )
        })?;
        Some(canonical)
    } else {
        None
    };

    // ── 2. resolve artifact paths ─────────────────────────────────────────
    // When --vm-data is given, require kernel + disk in that dir and write
    // their absolute paths to vm.conf (dev workflow: custom kernel/initrd).
    //
    // When --vm-data is absent, only locate the disk here; kernel/initrd are
    // NOT written to vm.conf so that runtime discovery finds them via the
    // Homebrew pkgshare symlink, which always resolves to the current keg.
    // This prevents vm.conf from going stale after `brew upgrade`.
    let (kernel_for_conf, initrd_for_conf, src_disk) = if let Some(ref dir) = src_dir {
        let kernel = ["vmlinuz", "ubuntu-vmlinuz"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("vmlinuz not found in {}", dir.display()),
                )
            })?;
        let initrd = ["initramfs.gz", "initramfs-custom.gz"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists());
        let disk = dir.join("root.img");
        if !disk.exists() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("root.img not found in {}", dir.display()),
            ));
        }
        (Some(kernel), initrd, disk)
    } else {
        // No explicit vm-data: find the disk only. The kernel/initrd will be
        // discovered at runtime from the Homebrew pkgshare.
        let binary = std::env::current_exe()
            .map_err(|e| Error::new(ErrorKind::NotFound, format!("cannot locate binary: {}", e)))?;
        let bin_dir = binary
            .parent()
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "binary has no parent directory"))?;
        let candidates = [
            bin_dir.join("../share/pelagos-mac"),
            bin_dir.join("share/pelagos-mac"),
            bin_dir.join("../../../out"),
        ];
        let disk = candidates
            .iter()
            .map(|d| d.join("root.img"))
            .find(|p| p.exists())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    "root.img not found. Specify --vm-data <dir> or run \
                     'brew install pelagos-containers/tap/pelagos-mac' first.",
                )
            })?;
        (None, None, disk)
    };

    // ── 3. prepare profile state dir and disk path ────────────────────────
    let state_dir = state::profile_dir(profile)?;
    std::fs::create_dir_all(&state_dir)?;

    let dst_disk = state_dir.join("root.img");
    let vm_conf = state_dir.join("vm.conf");

    if vm_conf.exists() && !force {
        return Err(Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "vm.conf already exists at {}. Use --force to overwrite.",
                vm_conf.display()
            ),
        ));
    }

    // ── 3b. stop any running VM so the new initramfs takes effect ─────────
    if force {
        if let Ok(state) = state::StateDir::open_profile(profile) {
            if let Some(pid) = state.running_pid() {
                println!("Stopping running VM (pid {}) …", pid);
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while std::time::Instant::now() < deadline {
                    if state.running_pid().is_none() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if state.running_pid().is_some() {
                    log::warn!("VM daemon (pid {}) did not exit within 15 s", pid);
                }
            }
        }
    }

    // Copy root.img only if the destination doesn't already exist or --force.
    // The image is a fresh sparse placeholder (~0 bytes on-disk); use cp to
    // preserve sparseness rather than reading/writing every zero byte.
    if !dst_disk.exists() || force {
        print!("Copying root.img to {} … ", state_dir.display());
        let _ = std::io::stdout().flush();
        std::fs::copy(&src_disk, &dst_disk)?;
        println!("done");
    } else {
        println!(
            "root.img already present at {} — reusing",
            dst_disk.display()
        );
    }

    // ── 4. write vm.conf ──────────────────────────────────────────────────
    // Only kernel/initrd overrides (from --vm-data) are written here.
    // When absent, runtime discovery resolves them from the Homebrew pkgshare
    // symlink so the paths never go stale after `brew upgrade`.
    let mut conf = format!(
        "# vm.conf — written by pelagos vm init\n\
         # kernel and initrd are discovered automatically from the Homebrew\n\
         # pkgshare unless overridden here (e.g. via --vm-data for dev builds).\n\
         disk   = {}\n",
        dst_disk.display(),
    );
    if let Some(ref k) = kernel_for_conf {
        conf.push_str(&format!("kernel = {}\n", k.display()));
    }
    if let Some(ref i) = initrd_for_conf {
        conf.push_str(&format!("initrd = {}\n", i.display()));
    }

    std::fs::write(&vm_conf, &conf)?;
    println!("Wrote {}", vm_conf.display());

    // ── 5. next-step guidance ─────────────────────────────────────────────
    let ping_cmd = if profile == "default" {
        "pelagos ping".to_string()
    } else {
        format!("pelagos --profile {} ping", profile)
    };
    let run_cmd = if profile == "default" {
        "pelagos run alpine echo hello".to_string()
    } else {
        format!("pelagos --profile {} run alpine echo hello", profile)
    };
    println!();
    println!("VM initialised. To verify:");
    println!(
        "  {}   # cold-boots the VM with the new image, should print 'pong'",
        ping_cmd
    );
    println!("  {}  # should print 'hello'", run_cmd);

    Ok(())
}

// ---------------------------------------------------------------------------
// VM artifact auto-discovery
// ---------------------------------------------------------------------------

struct DiscoveredArtifacts {
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    disk: PathBuf,
}

/// Probe well-known locations for VM artifacts (kernel, initrd, disk image).
///
/// Kernel/initrd and disk are searched independently so that vm.conf can
/// omit `kernel =` / `initrd =` (letting them be discovered at runtime from
/// the Homebrew pkgshare symlink) while still specifying a custom disk path.
///
/// Kernel search order (no disk required):
///   1. `$(binary)/../share/pelagos-mac/`  — Homebrew formula pkgshare
///   2. `$(binary)/share/pelagos-mac/`     — Homebrew cask staged layout
///   3. `$(binary)/../../../out/`           — dev build tree
///
/// Disk search order (fallback; vm.conf `disk =` takes priority in caller):
///   1. `~/.local/share/pelagos/`          — XDG data dir (written by vm init)
///   2. same dirs as kernel search         — fresh-install fallback
fn discover_vm_artifacts() -> Option<DiscoveredArtifacts> {
    let binary = std::env::current_exe().ok()?;
    let bin_dir = binary.parent()?;

    let kernel_dirs = [
        bin_dir.join("../share/pelagos-mac"),
        bin_dir.join("share/pelagos-mac"),
        bin_dir.join("../../../out"),
    ];

    // Find kernel (and optional initrd) from install/dev dirs.
    let (kernel, initrd) = kernel_dirs.iter().find_map(|dir| {
        let kernel = ["vmlinuz", "ubuntu-vmlinuz"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists())?;
        let initrd = ["initramfs.gz", "initramfs-custom.gz"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.exists());
        log::debug!("auto-discovered kernel in {}", dir.display());
        Some((kernel, initrd))
    })?;

    // Find disk: user data dir first, then fall back to install/dev dirs.
    let mut disk_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(base) = state::pelagos_base() {
        disk_dirs.push(base);
    }
    disk_dirs.extend(kernel_dirs.iter().cloned());
    let disk = disk_dirs.iter().map(|d| d.join("root.img")).find(|p| p.exists())?;

    Some(DiscoveredArtifacts { kernel, initrd, disk })
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

/// Send ImageInspect to the guest, accumulate the JSON image list, and print
/// the single entry whose "reference" field matches `reference`. Exits 0 on
/// success, 1 if the image is not found or the response cannot be parsed.
fn inspect_image_command(stream: UnixStream, reference: &str) -> i32 {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    let cmd = GuestCommand::ImageInspect {
        reference: reference.to_string(),
    };
    let mut msg = serde_json::to_string(&cmd).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("write error: {}", e);
        return 1;
    }

    // Accumulate stdout chunks; the guest streams `pelagos image ls --format json`.
    let mut stdout_buf = String::new();
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
                    stdout_buf.push_str(&data);
                }
            }
            Ok(GuestResponse::Exit { exit }) => {
                exit_code = exit;
                break;
            }
            Ok(GuestResponse::Error { error }) => {
                log::error!("guest error: {}", error);
                return 1;
            }
            Ok(resp) => {
                log::warn!("unexpected response: {:?}", resp);
            }
            Err(e) => {
                log::error!("parse error: {} (line: {:?})", e, trimmed);
                return 1;
            }
        }
    }

    if exit_code != 0 {
        return exit_code;
    }

    // Parse the JSON image list and find the matching entry.
    let images: Vec<serde_json::Value> = match serde_json::from_str(&stdout_buf) {
        Ok(v) => v,
        Err(e) => {
            log::error!("failed to parse image list JSON: {}", e);
            return 1;
        }
    };

    let matched = images
        .into_iter()
        .find(|img| img.get("reference").and_then(|r| r.as_str()) == Some(reference));

    match matched {
        Some(img) => {
            println!("{}", serde_json::to_string_pretty(&img).unwrap_or_default());
            0
        }
        None => {
            eprintln!("Error: image not found: {}", reference);
            1
        }
    }
}

/// Collect registry credentials on the host and forward as `ImageLogin` over vsock.
///
/// Reads the password from stdin when `--password-stdin` is set, prompts
/// interactively otherwise.  The username is also prompted when not supplied
/// via `--username`.  Credentials are forwarded to the guest as plain text
/// inside the already-encrypted vsock channel; the guest never exposes them in
/// a process argument list (it uses `--password-stdin` on its own pelagos call).
fn image_login_command(
    stream: UnixStream,
    registry: &str,
    username: Option<&str>,
    password_stdin: bool,
) -> i32 {
    let username = match username {
        Some(u) => u.to_string(),
        None => {
            eprint!("Username: ");
            let mut u = String::new();
            if io::stdin().read_line(&mut u).is_err() || u.trim().is_empty() {
                eprintln!("error: username is required");
                return 1;
            }
            u.trim().to_string()
        }
    };

    let password = if password_stdin {
        let mut p = String::new();
        if io::stdin().read_line(&mut p).is_err() {
            eprintln!("error: failed to read password from stdin");
            return 1;
        }
        p.trim_end_matches('\n').trim_end_matches('\r').to_string()
    } else {
        match rpassword::prompt_password("Password: ") {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: failed to read password: {}", e);
                return 1;
            }
        }
    };

    passthrough_command(
        stream,
        GuestCommand::ImageLogin {
            registry: registry.to_string(),
            username,
            password,
        },
    )
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
// compose helpers
// ---------------------------------------------------------------------------

/// Derive the compose project name for port-tracking purposes.
///
/// Matches Docker Compose v2 convention: use the explicit `--project-name`
/// flag if given, otherwise fall back to the parent directory name of the
/// compose file (lower-cased).
/// Scan a compose `.reml` file for `(env "NAME")` calls and return a map of
/// NAME → value for every such variable that is set in the current process
/// environment.  Only variables explicitly referenced in the file are included;
/// the rest of the host environment is never forwarded.
fn collect_compose_env(file: &std::path::Path) -> std::collections::HashMap<String, String> {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return std::collections::HashMap::new(),
    };
    let mut result = std::collections::HashMap::new();
    let needle = "(env \"";
    let mut search = content.as_str();
    while let Some(start) = search.find(needle) {
        search = &search[start + needle.len()..];
        if let Some(end) = search.find('"') {
            let name = &search[..end];
            if !name.is_empty() {
                if let Ok(val) = std::env::var(name) {
                    result.insert(name.to_string(), val);
                }
            }
            search = &search[end + 1..];
        }
    }
    result
}

fn compose_project_name(file: &std::path::Path, project_flag: Option<&str>) -> String {
    if let Some(p) = project_flag {
        return p.to_string();
    }
    file.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "default".to_string())
}

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

/// Subscribe to container lifecycle events; stream NDJSON to stdout until
/// the guest disconnects or the process is killed.
fn subscribe_command(stream: UnixStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    // Send Subscribe command.
    let mut msg = serde_json::to_string(&GuestCommand::Subscribe).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::error!("subscribe: write error: {}", e);
        return;
    }

    // Forward every NDJSON line from the guest to our stdout verbatim.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                print!("{}", line);
                if std::io::stdout().flush().is_err() {
                    break;
                }
            }
        }
    }
}

/// Query the guest daemon for its version and print it to stdout.
fn version_command(stream: UnixStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = stream;

    let mut msg = serde_json::to_string(&GuestCommand::Version).unwrap();
    msg.push('\n');
    if let Err(e) = writer.write_all(msg.as_bytes()) {
        log::warn!("version query write error: {}", e);
        return;
    }

    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => {
            log::warn!("no version response from guest");
            return;
        }
        Ok(_) => {}
    }
    match serde_json::from_str::<GuestResponse>(line.trim_end()) {
        Ok(GuestResponse::VersionInfo {
            guest,
            runtime,
            kernel,
        }) => {
            println!("pelagos-guest  {}", guest);
            if let Some(v) = runtime {
                println!("pelagos        {}", v);
            }
            if let Some(k) = kernel {
                println!("kernel         {}", k);
            }
        }
        other => {
            log::warn!("unexpected version response: {:?}", other);
        }
    }
}

/// Ping a non-pelagos VM (e.g. Ubuntu build VM) by waiting for SSH to respond.
/// The daemon is already running when this is called; we just retry SSH until
/// it accepts a connection, then print "pong".  Retries for up to 5 minutes.
fn ping_ssh(cli: &Cli) -> i32 {
    let profile = &cli.profile;
    let ssh_key = match state::global_ssh_key_file() {
        Ok(p) => p,
        Err(e) => {
            log::error!("SSH key: {}", e);
            return 1;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::error!("current_exe: {}", e);
            return 1;
        }
    };
    let proxy_cmd = format!(
        "{} --profile {} ssh-relay-proxy {}",
        exe.display(),
        profile,
        VM_SSH_PORT
    );
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(600);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let elapsed = start.elapsed().as_secs();
        eprint!("\r[ping] attempt {} ({}s elapsed) ...", attempt, elapsed);
        let _ = std::io::Write::flush(&mut std::io::stderr());

        // Capture SSH stderr for progress diagnostics.
        let out = std::process::Command::new("ssh")
            .arg("-i")
            .arg(&ssh_key)
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg("-o")
            .arg("ConnectTimeout=30")
            .arg("-o")
            .arg(format!("ProxyCommand={}", proxy_cmd))
            .arg("root@vm")
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if ok {
            eprintln!(); // newline after progress
            println!("pong");
            return 0;
        }
        // Print SSH's error message so failures are immediately visible.
        if let Ok(ref o) = out {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            if !msg.is_empty() {
                eprint!("\r[ping] attempt {} ({}s): {}\n", attempt, elapsed, msg);
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!();
            log::error!("SSH ping timed out after 10 minutes");
            return 1;
        }
        // Wait between retries (15s gap matches the utun ping timeout).
        std::thread::sleep(std::time::Duration::from_secs(15));
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
    //
    // IMPORTANT: do NOT use io::stdin() here.  Rust's Stdin holds a global
    // Mutex<BufReader<StdinRaw>> with an 8192-byte internal buffer.  When
    // buf.len() (4096) < buffer capacity (8192), BufReader::read pre-fills the
    // entire 8192-byte internal buffer from the kernel fd before returning only
    // 4096 bytes.  The extra bytes are consumed from the kernel fd but not
    // returned — so poll(STDIN_FILENO) no longer fires for them and they sit in
    // the BufReader forever.  This causes the hang described in issue #119:
    // the last ≤4096 bytes before a pause (e.g. VS Code's server start command
    // appended right after the 74 MB tarball) get stuck in the internal buffer.
    //
    // Fix: bypass io::stdin() and call libc::read(STDIN_FILENO) directly.  A
    // direct read only consumes exactly what poll saw in the kernel fd buffer,
    // so there is no hidden buffer between poll and the framed relay.
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
            // Include POLLHUP so that when the write end of the pipe is closed
            // (and no more bytes remain), read() returns 0 and we break cleanly.
            // Without POLLHUP here, poll() fires instantly every iteration but
            // POLLIN is never set → read() is never called → 100% CPU spin.
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let n = unsafe {
                    libc::read(
                        libc::STDIN_FILENO,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n < 0 {
                    // EINTR is harmless: re-poll.
                    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        let mut w = writer_stdin.lock().unwrap();
                        let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                        break;
                    }
                } else if n == 0 {
                    // EOF — tell the guest to close the child's stdin pipe.
                    let mut w = writer_stdin.lock().unwrap();
                    let _ = send_frame(&mut *w, FRAME_STDIN, &[]);
                    break;
                } else {
                    let mut w = writer_stdin.lock().unwrap();
                    if send_frame(&mut *w, FRAME_STDIN, &buf[..n as usize]).is_err() {
                        break;
                    }
                }
            }
            // POLLERR or POLLNVAL: unrecoverable — send EOF and stop.
            // POLLNVAL (0x20 on macOS) fires when the fd is closed (e.g. bash
            // closes fd 0 when backgrounding a process with job control inactive).
            if fds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                log::warn!(
                    "exec: stdin fd closed/error (revents={:#x}), sending EOF",
                    fds[0].revents
                );
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
                log::warn!("exec: unexpected frame type {} — closing", frame_type);
                return 1;
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
        recv_frame, send_frame, Cli, Commands, GuestCommand, GuestMount, GuestResponse,
        FRAME_EXIT, FRAME_RESIZE, FRAME_STDIN, FRAME_STDOUT,
    };
    use clap::Parser as _;
    use std::io::Cursor;
    use std::path::PathBuf;

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
            publish: vec![],
            network: None,
            dns: vec![],
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
            publish: vec![],
            network: None,
            dns: vec![],
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
            publish: vec![],
            network: None,
            dns: vec![],
        };
        let json = serde_json::to_string(&cmd).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "run");
        assert_eq!(v["mounts"][0]["tag"], "share0");
        assert_eq!(v["mounts"][0]["container_path"], "/data");
    }

    #[test]
    fn ps_command_serializes() {
        let cmd = GuestCommand::Ps {
            all: true,
            json: false,
        };
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
            name: None,
            mounts: vec![],
            labels: vec![],
            publish: vec![],
            network: None,
            dns: vec![],
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

    #[test]
    fn cli_extra_disk_parses() {
        let cli = Cli::try_parse_from([
            "pelagos",
            "--extra-disk",
            "/out/build.img",
            "--kernel",
            "/out/vmlinuz",
            "--disk",
            "/out/root.img",
            "ping",
        ])
        .unwrap();
        assert_eq!(cli.extra_disks, vec![PathBuf::from("/out/build.img")]);
    }

    #[test]
    fn cli_extra_disk_multiple() {
        let cli = Cli::try_parse_from([
            "pelagos",
            "--extra-disk",
            "/out/a.img",
            "--extra-disk",
            "/out/b.img",
            "--kernel",
            "/out/vmlinuz",
            "--disk",
            "/out/root.img",
            "ping",
        ])
        .unwrap();
        assert_eq!(
            cli.extra_disks,
            vec![PathBuf::from("/out/a.img"), PathBuf::from("/out/b.img")]
        );
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
    fn image_ls_serializes() {
        let cmd = GuestCommand::ImageLs { json: false };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_ls");
        // json=false is skip_serializing, so the field must be absent
        assert!(v.get("json").is_none());
    }

    #[test]
    fn image_pull_serializes() {
        let cmd = GuestCommand::ImagePull {
            reference: "alpine:latest".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_pull");
        assert_eq!(v["reference"], "alpine:latest");
    }

    #[test]
    fn image_rm_serializes() {
        let cmd = GuestCommand::ImageRm {
            reference: "alpine:latest".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_rm");
        assert_eq!(v["reference"], "alpine:latest");
    }

    #[test]
    fn image_tag_serializes() {
        let cmd = GuestCommand::ImageTag {
            source: "alpine:latest".into(),
            target: "alpine:sparky".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_tag");
        assert_eq!(v["source"], "alpine:latest");
        assert_eq!(v["target"], "alpine:sparky");
    }

    #[test]
    fn image_inspect_serializes() {
        let cmd = GuestCommand::ImageInspect {
            reference: "alpine:latest".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_inspect");
        assert_eq!(v["reference"], "alpine:latest");
    }

    #[test]
    fn image_login_serializes() {
        let cmd = GuestCommand::ImageLogin {
            registry: "public.ecr.aws".into(),
            username: "AWS".into(),
            password: "s3cr3t".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_login");
        assert_eq!(v["registry"], "public.ecr.aws");
        assert_eq!(v["username"], "AWS");
        assert_eq!(v["password"], "s3cr3t");
    }

    #[test]
    fn image_logout_serializes() {
        let cmd = GuestCommand::ImageLogout {
            registry: "public.ecr.aws".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_logout");
        assert_eq!(v["registry"], "public.ecr.aws");
    }

    #[test]
    fn image_login_wire_format() {
        // Verify the JSON wire format matches what the guest expects to parse.
        // The guest's GuestCommand uses serde tag="cmd", rename_all="snake_case",
        // so the discriminant must be "image_login" and all fields present.
        let cmd = GuestCommand::ImageLogin {
            registry: "ghcr.io".into(),
            username: "octocat".into(),
            password: "ghp_token".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        // Round-trip through Value to check field names without importing guest crate.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cmd"], "image_login", "discriminant must be image_login");
        assert_eq!(v["registry"], "ghcr.io");
        assert_eq!(v["username"], "octocat");
        assert_eq!(v["password"], "ghp_token");
    }

    #[test]
    fn image_logout_wire_format() {
        let cmd = GuestCommand::ImageLogout {
            registry: "ghcr.io".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["cmd"], "image_logout",
            "discriminant must be image_logout"
        );
        assert_eq!(v["registry"], "ghcr.io");
        // No extra fields.
        assert!(v.get("username").is_none());
        assert!(v.get("password").is_none());
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

    /// Regression test for issue #119: the stdin relay in exec_command must use
    /// unbuffered reads (libc::read) rather than io::stdin() to avoid the
    /// BufReader pre-fetch problem.
    ///
    /// io::Stdin holds a Mutex<BufReader<StdinRaw>> with 8192-byte capacity.
    /// When buf.len()=4096 < capacity=8192, BufReader::read pre-fills the full
    /// 8192-byte internal buffer from the kernel fd before returning only 4096
    /// bytes.  This consumes more bytes from the fd than poll(STDIN_FILENO)
    /// "knows about", so the leftover bytes sit in the BufReader forever once
    /// the producer pauses.
    ///
    /// This test verifies the property that a single libc::read(4096) on a
    /// pipe with 8000 bytes leaves the remaining 3904 bytes in the kernel pipe
    /// buffer (i.e. poll fires again for the second read).
    #[test]
    fn stdin_relay_uses_unbuffered_read_issue_119() {
        // Create an anonymous pipe to simulate the stdin fd.
        let mut pipe_fds = [-1i32; 2];
        let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "pipe() failed");
        let read_fd = pipe_fds[0];
        let write_fd = pipe_fds[1];

        // Write 8000 bytes to the write end.
        let data = vec![0xABu8; 8000];
        let written =
            unsafe { libc::write(write_fd, data.as_ptr() as *const libc::c_void, data.len()) };
        assert_eq!(written, 8000, "write to pipe failed");

        // First libc::read with 4096-byte buffer — should return exactly 4096.
        let mut buf = [0u8; 4096];
        let n1 = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert_eq!(n1, 4096, "first read should return exactly 4096 bytes");

        // poll must still fire for read_fd because 3904 bytes remain in the
        // kernel pipe buffer (not consumed by a BufReader into user-space).
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_ret = unsafe { libc::poll(&mut pfd as *mut _, 1, 0) }; // timeout=0 (non-blocking)
        assert_eq!(poll_ret, 1, "poll should return 1 fd ready");
        assert!(
            pfd.revents & libc::POLLIN != 0,
            "POLLIN must be set — remaining bytes must be visible to poll (issue #119 regression)"
        );

        // Second read consumes the rest.
        let mut buf2 = [0u8; 4096];
        let n2 = unsafe { libc::read(read_fd, buf2.as_mut_ptr() as *mut libc::c_void, buf2.len()) };
        assert_eq!(
            n2, 3904,
            "second read should return the remaining 3904 bytes"
        );

        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }
}
