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
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub const VSOCK_PORT: u32 = 1024;

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
    },
    Exec {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        tty: bool,
    },
    Ping,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestResponse {
    Stream { stream: StreamKind, data: String },
    Exit { exit: i32 },
    Pong { pong: bool },
    Error { error: String },
    Ready { ready: bool },
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

    loop {
        let conn_fd = match accept_vsock(&listener) {
            Ok(fd) => fd,
            Err(e) => {
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
    let reader = BufReader::new(FdReader(fd));
    let mut writer = FdWriter(fd);

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let cmd: GuestCommand = match serde_json::from_str(&line) {
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
                send_response(&mut writer, &GuestResponse::Pong { pong: true })?;
                return Ok(());
            }
            GuestCommand::Run { image, args, env, mounts } => {
                run_container(&mut writer, &image, &args, &env, &mounts)?;
            }
            GuestCommand::Exec { image, args, env, tty } => {
                handle_exec(fd, &image, &args, &env, tty)?;
                return Ok(());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pull helper (shared by run_container and handle_exec)
// ---------------------------------------------------------------------------

/// Pull the image, streaming stderr lines back via the provided writer.
/// Returns Ok(true) on success, Ok(false) on failure (error response sent).
fn pull_image(writer: &mut impl Write, image: &str) -> std::io::Result<bool> {
    let pelagos = std::env::var("PELAGOS_BIN").unwrap_or_else(|_| "/usr/local/bin/pelagos".into());

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
    send_response(
        writer,
        &GuestResponse::Error { error: pull_error },
    )?;
    Ok(false)
}

fn run_container(
    writer: &mut impl Write,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    mounts: &[GuestMount],
) -> std::io::Result<()> {
    let pelagos = std::env::var("PELAGOS_BIN").unwrap_or_else(|_| "/usr/local/bin/pelagos".into());

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
    // Pass each virtiofs guest-side path as a -v bind mount to pelagos run.
    for mount in mounts {
        let guest_mnt = format!("/mnt/{}", mount.tag);
        cmd.arg("-v").arg(format!("{}:{}", guest_mnt, mount.container_path));
    }
    cmd.arg(image);
    if !args.is_empty() {
        cmd.args(args);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
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

    // Stream stdout and stderr concurrently using threads.
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

    // Relay chunks to the vsock writer until both streams signal Done.
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
// Exec handler
// ---------------------------------------------------------------------------

fn handle_exec(
    fd: libc::c_int,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
    tty: bool,
) -> std::io::Result<()> {
    let pelagos = std::env::var("PELAGOS_BIN").unwrap_or_else(|_| "/usr/local/bin/pelagos".into());

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

    if tty {
        handle_exec_tty(fd, &pelagos, image, args, env)
    } else {
        handle_exec_piped(fd, &pelagos, image, args, env)
    }
}

/// Non-TTY exec: spawn with piped stdin/stdout/stderr, forward via frames.
fn handle_exec_piped(
    fd: libc::c_int,
    pelagos: &str,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let mut cmd = Command::new(pelagos);
    cmd.arg("run").arg(image);
    if !args.is_empty() {
        cmd.args(args);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
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
fn handle_exec_tty(
    fd: libc::c_int,
    pelagos: &str,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
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

    // Spawn child with slave as stdin/stdout/stderr.
    let mut cmd = Command::new(pelagos);
    cmd.arg("run").arg(image);
    if !args.is_empty() {
        cmd.args(args);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(slave_fd));
        cmd.stdout(Stdio::from_raw_fd(slave_fd));
        cmd.stderr(Stdio::from_raw_fd(slave_fd));
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
                        libc::write(
                            *mfd,
                            data.as_ptr() as *const libc::c_void,
                            data.len(),
                        )
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
            GuestCommand::Run { image, args, mounts, .. } => {
                assert_eq!(image, "alpine");
                assert_eq!(args, vec!["/bin/echo", "hello"]);
                assert!(mounts.is_empty());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn exec_deserializes() {
        let json = r#"{"cmd":"exec","image":"alpine","args":["sh"],"tty":true}"#;
        let cmd: GuestCommand = serde_json::from_str(json).expect("parse failed");
        match cmd {
            GuestCommand::Exec { image, args, tty, .. } => {
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
