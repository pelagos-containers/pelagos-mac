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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::OwnedFd;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub const VSOCK_PORT: u32 = 1024;

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum GuestCommand {
    Run {
        image: String,
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
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
        if let Err(e) = handle_connection(conn_fd) {
            log::error!("connection handler error: {}", e);
        }
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
            GuestCommand::Run { image, args, env } => {
                run_container(&mut writer, &image, &args, &env)?;
            }
        }
    }
    Ok(())
}

fn run_container(
    writer: &mut impl Write,
    image: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let pelagos = std::env::var("PELAGOS_BIN").unwrap_or_else(|_| "/usr/local/bin/pelagos".into());

    // Pull the image before running — pelagos run does not auto-pull.
    // Retry up to 3 times with 2-second backoff: the AVF NAT may not be ready
    // for outbound TCP immediately after the network interface comes up.
    const PULL_ATTEMPTS: u32 = 10;
    let mut pull_error = String::new();
    let mut pulled = false;
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
                return Ok(());
            }
        };
        // Collect pull output and relay it line-by-line as stderr stream.
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
            pulled = true;
            break;
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
    if !pulled {
        send_response(
            writer,
            &GuestResponse::Error { error: pull_error },
        )?;
        return Ok(());
    }

    let mut cmd = Command::new(&pelagos);
    cmd.arg("run").arg(image);
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
            GuestCommand::Run { image, args, .. } => {
                assert_eq!(image, "alpine");
                assert_eq!(args, vec!["/bin/echo", "hello"]);
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
    fn error_serializes() {
        let resp = GuestResponse::Error {
            error: "oops".into(),
        };
        let json = serde_json::to_string(&resp).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["error"]["error"], "oops");
    }
}
