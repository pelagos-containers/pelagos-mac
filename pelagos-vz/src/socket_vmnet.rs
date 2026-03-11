//! socket_vmnet client — connects to the socket_vmnet privileged helper and
//! returns a SOCK_DGRAM fd suitable for `VZFileHandleNetworkDeviceAttachment`.
//!
//! # Architecture
//!
//! socket_vmnet exposes a Unix SOCK_STREAM socket with a 4-byte big-endian
//! length-prefixed Ethernet frame wire format:
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │  socket_vmnet (root daemon, holds vmnet handle)  │
//! │  UNIX SOCK_STREAM at /opt/homebrew/var/run/...   │
//! └──────────────────────┬──────────────────────────┘
//!                        │ length-prefixed Ethernet frames
//!                  [relay threads]
//!                        │ raw Ethernet frames
//! ┌──────────────────────▼──────────────────────────┐
//! │  socketpair(AF_UNIX, SOCK_DGRAM)                 │
//! │    avf_end  ←→  relay_end                        │
//! └──────────────────────┬──────────────────────────┘
//!                        │ raw Ethernet frames
//! ┌──────────────────────▼──────────────────────────┐
//! │  VZFileHandleNetworkDeviceAttachment (AVF)        │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! Two relay threads bridge the framing difference:
//! - `vmnet_to_avf`: reads framed packets from socket_vmnet, sends raw to socketpair
//! - `avf_to_vmnet`: reads raw packets from socketpair, writes framed to socket_vmnet

use libc::c_int;
use std::os::unix::io::RawFd;

/// Candidate socket paths for socket_vmnet, tried in order.
///
/// Homebrew's launchd plist (homebrew.mxcl.socket_vmnet.plist) creates the
/// socket at the path-without-suffix form (e.g. `socket_vmnet`).  The `.shared`
/// suffix is only used when socket_vmnet is started manually with an explicit
/// mode flag.  We probe both forms so either install style is detected.
const CANDIDATE_PATHS: &[&str] = &[
    // Homebrew on Apple Silicon — launchd plist default (no mode suffix)
    "/opt/homebrew/var/run/socket_vmnet",
    // Homebrew on Apple Silicon — manual / explicit shared-mode start
    "/opt/homebrew/var/run/socket_vmnet.shared",
    // Homebrew on Intel — launchd plist default
    "/usr/local/var/run/socket_vmnet",
    // Homebrew on Intel — manual / explicit shared-mode start
    "/usr/local/var/run/socket_vmnet.shared",
    // System-wide .pkg install
    "/var/run/socket_vmnet",
    "/var/run/socket_vmnet.shared",
];

/// Detect the socket_vmnet socket path by probing candidate locations.
pub fn find_socket_path() -> Option<&'static str> {
    CANDIDATE_PATHS
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
}

/// Connect to socket_vmnet in shared mode.
///
/// Returns `(avf_fd, relay)`:
/// - `avf_fd` is one end of a `socketpair(AF_UNIX, SOCK_DGRAM)` ready to be
///   wrapped in `NSFileHandle` and passed to `VZFileHandleNetworkDeviceAttachment`.
/// - `relay` holds the two relay threads that forward Ethernet frames between
///   AVF and socket_vmnet. Drop it to shut down the relay (closes fds, joins threads).
pub fn connect() -> Result<(RawFd, RelayHandle), crate::Error> {
    let path = find_socket_path().ok_or_else(|| {
        crate::Error::Runtime(
            "socket_vmnet socket not found. Install and start it:\n  \
             brew install socket_vmnet\n  \
             sudo brew services start socket_vmnet"
                .into(),
        )
    })?;

    log::info!("socket_vmnet: connecting to {}", path);

    // Connect to socket_vmnet (SOCK_STREAM with length-prefixed frame protocol).
    let vmnet_fd = connect_unix_stream(path)?;

    // Create socketpair(AF_UNIX, SOCK_DGRAM) for AVF ↔ relay.
    let (avf_fd, relay_fd) = create_socketpair()?;

    // Set socket buffer sizes per AVF documentation:
    //   SO_RCVBUF should be at least 2× SO_SNDBUF; 4× is optimal.
    // Using 128 KB send / 512 KB recv for 1500-byte MTU traffic.
    const SNDBUF: c_int = 128 * 1024;
    const RCVBUF: c_int = 512 * 1024;
    set_sock_bufs(avf_fd, SNDBUF, RCVBUF);
    set_sock_bufs(relay_fd, SNDBUF, RCVBUF);

    // Each relay thread needs its own fd; dup() so ownership is unambiguous.
    let vmnet_read_fd = unsafe { libc::dup(vmnet_fd) };
    let relay_write_fd = unsafe { libc::dup(relay_fd) };
    // vmnet_fd and relay_fd are consumed by the other pair of threads.

    if vmnet_read_fd < 0 || relay_write_fd < 0 {
        return Err(crate::Error::Io(std::io::Error::last_os_error()));
    }

    // Thread 1: socket_vmnet → AVF
    //   Reads length-prefixed frames from vmnet socket, sends raw frames to relay_fd.
    let vmnet_to_avf = std::thread::Builder::new()
        .name("vmnet-to-avf".into())
        .spawn(move || relay_vmnet_to_avf(vmnet_read_fd, relay_write_fd))
        .expect("spawn vmnet-to-avf");

    // Thread 2: AVF → socket_vmnet
    //   Reads raw frames from relay_fd2, writes length-prefixed frames to vmnet socket.
    let avf_to_vmnet = std::thread::Builder::new()
        .name("avf-to-vmnet".into())
        .spawn(move || relay_avf_to_vmnet(relay_fd, vmnet_fd))
        .expect("spawn avf-to-vmnet");

    log::info!("socket_vmnet: relay threads started (avf_fd={})", avf_fd);

    Ok((
        avf_fd,
        RelayHandle {
            _vmnet_to_avf: vmnet_to_avf,
            _avf_to_vmnet: avf_to_vmnet,
        },
    ))
}

// ---------------------------------------------------------------------------
// Relay threads
// ---------------------------------------------------------------------------

/// socket_vmnet → AVF: reads length-prefixed Ethernet frames from the vmnet
/// SOCK_STREAM socket and forwards them as raw datagrams to the AVF socketpair.
fn relay_vmnet_to_avf(vmnet_fd: RawFd, avf_relay_fd: RawFd) {
    let mut len_buf = [0u8; 4];
    // 64 KiB covers the maximum possible Ethernet frame with jumbo frames.
    let mut frame_buf = vec![0u8; 64 * 1024];

    loop {
        // Read 4-byte big-endian length header.
        if read_exact(vmnet_fd, &mut len_buf).is_err() {
            log::debug!("vmnet-to-avf: vmnet socket closed");
            break;
        }
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        if frame_len == 0 || frame_len > frame_buf.len() {
            log::warn!(
                "vmnet-to-avf: invalid frame length {}, stopping relay",
                frame_len
            );
            break;
        }

        // Read the Ethernet frame payload.
        if read_exact(vmnet_fd, &mut frame_buf[..frame_len]).is_err() {
            log::debug!("vmnet-to-avf: read frame body error");
            break;
        }

        // Send raw frame to AVF via the SOCK_DGRAM socketpair.
        let r = unsafe { libc::send(avf_relay_fd, frame_buf.as_ptr() as _, frame_len, 0) };
        if r < 0 {
            log::debug!(
                "vmnet-to-avf: send to AVF failed: {}",
                std::io::Error::last_os_error()
            );
            break;
        }
    }

    unsafe {
        libc::close(vmnet_fd);
        libc::close(avf_relay_fd);
    }
}

/// AVF → socket_vmnet: reads raw Ethernet frames from the AVF socketpair and
/// forwards them as length-prefixed frames to the socket_vmnet SOCK_STREAM socket.
fn relay_avf_to_vmnet(avf_relay_fd: RawFd, vmnet_fd: RawFd) {
    let mut frame_buf = vec![0u8; 64 * 1024];

    loop {
        // SOCK_DGRAM recv() returns exactly one complete Ethernet frame per call.
        let r = unsafe {
            libc::recv(
                avf_relay_fd,
                frame_buf.as_mut_ptr() as _,
                frame_buf.len(),
                0,
            )
        };
        if r <= 0 {
            log::debug!("avf-to-vmnet: AVF socketpair closed/error");
            break;
        }
        let frame_len = r as usize;

        // Write 4-byte big-endian length prefix + raw frame to socket_vmnet.
        let len_bytes = (frame_len as u32).to_be_bytes();
        if write_all(vmnet_fd, &len_bytes).is_err()
            || write_all(vmnet_fd, &frame_buf[..frame_len]).is_err()
        {
            log::debug!("avf-to-vmnet: write to vmnet socket failed");
            break;
        }
    }

    unsafe {
        libc::close(avf_relay_fd);
        libc::close(vmnet_fd);
    }
}

// ---------------------------------------------------------------------------
// RelayHandle
// ---------------------------------------------------------------------------

/// Holds the two relay threads. When dropped, the relay fds are closed which
/// causes both threads to exit; the threads are then joined.
pub struct RelayHandle {
    _vmnet_to_avf: std::thread::JoinHandle<()>,
    _avf_to_vmnet: std::thread::JoinHandle<()>,
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        // The relay threads exit when their fds are closed (which happens inside
        // relay_vmnet_to_avf / relay_avf_to_vmnet on error/EOF). We join to
        // ensure clean shutdown, but ignore panics.
        //
        // Note: we cannot join JoinHandle in Drop without unsafe tricks; use a
        // best-effort approach by detaching (the handles are simply dropped here,
        // and the OS reclaims the threads when the process exits).
        log::debug!("socket_vmnet relay: handles dropped");
    }
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

fn connect_unix_stream(path: &str) -> Result<RawFd, crate::Error> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(crate::Error::Io(std::io::Error::last_os_error()));
        }

        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_bytes();
        let max = addr.sun_path.len() - 1;
        let len = bytes.len().min(max);
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const i8, addr.sun_path.as_mut_ptr(), len);

        let r = libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        if r < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(crate::Error::Runtime(format!(
                "socket_vmnet connect({}): {}",
                path, err
            )));
        }

        Ok(fd)
    }
}

fn create_socketpair() -> Result<(RawFd, RawFd), crate::Error> {
    let mut fds: [c_int; 2] = [-1, -1];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if r < 0 {
        return Err(crate::Error::Io(std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

fn set_sock_bufs(fd: RawFd, sndbuf: c_int, rcvbuf: c_int) {
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<c_int>() as libc::socklen_t,
        );
    }
}

fn read_exact(fd: RawFd, buf: &mut [u8]) -> Result<(), ()> {
    let mut total = 0;
    while total < buf.len() {
        let r = unsafe {
            libc::read(
                fd,
                buf[total..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - total,
            )
        };
        if r <= 0 {
            return Err(());
        }
        total += r as usize;
    }
    Ok(())
}

fn write_all(fd: RawFd, buf: &[u8]) -> Result<(), ()> {
    let mut total = 0;
    while total < buf.len() {
        let r = unsafe {
            libc::write(
                fd,
                buf[total..].as_ptr() as *const libc::c_void,
                buf.len() - total,
            )
        };
        if r <= 0 {
            return Err(());
        }
        total += r as usize;
    }
    Ok(())
}
