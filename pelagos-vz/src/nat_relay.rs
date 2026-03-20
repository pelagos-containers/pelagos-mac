//! Pure-Rust userspace NAT relay for VM networking.
//!
//! Replaces socket_vmnet (vmnet.framework) with a smoltcp-based userspace
//! TCP/IP stack. This eliminates the vmnet NAT connection table exhaustion
//! that caused network degradation after heavy TCP workloads (e.g. apt-get).
//!
//! # Architecture
//!
//! ```text
//! AVF virtio-net (raw Ethernet frames via SOCK_DGRAM socketpair)
//!          │
//!    [nat_relay poll thread — smoltcp poll loop, ~1ms tick]
//!          │
//!    smoltcp Interface (Ethernet, IPv4, ARP)
//!    ├─ ARP: auto-handled for gateway MAC (192.168.105.1)
//!    ├─ TCP: dynamic per-port listener sockets (created on first SYN)
//!    │   └─ per-connection proxy thread: smoltcp ↔ std::net::TcpStream
//!    └─ UDP: per-datagram proxy threads (non-blocking poll loop)
//! ```
//!
//! # TCP listener strategy
//!
//! smoltcp does not support `listen(port: 0)` as a wildcard — it returns
//! `Err(Unaddressable)` for port 0. Instead we pre-scan each batch of
//! incoming Ethernet frames before handing them to `iface.poll()`: for
//! every TCP SYN we see, we ensure a smoltcp listener socket exists on
//! that exact destination port. `iface.poll()` then finds the listener
//! and completes the three-way handshake normally.
//!
//! # VM network configuration
//!
//! The VM uses a static IP (udhcpc requires CONFIG_PACKET which is disabled):
//! - Guest IP:   192.168.105.2/24
//! - Gateway:    192.168.105.1  (the relay answers ARP for this)
//! - DNS:        8.8.8.8 (forwarded through the relay)
//!
//! # Interface to vm.rs
//!
//! `start()` returns `(avf_fd, RelayHandle)` — identical contract to
//! the old `socket_vmnet::connect()` so vm.rs needs only a one-line change.

use libc::c_int;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the NAT relay.
///
/// Returns `(avf_fd, relay)`:
/// - `avf_fd` is one end of a `socketpair(AF_UNIX, SOCK_DGRAM)` ready to be
///   wrapped in `NSFileHandle` and passed to `VZFileHandleNetworkDeviceAttachment`.
/// - `relay` holds the relay thread. Drop it to initiate shutdown.
pub fn start() -> Result<(RawFd, RelayHandle), crate::Error> {
    let (avf_fd, relay_fd) = create_socketpair()?;

    // Buffer sizes: 128 KB send / 512 KB recv per AVF documentation.
    const SNDBUF: c_int = 128 * 1024;
    const RCVBUF: c_int = 512 * 1024;
    set_sock_bufs(avf_fd, SNDBUF, RCVBUF);
    set_sock_bufs(relay_fd, SNDBUF, RCVBUF);

    // Channel for inbound port-forward requests from the relay proxy port.
    let (inbound_tx, inbound_rx) = mpsc::channel::<(TcpStream, u16)>();

    // Spawn the inbound proxy listener (macOS 127.0.0.1:RELAY_PROXY_PORT).
    std::thread::Builder::new()
        .name("nat-relay-proxy-listener".into())
        .spawn(move || inbound_proxy_listener(inbound_tx))
        .expect("spawn nat-relay-proxy-listener");

    let thread = std::thread::Builder::new()
        .name("nat-relay".into())
        .spawn(move || run_relay(relay_fd, inbound_rx))
        .expect("spawn nat-relay");

    log::info!(
        "nat_relay: started (avf_fd={}, proxy_port={})",
        avf_fd,
        RELAY_PROXY_PORT
    );
    Ok((avf_fd, RelayHandle { _thread: thread }))
}

/// Holds the relay thread. When dropped, the relay_fd is closed which
/// causes the poll thread to exit on next iteration.
pub struct RelayHandle {
    _thread: std::thread::JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// smoltcp Device implementation backed by the SOCK_DGRAM socketpair
// ---------------------------------------------------------------------------

struct AvfDevice {
    relay_fd: RawFd,
    rx_buf: Vec<u8>,
    /// Frames pre-read by `pre_scan_frames` and queued for smoltcp to consume.
    /// smoltcp calls `receive()` once per frame; we drain this queue first.
    /// Any frame that arrives *during* `iface.poll()` is also pushed here
    /// so it gets the full pre-scan treatment on the next cycle.
    pending_frames: VecDeque<Vec<u8>>,
}

impl AvfDevice {
    fn new(relay_fd: RawFd) -> Self {
        Self {
            relay_fd,
            rx_buf: vec![0u8; 64 * 1024],
            pending_frames: VecDeque::new(),
        }
    }
}

struct AvfRxToken {
    buf: Vec<u8>,
}

struct AvfTxToken {
    fd: RawFd,
    buf: Vec<u8>,
}

impl smoltcp::phy::RxToken for AvfRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.buf)
    }
}

impl smoltcp::phy::TxToken for AvfTxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, len: usize, f: F) -> R {
        self.buf.resize(len, 0);
        let result = f(&mut self.buf);
        unsafe {
            libc::send(self.fd, self.buf.as_ptr() as _, len, 0);
        }
        result
    }
}

impl smoltcp::phy::Device for AvfDevice {
    type RxToken<'a>
        = AvfRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = AvfTxToken
    where
        Self: 'a;

    fn receive(&mut self, _ts: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Drain pre-buffered frames first (set up by pre_scan_frames).
        if let Some(frame) = self.pending_frames.pop_front() {
            return Some((
                AvfRxToken { buf: frame },
                AvfTxToken {
                    fd: self.relay_fd,
                    buf: Vec::new(),
                },
            ));
        }

        // Any frame arriving *during* iface.poll() bypassed pre_scan.
        // Buffer it for the next cycle so SYN detection can run on it.
        let r = unsafe {
            libc::recv(
                self.relay_fd,
                self.rx_buf.as_mut_ptr() as _,
                self.rx_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if r > 0 {
            self.pending_frames
                .push_back(self.rx_buf[..r as usize].to_vec());
        }
        None
    }

    fn transmit(&mut self, _ts: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(AvfTxToken {
            fd: self.relay_fd,
            buf: Vec::new(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

// ---------------------------------------------------------------------------
// Per-connection TCP proxy state
// ---------------------------------------------------------------------------

enum ProxyMsg {
    /// Data from the macOS TcpStream destined for the VM (smoltcp send).
    FromHost(Vec<u8>),
    /// The host side closed the connection.
    HostClosed,
}

struct TcpConn {
    /// Receives data from the macOS TcpStream proxy thread.
    rx: Receiver<ProxyMsg>,
    /// Sends data from smoltcp to the macOS TcpStream proxy thread.
    tx: Sender<Vec<u8>>,
    /// Bytes not yet written to the smoltcp TX buffer due to a partial
    /// `send_slice` (occurs when the buffer had less space than the chunk).
    /// Must be flushed before consuming more from `rx`.
    pending_send: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Main relay loop
// ---------------------------------------------------------------------------

/// Gateway IP: the relay pretends to be a router at this address.
const GATEWAY_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 105, 1);
/// Fabricated MAC for the gateway (locally administered, unicast).
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

/// Receive buffer per smoltcp TCP socket (bytes).
/// Large enough to avoid stalling downloads during poll-loop iterations.
const TCP_RX_BUF: usize = 256 * 1024;
/// Send buffer per smoltcp TCP socket (bytes).
const TCP_SEND_BUF: usize = 256 * 1024;

/// Well-known loopback port the relay binds on macOS for inbound port
/// forwarding.  macOS processes that want a connection forwarded to the
/// VM send a 2-byte big-endian container-port number, then use the socket
/// as a bidirectional stream.
pub const RELAY_PROXY_PORT: u16 = 17900;

fn smol_now() -> SmolInstant {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    SmolInstant::from_millis(d.as_millis() as i64)
}

/// Read all currently-available Ethernet frames into `device.pending_frames`.
///
/// For each frame:
/// - ICMP echo requests are answered immediately (fake reply) and not buffered.
/// - TCP SYNs trigger creation of a smoltcp listener on the destination port
///   if one does not already exist in Listen state.
///
/// Must be called *before* `iface.poll()` so that listeners are in place
/// when smoltcp processes the SYNs.
fn pre_scan_frames(
    device: &mut AvfDevice,
    sockets: &mut SocketSet<'_>,
    listeners: &mut Vec<smoltcp::iface::SocketHandle>,
) {
    let mut frame_buf = vec![0u8; 64 * 1024];
    loop {
        let r = unsafe {
            libc::recv(
                device.relay_fd,
                frame_buf.as_mut_ptr() as _,
                frame_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if r <= 0 {
            break;
        }
        let len = r as usize;
        let frame = frame_buf[..len].to_vec();

        // Handle ICMP echo requests inline — reply and discard.
        if icmp_echo_reply(device.relay_fd, &frame) {
            continue;
        }

        // Handle UDP datagrams inline — proxy to real host, reply in thread.
        // smoltcp udp::Socket::bind(port=0) returns Err(Unaddressable), so we
        // bypass smoltcp entirely for UDP and handle frames raw.
        if handle_udp_frame(device.relay_fd, &frame) {
            continue;
        }

        // For TCP SYNs, ensure a listener exists on the destination port.
        if let Some(dst_port) = tcp_syn_dst_port(&frame) {
            let has_listener = listeners.iter().any(|&h| {
                let s = sockets.get::<tcp::Socket>(h);
                s.state() == tcp::State::Listen && s.listen_endpoint().port == dst_port
            });
            if !has_listener {
                let h = add_tcp_listener_on_port(sockets, dst_port);
                listeners.push(h);
                log::debug!("nat_relay: added listener on port {}", dst_port);
            }
        }

        device.pending_frames.push_back(frame);
    }
}

fn run_relay(relay_fd: RawFd, inbound_rx: Receiver<(TcpStream, u16)>) {
    let mut device = AvfDevice::new(relay_fd);

    // Configure the smoltcp interface as the gateway (192.168.105.1).
    let mut config = Config::new(GATEWAY_MAC.into());
    config.random_seed = 0xdeadbeef_cafebabe;
    let mut iface = Interface::new(config, &mut device, smol_now());
    iface.set_any_ip(true);
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP), 24))
            .ok();
    });
    // any_ip=true alone is not enough: smoltcp also requires a route to the
    // destination that resolves to one of our own IPs.  A default route
    // pointing back to ourselves satisfies this for all external destinations.
    iface
        .routes_mut()
        .add_default_ipv4_route(GATEWAY_IP)
        .expect("add default route");

    let mut sockets = SocketSet::new(vec![]);

    // Active connections: smoltcp SocketHandle → TcpConn.
    let mut tcp_conns: HashMap<smoltcp::iface::SocketHandle, TcpConn> = HashMap::new();

    // Current listener sockets (created dynamically on first SYN for each port).
    let mut listeners: Vec<smoltcp::iface::SocketHandle> = vec![];

    // Inbound (macOS→VM): smoltcp connect sockets waiting to reach Established.
    let mut inbound_pending: HashMap<smoltcp::iface::SocketHandle, TcpStream> = HashMap::new();
    let mut next_local_port: u16 = 49152;

    log::info!(
        "nat_relay: poll loop started (proxy_port={})",
        RELAY_PROXY_PORT
    );

    loop {
        // Pre-scan: read all pending frames, handle ICMP, ensure TCP listeners exist.
        pre_scan_frames(&mut device, &mut sockets, &mut listeners);

        let now = smol_now();
        iface.poll(now, &mut device, &mut sockets);

        // ---- Inbound: accept new macOS→VM port-forward requests ----
        loop {
            match inbound_rx.try_recv() {
                Ok((macos_sock, container_port)) => {
                    let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
                    let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SEND_BUF]);
                    let mut sock = tcp::Socket::new(rx_buf, tx_buf);
                    let local_port = next_local_port;
                    next_local_port = next_local_port.wrapping_add(1).max(49152);
                    let remote = IpEndpoint {
                        addr: IpAddress::Ipv4(std::net::Ipv4Addr::new(192, 168, 105, 2)),
                        port: container_port,
                    };
                    let local = IpListenEndpoint {
                        addr: Some(IpAddress::Ipv4(GATEWAY_IP)),
                        port: local_port,
                    };
                    if sock.connect(iface.context(), remote, local).is_ok() {
                        let handle = sockets.add(sock);
                        inbound_pending.insert(handle, macos_sock);
                        log::debug!(
                            "nat_relay: inbound connect to 192.168.105.2:{}",
                            container_port
                        );
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // ---- Inbound pending: promote to active TcpConn once Established ----
        let pending_handles: Vec<smoltcp::iface::SocketHandle> =
            inbound_pending.keys().copied().collect();
        for handle in pending_handles {
            let sock = sockets.get_mut::<tcp::Socket>(handle);
            if sock.state() == tcp::State::Established {
                let macos_sock = inbound_pending.remove(&handle).unwrap();
                let (to_smol_tx, to_smol_rx) = mpsc::channel::<ProxyMsg>();
                let (from_smol_tx, from_smol_rx) = mpsc::channel::<Vec<u8>>();
                let tx2 = to_smol_tx.clone();
                std::thread::Builder::new()
                    .name("tcp-inbound-bridge".into())
                    .spawn(move || inbound_bridge_thread(macos_sock, from_smol_rx, tx2))
                    .ok();
                tcp_conns.insert(
                    handle,
                    TcpConn {
                        rx: to_smol_rx,
                        tx: from_smol_tx,
                        pending_send: None,
                    },
                );
            } else if sock.state() == tcp::State::Closed || sock.state() == tcp::State::TimeWait {
                inbound_pending.remove(&handle);
                sockets.remove(handle);
            }
        }

        // ---- Outbound: promote accepted listeners to active TcpConn ----
        // A listener transitions out of Listen state when it accepts a SYN.
        let mut promoted: Vec<smoltcp::iface::SocketHandle> = vec![];
        listeners.retain(|&handle| {
            let sock = sockets.get::<tcp::Socket>(handle);
            if sock.state() != tcp::State::Listen {
                promoted.push(handle);
                false // remove from listeners
            } else {
                true // keep listening
            }
        });

        for handle in promoted {
            let sock = sockets.get_mut::<tcp::Socket>(handle);
            let remote = sock.remote_endpoint();
            let local = sock.local_endpoint();
            log::debug!(
                "nat_relay: TCP {} → {}",
                remote.map(|e| e.to_string()).unwrap_or_default(),
                local.map(|e| e.to_string()).unwrap_or_default()
            );
            if let Some(local_ep) = local {
                let dest_addr: SocketAddr = match local_ep.addr {
                    IpAddress::Ipv4(a) => SocketAddr::new(std::net::IpAddr::V4(a), local_ep.port),
                    #[allow(unreachable_patterns)]
                    _ => {
                        sock.abort();
                        sockets.remove(handle);
                        continue;
                    }
                };
                let (to_smol_tx, to_smol_rx) = mpsc::channel::<ProxyMsg>();
                let (from_smol_tx, from_smol_rx) = mpsc::channel::<Vec<u8>>();
                let tx2 = to_smol_tx.clone();
                std::thread::Builder::new()
                    .name(format!("tcp-proxy-{}", dest_addr))
                    .spawn(move || tcp_proxy_thread(dest_addr, from_smol_rx, tx2))
                    .ok();
                tcp_conns.insert(
                    handle,
                    TcpConn {
                        rx: to_smol_rx,
                        tx: from_smol_tx,
                        pending_send: None,
                    },
                );
            } else {
                // No local endpoint — connection failed before Established.
                sockets.remove(handle);
            }
        }

        // ---- TCP: service active connections ----
        let handles: Vec<smoltcp::iface::SocketHandle> = tcp_conns.keys().copied().collect();
        let mut to_remove: Vec<smoltcp::iface::SocketHandle> = vec![];
        for handle in handles {
            let sock = sockets.get_mut::<tcp::Socket>(handle);
            let conn = tcp_conns.get_mut(&handle).unwrap();

            if sock.can_recv() {
                let mut buf = vec![0u8; 4096];
                if let Ok(n) = sock.recv_slice(&mut buf) {
                    if n > 0 {
                        buf.truncate(n);
                        if conn.tx.send(buf).is_err() {
                            sock.close();
                        }
                    }
                }
            }

            // Flush any bytes left over from a previous partial send_slice.
            if let Some(pending) = conn.pending_send.take() {
                let n = sock.send_slice(&pending).unwrap_or(0);
                if n < pending.len() {
                    conn.pending_send = Some(pending[n..].to_vec());
                    // TX buffer still full — skip consuming more this cycle.
                }
            }

            // Consume from host-side channel and write to smoltcp TX buffer.
            // Each chunk may only be partially accepted if the buffer is near
            // full — save the remainder in pending_send rather than discarding.
            if conn.pending_send.is_none() {
                loop {
                    if !sock.can_send() {
                        break;
                    }
                    match conn.rx.try_recv() {
                        Ok(ProxyMsg::FromHost(data)) => {
                            let n = sock.send_slice(&data).unwrap_or(0);
                            if n < data.len() {
                                conn.pending_send = Some(data[n..].to_vec());
                                break;
                            }
                        }
                        Ok(ProxyMsg::HostClosed) | Err(mpsc::TryRecvError::Disconnected) => {
                            // Host side closed — initiate graceful FIN.  Do NOT
                            // remove the socket here; it is still in the smoltcp
                            // state machine (FIN_WAIT / CLOSE_WAIT).  Removal
                            // happens below once the socket reaches Closed /
                            // TimeWait, preventing an immediate RST on large
                            // transfers whose FIN handshake has not yet completed.
                            sock.close();
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                }
            }

            if sock.state() == tcp::State::Closed || sock.state() == tcp::State::TimeWait {
                to_remove.push(handle);
            }
        }
        for handle in to_remove {
            tcp_conns.remove(&handle);
            sockets.remove(handle);
        }

        let delay = iface
            .poll_delay(smol_now(), &sockets)
            .unwrap_or(smoltcp::time::Duration::from_millis(1));
        let sleep_ms = delay.millis().min(1) as u64;
        std::thread::sleep(Duration::from_millis(sleep_ms.max(1)));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the destination TCP port if `frame` is a TCP SYN (not SYN-ACK).
fn tcp_syn_dst_port(frame: &[u8]) -> Option<u16> {
    // Minimum: 14 (Ethernet) + 20 (IPv4) + 14 (TCP flags offset) = 48 bytes.
    if frame.len() < 48 {
        return None;
    }
    // Ethertype = IPv4.
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return None;
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    if frame.len() < 14 + ihl + 14 {
        return None;
    }
    // Protocol = TCP.
    if frame[14 + 9] != 6 {
        return None;
    }
    let tcp_off = 14 + ihl;
    let flags = frame[tcp_off + 13];
    let syn = (flags & 0x02) != 0;
    let ack = (flags & 0x10) != 0;
    // SYN only (not SYN-ACK).
    if !syn || ack {
        return None;
    }
    Some(u16::from_be_bytes([frame[tcp_off + 2], frame[tcp_off + 3]]))
}

fn add_tcp_listener_on_port(
    sockets: &mut SocketSet<'_>,
    port: u16,
) -> smoltcp::iface::SocketHandle {
    let rx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
    let tx_buf = tcp::SocketBuffer::new(vec![0u8; TCP_SEND_BUF]);
    let mut sock = tcp::Socket::new(rx_buf, tx_buf);
    sock.listen(IpListenEndpoint { addr: None, port }).ok();
    sockets.add(sock)
}

/// Host-side TCP proxy thread: connects to `dest`, relays data via channels.
fn tcp_proxy_thread(dest: SocketAddr, from_smol: Receiver<Vec<u8>>, to_smol: Sender<ProxyMsg>) {
    let stream = match TcpStream::connect_timeout(&dest, Duration::from_secs(10)) {
        Ok(s) => s,
        Err(e) => {
            log::debug!("nat_relay: TCP connect to {} failed: {}", dest, e);
            let _ = to_smol.send(ProxyMsg::HostClosed);
            return;
        }
    };

    let stream2 = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("nat_relay: TcpStream clone failed: {}", e);
            let _ = to_smol.send(ProxyMsg::HostClosed);
            return;
        }
    };

    let to_smol2 = to_smol.clone();

    std::thread::Builder::new()
        .name(format!("tcp-host-rx-{}", dest))
        .spawn(move || {
            let mut s = stream2;
            let mut buf = vec![0u8; 8192];
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = to_smol2.send(ProxyMsg::HostClosed);
                        break;
                    }
                    Ok(n) => {
                        if to_smol2
                            .send(ProxyMsg::FromHost(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .ok();

    let mut s = stream;
    for data in from_smol {
        if s.write_all(&data).is_err() {
            break;
        }
    }
    let _ = s.shutdown(std::net::Shutdown::Write);
}

/// Listen on 127.0.0.1:RELAY_PROXY_PORT for inbound port-forward requests.
fn inbound_proxy_listener(tx: Sender<(TcpStream, u16)>) {
    let listener = match TcpListener::bind(("127.0.0.1", RELAY_PROXY_PORT)) {
        Ok(l) => l,
        Err(e) => {
            log::error!(
                "nat_relay: failed to bind relay proxy port {}: {}",
                RELAY_PROXY_PORT,
                e
            );
            return;
        }
    };
    log::info!(
        "nat_relay: inbound proxy listening on 127.0.0.1:{}",
        RELAY_PROXY_PORT
    );
    for incoming in listener.incoming() {
        let mut sock = match incoming {
            Ok(s) => s,
            Err(e) => {
                log::warn!("nat_relay: inbound proxy accept: {}", e);
                continue;
            }
        };
        let mut port_bytes = [0u8; 2];
        if sock.read_exact(&mut port_bytes).is_err() {
            continue;
        }
        let container_port = u16::from_be_bytes(port_bytes);
        let _ = tx.send((sock, container_port));
    }
}

/// Bridge an already-connected macOS TcpStream to/from a smoltcp socket via channels.
fn inbound_bridge_thread(
    stream: TcpStream,
    from_smol: Receiver<Vec<u8>>,
    to_smol: Sender<ProxyMsg>,
) {
    let stream2 = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("nat_relay: inbound bridge clone failed: {}", e);
            let _ = to_smol.send(ProxyMsg::HostClosed);
            return;
        }
    };

    let to_smol2 = to_smol.clone();

    std::thread::Builder::new()
        .name("tcp-inbound-rx".into())
        .spawn(move || {
            let mut s = stream2;
            let mut buf = vec![0u8; 8192];
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = to_smol2.send(ProxyMsg::HostClosed);
                        break;
                    }
                    Ok(n) => {
                        if to_smol2
                            .send(ProxyMsg::FromHost(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .ok();

    let mut s = stream;
    for data in from_smol {
        if s.write_all(&data).is_err() {
            break;
        }
    }
    let _ = s.shutdown(std::net::Shutdown::Write);
}

/// Intercept a raw IPv4 UDP frame, proxy it to the real destination, and
/// synthesize a UDP reply frame back to the VM.  Returns true if the frame
/// was a UDP datagram and has been handled (caller must not push it to smoltcp).
///
/// smoltcp's `udp::Socket::bind(port=0)` returns `Err(Unaddressable)`, so we
/// cannot use smoltcp's UDP socket as a wildcard listener.  Intercepting UDP
/// at the raw frame level (as we do for ICMP) sidesteps the issue entirely.
fn handle_udp_frame(relay_fd: RawFd, frame: &[u8]) -> bool {
    // Ethernet(14) + IPv4(20) + UDP(8) = 42 bytes minimum.
    if frame.len() < 42 {
        return false;
    }
    // Ethertype = IPv4.
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    if frame.len() < 14 + ihl + 8 {
        return false;
    }
    // Protocol = UDP (17).
    if frame[14 + 9] != 17 {
        return false;
    }
    let udp_off = 14 + ihl;
    let udp_len = u16::from_be_bytes([frame[udp_off + 4], frame[udp_off + 5]]) as usize;
    if udp_len < 8 || udp_off + udp_len > frame.len() {
        return false;
    }

    let src_port = u16::from_be_bytes([frame[udp_off], frame[udp_off + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_off + 2], frame[udp_off + 3]]);
    let dst_ip: [u8; 4] = frame[14 + 16..14 + 20].try_into().unwrap();
    let src_ip: [u8; 4] = frame[14 + 12..14 + 16].try_into().unwrap();
    let payload = frame[udp_off + 8..udp_off + udp_len].to_vec();

    let dest_addr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(dst_ip)),
        dst_port,
    );

    let frame_owned = frame[..14 + ihl].to_vec(); // save Ethernet + IP header for reply

    std::thread::Builder::new()
        .name(format!("udp-raw-{}:{}", dest_addr.ip(), dst_port))
        .spawn(move || match udp_proxy_once(&payload, dest_addr) {
            Ok(reply) => send_udp_reply(
                relay_fd,
                &frame_owned,
                ihl,
                &reply,
                src_ip,
                dst_ip,
                src_port,
                dst_port,
            ),
            Err(e) => {
                log::debug!("nat_relay: UDP proxy to {} failed: {}", dest_addr, e);
            }
        })
        .ok();

    true
}

/// Synthesize a UDP reply Ethernet frame and send it back to the VM.
fn send_udp_reply(
    relay_fd: RawFd,
    orig_eth_ip_hdr: &[u8], // original Ethernet + IPv4 header bytes
    ihl: usize,
    reply_payload: &[u8],
    orig_src_ip: [u8; 4],
    orig_dst_ip: [u8; 4],
    orig_src_port: u16,
    orig_dst_port: u16,
) {
    let udp_len = 8 + reply_payload.len();
    let ip_total_len = ihl + udp_len;
    let total_len = 14 + ip_total_len;
    let mut reply = vec![0u8; total_len];

    // Ethernet: swap src/dst MAC.
    reply[..6].copy_from_slice(&orig_eth_ip_hdr[6..12]); // dst ← original src
    reply[6..12].copy_from_slice(&orig_eth_ip_hdr[..6]); // src ← original dst
    reply[12] = 0x08;
    reply[13] = 0x00;

    // IPv4 header: copy IHL/version/options from original, update length + addrs.
    reply[14..14 + ihl].copy_from_slice(&orig_eth_ip_hdr[14..14 + ihl]);
    let ip_total_u16 = ip_total_len as u16;
    reply[16] = (ip_total_u16 >> 8) as u8;
    reply[17] = (ip_total_u16 & 0xff) as u8;
    // Clear identification, flags, frag-offset.
    reply[18] = 0;
    reply[19] = 0;
    reply[20] = 0;
    reply[21] = 0;
    reply[22] = 64; // TTL
    reply[23] = 17; // UDP
                    // Swap IP addresses.
    reply[26..30].copy_from_slice(&orig_dst_ip); // src ← original dst
    reply[30..34].copy_from_slice(&orig_src_ip); // dst ← original src
                                                 // Recompute IP header checksum.
    reply[24] = 0;
    reply[25] = 0;
    let ip_cksum = inet_checksum(&reply[14..14 + ihl]);
    reply[24] = (ip_cksum >> 8) as u8;
    reply[25] = (ip_cksum & 0xff) as u8;

    // UDP header: swap ports, set length, zero checksum (valid for IPv4).
    let udp_off = 14 + ihl;
    reply[udp_off] = (orig_dst_port >> 8) as u8;
    reply[udp_off + 1] = (orig_dst_port & 0xff) as u8;
    reply[udp_off + 2] = (orig_src_port >> 8) as u8;
    reply[udp_off + 3] = (orig_src_port & 0xff) as u8;
    let udp_len_u16 = udp_len as u16;
    reply[udp_off + 4] = (udp_len_u16 >> 8) as u8;
    reply[udp_off + 5] = (udp_len_u16 & 0xff) as u8;
    reply[udp_off + 6] = 0; // checksum = 0 (disabled, valid in IPv4)
    reply[udp_off + 7] = 0;
    reply[udp_off + 8..].copy_from_slice(reply_payload);

    unsafe {
        libc::send(relay_fd, reply.as_ptr() as _, reply.len(), 0);
    }
}

/// Send a single UDP datagram to `dest` and return the reply (best-effort).
fn udp_proxy_once(data: &[u8], dest: SocketAddr) -> Result<Vec<u8>, std::io::Error> {
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let sock = UdpSocket::bind(bind_addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    sock.send_to(data, dest)?;
    let mut buf = vec![0u8; 8192];
    let (n, _) = sock.recv_from(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// ICMP echo reply synthesizer
// ---------------------------------------------------------------------------

/// If `frame` is an IPv4 ICMP echo request (type 8), synthesize an echo reply
/// and write it back to `relay_fd`.  Returns true if the frame was handled.
fn icmp_echo_reply(relay_fd: RawFd, frame: &[u8]) -> bool {
    if frame.len() < 42 {
        return false;
    }
    if frame[12] != 0x08 || frame[13] != 0x00 {
        return false;
    }
    let ihl = ((frame[14] & 0x0f) as usize) * 4;
    if frame.len() < 14 + ihl + 8 {
        return false;
    }
    if frame[14 + 9] != 1 {
        return false;
    }
    let icmp_off = 14 + ihl;
    if frame[icmp_off] != 8 || frame[icmp_off + 1] != 0 {
        return false;
    }

    let mut reply = frame.to_vec();

    reply.copy_within(0..6, 6);
    reply[..6].copy_from_slice(&frame[6..12]);
    reply[6..12].copy_from_slice(&frame[0..6]);

    let src_off = 14 + 12;
    let dst_off = 14 + 16;
    let src_ip: [u8; 4] = frame[src_off..src_off + 4].try_into().unwrap();
    let dst_ip: [u8; 4] = frame[dst_off..dst_off + 4].try_into().unwrap();
    reply[src_off..src_off + 4].copy_from_slice(&dst_ip);
    reply[dst_off..dst_off + 4].copy_from_slice(&src_ip);

    reply[14 + 8] = 64;

    reply[14 + 10] = 0;
    reply[14 + 11] = 0;
    let hdr_cksum = inet_checksum(&reply[14..14 + ihl]);
    reply[14 + 10] = (hdr_cksum >> 8) as u8;
    reply[14 + 11] = (hdr_cksum & 0xff) as u8;

    reply[icmp_off] = 0;
    reply[icmp_off + 2] = 0;
    reply[icmp_off + 3] = 0;
    let icmp_cksum = inet_checksum(&reply[icmp_off..]);
    reply[icmp_off + 2] = (icmp_cksum >> 8) as u8;
    reply[icmp_off + 3] = (icmp_cksum & 0xff) as u8;

    unsafe {
        libc::send(relay_fd, reply.as_ptr() as _, reply.len(), 0);
    }
    true
}

/// One's complement Internet checksum (RFC 1071).
fn inet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// socketpair helpers
// ---------------------------------------------------------------------------

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
