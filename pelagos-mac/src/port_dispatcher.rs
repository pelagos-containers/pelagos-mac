//! Single-threaded port-forward dispatcher using `libc::poll`.
//!
//! One `PortDispatcher` thread manages arbitrarily many `TcpListener`s via a
//! single `poll(2)` call.  This keeps the listener thread count constant at 1
//! regardless of how many ports or containers are active.
//!
//! Each accepted connection spawns two threads (one per direction of
//! `io::copy`) for the duration of that connection.  See the tokio issue (#165)
//! for the planned upgrade to O(1) connection threads.
//!
//! # Protocol
//!
//! Callers interact with the dispatcher via a `mpsc::SyncSender<DispatchCmd>`.
//! When a command is sent, a byte is written to a wakeup pipe; the poll loop
//! wakes up, drains the command channel, and adds/removes listeners before
//! returning to `poll`.
//!
//! # Relay protocol
//!
//! Each accepted connection is forwarded to the smoltcp NAT relay at
//! `127.0.0.1:RELAY_PROXY_PORT`.  The caller writes a 2-byte big-endian
//! container port number immediately after connecting; the relay then
//! bidirectionally streams to `VM_IP:container_port`.

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc;
use std::time::Duration;

use pelagos_vz::nat_relay::RELAY_PROXY_PORT;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Commands sent to the dispatcher thread.
#[derive(Debug)]
pub enum DispatchCmd {
    /// Start listening on `host_port`; relay accepted connections to `container_port`.
    Add { host_port: u16, container_port: u16 },
    /// Stop listening on `host_port`.  Active connections are not affected.
    Remove { host_port: u16 },
    /// Shut down the dispatcher; all listeners are closed.
    Shutdown,
}

/// Handle to the background dispatcher thread.
///
/// Dropping this handle does NOT shut down the thread — send `DispatchCmd::Shutdown`
/// explicitly before dropping, or the thread will run until the process exits.
pub struct PortDispatcher {
    cmd_tx: mpsc::SyncSender<DispatchCmd>,
    /// Write end of the wakeup pipe (read end is in the dispatcher thread).
    wake_tx: RawFd,
}

impl PortDispatcher {
    /// Spawn the dispatcher thread and return a handle.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<DispatchCmd>(64);
        let mut pipe_fds: [libc::c_int; 2] = [-1; 2];
        // SAFETY: pipe() is a safe, well-defined syscall.
        let rc = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe failed: {}", std::io::Error::last_os_error());
        // Set O_CLOEXEC on both ends so they don't leak into child processes.
        unsafe {
            libc::fcntl(pipe_fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(pipe_fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        };
        let wake_rx = pipe_fds[0];
        let wake_tx = pipe_fds[1];

        std::thread::Builder::new()
            .name("port-dispatcher".into())
            .spawn(move || dispatcher_loop(cmd_rx, wake_rx))
            .expect("spawn port-dispatcher");

        Self { cmd_tx, wake_tx }
    }

    /// Send a command to the dispatcher and wake it up.
    pub fn send(&self, cmd: DispatchCmd) {
        // Best-effort: if the channel is full or the thread is gone, log and move on.
        if self.cmd_tx.try_send(cmd).is_err() {
            log::warn!("port-dispatcher: command channel full or closed");
            return;
        }
        // Wake the poll loop by writing one byte to the pipe.
        let byte: [u8; 1] = [1];
        // SAFETY: write to a valid pipe fd.
        unsafe { libc::write(self.wake_tx, byte.as_ptr() as *const libc::c_void, 1) };
    }
}

impl Drop for PortDispatcher {
    fn drop(&mut self) {
        // Close the write end of the wakeup pipe.  The dispatcher thread will
        // see EOF on the read end and exit its loop.
        // SAFETY: closing a valid fd.
        unsafe { libc::close(self.wake_tx) };
    }
}

// ---------------------------------------------------------------------------
// Dispatcher thread
// ---------------------------------------------------------------------------

struct ListenerEntry {
    listener: TcpListener,
    container_port: u16,
}

fn dispatcher_loop(cmd_rx: mpsc::Receiver<DispatchCmd>, wake_rx: RawFd) {
    // host_port → entry
    let mut listeners: HashMap<u16, ListenerEntry> = HashMap::new();
    // Reusable poll-fds vec; rebuilt each iteration.
    let mut pollfds: Vec<libc::pollfd> = Vec::new();

    loop {
        // Build the pollfd array: wakeup pipe first, then all listener fds.
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: wake_rx,
            events: libc::POLLIN,
            revents: 0,
        });
        for entry in listeners.values() {
            pollfds.push(libc::pollfd {
                fd: entry.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // Block until at least one fd is ready (no timeout).
        // SAFETY: pollfds is valid for the duration of the call.
        let rc = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                -1, // infinite timeout
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("port-dispatcher: poll error: {}", err);
            break;
        }

        // Check wakeup pipe (index 0).
        if pollfds[0].revents & libc::POLLIN != 0 {
            // Drain the pipe.
            let mut buf = [0u8; 64];
            // SAFETY: read from a valid pipe fd.
            unsafe { libc::read(wake_rx, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

            // Process all pending commands.
            loop {
                match cmd_rx.try_recv() {
                    Ok(DispatchCmd::Add {
                        host_port,
                        container_port,
                    }) => {
                        add_listener(&mut listeners, host_port, container_port);
                    }
                    Ok(DispatchCmd::Remove { host_port }) => {
                        if listeners.remove(&host_port).is_some() {
                            log::info!("port-dispatcher: stopped listener 0.0.0.0:{}", host_port);
                        }
                    }
                    Ok(DispatchCmd::Shutdown) => {
                        log::info!(
                            "port-dispatcher: shutting down ({} listeners)",
                            listeners.len()
                        );
                        // Dropping listeners closes the TcpListeners.
                        listeners.clear();
                        // SAFETY: closing the read end of the pipe.
                        unsafe { libc::close(wake_rx) };
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        log::info!("port-dispatcher: command channel closed, exiting");
                        // SAFETY: closing the read end of the pipe.
                        unsafe { libc::close(wake_rx) };
                        return;
                    }
                }
            }
        }

        // Check each listener fd (indices 1..).  Collect ready host_ports to
        // avoid borrowing `listeners` mutably while iterating values.
        let ready_ports: Vec<u16> = listeners
            .iter()
            .zip(pollfds.iter().skip(1))
            .filter(|(_, pfd)| pfd.revents & libc::POLLIN != 0)
            .map(|((&port, _), _)| port)
            .collect();

        for host_port in ready_ports {
            if let Some(entry) = listeners.get(&host_port) {
                match entry.listener.accept() {
                    Ok((client, peer)) => {
                        log::debug!("port-dispatcher: accepted {} → 0.0.0.0:{}", peer, host_port);
                        let container_port = entry.container_port;
                        spawn_connection_handler(client, host_port, container_port);
                    }
                    Err(e) => {
                        log::warn!("port-dispatcher: accept on {}: {}", host_port, e);
                    }
                }
            }
        }
    }
}

fn add_listener(listeners: &mut HashMap<u16, ListenerEntry>, host_port: u16, container_port: u16) {
    if listeners.contains_key(&host_port) {
        log::debug!("port-dispatcher: listener already active on {}", host_port);
        return;
    }
    match TcpListener::bind(("0.0.0.0", host_port)) {
        Ok(listener) => {
            log::info!(
                "port-dispatcher: listening 0.0.0.0:{} → VM:{} (relay :{})",
                host_port,
                container_port,
                RELAY_PROXY_PORT
            );
            listeners.insert(
                host_port,
                ListenerEntry {
                    listener,
                    container_port,
                },
            );
        }
        Err(e) => {
            log::error!("port-dispatcher: bind 0.0.0.0:{} failed: {}", host_port, e);
        }
    }
}

fn spawn_connection_handler(client: TcpStream, host_port: u16, container_port: u16) {
    std::thread::Builder::new()
        .name(format!("port-conn-{}", host_port))
        .spawn(move || {
            let relay_addr = std::net::SocketAddr::from(([127, 0, 0, 1], RELAY_PROXY_PORT));
            let mut server = match TcpStream::connect_timeout(&relay_addr, Duration::from_secs(5)) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("port-dispatcher: connect relay:{}: {}", RELAY_PROXY_PORT, e);
                    return;
                }
            };
            // Send 2-byte big-endian container port to the relay.
            if server.write_all(&container_port.to_be_bytes()).is_err() {
                return;
            }
            proxy_bidirectional(client, server);
        })
        .ok();
}

/// Bidirectionally proxy two TCP streams; returns when either side closes.
fn proxy_bidirectional(client: TcpStream, server: TcpStream) {
    let mut client_read = client;
    let mut server_read = server;
    let mut client_write = client_read.try_clone().expect("tcp clone");
    let mut server_write = server_read.try_clone().expect("tcp clone");

    let t1 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut server_write);
        let _ = server_write.shutdown(std::net::Shutdown::Write);
    });
    let t2 = std::thread::spawn(move || {
        let _ = std::io::copy(&mut server_read, &mut client_write);
        let _ = client_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = t1.join();
    let _ = t2.join();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn free_port() -> u16 {
        // Bind port 0 to get an OS-assigned free port, then release it.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn dispatcher_binds_on_add() {
        let d = PortDispatcher::spawn();
        let port = free_port();
        d.send(DispatchCmd::Add {
            host_port: port,
            container_port: 80,
        });
        // Give the dispatcher time to process the command.
        std::thread::sleep(Duration::from_millis(50));
        // Port should now be in use — binding it again must fail.
        assert!(
            TcpListener::bind(("0.0.0.0", port)).is_err(),
            "port {} should be bound by dispatcher",
            port
        );
        d.send(DispatchCmd::Shutdown);
    }

    #[test]
    fn dispatcher_unbinds_on_remove() {
        let d = PortDispatcher::spawn();
        let port = free_port();
        d.send(DispatchCmd::Add {
            host_port: port,
            container_port: 80,
        });
        std::thread::sleep(Duration::from_millis(50));
        d.send(DispatchCmd::Remove { host_port: port });
        std::thread::sleep(Duration::from_millis(50));
        // Port should now be free again.
        let l = TcpListener::bind(("0.0.0.0", port));
        assert!(l.is_ok(), "port {} should be free after remove", port);
        d.send(DispatchCmd::Shutdown);
    }

    #[test]
    fn dispatcher_shutdown_closes_all() {
        let d = PortDispatcher::spawn();
        let port = free_port();
        d.send(DispatchCmd::Add {
            host_port: port,
            container_port: 80,
        });
        std::thread::sleep(Duration::from_millis(50));
        d.send(DispatchCmd::Shutdown);
        std::thread::sleep(Duration::from_millis(50));
        // After shutdown all listeners should be closed.
        let l = TcpListener::bind(("0.0.0.0", port));
        assert!(l.is_ok(), "port {} should be free after shutdown", port);
    }
}
