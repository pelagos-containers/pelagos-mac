//! pelagos-guest — vsock command daemon running inside the Linux VM.
//!
//! Listens on AF_VSOCK port VSOCK_PORT. For each connection, reads a JSON
//! `GuestCommand`, executes the appropriate pelagos sub-command, and streams
//! stdout/stderr back over the socket, followed by a terminal `GuestResponse`.
//!
//! Cross-compiled to aarch64-unknown-linux-gnu and baked into the VM disk image.
//!
//! # Protocol (newline-delimited JSON)
//!
//! Request  (host → guest): `{"cmd":"run","image":"alpine","args":[...],"env":{}}`
//! Response (guest → host): `{"stream":"stdout","data":"hello\n"}` …
//!                           `{"exit":0}`
//!
//! # Exec protocol
//!
//! After JSON handshake, both sides switch to framed binary:
//!   [type: u8][length: u32 big-endian][data: length bytes]
//!
//! Frame types:
//!   0 = Stdin  (host → guest)
//!   1 = Stdout (guest → host)
//!   2 = Stderr (guest → host)
//!   3 = Exit   (guest → host, 4 bytes i32 big-endian)
//!   4 = Resize (host → guest, 4 bytes u16 rows + u16 cols big-endian)

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub const VSOCK_PORT: u32 = 1024;

// ---------------------------------------------------------------------------
// Global subscriber registry (Subscribe command support)
// ---------------------------------------------------------------------------

/// Each active Subscribe connection holds a sender in this list.
/// The poller thread broadcasts events to all senders and prunes dead ones.
type SubscriberList = Arc<Mutex<Vec<std::sync::mpsc::SyncSender<GuestEvent>>>>;

fn subscriber_list() -> &'static SubscriberList {
    static LIST: std::sync::OnceLock<SubscriberList> = std::sync::OnceLock::new();
    LIST.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

// ---------------------------------------------------------------------------
// Framed binary protocol constants
// ---------------------------------------------------------------------------

const FRAME_STDIN: u8 = 0;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const FRAME_EXIT: u8 = 3;
const FRAME_RESIZE: u8 = 4;

fn send_frame(w: &mut impl Write, frame_type: u8, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[frame_type])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn recv_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
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
// Protocol types
// ---------------------------------------------------------------------------

/// A single virtiofs bind mount to apply inside the container.
#[derive(Debug, Deserialize, Clone)]
pub struct GuestMount {
    /// virtiofs tag — the directory is already mounted at `/mnt/<tag>` in the guest.
    pub tag: String,
    /// Relative subpath within the virtiofs mount (empty = root of the share).
    /// When the daemon uses a broad share (e.g. $HOME as share0), the subpath
    /// identifies the specific directory to bind-mount into the container.
    #[serde(default)]
    pub subpath: String,
    /// Absolute path inside the container.
    pub container_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum GuestCommand {
    Run {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        mounts: Vec<GuestMount>,
        /// Optional container name passed to `pelagos run --name`.
        #[serde(default)]
        name: Option<String>,
        /// Run detached; maps to `pelagos run --detach`.
        #[serde(default)]
        detach: bool,
        /// Labels KEY=VALUE forwarded to `pelagos run --label`.
        #[serde(default)]
        labels: Vec<String>,
        /// Port mappings HOST:CONTAINER forwarded to `pelagos run --publish`.
        #[serde(default)]
        publish: Vec<String>,
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        tty: bool,
        /// Optional container name passed to `pelagos run --name`.
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mounts: Vec<GuestMount>,
        #[serde(default)]
        labels: Vec<String>,
        /// Port mappings HOST:CONTAINER forwarded to `pelagos run --publish`.
        #[serde(default)]
        publish: Vec<String>,
    },
    /// Exec a command inside an already-running container by name.
    /// Enters the container's namespaces via setns(2) and execs the command.
    ExecInto {
        container: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        tty: bool,
        /// Working directory inside the container (Docker exec -w).
        #[serde(default)]
        workdir: Option<String>,
    },
    /// List containers; maps to `pelagos ps [--all]`.
    Ps {
        #[serde(default)]
        all: bool,
        /// Request JSON output; maps to `pelagos ps --format json`.
        #[serde(default)]
        json: bool,
    },
    /// Print container logs; maps to `pelagos logs [--follow] <name>`.
    Logs {
        name: String,
        #[serde(default)]
        follow: bool,
    },
    /// Inspect a container; maps to `pelagos container inspect <name>`.
    ContainerInspect {
        name: String,
    },
    /// Remove all exited containers by reading state files directly.
    ContainerPrune,
    /// Restart a stopped container; maps to `pelagos start <name>`.
    Start {
        name: String,
        /// Run interactively with a PTY (maps to `pelagos start -i`).
        #[serde(default)]
        interactive: bool,
        /// Override the command for this run only (maps to `pelagos start -- CMD`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cmd_override: Vec<String>,
    },
    /// Stop a running container; maps to `pelagos stop <name>`.
    Stop {
        name: String,
    },
    /// Stop then restart a container; maps to `pelagos restart <name>`.
    Restart {
        name: String,
        #[serde(default = "default_restart_time")]
        time: u64,
    },
    /// Remove a container; maps to `pelagos rm [--force] <name>`.
    Rm {
        name: String,
        #[serde(default)]
        force: bool,
    },
    /// Open a shell directly in the VM (no container, no namespaces).
    Shell {
        #[serde(default)]
        tty: bool,
    },
    Ping,
    /// Return the guest daemon's version string.
    Version,
    /// Build an OCI image from a Dockerfile-compatible Remfile.
    /// The build context is streamed as a gzipped tar immediately after the
    /// JSON command line (raw bytes, no framing; length given by context_size).
    Build {
        tag: String,
        #[serde(default = "default_dockerfile")]
        dockerfile: String,
        #[serde(default)]
        build_args: Vec<String>,
        #[serde(default)]
        no_cache: bool,
        context_size: u64,
    },
    /// Manage named volumes: sub is "create", "ls", or "rm".
    Volume {
        sub: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Manage named networks: sub is "create", "ls", "rm", or "inspect".
    /// All remaining args (name, --subnet, etc.) are forwarded verbatim.
    Network {
        sub: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Copy a path out of a running container.
    /// Response: GuestResponse::RawBytes { size } line, then `size` raw tar bytes, then Exit.
    CpFrom {
        container: String,
        /// Absolute path inside the container (file or directory).
        src: String,
    },
    /// Copy a tar payload into a running container.
    /// `data_size` raw tar bytes follow immediately after the JSON command line.
    CpTo {
        container: String,
        /// Destination directory inside the container.
        dst: String,
        data_size: u64,
    },
    /// Subscribe to container lifecycle events.
    ///
    /// The guest immediately sends a `GuestEvent::Snapshot`, then streams
    /// `GuestEvent::ContainerStarted` / `GuestEvent::ContainerExited` lines
    /// (NDJSON, one per line) until the client disconnects.  This connection
    /// is held open indefinitely — it does not produce a `GuestResponse`.
    Subscribe,
}

// ---------------------------------------------------------------------------
// Push event types (used by the Subscribe command)
// ---------------------------------------------------------------------------

/// A container state record read directly from `/run/pelagos/containers/`.
/// Intentionally flat so it serializes identically to `pelagos ps --json` output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContainerSnapshot {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub pid: i32,
    pub rootfs: String,
    pub started_at: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Port mappings from `pelagos run -p HOST:CONTAINER` (e.g. `["8080:80"]`).
    #[serde(default)]
    pub ports: Vec<String>,
}

/// Events streamed over a Subscribe connection (NDJSON, one per line).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestEvent {
    /// Initial snapshot sent immediately on subscribe.
    Snapshot {
        containers: Vec<ContainerSnapshot>,
        vm_running: bool,
    },
    /// A container transitioned to running.
    ContainerStarted { container: ContainerSnapshot },
    /// A container exited or was removed.
    ContainerExited {
        name: String,
        exit_code: Option<i32>,
    },
    /// Periodic liveness ping.  Clients that receive no message for > 15s
    /// should reconnect.  Sent every 5s when no other events are queued.
    Heartbeat { ts: u64 },
}

fn default_dockerfile() -> String {
    "Dockerfile".to_string()
}

fn default_restart_time() -> u64 {
    10
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestResponse {
    Stream {
        stream: StreamKind,
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
    /// Precedes a raw binary payload of `size` bytes written directly to the socket (no JSON framing).
    RawBytes {
        size: u64,
    },
    /// Version information for all components inside the VM.
    /// Fields are additive — omit unknown ones with skip_serializing_if so older
    /// mac clients ignore fields they don't recognise.
    /// Planned future field: `vm_image: Option<String>` for a version token baked
    /// into the disk image at build time (independent release cadence from
    /// pelagos-guest, kernel, and runtime).
    VersionInfo {
        /// pelagos-guest version.
        guest: String,
        /// pelagos runtime version, obtained by running `pelagos --version`.
        #[serde(skip_serializing_if = "Option::is_none")]
        runtime: Option<String>,
        /// VM kernel version (`uname -r`).
        #[serde(skip_serializing_if = "Option::is_none")]
        kernel: Option<String>,
    },
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::init();
    let listener = create_vsock_listener(VSOCK_PORT).expect("failed to create vsock listener");
    log::info!("pelagos-guest listening on vsock port {}", VSOCK_PORT);

    // Launch the container state poller.  Reads /run/pelagos/containers/ every
    // second — no pelagos CLI involved, so no global runtime lock contention.
    start_event_poller();

    loop {
        let conn_fd = match accept_vsock(&listener) {
            Ok(fd) => fd,
            Err(e) => {
                // EMFILE/ENFILE: fd table exhausted — back off so we don't
                // spin hot filling the log while waiting for fds to be freed.
                if let Some(os_err) = e.raw_os_error() {
                    if os_err == libc::EMFILE || os_err == libc::ENFILE {
                        log::warn!("accept: fd table exhausted ({}), backing off", e);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                }
                log::error!("accept failed: {}", e);
                continue;
            }
        };
        log::debug!("accepted connection");
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(conn_fd) {
                log::error!("connection handler error: {}", e);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

fn handle_connection(fd: libc::c_int) -> std::io::Result<()> {
    // RAII guard — closes fd on all exit paths (early returns, ? propagation).
    struct ConnFd(libc::c_int);
    impl Drop for ConnFd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let _guard = ConnFd(fd);

    // FdReader/FdWriter use libc::read/write directly — no OwnedFd involved.
    // Use a named BufReader (not an iterator) so it can be passed by value to
    // handle_build, which must read the raw tar body after the JSON header line.
    let mut reader = BufReader::new(FdReader(fd));
    let mut writer = FdWriter(fd);

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let cmd: GuestCommand = match serde_json::from_str(line) {
            Ok(c) => c,
            Err(e) => {
                send_response(
                    &mut writer,
                    &GuestResponse::Error {
                        error: format!("parse error: {}", e),
                    },
                )?;
                continue;
            }
        };
        match cmd {
            GuestCommand::Ping => {
                log::debug!("ping");
                send_response(&mut writer, &GuestResponse::Pong { pong: true })?;
                return Ok(());
            }
            GuestCommand::Version => {
                let guest = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH")).to_string();
                // Run `pelagos --version` to get the container runtime version.
                // Output is "pelagos <version>"; strip the binary-name prefix.
                let runtime = std::process::Command::new("pelagos")
                    .arg("--version")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| {
                        let s = s.trim();
                        s.strip_prefix("pelagos ").unwrap_or(s).to_string()
                    });
                let kernel = std::process::Command::new("uname")
                    .arg("-r")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string());
                log::debug!(
                    "version: guest={} runtime={:?} kernel={:?}",
                    guest,
                    runtime,
                    kernel
                );
                send_response(
                    &mut writer,
                    &GuestResponse::VersionInfo {
                        guest,
                        runtime,
                        kernel,
                    },
                )?;
                return Ok(());
            }
            GuestCommand::Run {
                image,
                args,
                env,
                mounts,
                name,
                detach,
                labels,
                publish,
            } => {
                run_container(
                    &mut writer,
                    &image,
                    &args,
                    &env,
                    &mounts,
                    name.as_deref(),
                    detach,
                    &labels,
                    &publish,
                )?;
            }
            GuestCommand::Exec {
                image,
                args,
                env,
                tty,
                name,
                mounts,
                labels,
                publish,
            } => {
                handle_exec(fd, &image, &args, &env, tty, name.as_deref(), &mounts, &labels, &publish)?;
                return Ok(());
            }
            GuestCommand::ExecInto {
                container,
                args,
                env,
                tty,
                workdir,
            } => {
                handle_exec_into(fd, &container, &args, &env, tty, workdir.as_deref())?;
                return Ok(());
            }
            GuestCommand::Ps { all, json } => {
                log::debug!("ps all={} json={}", all, json);
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("ps");
                if all {
                    cmd.arg("--all");
                }
                if json {
                    cmd.arg("--format").arg("json");
                }
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::Logs { name, follow } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("logs").arg(&name);
                if follow {
                    cmd.arg("--follow");
                }
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::ContainerInspect { name } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("container").arg("inspect").arg(&name);
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::ContainerPrune => {
                // Read container state files directly — no subprocess for enumeration.
                // Containers run as root; state lives in /run/pelagos/containers/.
                let containers_dir = std::path::PathBuf::from("/run/pelagos/containers");
                let mut removed = 0usize;
                if let Ok(entries) = std::fs::read_dir(&containers_dir) {
                    for entry in entries.flatten() {
                        let state_path = entry.path().join("state.json");
                        let Ok(data) = std::fs::read_to_string(&state_path) else {
                            continue;
                        };
                        let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) else {
                            continue;
                        };
                        if val.get("status").and_then(|s| s.as_str()) != Some("exited") {
                            continue;
                        }
                        let raw = entry.file_name();
                        let name = raw.to_string_lossy();
                        let out = Command::new(pelagos_bin())
                            .arg("rm")
                            .arg(name.as_ref())
                            .output();
                        match out {
                            Ok(o) if o.status.success() => {
                                send_response(
                                    &mut writer,
                                    &GuestResponse::Stream {
                                        stream: StreamKind::Stdout,
                                        data: format!("{}\n", name),
                                    },
                                )?;
                                removed += 1;
                            }
                            Ok(o) => {
                                let msg = String::from_utf8_lossy(&o.stderr);
                                send_response(
                                    &mut writer,
                                    &GuestResponse::Stream {
                                        stream: StreamKind::Stderr,
                                        data: format!("error removing {}: {}\n", name, msg.trim()),
                                    },
                                )?;
                            }
                            Err(e) => {
                                send_response(
                                    &mut writer,
                                    &GuestResponse::Stream {
                                        stream: StreamKind::Stderr,
                                        data: format!("error removing {}: {}\n", name, e),
                                    },
                                )?;
                            }
                        }
                    }
                }
                send_response(
                    &mut writer,
                    &GuestResponse::Stream {
                        stream: StreamKind::Stdout,
                        data: format!("{} container(s) removed\n", removed),
                    },
                )?;
                send_response(&mut writer, &GuestResponse::Exit { exit: 0 })?;
            }
            GuestCommand::Start {
                name,
                interactive,
                cmd_override,
            } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("start");
                if interactive {
                    cmd.arg("-i");
                }
                cmd.arg(&name);
                if !cmd_override.is_empty() {
                    cmd.arg("--");
                    cmd.args(&cmd_override);
                }
                if interactive {
                    log::info!("start: container={} interactive=true", name);
                    {
                        let mut w = FdWriter(fd);
                        send_response(&mut w, &GuestResponse::Ready { ready: true })?;
                    }
                    handle_exec_tty(fd, cmd)?;
                    return Ok(());
                } else {
                    spawn_and_stream(&mut writer, cmd)?;
                }
            }
            GuestCommand::Stop { name } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("stop").arg(&name);
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::Restart { name, time } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("restart")
                    .arg("--time")
                    .arg(time.to_string())
                    .arg(&name);
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::Rm { name, force } => {
                let mut cmd = Command::new(pelagos_bin());
                cmd.arg("rm").arg(&name);
                if force {
                    cmd.arg("--force");
                }
                spawn_and_stream(&mut writer, cmd)?;
            }
            GuestCommand::Shell { tty } => {
                // Send ready ack — switches the connection to framed binary protocol.
                send_response(&mut writer, &GuestResponse::Ready { ready: true })?;
                let mut cmd = Command::new("/bin/sh");
                // Set a sane PATH so busybox applet symlinks are findable.
                // pelagos-guest may not inherit PATH from the init script.
                cmd.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                if tty {
                    handle_exec_tty(fd, cmd)?;
                } else {
                    handle_exec_piped(fd, cmd)?;
                }
                return Ok(());
            }
            GuestCommand::Build {
                tag,
                dockerfile,
                build_args,
                no_cache,
                context_size,
            } => {
                handle_build(
                    &mut writer,
                    reader,
                    &tag,
                    &dockerfile,
                    &build_args,
                    no_cache,
                    context_size,
                )?;
                return Ok(());
            }
            GuestCommand::Volume { sub, name } => {
                handle_volume(&mut writer, &sub, name.as_deref())?;
            }
            GuestCommand::Network { sub, args } => {
                handle_network(&mut writer, &sub, &args)?;
            }
            GuestCommand::CpFrom { container, src } => {
                handle_cp_from(&mut writer, &container, &src)?;
                return Ok(());
            }
            GuestCommand::CpTo {
                container,
                dst,
                data_size,
            } => {
                handle_cp_to(&mut writer, reader, &container, &dst, data_size)?;
                return Ok(());
            }
            GuestCommand::Subscribe => {
                // Holds connection open; does not participate in the read loop.
                handle_subscribe(&mut writer)?;
                return Ok(());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pelagos binary path
// ---------------------------------------------------------------------------

fn pelagos_bin() -> String {
    std::env::var("PELAGOS_BIN").unwrap_or_else(|_| "/usr/local/bin/pelagos".into())
}

// ---------------------------------------------------------------------------
// Pull helper (shared by run_container and handle_exec)
// ---------------------------------------------------------------------------

/// Pull the image, streaming stderr lines back via the provided writer.
/// Returns Ok(true) on success, Ok(false) on failure (error response sent).
/// Return true if the image is already present in the local pelagos image store.
///
/// pelagos stores images at `<data_dir>/images/<dirname>/manifest.json` where
/// dirname is the reference with ':', '/', '@' replaced by '_'.  If that file
/// exists the image is fully cached and no network pull is needed.
fn image_cached_locally(image: &str) -> bool {
    // Normalize the reference the same way pelagos does before storing:
    // add ":latest" if no tag or digest, so the dirname matches what pelagos
    // wrote (e.g. "public.ecr.aws/docker/library/alpine" →
    // "public.ecr.aws_docker_library_alpine_latest").
    let normalized = if !image.contains(':') && !image.contains('@') {
        format!("{}:latest", image)
    } else {
        image.to_string()
    };
    let dirname: String = normalized
        .chars()
        .map(|c| if matches!(c, ':' | '/' | '@') { '_' } else { c })
        .collect();
    std::path::Path::new("/var/lib/pelagos/images")
        .join(&dirname)
        .join("manifest.json")
        .exists()
}

/// Substitute `$VAR` and `${VAR}` in `text` using the provided map.
/// Unknown variables are left as empty string (matches Docker behaviour).
fn substitute_build_args(text: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        if bytes[i] == b'$' {
            if i + 1 < len && bytes[i + 1] == b'{' {
                if let Some(close) = text[i + 2..].find('}') {
                    let name = &text[i + 2..i + 2 + close];
                    if let Some(val) = vars.get(name) {
                        out.push_str(val);
                    }
                    i = i + 2 + close + 1;
                } else {
                    out.push('$');
                    i += 1;
                }
            } else if i + 1 < len && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
            {
                let start = i + 1;
                let mut end = start;
                while end < len && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                    end += 1;
                }
                let name = &text[start..end];
                if let Some(val) = vars.get(name) {
                    out.push_str(val);
                }
                i = end;
            } else {
                out.push('$');
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn pull_image(writer: &mut impl Write, image: &str) -> std::io::Result<bool> {
    // Skip the registry round-trip entirely when the image is already cached.
    // pelagos image pull always checks the remote manifest even for cached
    // images, which burns through ECR unauthenticated rate limits quickly.
    if image_cached_locally(image) {
        return Ok(true);
    }

    let pelagos = pelagos_bin();

    const PULL_ATTEMPTS: u32 = 10;
    let mut pull_error = String::new();
    for attempt in 1..=PULL_ATTEMPTS {
        let mut pull = match Command::new(&pelagos)
            .arg("image")
            .arg("pull")
            .arg(image)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                send_response(
                    writer,
                    &GuestResponse::Error {
                        error: format!("image pull spawn failed: {}", e),
                    },
                )?;
                return Ok(false);
            }
        };
        let pull_stderr = pull.stderr.take().unwrap();
        let pull_stdout = pull.stdout.take().unwrap();
        for l in BufReader::new(pull_stderr)
            .lines()
            .chain(BufReader::new(pull_stdout).lines())
            .flatten()
        {
            send_response(
                writer,
                &GuestResponse::Stream {
                    stream: StreamKind::Stderr,
                    data: l + "\n",
                },
            )?;
        }
        let pull_status = pull.wait()?;
        if pull_status.success() {
            return Ok(true);
        }
        pull_error = format!(
            "image pull failed (exit {})",
            pull_status.code().unwrap_or(-1)
        );
        if attempt < PULL_ATTEMPTS {
            log::warn!(
                "pull attempt {}/{} failed, retrying in 2s: {}",
                attempt,
                PULL_ATTEMPTS,
                pull_error
            );
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    send_response(writer, &GuestResponse::Error { error: pull_error })?;
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn run_container(
    writer: &mut impl Write,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    mounts: &[GuestMount],
    name: Option<&str>,
    detach: bool,
    labels: &[String],
    publish: &[String],
) -> std::io::Result<()> {
    let pelagos = pelagos_bin();

    if !pull_image(writer, image)? {
        return Ok(());
    }

    // Ensure each virtiofs mountpoint exists inside the guest before invoking
    // pelagos run.  The init script already ran `mount -t virtiofs <tag>
    // /mnt/<tag>` for each tag passed on the kernel cmdline.
    for mount in mounts {
        let guest_mnt = format!("/mnt/{}", mount.tag);
        // The virtiofs directory should already be mounted at /mnt/<tag> by
        // the init script.  If the mount point is absent something went wrong,
        // but we proceed and let pelagos report the error rather than silently
        // dropping the mount.
        log::debug!("virtiofs share: {} → {}", guest_mnt, mount.container_path);
    }

    let mut cmd = Command::new(&pelagos);
    cmd.arg("run");
    if let Some(n) = name {
        cmd.arg("--name").arg(n);
    }
    if detach {
        cmd.arg("--detach");
    }
    // Pass each virtiofs guest-side path as a -v bind mount to pelagos run.
    // If a subpath is specified, the bind source is a subdirectory of the share
    // (used when $HOME is shared as share0 and individual project dirs are subpaths).
    for mount in mounts {
        let guest_src = if mount.subpath.is_empty() {
            format!("/mnt/{}", mount.tag)
        } else {
            format!(
                "/mnt/{}/{}",
                mount.tag,
                mount.subpath.trim_start_matches('/')
            )
        };
        cmd.arg("-v")
            .arg(format!("{}:{}", guest_src, mount.container_path));
    }
    for label in labels {
        cmd.arg("--label").arg(label);
    }
    for port_spec in publish {
        cmd.arg("--publish").arg(port_spec);
    }
    cmd.arg(image);
    if !args.is_empty() {
        cmd.args(args);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    if detach {
        run_detached(writer, cmd)
    } else {
        spawn_and_stream(writer, cmd)
    }
}

// ---------------------------------------------------------------------------
// spawn_and_stream — generic helper for non-interactive pelagos subcommands
// ---------------------------------------------------------------------------

/// Spawn `cmd` with piped stdout/stderr, relay both streams as JSON
/// `GuestResponse::Stream` messages, then send a final `GuestResponse::Exit`.
fn spawn_and_stream(writer: &mut impl Write, mut cmd: Command) -> std::io::Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: e.to_string(),
                },
            )?;
            return Ok(());
        }
    };

    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    use std::sync::mpsc;
    #[derive(Debug)]
    enum Chunk {
        Out(String),
        Err(String),
        Done,
    }
    let (tx, rx) = mpsc::channel::<Chunk>();

    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout_pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    let _ = tx_out.send(Chunk::Out(l + "\n"));
                }
                Err(_) => break,
            }
        }
        let _ = tx_out.send(Chunk::Done);
    });

    let tx_err = tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr_pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    let _ = tx_err.send(Chunk::Err(l + "\n"));
                }
                Err(_) => break,
            }
        }
        let _ = tx_err.send(Chunk::Done);
    });

    let mut done_count = 0;
    while done_count < 2 {
        match rx.recv() {
            Ok(Chunk::Out(data)) => {
                send_response(
                    writer,
                    &GuestResponse::Stream {
                        stream: StreamKind::Stdout,
                        data,
                    },
                )?;
            }
            Ok(Chunk::Err(data)) => {
                send_response(
                    writer,
                    &GuestResponse::Stream {
                        stream: StreamKind::Stderr,
                        data,
                    },
                )?;
            }
            Ok(Chunk::Done) => done_count += 1,
            Err(_) => break,
        }
    }

    let status = child.wait()?;
    let code = status.code().unwrap_or(-1);
    send_response(writer, &GuestResponse::Exit { exit: code })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run_detached — reads container name then drops stdout pipe immediately
// ---------------------------------------------------------------------------

/// Run pelagos with `--detach`, read the one-line container name from stdout,
/// then drop the pipe so the watcher child (which holds the write end) does not
/// block this function.  Sends `GuestResponse::Stream` with the name followed
/// by `GuestResponse::Exit`.
fn run_detached(writer: &mut impl Write, mut cmd: Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: e.to_string(),
                },
            )?;
            return Ok(());
        }
    };

    // Read exactly one line (the container name) and immediately drop the pipe.
    // The watcher child keeps the write end open for the container's lifetime;
    // dropping our read end here prevents blocking on the watcher.
    let name_line = {
        let stdout_pipe = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout_pipe);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let trimmed = line
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();
        trimmed
        // stdout_pipe (via reader) is dropped here — write end stays open in watcher
    };

    // The pelagos parent exits after printing the name; wait for it (fast).
    let status = child.wait()?;
    let code = status.code().unwrap_or(-1);

    if !name_line.is_empty() {
        send_response(
            writer,
            &GuestResponse::Stream {
                stream: StreamKind::Stdout,
                data: name_line + "\n",
            },
        )?;
    }
    send_response(writer, &GuestResponse::Exit { exit: code })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Build handler
// ---------------------------------------------------------------------------

/// Receive a gzipped tar build context over vsock, extract it, and run
/// `pelagos build`.  The raw tar bytes follow immediately after the JSON
/// command line; their length is given by `context_size`.
fn handle_build(
    writer: &mut impl Write,
    mut reader: BufReader<FdReader>,
    tag: &str,
    dockerfile: &str,
    build_args: &[String],
    no_cache: bool,
    context_size: u64,
) -> std::io::Result<()> {
    let build_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let ctx_dir = format!("/tmp/pelagos-build-{}", build_id);
    std::fs::create_dir_all(&ctx_dir)?;

    // Pipe exactly context_size bytes from the vsock reader into `tar xzf -`.
    {
        let mut tar_proc = match Command::new("tar")
            .arg("xzf")
            .arg("-")
            .arg("-C")
            .arg(&ctx_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&ctx_dir);
                send_response(
                    writer,
                    &GuestResponse::Error {
                        error: format!("tar spawn failed: {}", e),
                    },
                )?;
                return Ok(());
            }
        };
        let tar_stdin = tar_proc.stdin.take().unwrap();
        let tar_stderr = tar_proc.stderr.take().unwrap();

        // Drain tar stderr concurrently so the pipe never fills.
        let stderr_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(tar_stderr).read_to_string(&mut s);
            s
        });

        let copy_result = {
            let mut sink = tar_stdin;
            let mut limited = (&mut reader).take(context_size);
            std::io::copy(&mut limited, &mut sink)
        }; // sink dropped here → EOF to tar

        let tar_status = tar_proc.wait()?;
        let tar_stderr_str = stderr_thread.join().unwrap_or_default();

        if copy_result.is_err() || !tar_status.success() {
            let _ = std::fs::remove_dir_all(&ctx_dir);
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!(
                        "build context extract failed (exit {}): {}",
                        tar_status.code().unwrap_or(-1),
                        tar_stderr_str.trim()
                    ),
                },
            )?;
            return Ok(());
        }
    }

    // Pre-pull all distinct registry base images declared in FROM lines.
    //
    // Multi-stage builds have multiple FROM lines. Each stage either names a
    // registry image (must be pulled) or references an earlier stage by alias
    // (already in the local store — no pull needed).  We track stage aliases as
    // we scan so we can skip them.
    //
    // Build-args (--build-arg KEY=VALUE) are substituted into image references
    // before pulling, since devcontainer and other tooling use patterns like
    // `FROM $_DEV_CONTAINERS_BASE_IMAGE` with the actual image passed via
    // --build-arg.
    let dockerfile_path = format!("{}/{}", ctx_dir, dockerfile);
    if let Ok(content) = std::fs::read_to_string(&dockerfile_path) {
        // Parse --build-arg KEY=VALUE pairs and ARG KEY=DEFAULT declarations
        // into a substitution map, with build-args taking precedence.
        let mut arg_defaults: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut build_arg_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for a in build_args {
            if let Some((k, v)) = a.split_once('=') {
                build_arg_map.insert(k.to_string(), v.to_string());
            }
        }
        // First pass: collect ARG defaults from the Dockerfile.
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.to_ascii_uppercase().starts_with("ARG") {
                let rest = trimmed[3..].trim();
                if let Some((k, v)) = rest.split_once('=') {
                    arg_defaults
                        .entry(k.trim().to_string())
                        .or_insert_with(|| v.trim().to_string());
                }
            }
        }
        // Merge: build-args override defaults.
        let mut sub_vars = arg_defaults;
        sub_vars.extend(build_arg_map);

        let mut stage_aliases: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut pulled: std::collections::HashSet<String> = std::collections::HashSet::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.to_ascii_uppercase().starts_with("FROM") {
                continue;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let raw_base = parts.get(1).copied().unwrap_or("");
            // Record "FROM <image> AS <alias>" aliases so subsequent stages that
            // reference them are not mistaken for registry images.
            if parts.len() >= 4 && parts[2].eq_ignore_ascii_case("AS") {
                stage_aliases.insert(parts[3].to_ascii_lowercase());
            }
            // Apply $VAR / ${VAR} substitution using build-args and ARG defaults.
            let base = substitute_build_args(raw_base, &sub_vars);
            // Skip scratch, already-pulled images, stage alias references, and
            // any reference that still contains an unresolved variable (starts
            // with '$' after substitution — indicates a missing build-arg that
            // pelagos build itself will handle or fail on clearly).
            if base.is_empty()
                || base.eq_ignore_ascii_case("scratch")
                || base.starts_with('$')
                || pulled.contains(&base.to_ascii_lowercase())
                || stage_aliases.contains(&base.to_ascii_lowercase())
            {
                continue;
            }
            if !pull_image(writer, &base)? {
                let _ = std::fs::remove_dir_all(&ctx_dir);
                return Ok(());
            }
            pulled.insert(base.to_ascii_lowercase());
        }
    }

    // Run pelagos build.
    // Use --network pasta: pasta is a userspace TCP/UDP proxy that works without
    // bridge/veth kernel modules. Falls back gracefully when RUN steps don't need
    // network. The default "auto" mode would pick "bridge" (we run as root) which
    // requires kernel bridge support we don't have in the virt kernel.
    let mut cmd = Command::new(pelagos_bin());
    cmd.arg("build")
        .arg("-t")
        .arg(tag)
        .arg("-f")
        .arg(&dockerfile_path)
        .arg("--network")
        .arg("pasta");
    for arg in build_args {
        cmd.arg("--build-arg").arg(arg);
    }
    if no_cache {
        cmd.arg("--no-cache");
    }
    cmd.arg(&ctx_dir);

    let result = spawn_and_stream(writer, cmd);
    let _ = std::fs::remove_dir_all(&ctx_dir);
    result
}

// ---------------------------------------------------------------------------
// Volume and network passthrough handlers
// ---------------------------------------------------------------------------

fn handle_volume(writer: &mut impl Write, sub: &str, name: Option<&str>) -> std::io::Result<()> {
    let mut cmd = Command::new(pelagos_bin());
    cmd.arg("volume").arg(sub);
    if let Some(n) = name {
        cmd.arg(n);
    }
    spawn_and_stream(writer, cmd)
}

fn handle_network(writer: &mut impl Write, sub: &str, args: &[String]) -> std::io::Result<()> {
    let mut cmd = Command::new(pelagos_bin());
    cmd.arg("network").arg(sub).args(args);
    spawn_and_stream(writer, cmd)
}

// ---------------------------------------------------------------------------
// docker cp handlers
// ---------------------------------------------------------------------------

/// Copy a path out of a running container by entering its namespaces directly.
/// Runs `tar -cC <parent> <name>` inside the container namespace, captures raw
/// tar bytes, sends a RawBytes header, writes the bytes, then sends Exit.
fn handle_cp_from(writer: &mut impl Write, container: &str, src: &str) -> std::io::Result<()> {
    use std::path::Path;

    let pid = match get_container_pid(container) {
        Ok(p) => p,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: {}", e),
                },
            )?;
            return Ok(());
        }
    };
    let ns_fds = match open_ns_fds(pid) {
        Ok(f) => f,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: open ns fds: {}", e),
                },
            )?;
            return Ok(());
        }
    };
    let root_fd = match open_root_fd(pid) {
        Ok(f) => f,
        Err(e) => {
            for &nfd in &ns_fds {
                unsafe { libc::close(nfd) };
            }
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: open root fd: {}", e),
                },
            )?;
            return Ok(());
        }
    };

    let src_path = Path::new(src);
    let parent = src_path
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("/");
    let name = src_path.file_name().and_then(|n| n.to_str()).unwrap_or(".");

    let mut cmd = Command::new("tar");
    cmd.arg("-cC").arg(parent).arg(name);
    // Enter container namespaces and anchor root dentry to container rootfs.
    unsafe {
        cmd.pre_exec(move || {
            for &ns_fd in &ns_fds {
                if call_setns(ns_fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ns_fd);
            }
            if libc::fchdir(root_fd) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chroot(c".".as_ptr()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(c"/".as_ptr()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(root_fd);
            Ok(())
        });
    }

    let output = cmd.output();
    // Close parent's copies of ns_fds and root_fd.
    for &ns_fd in &ns_fds {
        unsafe { libc::close(ns_fd) };
    }
    unsafe { libc::close(root_fd) };

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: e.to_string(),
                },
            )?;
            return Ok(());
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        send_response(
            writer,
            &GuestResponse::Error {
                error: format!(
                    "cp: tar failed (exit {}): {}",
                    output.status.code().unwrap_or(-1),
                    stderr.trim()
                ),
            },
        )?;
        return Ok(());
    }

    let size = output.stdout.len() as u64;
    send_response(writer, &GuestResponse::RawBytes { size })?;
    writer.write_all(&output.stdout)?;
    send_response(writer, &GuestResponse::Exit { exit: 0 })?;
    Ok(())
}

/// Copy a tar payload into a running container by entering its namespaces directly.
/// Reads `data_size` raw bytes from `reader`, pipes them to `tar -xC <dst>`
/// running inside the container namespace.
fn handle_cp_to(
    writer: &mut impl Write,
    mut reader: BufReader<FdReader>,
    container: &str,
    dst: &str,
    data_size: u64,
) -> std::io::Result<()> {
    let pid = match get_container_pid(container) {
        Ok(p) => p,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: {}", e),
                },
            )?;
            return Ok(());
        }
    };
    let ns_fds = match open_ns_fds(pid) {
        Ok(f) => f,
        Err(e) => {
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: open ns fds: {}", e),
                },
            )?;
            return Ok(());
        }
    };
    let root_fd = match open_root_fd(pid) {
        Ok(f) => f,
        Err(e) => {
            for &nfd in &ns_fds {
                unsafe { libc::close(nfd) };
            }
            send_response(
                writer,
                &GuestResponse::Error {
                    error: format!("cp: open root fd: {}", e),
                },
            )?;
            return Ok(());
        }
    };

    let mut cmd = Command::new("tar");
    cmd.arg("-xC")
        .arg(dst)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());
    // Enter container namespaces and anchor root dentry to container rootfs.
    unsafe {
        cmd.pre_exec(move || {
            for &ns_fd in &ns_fds {
                if call_setns(ns_fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ns_fd);
            }
            if libc::fchdir(root_fd) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chroot(c".".as_ptr()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(c"/".as_ptr()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(root_fd);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            for &ns_fd in &ns_fds {
                unsafe { libc::close(ns_fd) };
            }
            unsafe { libc::close(root_fd) };
            send_response(
                writer,
                &GuestResponse::Error {
                    error: e.to_string(),
                },
            )?;
            return Ok(());
        }
    };
    // Close parent's copies of ns_fds and root_fd.
    for &ns_fd in &ns_fds {
        unsafe { libc::close(ns_fd) };
    }
    unsafe { libc::close(root_fd) };

    let mut tar_stdin = child.stdin.take().unwrap();
    let copy_result = {
        let mut limited = (&mut reader).take(data_size);
        std::io::copy(&mut limited, &mut tar_stdin)
    };
    drop(tar_stdin); // EOF to tar

    if let Err(e) = copy_result {
        let _ = child.wait();
        send_response(
            writer,
            &GuestResponse::Error {
                error: e.to_string(),
            },
        )?;
        return Ok(());
    }

    let stderr_pipe = child.stderr.take().unwrap();
    let stderr_str = {
        let mut s = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut s);
        s
    };
    let status = child.wait()?;
    let code = status.code().unwrap_or(-1);
    if !stderr_str.is_empty() {
        send_response(
            writer,
            &GuestResponse::Stream {
                stream: StreamKind::Stderr,
                data: stderr_str,
            },
        )?;
    }
    send_response(writer, &GuestResponse::Exit { exit: code })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Exec handler
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn handle_exec(
    fd: libc::c_int,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    tty: bool,
    name: Option<&str>,
    mounts: &[GuestMount],
    labels: &[String],
    publish: &[String],
) -> std::io::Result<()> {
    let pelagos = pelagos_bin();

    // Pull the image before sending ready; relay stderr as JSON stream so the
    // host can display pull progress while waiting.
    {
        let mut tmp_writer = FdWriter(fd);
        if !pull_image(&mut tmp_writer, image)? {
            return Ok(());
        }
    }

    // Send ready ack — both sides now switch to framed binary protocol.
    {
        let mut tmp_writer = FdWriter(fd);
        send_response(&mut tmp_writer, &GuestResponse::Ready { ready: true })?;
    }

    let mut cmd = Command::new(&pelagos);
    cmd.arg("run");
    // --interactive tells pelagos to allocate a PTY for the container process
    // and relay raw bytes through its own stdin/stdout.  We then use the piped
    // (non-PTY-wrapper) path here because pelagos manages the PTY internally —
    // adding another PTY layer on top would double-process terminal escapes.
    if tty {
        cmd.arg("--interactive");
    }
    if let Some(n) = name {
        cmd.arg("--name").arg(n);
    }
    for label in labels {
        cmd.arg("--label").arg(label);
    }
    for port in publish {
        cmd.arg("--publish").arg(port);
    }
    for m in mounts {
        let host_dir = if m.subpath.is_empty() {
            format!("/mnt/{}", m.tag)
        } else {
            format!("/mnt/{}/{}", m.tag, m.subpath)
        };
        cmd.arg("--volume").arg(format!("{}:{}", host_dir, m.container_path));
    }
    cmd.arg(image);
    if !args.is_empty() {
        cmd.args(args);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    // Always use the piped path: for tty=true, pelagos run --interactive owns
    // the PTY; for tty=false, plain pipes are correct.
    handle_exec_piped(fd, cmd)
}

/// Exec into a running container by joining all its Linux namespaces natively.
///
/// Uses a double-fork to correctly place the exec'd process inside the container's
/// PID namespace without depending on the external `nsenter` binary:
///
/// - **Parent**: opens namespace fds and root fd, prepares execve arguments, forks.
/// - **Intermediate** (child): joins net/uts/ipc/mnt namespaces, chroots, calls
///   `setns(CLONE_NEWPID)` to update `pid_for_children`, then forks again.
/// - **Grandchild**: born inside the PID namespace (gets a namespace-local PID),
///   dup2s I/O fds, and execs the target program.
/// - **Intermediate continues**: waits for grandchild, writes 4-byte exit code to
///   status pipe, then `_exit(0)`.
/// - **Parent**: relays I/O via frames, reads exit code from status pipe, sends
///   `FRAME_EXIT`, then reaps the intermediate.
///
/// Why double-fork: `setns(CLONE_NEWPID)` only updates `pid_for_children`; the
/// calling process retains its old namespace PID.  Only the immediately subsequent
/// `fork()` child gets a namespace-local PID.  Without it, `/proc/self` is a
/// dangling symlink in the container's procfs, which breaks VS Code
/// `resolveAuthority` and any other tool that reads `/proc/self`.
fn handle_exec_into(
    fd: libc::c_int,
    container: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    tty: bool,
    workdir: Option<&str>,
) -> std::io::Result<()> {
    log::info!("exec: container={} tty={} args={:?}", container, tty, args);
    if args.is_empty() {
        let mut w = FdWriter(fd);
        let _ = send_response(
            &mut w,
            &GuestResponse::Error {
                error: "exec: no command specified".into(),
            },
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "exec: no command specified",
        ));
    }

    // Send ready — both sides switch to framed binary protocol.
    {
        let mut w = FdWriter(fd);
        send_response(&mut w, &GuestResponse::Ready { ready: true })?;
    }

    // Delegate to `pelagos exec` which correctly enters all container
    // namespaces (net, uts, ipc, mnt, pid) as of pelagos v0.58.0.
    let pelagos = pelagos_bin();
    let mut cmd = Command::new(&pelagos);
    cmd.arg("exec");
    if tty {
        cmd.arg("--interactive");
    }
    if let Some(w) = workdir {
        cmd.arg("--workdir").arg(w);
    }
    for (k, v) in env {
        cmd.arg("--env").arg(format!("{}={}", k, v));
    }
    cmd.arg(container);
    cmd.args(args);

    if tty {
        handle_exec_tty(fd, cmd)
    } else {
        handle_exec_piped(fd, cmd)
    }
}

/// Parse `pelagos ps --all` output and return the PID of the named container.
///
/// The PID column can be `-` when the container was created but the process has
/// not yet started (or failed to start).  Treat `-` as "not running" rather
/// than returning a parse error.
fn get_container_pid(container: &str) -> std::io::Result<u32> {
    let out = Command::new(pelagos_bin()).args(["ps", "--all"]).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Output format: NAME  STATUS  PID  ROOTFS  COMMAND  HEALTH  STARTED
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 3 && cols[0] == container {
            let pid_str = cols[2];
            if pid_str == "-" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "container '{}' has no running process (PID is '-')",
                        container
                    ),
                ));
            }
            return pid_str
                .parse::<u32>()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("container '{}' not found or not running", container),
    ))
}

/// Thin wrapper around setns(2) that compiles on all platforms.
/// pelagos-guest only ever runs on Linux; the non-Linux branch is unreachable
/// but must exist so `cargo check` / `cargo fmt` work on the macOS host.
/// async-signal-safe: safe to call in pre_exec.
#[cfg(target_os = "linux")]
unsafe fn call_setns(fd: libc::c_int) -> libc::c_int {
    libc::setns(fd, 0)
}
#[cfg(not(target_os = "linux"))]
unsafe fn call_setns(_fd: libc::c_int) -> libc::c_int {
    panic!("setns is Linux-only; pelagos-guest only runs inside the Linux VM")
}

/// Resolve the PID that actually called `pivot_root` for the container.
///
/// `pelagos ps` returns `state.pid = P`, the intermediate process spawned by
/// pelagos.  When a PID namespace is active, P never calls `pivot_root` — that
/// is done by C, P's only child (PID 1 inside the container).  P's
/// `/proc/P/root` therefore points to the Alpine (host) root, not the
/// container's overlay.
///
/// If P has exactly one child, that child is C.  Otherwise (no PID namespace,
/// or the container has forked additional children) P itself is the container
/// process and its `/proc/P/root` is correct.  Same logic as pelagos's own
/// `find_root_pid()` in `src/cli/exec.rs`.
fn find_root_pid(pid: u32) -> u32 {
    let path = format!("/proc/{}/task/{}/children", pid, pid);
    if let Ok(content) = std::fs::read_to_string(&path) {
        let children: Vec<u32> = content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if children.len() == 1 {
            return children[0];
        }
    }
    pid
}

/// Open the container's root directory as an fd via `/proc/<root_pid>/root`.
///
/// `setns(CLONE_NEWNS)` changes the mount namespace but does NOT update the
/// calling process's root dentry — absolute paths still resolve through the
/// old (Alpine) root.  Opening this fd BEFORE fork and then doing
/// `fchdir(root_fd); chroot(".")` AFTER all setns calls in pre_exec is the
/// correct pattern for entering the container's rootfs (same approach as
/// pelagos's own exec.rs / nsenter(1)).
///
/// Uses `find_root_pid` to resolve P → C when a PID namespace is active, so
/// that we open the root of the process that actually called `pivot_root`.
///
/// Must be opened in the parent (before fork) while `/proc/<pid>/root` is
/// still accessible.  No O_CLOEXEC: fd must survive into pre_exec.
fn open_root_fd(pid: u32) -> std::io::Result<libc::c_int> {
    let root_pid = find_root_pid(pid);
    let path = format!("/proc/{}/root", root_pid);
    let cpath = std::ffi::CString::new(path.as_str())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // O_CLOEXEC is safe here: pre_exec runs after fork() but before exec(), so
    // the fd is still accessible in pre_exec.  O_CLOEXEC only closes it at exec()
    // time, which prevents it leaking into unrelated child processes spawned by
    // pelagos-guest on other threads — without this, every subprocess inherits
    // all open namespace fds, exhausting the fd table (EMFILE).
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn open_ns_fds(pid: u32) -> std::io::Result<[libc::c_int; 5]> {
    let ns_names = ["net", "uts", "ipc", "pid", "mnt"];
    let mut fds = [-1i32; 5];
    for (i, ns) in ns_names.iter().enumerate() {
        let path = format!("/proc/{}/ns/{}", pid, ns);
        let cpath = std::ffi::CString::new(path.as_str())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // O_CLOEXEC is safe here: pre_exec runs after fork() but before exec(),
        // so the fd is accessible in pre_exec.  It only closes at exec() time,
        // preventing leakage into unrelated child processes (which would exhaust
        // the fd table and cause EMFILE).
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            for fd in fds.iter().take(i) {
                unsafe { libc::close(*fd) };
            }
            return Err(std::io::Error::last_os_error());
        }
        fds[i] = fd;
    }
    Ok(fds)
}

/// Non-TTY exec: spawn with piped stdin/stdout/stderr, forward via frames.
fn handle_exec_piped(fd: libc::c_int, mut cmd: Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut w = FdWriter(fd);
            let err = format!("exec spawn failed: {}", e);
            let _ = send_frame(&mut w, FRAME_STDERR, err.as_bytes());
            let _ = send_frame(&mut w, FRAME_EXIT, &1i32.to_be_bytes());
            return Ok(());
        }
    };

    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    use std::sync::{Arc, Mutex};

    // Shared writer protected by a mutex so stdin-reader and stdout/stderr threads
    // can all write frames concurrently.
    let writer = Arc::new(Mutex::new(FdWriter(fd)));

    // Stdin thread: read FRAME_STDIN frames from vsock, write to child stdin.
    let w_stdin = Arc::clone(&writer);
    let stdin_thread = std::thread::spawn(move || {
        let mut child_stdin = child_stdin;
        let mut reader = FdReader(fd);
        loop {
            match recv_frame(&mut reader) {
                Ok((FRAME_STDIN, data)) => {
                    if data.is_empty() {
                        break; // zero-length = EOF signal; drop child_stdin below
                    }
                    if child_stdin.write_all(&data).is_err() {
                        break;
                    }
                }
                Ok((FRAME_RESIZE, _)) => {
                    // No PTY in piped mode; ignore resize frames.
                }
                Ok(_) | Err(_) => break,
            }
        }
        drop(child_stdin); // signal EOF to child
        drop(w_stdin); // keep Arc alive until we're done
    });

    // Stdout thread: read child stdout, send as FRAME_STDOUT.
    let w_out = Arc::clone(&writer);
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut src = child_stdout;
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut w = w_out.lock().unwrap();
                    if send_frame(&mut *w, FRAME_STDOUT, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Stderr thread: read child stderr, send as FRAME_STDERR.
    let w_err = Arc::clone(&writer);
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut src = child_stderr;
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut w = w_err.lock().unwrap();
                    if send_frame(&mut *w, FRAME_STDERR, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Wait for stdout/stderr to drain, then collect exit code.
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    // Send exit frame.
    let mut w = writer.lock().unwrap();
    let _ = send_frame(&mut *w, FRAME_EXIT, &code.to_be_bytes());

    // The stdin thread may be blocked on recv_frame; drop the writer lock so
    // it can finish, then just let it be — the fd will be closed on return.
    drop(w);
    drop(stdin_thread); // detach; fd close will unblock it

    Ok(())
}

/// TTY exec: allocate a pseudo-TTY, spawn child with it, forward via frames.
fn handle_exec_tty(fd: libc::c_int, mut cmd: Command) -> std::io::Result<()> {
    use std::os::unix::io::FromRawFd;

    // Open a pseudo-TTY.
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let ret = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret < 0 {
        let mut w = FdWriter(fd);
        let err = format!("openpty failed: {}", std::io::Error::last_os_error());
        let _ = send_frame(&mut w, FRAME_STDERR, err.as_bytes());
        let _ = send_frame(&mut w, FRAME_EXIT, &1i32.to_be_bytes());
        return Ok(());
    }

    // Set slave as stdin/stdout/stderr and configure PTY session.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(slave_fd));
        cmd.stdout(Stdio::from_raw_fd(slave_fd));
        cmd.stderr(Stdio::from_raw_fd(slave_fd));
    }

    // In the child (after fork, before exec): become a new session leader and
    // acquire the PTY slave as the controlling terminal.  Without setsid() the
    // child inherits the parent's session and TIOCSCTTY fails; without
    // TIOCSCTTY the shell cannot set up job control and prints
    // "can't access tty; job control turned off".
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0i32) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            let mut w = FdWriter(fd);
            let err = format!("exec spawn failed: {}", e);
            let _ = send_frame(&mut w, FRAME_STDERR, err.as_bytes());
            let _ = send_frame(&mut w, FRAME_EXIT, &1i32.to_be_bytes());
            return Ok(());
        }
    };

    // Close slave in parent — child owns it now.
    unsafe { libc::close(slave_fd) };

    use std::sync::{Arc, Mutex};

    // Dup master_fd so the read and write sides each have their own fd.
    let master_read_fd = unsafe { libc::dup(master_fd) };
    if master_read_fd < 0 {
        unsafe { libc::close(master_fd) };
        let _ = child.wait();
        let mut w = FdWriter(fd);
        let _ = send_frame(&mut w, FRAME_EXIT, &1i32.to_be_bytes());
        return Ok(());
    }

    let master_write = Arc::new(Mutex::new(master_fd));
    let master_write2 = Arc::clone(&master_write);

    // Stdin/resize thread: read frames from vsock, write to master or ioctl.
    let stdin_thread = std::thread::spawn(move || {
        let mut reader = FdReader(fd);
        loop {
            match recv_frame(&mut reader) {
                Ok((FRAME_STDIN, data)) => {
                    let mfd = master_write2.lock().unwrap();
                    let ret = unsafe {
                        libc::write(*mfd, data.as_ptr() as *const libc::c_void, data.len())
                    };
                    if ret < 0 {
                        break;
                    }
                }
                Ok((FRAME_RESIZE, data)) if data.len() == 4 => {
                    let rows = u16::from_be_bytes([data[0], data[1]]);
                    let cols = u16::from_be_bytes([data[2], data[3]]);
                    let ws = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    let mfd = master_write2.lock().unwrap();
                    unsafe { libc::ioctl(*mfd, libc::TIOCSWINSZ, &ws) };
                }
                Ok(_) | Err(_) => break,
            }
        }
    });

    // Master-read thread: read from master (child's output), send FRAME_STDOUT.
    let writer = Arc::new(Mutex::new(FdWriter(fd)));
    let w_out = Arc::clone(&writer);
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    master_read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            let mut w = w_out.lock().unwrap();
            if send_frame(&mut *w, FRAME_STDOUT, &buf[..n as usize]).is_err() {
                break;
            }
        }
        unsafe { libc::close(master_read_fd) };
    });

    // Wait for child to exit.
    let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    // Close master write fd — this will cause the read thread to get EOF.
    {
        let mfd = master_write.lock().unwrap();
        unsafe { libc::close(*mfd) };
    }

    let _ = stdout_thread.join();
    drop(stdin_thread); // detach

    // Send exit frame.
    let mut w = writer.lock().unwrap();
    let _ = send_frame(&mut *w, FRAME_EXIT, &code.to_be_bytes());

    Ok(())
}

/// Reads directly from a raw fd using libc::read — no OwnedFd or File involved.
struct FdReader(libc::c_int);

impl Read for FdReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

/// Writes directly to a raw fd using libc::write — no OwnedFd or File involved.
struct FdWriter(libc::c_int);

impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Container state reader (for the event poller — no pelagos CLI needed)
// ---------------------------------------------------------------------------

/// Read container state directly from `/run/pelagos/containers/*/state.json`.
/// Returns an empty vec on any I/O error so the poller stays alive regardless.
fn read_container_states() -> Vec<ContainerSnapshot> {
    let dir = std::path::Path::new("/run/pelagos/containers");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let state_path = entry.path().join("state.json");
        let Ok(data) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        // Parse only the fields we need; ignore unknown keys.
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let name = val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let status = val
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let pid = val.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let rootfs = val
            .get("rootfs")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let started_at = val
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let exit_code = val
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|c| c as i32);
        // publish is nested under spawn_config in state.json.
        let ports: Vec<String> = val
            .get("spawn_config")
            .and_then(|sc| sc.get("publish"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.push(ContainerSnapshot {
            name,
            status,
            pid,
            rootfs,
            started_at,
            exit_code,
            ports,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------------------------------------------------------------------------
// Event poller (broadcasts container diffs to all Subscribe connections)
// ---------------------------------------------------------------------------

fn start_event_poller() {
    std::thread::Builder::new()
        .name("event-poller".into())
        .spawn(event_poller_loop)
        .expect("failed to spawn event poller");
}

fn event_poller_loop() {
    let mut prev: Vec<ContainerSnapshot> = read_container_states();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let current = read_container_states();
        let events = diff_container_states(&prev, &current);
        if !events.is_empty() {
            broadcast_events(&events);
        }
        prev = current;
    }
}

fn diff_container_states(
    prev: &[ContainerSnapshot],
    current: &[ContainerSnapshot],
) -> Vec<GuestEvent> {
    let mut events = Vec::new();

    // Containers that appeared or transitioned to running.
    for c in current {
        match prev.iter().find(|p| p.name == c.name) {
            None => {
                events.push(GuestEvent::ContainerStarted {
                    container: c.clone(),
                });
                // Container was never seen while running (exited between polls).
                // Immediately emit ContainerExited so the TUI can track its
                // final state rather than leaving it stuck in ContainerStarted.
                if c.status != "running" {
                    events.push(GuestEvent::ContainerExited {
                        name: c.name.clone(),
                        exit_code: c.exit_code,
                    });
                }
            }
            Some(p) if p.status != "running" && c.status == "running" => {
                events.push(GuestEvent::ContainerStarted {
                    container: c.clone(),
                });
            }
            _ => {}
        }
    }

    // Containers that disappeared or transitioned away from running.
    for p in prev {
        match current.iter().find(|c| c.name == p.name) {
            None => {
                events.push(GuestEvent::ContainerExited {
                    name: p.name.clone(),
                    exit_code: p.exit_code,
                });
            }
            Some(c) if p.status == "running" && c.status != "running" => {
                events.push(GuestEvent::ContainerExited {
                    name: c.name.clone(),
                    exit_code: c.exit_code,
                });
            }
            _ => {}
        }
    }

    events
}

fn broadcast_events(events: &[GuestEvent]) {
    let mut subs = subscriber_list().lock().unwrap();
    subs.retain(|tx| {
        for e in events {
            match tx.try_send(e.clone()) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    // Channel temporarily full: drop this event batch but keep
                    // the subscriber alive.  It is still connected; head-of-line
                    // blocking should resolve on the next broadcast cycle.
                    log::warn!("subscriber channel full — dropping event batch");
                    return true;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return false; // subscriber gone; remove from list
                }
            }
        }
        true
    });
}

// ---------------------------------------------------------------------------
// Subscribe command handler
// ---------------------------------------------------------------------------

fn handle_subscribe(writer: &mut FdWriter) -> std::io::Result<()> {
    // Register subscriber before reading snapshot to avoid missing an event
    // between snapshot and registration.
    let (tx, rx) = std::sync::mpsc::sync_channel(256);
    subscriber_list().lock().unwrap().push(tx);

    // Send initial snapshot.
    let snapshot = GuestEvent::Snapshot {
        containers: read_container_states(),
        vm_running: true,
    };
    send_event(writer, &snapshot)?;

    // Stream events until the client disconnects.  Use recv_timeout so we can
    // send a Heartbeat every ~5s even when no container events are occurring.
    // Heartbeats allow TUI clients to detect a silently-dead connection.
    let heartbeat_interval = std::time::Duration::from_secs(5);
    loop {
        match rx.recv_timeout(heartbeat_interval) {
            Ok(event) => {
                if send_event(writer, &event).is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if send_event(writer, &GuestEvent::Heartbeat { ts }).is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn send_event(writer: &mut impl Write, event: &GuestEvent) -> std::io::Result<()> {
    let mut json = serde_json::to_string(event).map_err(std::io::Error::other)?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;
    writer.flush()
}

fn send_response(writer: &mut impl Write, resp: &GuestResponse) -> std::io::Result<()> {
    let mut json = serde_json::to_string(resp).map_err(std::io::Error::other)?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;
    writer.flush()
}

// ---------------------------------------------------------------------------
// AF_VSOCK socket helpers (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn create_vsock_listener(port: u32) -> std::io::Result<OwnedFd> {
    use std::os::unix::io::FromRawFd;

    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0u8; 4],
    };
    let addr_len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            addr_len,
        )
    };
    if rc < 0 {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::last_os_error());
    }

    let rc = unsafe { libc::listen(fd, 16) };
    if rc < 0 {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn accept_vsock(listener: &OwnedFd) -> std::io::Result<libc::c_int> {
    use std::os::unix::io::AsRawFd;
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

// Stub for non-Linux builds (e.g. cargo check on macOS).
#[cfg(not(target_os = "linux"))]
fn create_vsock_listener(_port: u32) -> std::io::Result<OwnedFd> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "AF_VSOCK is Linux-only",
    ))
}

#[cfg(not(target_os = "linux"))]
fn accept_vsock(_listener: &OwnedFd) -> std::io::Result<libc::c_int> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "AF_VSOCK is Linux-only",
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{GuestCommand, GuestResponse};

    #[test]
    fn ping_deserializes() {
        let json = r#"{"cmd":"ping"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Ping));
    }

    #[test]
    fn run_deserializes() {
        let json = r#"{"cmd":"run","image":"alpine","args":["/bin/echo","hello"]}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Run {
                image,
                args,
                mounts,
                name,
                detach,
                ..
            } => {
                assert_eq!(image, "alpine");
                assert_eq!(args, vec!["/bin/echo", "hello"]);
                assert!(mounts.is_empty());
                assert!(name.is_none());
                assert!(!detach);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn run_with_publish_deserializes() {
        let json = r#"{"cmd":"run","image":"nginx","args":[],"publish":["8080:80","443:443"]}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Run { image, publish, .. } => {
                assert_eq!(image, "nginx");
                assert_eq!(publish, vec!["8080:80", "443:443"]);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn run_with_name_and_detach_deserializes() {
        let json =
            r#"{"cmd":"run","image":"alpine","args":["sleep","30"],"name":"mybox","detach":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Run { name, detach, .. } => {
                assert_eq!(name.as_deref(), Some("mybox"));
                assert!(detach);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn exec_deserializes() {
        let json = r#"{"cmd":"exec","image":"alpine","args":["sh"],"tty":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Exec {
                image, args, tty, ..
            } => {
                assert_eq!(image, "alpine");
                assert_eq!(args, vec!["sh"]);
                assert!(tty);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn exec_deserializes_defaults() {
        let json = r#"{"cmd":"exec","image":"alpine","args":["cat"]}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Exec { tty, env, .. } => {
                assert!(!tty);
                assert!(env.is_empty());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn run_with_mounts_deserializes() {
        let json = r#"{"cmd":"run","image":"alpine","args":["cat","/data/f"],"mounts":[{"tag":"share0","container_path":"/data"}]}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Run { image, mounts, .. } => {
                assert_eq!(image, "alpine");
                assert_eq!(mounts.len(), 1);
                assert_eq!(mounts[0].tag, "share0");
                assert_eq!(mounts[0].container_path, "/data");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn ps_deserializes() {
        let json = r#"{"cmd":"ps","all":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(
            cmd,
            GuestCommand::Ps {
                all: true,
                json: false
            }
        ));
    }

    #[test]
    fn ps_defaults_all_false() {
        let json = r#"{"cmd":"ps"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(
            cmd,
            GuestCommand::Ps {
                all: false,
                json: false
            }
        ));
    }

    #[test]
    fn ps_json_flag_deserializes() {
        let wire = r#"{"cmd":"ps","all":true,"json":true}"#;
        let cmd: GuestCommand = serde_json::from_str(wire).expect("parse failed");
        assert!(matches!(
            cmd,
            GuestCommand::Ps {
                all: true,
                json: true
            }
        ));
    }

    #[test]
    fn logs_deserializes() {
        let json = r#"{"cmd":"logs","name":"mybox","follow":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Logs { name, follow } => {
                assert_eq!(name, "mybox");
                assert!(follow);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn stop_deserializes() {
        let json = r#"{"cmd":"stop","name":"mybox"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Stop { name } if name == "mybox"));
    }

    #[test]
    fn rm_deserializes() {
        let json = r#"{"cmd":"rm","name":"mybox","force":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Rm { name, force: true } if name == "mybox"));
    }

    #[test]
    fn shell_deserializes() {
        let json = r#"{"cmd":"shell","tty":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Shell { tty: true }));
    }

    #[test]
    fn shell_defaults_tty_false() {
        let json = r#"{"cmd":"shell"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Shell { tty: false }));
    }

    #[test]
    fn pong_serializes() {
        let resp = GuestResponse::Pong { pong: true };
        let json = serde_json::to_string(&resp).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["pong"]["pong"], true);
    }

    #[test]
    fn ready_serializes() {
        let resp = GuestResponse::Ready { ready: true };
        let json = serde_json::to_string(&resp).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ready"]["ready"], true);
    }

    #[test]
    fn error_serializes() {
        let resp = GuestResponse::Error {
            error: "oops".into(),
        };
        let json = serde_json::to_string(&resp).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["error"]["error"], "oops");
    }

    #[test]
    fn build_deserializes() {
        let json =
            r#"{"cmd":"build","tag":"myapp:latest","dockerfile":"Dockerfile","context_size":1234}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Build {
                tag,
                dockerfile,
                build_args,
                no_cache,
                context_size,
            } => {
                assert_eq!(tag, "myapp:latest");
                assert_eq!(dockerfile, "Dockerfile");
                assert!(build_args.is_empty());
                assert!(!no_cache);
                assert_eq!(context_size, 1234);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn build_deserializes_with_build_args() {
        let json = r#"{"cmd":"build","tag":"myapp:1.0","dockerfile":"Dockerfile.prod","build_args":["KEY=VAL"],"no_cache":true,"context_size":9999}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Build {
                tag,
                build_args,
                no_cache,
                ..
            } => {
                assert_eq!(tag, "myapp:1.0");
                assert_eq!(build_args, vec!["KEY=VAL"]);
                assert!(no_cache);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn volume_deserializes() {
        let json = r#"{"cmd":"volume","sub":"create","name":"myvol"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Volume { sub, name } => {
                assert_eq!(sub, "create");
                assert_eq!(name.as_deref(), Some("myvol"));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn volume_ls_deserializes() {
        let json = r#"{"cmd":"volume","sub":"ls"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        assert!(matches!(cmd, GuestCommand::Volume { ref sub, name: None } if sub == "ls"));
    }

    #[test]
    fn network_deserializes() {
        let json = r#"{"cmd":"network","sub":"create","args":["--subnet","10.88.1.0/24","mynet"]}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Network { sub, args } => {
                assert_eq!(sub, "create");
                assert_eq!(args[2], "mynet");
                assert_eq!(args[0], "--subnet");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn cp_from_deserializes() {
        let json = r#"{"cmd":"cp_from","container":"mybox","src":"/etc/os-release"}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::CpFrom { container, src } => {
                assert_eq!(container, "mybox");
                assert_eq!(src, "/etc/os-release");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn cp_to_deserializes() {
        let json = r#"{"cmd":"cp_to","container":"mybox","dst":"/tmp/","data_size":4096}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::CpTo {
                container,
                dst,
                data_size,
            } => {
                assert_eq!(container, "mybox");
                assert_eq!(dst, "/tmp/");
                assert_eq!(data_size, 4096);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn frame_roundtrip() {
        use super::{recv_frame, send_frame, FRAME_STDOUT};
        use std::io::Cursor;
        let payload = b"hello exec";
        let mut buf = Vec::new();
        send_frame(&mut buf, FRAME_STDOUT, payload).unwrap();
        let mut cursor = Cursor::new(buf);
        let (ft, data) = recv_frame(&mut cursor).unwrap();
        assert_eq!(ft, FRAME_STDOUT);
        assert_eq!(data, payload);
    }
}
