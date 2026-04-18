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
use smoltcp::wire::{
    EthernetAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv6Address,
};
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
/// `relay_proxy_port` is the loopback TCP port this relay will bind on macOS.
/// Each VM profile must use a distinct port so that multiple profiles can run
/// simultaneously without one hijacking the other's port-forward connections.
/// Use [`relay_proxy_port_for_profile`] in `pelagos-mac` to derive the port
/// from the profile name, or pass [`RELAY_PROXY_PORT`] for the default profile.
///
/// Returns `(avf_fd, relay)`:
/// - `avf_fd` is one end of a `socketpair(AF_UNIX, SOCK_DGRAM)` ready to be
///   wrapped in `NSFileHandle` and passed to `VZFileHandleNetworkDeviceAttachment`.
/// - `relay` holds the relay thread. Drop it to initiate shutdown.
pub fn start(relay_proxy_port: u16) -> Result<(RawFd, RelayHandle), crate::Error> {
    let (avf_fd, relay_fd) = create_socketpair()?;

    // Buffer sizes: 128 KB send / 512 KB recv per AVF documentation.
    const SNDBUF: c_int = 128 * 1024;
    const RCVBUF: c_int = 512 * 1024;
    set_sock_bufs(avf_fd, SNDBUF, RCVBUF);
    set_sock_bufs(relay_fd, SNDBUF, RCVBUF);

    // Channel for inbound port-forward requests from the relay proxy port.
    let (inbound_tx, inbound_rx) = mpsc::channel::<(TcpStream, u16)>();

    // Spawn the inbound proxy listener on the per-profile relay port.
    std::thread::Builder::new()
        .name("nat-relay-proxy-listener".into())
        .spawn(move || inbound_proxy_listener(relay_proxy_port, inbound_tx))
        .expect("spawn nat-relay-proxy-listener");

    let thread = std::thread::Builder::new()
        .name("nat-relay".into())
        .spawn(move || run_relay(relay_fd, relay_proxy_port, inbound_rx))
        .expect("spawn nat-relay");

    log::info!(
        "nat_relay: started (avf_fd={}, proxy_port={})",
        avf_fd,
        relay_proxy_port
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
    /// VM's MAC address, learned from the source MAC of the first received frame.
    /// VZ's virtual switch only forwards unicast frames back to the VM (broadcast
    /// and multicast sent by the relay are dropped by the switch).  Once we have
    /// the guest MAC we use it as the Ethernet dst for keepalive and NDP frames.
    guest_mac: Option<[u8; 6]>,
    /// Dynamic gateway IPv6 link-local, computed from guest_mac via
    /// `gateway_ip6_for_vm_mac`.  None until the first MAC is learned.
    gateway_ip6_ll: Option<[u8; 16]>,
    /// Solicited-node multicast MAC for gateway_ip6_ll.  None until first MAC.
    gateway_ip6_mcast_mac: Option<[u8; 6]>,
}

impl AvfDevice {
    fn new(relay_fd: RawFd) -> Self {
        Self {
            relay_fd,
            rx_buf: vec![0u8; 64 * 1024],
            pending_frames: VecDeque::new(),
            guest_mac: None,
            gateway_ip6_ll: None,
            gateway_ip6_mcast_mac: None,
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
        // Handle NDP and ICMPv6 echo inline (same as pre_scan_frames) so they
        // are not handed to smoltcp, which cannot resolve the guest MAC via NDP
        // (VZ MLD snooping blocks solicited-node NS from reaching the relay).
        // Buffer everything else for the next cycle so SYN detection can run.
        let r = unsafe {
            libc::recv(
                self.relay_fd,
                self.rx_buf.as_mut_ptr() as _,
                self.rx_buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if r > 0 {
            let frame = self.rx_buf[..r as usize].to_vec();
            if frame.len() >= 14 {
                let et = u16::from_be_bytes([frame[12], frame[13]]);
                log::trace!("nat_relay: recv-during-poll frame len={} ethertype=0x{:04x} dst_mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    r, et, frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]);
            }
            if !ndp_neighbor_advertisement(self.relay_fd, &frame,
                    self.gateway_ip6_ll.as_ref(), self.gateway_ip6_mcast_mac.as_ref())
                && !icmpv6_echo_reply(self.relay_fd, &frame, self.gateway_ip6_ll.as_ref())
            {
                self.pending_frames.push_back(frame);
            }
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

/// Gateway IPv4: the relay pretends to be a router at this address.
const GATEWAY_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 105, 1);
/// Fabricated MAC for the gateway (locally administered, unicast).
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Gateway link-local IPv6 address derived from GATEWAY_MAC via EUI-64.
///
/// MAC  02:00:00:00:00:01
///   Flip U/L bit (bit 1 of first byte): 0x02 → 0x00
///   Insert ff:fe:  00:00:00:ff:fe:00:00:01
///   Prepend fe80:  fe80::00ff:fe00:0001  (i.e. fe80::ff:fe00:1)
const GATEWAY_IP6_LL: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0x00ff, 0xfe00, 0x0000, 0x0001);
/// Solicited-node multicast MAC for GATEWAY_IP6_ULA (fd00::1).
/// Last 3 bytes of fd00::1 are 00:00:01 → 33:33:ff:00:00:01.
/// VZ's virtual switch delivers frames sent to this multicast MAC to the relay,
/// so we advertise it as the TLLA in NA replies for fd00::1 (same trick as the LL gateway).
#[allow(dead_code)] // used in pre_scan_frames — referenced by name
const ULA_MCAST_MAC: [u8; 6] = [0x33, 0x33, 0xff, 0x00, 0x00, 0x01];

/// ULA (Unique Local Address) gateway address: fd00::1/64.
/// The VM is assigned fd00::2/64 in the initramfs init script.  smoltcp
/// responds to NDP and ICMPv6 echo for this address automatically once
/// it is added to the interface.
pub(crate) const GATEWAY_IP6_ULA: Ipv6Address = Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
/// Raw bytes of GATEWAY_IP6_ULA for use in Ethernet frame construction (smoltcp's
/// Ipv6Address wraps std::net::Ipv6Addr whose fields are private).
const GATEWAY_IP6_ULA_BYTES: [u8; 16] = [0xfd, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
/// Compute the dynamic gateway IPv6 link-local address for a given VM MAC.
///
/// VZ's virtual switch uses MLD snooping: it only delivers IPv6 multicast to
/// a port if that port has sent an MLD Membership Report for that group.  The
/// relay's own MLD Reports are ignored (relay is on the "router port").  The
/// VM, however, automatically joins the solicited-node multicast group for its
/// own link-local address — and VZ delivers that group to the relay too.
///
/// By choosing a gateway address whose last 3 bytes match VM_MAC[3..6], the
/// gateway's solicited-node group is identical to the VM's own group.  VZ then
/// delivers both NDP NS frames and ICMPv6 echo requests (sent to the gateway's
/// solicited-node multicast MAC) to the relay.
///
/// Formula: fe80::00ff:fe{MAC[3]}:{MAC[4]}{MAC[5]}
fn gateway_ip6_for_vm_mac(vm_mac: [u8; 6]) -> [u8; 16] {
    [0xfe, 0x80, 0, 0, 0, 0, 0, 0,
     0x00, 0x00, 0x00, 0xff, 0xfe, vm_mac[3], vm_mac[4], vm_mac[5]]
}

/// Compute the solicited-node multicast MAC for the dynamic gateway LL.
/// Result: 33:33:ff:{VM_MAC[3]}:{VM_MAC[4]}:{VM_MAC[5]}
fn gateway_ip6_mcast_mac_for_vm_mac(vm_mac: [u8; 6]) -> [u8; 6] {
    [0x33, 0x33, 0xff, vm_mac[3], vm_mac[4], vm_mac[5]]
}

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
        if frame.len() >= 14 {
            let et = u16::from_be_bytes([frame[12], frame[13]]);
            log::trace!("nat_relay: recv frame len={} ethertype=0x{:04x} dst_mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                len, et, frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]);
            // Learn guest MAC from ARP frames (Ethertype 0x0806).
            // ARP is the first unicast protocol the VM uses — the src MAC in the
            // ARP request is definitely the VM's real eth0 MAC.
            // Do NOT learn from MLD (0x86dd with dst 33:33:00:00:00:16) because
            // VZ sends MLD Membership Reports with its own internal proxy MAC.
            if et == 0x0806 {
                let src: [u8; 6] = frame[6..12].try_into().unwrap();
                if src[0] & 0x01 == 0 && src != [0u8; 6] && device.guest_mac != Some(src) {
                    log::info!("nat_relay: learned guest MAC from ARP: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        src[0], src[1], src[2], src[3], src[4], src[5]);
                    device.guest_mac = Some(src);
                    let gw6 = gateway_ip6_for_vm_mac(src);
                    log::info!("nat_relay: gateway IPv6 LL = fe80::00ff:fe{:02x}:{:02x}{:02x}",
                        src[3], src[4], src[5]);
                    device.gateway_ip6_ll = Some(gw6);
                    device.gateway_ip6_mcast_mac = Some(gateway_ip6_mcast_mac_for_vm_mac(src));
                }
            }
        }

        // Handle ICMP echo requests inline — reply and discard.
        if icmp_echo_reply(device.relay_fd, &frame) {
            continue;
        }
        // Handle ICMPv6 echo requests inline — reply and discard.
        if icmpv6_echo_reply(device.relay_fd, &frame, device.gateway_ip6_ll.as_ref()) {
            continue;
        }
        // Handle NDP Neighbor Solicitations targeting the gateway LL — reply and discard.
        if ndp_neighbor_advertisement(device.relay_fd, &frame,
                device.gateway_ip6_ll.as_ref(), device.gateway_ip6_mcast_mac.as_ref()) {
            continue;
        }
        // Handle NDP Neighbor Solicitations targeting the gateway ULA (fd00::1) — reply and discard.
        // smoltcp receives these NS frames but does not generate NA responses reliably due to
        // VZ MLD snooping: multicast delivery requires the relay to have joined the solicited-node
        // group ff02::1:ff00:0001, which smoltcp's MLD joins cannot accomplish here.
        // We handle it manually, advertising ULA_MCAST_MAC as TLLA so VZ delivers subsequent
        // packets to fd00::1 via the multicast Ethernet address the relay already receives.
        if ndp_neighbor_advertisement(device.relay_fd, &frame,
                Some(&GATEWAY_IP6_ULA_BYTES), Some(&ULA_MCAST_MAC)) {
            continue;
        }
        // Detect DAD NS (source IPv6 = ::, ICMPv6 type = 135).
        // The VM sends DAD when its link-local address comes up.  This frame
        // reaches the relay because the VM has joined its own solicited-node
        // multicast group (VZ MLD snooping forwards it).  Use it as a timing
        // trigger to immediately send a UNA for the gateway so the VM's
        // neighbor cache is seeded before it first tries to contact fe80::ff:fe00:1.
        //
        // IMPORTANT: VZ also sends MLD Membership Reports on behalf of the VM
        // using an internal proxy MAC (different from the VM's eth0 MAC).
        // Guest MAC learned from MLD frames would be wrong.  The NDP DAD frame
        // itself is sent by the VM's kernel with the actual eth0 MAC, so we
        // always extract the src MAC from the DAD frame directly and update
        // device.guest_mac to ensure the UNA is unicast to the right address.
        if is_dad_ns(&frame) {
            if frame.len() >= 12 {
                let ndp_src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
                if ndp_src_mac[0] & 0x01 == 0 && ndp_src_mac != [0u8; 6] {
                    if device.guest_mac != Some(ndp_src_mac) {
                        log::info!("nat_relay: updated guest MAC from DAD NS: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                            ndp_src_mac[0], ndp_src_mac[1], ndp_src_mac[2],
                            ndp_src_mac[3], ndp_src_mac[4], ndp_src_mac[5]);
                        device.guest_mac = Some(ndp_src_mac);
                        let gw6 = gateway_ip6_for_vm_mac(ndp_src_mac);
                        log::info!("nat_relay: gateway IPv6 LL = fe80::00ff:fe{:02x}:{:02x}{:02x} (from DAD NS)",
                            ndp_src_mac[3], ndp_src_mac[4], ndp_src_mac[5]);
                        device.gateway_ip6_ll = Some(gw6);
                        device.gateway_ip6_mcast_mac = Some(gateway_ip6_mcast_mac_for_vm_mac(ndp_src_mac));
                    }
                }
            }
            log::debug!("nat_relay: DAD NS detected — sending immediate NDP NA for gateway (LL + ULA)");
            if let (Some(gw6), Some(gw_mcast_mac)) = (device.gateway_ip6_ll, device.gateway_ip6_mcast_mac) {
                send_ndp_unsolicited_na(device.relay_fd, device.guest_mac, &gw6, &gw_mcast_mac);
            }
            // Also seed the VM neighbor cache for the ULA gateway (fd00::1).
            send_ndp_unsolicited_na(device.relay_fd, device.guest_mac, &GATEWAY_IP6_ULA_BYTES, &ULA_MCAST_MAC);
        }

        // Handle UDP datagrams inline — proxy to real host, reply in thread.
        // smoltcp udp::Socket::bind(port=0) returns Err(Unaddressable), so we
        // bypass smoltcp entirely for UDP and handle frames raw.
        if handle_udp_frame(device.relay_fd, &frame) {
            continue;
        }
        if handle_udp_frame_v6(device.relay_fd, &frame) {
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

/// Construct and transmit a gratuitous ARP request ("who has VM_IP? tell GATEWAY_IP")
/// directly to the relay fd (VM side of the socketpair).
///
/// Purpose: smoltcp's neighbor cache has a 60-second expiry (hardcoded in the library).
/// Ubuntu's systemd-networkd starts managing eth0 at approximately the same time,
/// creating a race where the ARP re-request arrives while the interface is briefly
/// in flux.  Sending a keepalive every 45 s ensures the cache entry is refreshed
/// *before* expiry, so the 60-second window never falls inside the networkd startup
/// period.
fn send_arp_keepalive(relay_fd: RawFd) {
    const VM_IP: [u8; 4] = [192, 168, 105, 2];
    const GW_IP: [u8; 4] = [192, 168, 105, 1];
    let gw_mac = GATEWAY_MAC.0;

    // 14-byte Ethernet header + 28-byte ARP payload = 42 bytes.
    let mut f = [0u8; 42];

    // Ethernet: broadcast dst, gateway src, Ethertype 0x0806 (ARP).
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&gw_mac);
    f[12] = 0x08;
    f[13] = 0x06;

    // ARP: Ethernet / IPv4 / request.
    f[14] = 0x00;
    f[15] = 0x01; // HW type = Ethernet
    f[16] = 0x08;
    f[17] = 0x00; // Proto type = IPv4
    f[18] = 6; // HW addr len
    f[19] = 4; // Proto addr len
    f[20] = 0x00;
    f[21] = 0x01; // Operation = Request
    f[22..28].copy_from_slice(&gw_mac); // sender MAC = gateway
    f[28..32].copy_from_slice(&GW_IP); // sender IP  = gateway
    f[32..38].copy_from_slice(&[0u8; 6]); // target MAC = unknown
    f[38..42].copy_from_slice(&VM_IP); // target IP  = VM

    unsafe {
        libc::send(relay_fd, f.as_ptr() as _, f.len(), 0);
    }
    log::debug!("nat_relay: sent ARP keepalive for 192.168.105.2");
}

/// Send an Unsolicited Neighbor Advertisement (UNA) for GATEWAY_IP6_LL to
/// the all-nodes multicast address (ff02::1).
///
/// Apple's VZ virtual switch performs MLD snooping.  If no node on the relay
/// side has joined the solicited-node multicast group ff02::1:ff00:1, the
/// switch will drop Neighbor Solicitation frames destined to that group before
/// they reach the relay socket.  Sending periodic UNAs proactively populates
/// the VM kernel's neighbor cache so it never needs to send an NS for the
/// gateway link-local address in the first place — mirroring how IPv4 uses
/// gratuitous ARP.
///
/// UNA format: NA (type 136) with S=0 (unsolicited), O=1 (override),
/// target = GATEWAY_IP6_LL, Target Link-Layer Address option = GATEWAY_MAC.
fn send_ndp_unsolicited_na(relay_fd: RawFd, guest_mac: Option<[u8; 6]>, gw6: &[u8; 16], gw_mcast_mac: &[u8; 6]) {
    let gw_mac = GATEWAY_MAC.0;
    let gw6 = *gw6;

    // Prefer unicast Ethernet dst (guest MAC) so the frame is not subject to
    // VZ's virtual-switch multicast/broadcast filtering.  Fall back to the
    // all-nodes multicast address if the guest MAC is not yet known.
    let all_nodes_mac: [u8; 6] = [0x33, 0x33, 0x00, 0x00, 0x00, 0x01];
    let eth_dst = guest_mac.unwrap_or(all_nodes_mac);

    let all_nodes_ip6: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

    // 14 (Eth) + 40 (IPv6) + 32 (ICMPv6 NA with TLLA option) = 86 bytes.
    let icmpv6_len: u16 = 32;
    let mut f = vec![0u8; 86];

    // Ethernet.
    f[0..6].copy_from_slice(&eth_dst);
    f[6..12].copy_from_slice(&gw_mac);
    f[12] = 0x86;
    f[13] = 0xdd;

    // IPv6 header.
    f[14] = 0x60;
    f[18] = (icmpv6_len >> 8) as u8;
    f[19] = (icmpv6_len & 0xff) as u8;
    f[20] = 58;  // ICMPv6
    f[21] = 255; // hop limit
    f[22..38].copy_from_slice(&gw6);      // src = GATEWAY_IP6_LL
    f[38..54].copy_from_slice(&all_nodes_ip6); // dst = ff02::1

    // ICMPv6 NA: type=136, code=0, flags O=1 (unsolicited: S=0).
    let na_off = 54;
    f[na_off] = 136;
    f[na_off + 4] = 0x20; // O flag only (bit 29 of 32-bit flags field)
    f[na_off + 8..na_off + 24].copy_from_slice(&gw6); // target
    // Advertise dynamic gateway multicast MAC as TLLA (see ndp_neighbor_advertisement for rationale).
    f[na_off + 24] = 2; // option: Target Link-Layer Address
    f[na_off + 25] = 1; // length = 1 (8 bytes)
    f[na_off + 26..na_off + 32].copy_from_slice(gw_mcast_mac);

    // ICMPv6 pseudo-header checksum.
    let mut pseudo: Vec<u8> = Vec::with_capacity(40 + 32);
    pseudo.extend_from_slice(&gw6);
    pseudo.extend_from_slice(&all_nodes_ip6);
    pseudo.extend_from_slice(&(icmpv6_len as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
    pseudo.extend_from_slice(&f[na_off..]);
    let cksum = inet_checksum(&pseudo);
    f[na_off + 2] = (cksum >> 8) as u8;
    f[na_off + 3] = (cksum & 0xff) as u8;

    let ret = unsafe { libc::send(relay_fd, f.as_ptr() as _, f.len(), 0) };
    if ret < 0 {
        log::debug!("nat_relay: send NDP unsolicited NA failed: {}", std::io::Error::last_os_error());
    } else {
        log::debug!("nat_relay: sent NDP unsolicited NA (eth_dst={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
            eth_dst[0], eth_dst[1], eth_dst[2], eth_dst[3], eth_dst[4], eth_dst[5]);
    }
}

fn run_relay(relay_fd: RawFd, relay_proxy_port: u16, inbound_rx: Receiver<(TcpStream, u16)>) {
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
        // Link-local IPv6 address derived from GATEWAY_MAC via EUI-64:
        //   MAC  02:00:00:00:00:01
        //   → flip U/L bit: 00:00:00:ff:fe:00:00:01
        //   → fe80::ff:fe00:1
        // smoltcp handles NDP (Neighbor Solicitation/Advertisement) automatically
        // once a link-local address is configured — no manual NS/NA synthesis needed.
        addrs
            .push(IpCidr::new(IpAddress::Ipv6(GATEWAY_IP6_LL), 64))
            .ok();
        // ULA address fd00::1/64.  The VM is assigned fd00::2/64 in the
        // initramfs init script.  smoltcp answers NDP and ICMPv6 echo for
        // this address automatically.
        addrs
            .push(IpCidr::new(IpAddress::Ipv6(GATEWAY_IP6_ULA), 64))
            .ok();
    });
    // any_ip=true alone is not enough: smoltcp also requires a route to the
    // destination that resolves to one of our own IPs.  A default route
    // pointing back to ourselves satisfies this for all external destinations.
    iface
        .routes_mut()
        .add_default_ipv4_route(GATEWAY_IP)
        .expect("add default IPv4 route");
    iface
        .routes_mut()
        .add_default_ipv6_route(GATEWAY_IP6_LL)
        .expect("add default IPv6 route");

    let mut sockets = SocketSet::new(vec![]);

    // Active connections: smoltcp SocketHandle → TcpConn.
    let mut tcp_conns: HashMap<smoltcp::iface::SocketHandle, TcpConn> = HashMap::new();

    // Current listener sockets (created dynamically on first SYN for each port).
    let mut listeners: Vec<smoltcp::iface::SocketHandle> = vec![];

    // Inbound (macOS→VM): smoltcp connect sockets waiting to reach Established.
    // Stores the macOS TcpStream and the insertion timestamp; stale entries
    // (older than INBOUND_PENDING_TTL) are pruned unconditionally so that
    // repeated ping_ssh retries during the boot ARP-resolution window don't
    // accumulate zombie sockets that flood sshd once ARP resolves.
    let mut inbound_pending: HashMap<
        smoltcp::iface::SocketHandle,
        (TcpStream, std::time::Instant),
    > = HashMap::new();
    /// Max age of a pending inbound smoltcp socket before it is aborted.
    /// Slightly longer than ping_ssh's ConnectTimeout (30 s) so that a live
    /// connection is never pruned while its ssh-relay-proxy is still running.
    const INBOUND_PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(40);
    let mut next_local_port: u16 = 49152;

    // ARP keepalive: smoltcp's neighbor cache expires every 60 s.  Send a
    // proactive ARP request to the VM every 45 s so the cache is always
    // refreshed before the expiry window aligns with networkd startup.
    const ARP_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(45);
    let mut last_arp_keepalive = std::time::Instant::now()
        .checked_sub(ARP_KEEPALIVE_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);

    // NDP unsolicited NA: Periodically send an unsolicited NA so the VM's
    // neighbor cache is seeded for the gateway link-local address.  Sent
    // only after the gateway LL is computed from the VM MAC.  First NA at
    // T+3 s and every 30 s thereafter (Linux base_reachable_time).
    const NDP_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const NDP_KEEPALIVE_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(3);
    let ndp_keepalive_start = std::time::Instant::now() + NDP_KEEPALIVE_INITIAL_DELAY;
    let mut last_ndp_keepalive: std::time::Instant =
        ndp_keepalive_start - NDP_KEEPALIVE_INTERVAL;

    log::info!(
        "nat_relay: poll loop started (proxy_port={})",
        relay_proxy_port
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
                        inbound_pending.insert(handle, (macos_sock, std::time::Instant::now()));
                        log::info!(
                            "nat_relay: inbound queued -> 192.168.105.2:{} (pending={})",
                            container_port,
                            inbound_pending.len()
                        );
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // ---- Inbound pending: prune stale + promote Established ----
        // Repeated ping_ssh retries during the boot ARP-resolution window each
        // add a new smoltcp SYN_SENT socket.  When ARP finally resolves they
        // all connect simultaneously, flooding sshd.  Prune any entry that has
        // been waiting longer than INBOUND_PENDING_TTL (slightly longer than
        // ping_ssh's ConnectTimeout=30s) — by that point ssh-relay-proxy has
        // definitely exited and the entry is stale.
        let pending_handles: Vec<smoltcp::iface::SocketHandle> =
            inbound_pending.keys().copied().collect();
        for handle in pending_handles {
            let age = inbound_pending[&handle].1.elapsed();
            if age > INBOUND_PENDING_TTL {
                inbound_pending.remove(&handle);
                sockets.get_mut::<tcp::Socket>(handle).abort();
                sockets.remove(handle);
                log::info!("nat_relay: pruned stale inbound pending ({:.1?} old)", age);
                continue;
            }

            let sock = sockets.get_mut::<tcp::Socket>(handle);
            if sock.state() == tcp::State::Established {
                log::info!(
                    "nat_relay: inbound pending -> established (age {:.1?})",
                    age
                );
                let (macos_sock, _) = inbound_pending.remove(&handle).unwrap();
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
                log::info!(
                    "nat_relay: inbound pending closed ({:?}, age {:.1?}) — port likely not open yet",
                    sock.state(), age
                );
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

        // ARP keepalive: proactive ARP request to refresh smoltcp's cache.
        if last_arp_keepalive.elapsed() >= ARP_KEEPALIVE_INTERVAL {
            send_arp_keepalive(device.relay_fd);
            last_arp_keepalive = std::time::Instant::now();
        }

        // NDP keepalive: unsolicited NA for LL + ULA to seed the VM's IPv6 neighbor cache.
        if std::time::Instant::now() >= ndp_keepalive_start
            && last_ndp_keepalive.elapsed() >= NDP_KEEPALIVE_INTERVAL
        {
            if let (Some(gw6), Some(gw_mcast_mac)) = (device.gateway_ip6_ll, device.gateway_ip6_mcast_mac) {
                send_ndp_unsolicited_na(device.relay_fd, device.guest_mac, &gw6, &gw_mcast_mac);
            }
            send_ndp_unsolicited_na(device.relay_fd, device.guest_mac, &GATEWAY_IP6_ULA_BYTES, &ULA_MCAST_MAC);
            last_ndp_keepalive = std::time::Instant::now();
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

/// Listen on `127.0.0.1:port` for inbound port-forward requests.
///
/// Each VM profile binds its own port so that multiple profiles can run
/// simultaneously.  If the bind fails with EADDRINUSE, another pelagos daemon
/// (possibly a different profile) is already holding that port.
fn inbound_proxy_listener(port: u16, tx: Sender<(TcpStream, u16)>) {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!(
                "nat_relay: failed to bind relay proxy port {}: {} \
                 — another pelagos daemon may already be using this port; \
                 run 'pelagos vm stop' for the conflicting profile first",
                port,
                e
            );
            return;
        }
    };
    log::info!("nat_relay: inbound proxy listening on 127.0.0.1:{}", port);
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
#[allow(clippy::too_many_arguments)]
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
// IPv6 UDP raw handler (Phase 3)
// ---------------------------------------------------------------------------

/// If `frame` is an IPv6 UDP datagram, proxy it to the real host destination
/// and send the reply back to the VM.  Returns true if the frame was consumed.
///
/// IPv6 UDP checksum is mandatory (RFC 2460 §8.1).  Extension headers are not
/// supported — frames with a next-header other than 17 (UDP) pass through.
fn handle_udp_frame_v6(relay_fd: RawFd, frame: &[u8]) -> bool {
    // Ethernet(14) + IPv6(40) + UDP(8) = 62 bytes minimum.
    if frame.len() < 62 {
        return false;
    }
    if frame[12] != 0x86 || frame[13] != 0xdd {
        return false;
    }
    // IPv6 next header: 17 = UDP.  Extension headers are not handled.
    if frame[14 + 6] != 17 {
        return false;
    }
    let udp_off = 14 + 40; // = 54
    let udp_len = u16::from_be_bytes([frame[udp_off + 4], frame[udp_off + 5]]) as usize;
    if udp_len < 8 || udp_off + udp_len > frame.len() {
        return false;
    }

    let src_port = u16::from_be_bytes([frame[udp_off],     frame[udp_off + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_off + 2], frame[udp_off + 3]]);
    let src_ip: [u8; 16] = frame[14 +  8..14 + 24].try_into().unwrap();
    let dst_ip: [u8; 16] = frame[14 + 24..14 + 40].try_into().unwrap();
    let payload = frame[udp_off + 8..udp_off + udp_len].to_vec();

    let dest_addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::from(dst_ip)),
        dst_port,
    );

    log::info!("nat_relay: IPv6 UDP frame: src_port={} dst={}", src_port, dest_addr);

    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
    let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();

    // UDP destined to the relay's own ULA address (fd00::1) is handled locally.
    // fd00::1 is the relay endpoint — there is no external host to proxy to.
    // Echo the payload back so the VM can use fd00::1 as a reachability probe.
    if dst_ip == GATEWAY_IP6_ULA_BYTES {
        log::info!("nat_relay: UDP6 to fd00::1 — echoing locally");
        send_udp_reply_v6(relay_fd, &src_mac, &dst_mac, &payload,
                          src_ip, dst_ip, src_port, dst_port);
        return true;
    }

    std::thread::Builder::new()
        .name(format!("udp6-{dst_port}"))
        .spawn(move || match udp_proxy_once_v6(&payload, dest_addr) {
            Ok(reply) => send_udp_reply_v6(
                relay_fd, &src_mac, &dst_mac, &reply,
                src_ip, dst_ip, src_port, dst_port,
            ),
            Err(e) => log::debug!("nat_relay: UDP6 proxy to {} failed: {}", dest_addr, e),
        })
        .ok();

    true
}

/// Synthesize an IPv6 UDP reply Ethernet frame and send it back to the VM.
#[allow(clippy::too_many_arguments)]
fn send_udp_reply_v6(
    relay_fd: RawFd,
    orig_src_mac: &[u8; 6],   // Ethernet src of the original request (VM MAC)
    orig_dst_mac: &[u8; 6],   // Ethernet dst of the original request (relay MAC)
    reply_payload: &[u8],
    orig_src_ip: [u8; 16],
    orig_dst_ip: [u8; 16],
    orig_src_port: u16,
    orig_dst_port: u16,
) {
    let udp_len = 8 + reply_payload.len();
    let mut reply = vec![0u8; 14 + 40 + udp_len];

    // Ethernet: swap src/dst MACs.
    reply[..6].copy_from_slice(orig_src_mac);   // dst ← VM MAC
    reply[6..12].copy_from_slice(orig_dst_mac); // src ← relay MAC
    reply[12] = 0x86;
    reply[13] = 0xdd;

    // IPv6 header (fixed 40 bytes).
    reply[14] = 0x60; // version=6, TC=0, flow=0
    let payload_len = udp_len as u16;
    reply[18] = (payload_len >> 8) as u8;
    reply[19] = (payload_len & 0xff) as u8;
    reply[20] = 17;  // next header = UDP
    reply[21] = 64;  // hop limit
    reply[22..38].copy_from_slice(&orig_dst_ip); // src ← original dst
    reply[38..54].copy_from_slice(&orig_src_ip); // dst ← original src

    // UDP header.
    let udp_off = 54;
    reply[udp_off]     = (orig_dst_port >> 8) as u8;
    reply[udp_off + 1] = (orig_dst_port & 0xff) as u8;
    reply[udp_off + 2] = (orig_src_port >> 8) as u8;
    reply[udp_off + 3] = (orig_src_port & 0xff) as u8;
    reply[udp_off + 4] = (payload_len >> 8) as u8;
    reply[udp_off + 5] = (payload_len & 0xff) as u8;
    // Checksum slot zeroed; computed below.
    reply[udp_off + 8..].copy_from_slice(reply_payload);

    // IPv6 UDP checksum is mandatory (RFC 2460 §8.1, RFC 768).
    // Pseudo-header: new-src(16) + new-dst(16) + UDP-length(4) + 0x00 0x00 0x00 17
    let mut pseudo: Vec<u8> = Vec::with_capacity(40 + udp_len);
    pseudo.extend_from_slice(&orig_dst_ip);                        // new src
    pseudo.extend_from_slice(&orig_src_ip);                        // new dst
    pseudo.extend_from_slice(&(udp_len as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 17u8]);
    pseudo.extend_from_slice(&reply[udp_off..]);
    let cksum = inet_checksum(&pseudo);
    // RFC 768: transmit 0xffff when the computed value is zero.
    let cksum = if cksum == 0 { 0xffff } else { cksum };
    reply[udp_off + 6] = (cksum >> 8) as u8;
    reply[udp_off + 7] = (cksum & 0xff) as u8;

    unsafe {
        libc::send(relay_fd, reply.as_ptr() as _, reply.len(), 0);
    }
}

/// Send a single IPv6 UDP datagram to `dest` and return the reply.
fn udp_proxy_once_v6(data: &[u8], dest: SocketAddr) -> Result<Vec<u8>, std::io::Error> {
    let sock = UdpSocket::bind("[::]:0")?;
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
    log::info!("nat_relay: icmpv4 echo request received — synthesizing reply");

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
// ICMPv6 echo reply synthesizer
// ---------------------------------------------------------------------------

/// If `frame` is an IPv6 ICMPv6 Echo Request (type 128) **addressed to one of
/// the relay's own IPv6 addresses**, synthesize an Echo Reply (type 129) and
/// write it back to `relay_fd`.  Returns true if handled.
///
/// Only responds to pings targeting the relay itself (gateway LL or ULA).
/// Echo requests to external addresses are NOT answered — doing so would fake
/// internet reachability and hide the absence of real IPv6 connectivity from
/// the VM's point of view.
///
/// ICMPv6 checksum covers a pseudo-header:
///   src IPv6 (16 B) | dst IPv6 (16 B) | ICMPv6 length (4 B) | zeros (3 B) | next-header=58 (1 B)
fn icmpv6_echo_reply(relay_fd: RawFd, frame: &[u8], gw6_ll: Option<&[u8; 16]>) -> bool {
    // Minimum: 14 (Ethernet) + 40 (IPv6) + 8 (ICMPv6 header) = 62 bytes.
    if frame.len() < 62 {
        return false;
    }
    // Ethertype must be 0x86dd (IPv6).
    if frame[12] != 0x86 || frame[13] != 0xdd {
        return false;
    }
    // IPv6 next header must be 58 (ICMPv6).  Extension headers are not handled.
    if frame[14 + 6] != 58 {
        return false;
    }
    // ICMPv6 type must be 128 (Echo Request).
    let icmp_off = 14 + 40;
    if frame[icmp_off] != 128 || frame[icmp_off + 1] != 0 {
        return false;
    }
    // Only answer pings to the relay's own addresses.  Synthesizing replies for
    // external destinations would silently fake IPv6 reachability.
    let dst6: [u8; 16] = frame[14 + 24..14 + 40].try_into().unwrap();
    let is_own_addr = dst6 == GATEWAY_IP6_ULA_BYTES
        || gw6_ll.map_or(false, |ll| dst6 == *ll);
    if !is_own_addr {
        log::debug!("nat_relay: icmpv6 echo to external dst {:?} — not answered (no NAT66)", dst6);
        return false;
    }
    log::info!("nat_relay: icmpv6 echo request received — synthesizing reply");

    let mut reply = frame.to_vec();

    // Ethernet dst = original src (VM MAC); src = GATEWAY_MAC (unicast, not the
    // multicast TLLA the VM used as dst, which is invalid as an Ethernet src).
    reply[..6].copy_from_slice(&frame[6..12]);    // dst ← original VM src MAC
    reply[6..12].copy_from_slice(&GATEWAY_MAC.0); // src ← gateway unicast MAC

    // Swap IPv6 src/dst (bytes 22–37 = src, 38–53 = dst within IPv6 header).
    let src6_off = 14 + 8;
    let dst6_off = 14 + 24;
    let src6: [u8; 16] = frame[src6_off..src6_off + 16].try_into().unwrap();
    let dst6: [u8; 16] = frame[dst6_off..dst6_off + 16].try_into().unwrap();
    reply[src6_off..src6_off + 16].copy_from_slice(&dst6);
    reply[dst6_off..dst6_off + 16].copy_from_slice(&src6);

    // Set ICMPv6 type to 129 (Echo Reply), code stays 0.
    reply[icmp_off] = 129;

    // Recompute ICMPv6 checksum over pseudo-header + ICMPv6 message.
    // Pseudo-header: new src (dst6) | new dst (src6) | ICMPv6 length (4 B) | 0x00 0x00 0x00 0x3a
    reply[icmp_off + 2] = 0;
    reply[icmp_off + 3] = 0;
    let icmpv6_len = reply.len() - icmp_off;
    let mut pseudo: Vec<u8> = Vec::with_capacity(40);
    pseudo.extend_from_slice(&dst6); // new src IPv6
    pseudo.extend_from_slice(&src6); // new dst IPv6
    pseudo.extend_from_slice(&(icmpv6_len as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]); // next-header = 58
    pseudo.extend_from_slice(&reply[icmp_off..]);
    let cksum = inet_checksum(&pseudo);
    reply[icmp_off + 2] = (cksum >> 8) as u8;
    reply[icmp_off + 3] = (cksum & 0xff) as u8;

    unsafe {
        libc::send(relay_fd, reply.as_ptr() as _, reply.len(), 0);
    }
    true
}

/// Return true if `frame` is a DAD Neighbor Solicitation (ICMPv6 type 135
/// with source IPv6 = :: — the unspecified address).
///
/// DAD NS frames are sent by the guest kernel when a new IPv6 address is
/// tentative.  VZ forwards them to the relay because the guest has joined its
/// own solicited-node multicast group.  We use this as a timing signal to
/// send a Unsolicited NA for the gateway immediately, seeding the neighbor
/// cache before the guest first tries to contact fe80::ff:fe00:1.
fn is_dad_ns(frame: &[u8]) -> bool {
    // 14 (Eth) + 40 (IPv6) + 24 (ICMPv6 NS min body) = 78 bytes minimum.
    if frame.len() < 78 {
        return false;
    }
    if frame[12] != 0x86 || frame[13] != 0xdd {
        return false;
    }
    // Next header must be ICMPv6 (58).
    if frame[14 + 6] != 58 {
        log::trace!("nat_relay: is_dad_ns: IPv6 but next_hdr={} (not ICMPv6)", frame[14 + 6]);
        return false;
    }
    // ICMPv6 type must be 135 (NS).
    let icmp_type = frame[14 + 40];
    if icmp_type != 135 {
        log::trace!("nat_relay: is_dad_ns: ICMPv6 type={} (not NS)", icmp_type);
        return false;
    }
    // Source IPv6 must be :: (all zeros) — the DAD unspecified source.
    let src6 = &frame[14 + 8..14 + 24];
    let is_dad = src6 == [0u8; 16];
    log::trace!("nat_relay: is_dad_ns: NS, src6={:02x?} is_dad={}", src6, is_dad);
    is_dad
}

/// If `frame` is an NDP Neighbor Solicitation (ICMPv6 type 135) targeting
/// GATEWAY_IP6_LL, synthesize a Neighbor Advertisement (type 136) with the
/// gateway MAC in the Target Link-Layer Address option and write it back to
/// `relay_fd`. Returns true if the frame was handled.
///
/// Frame layout of the reply:
///   14 (Ethernet) + 40 (IPv6) + 4 (type/code/cksum) + 4 (flags) + 16 (target) + 8 (option) = 86 bytes
fn ndp_neighbor_advertisement(relay_fd: RawFd, frame: &[u8], gw_ip6_ll: Option<&[u8; 16]>, gw_mcast_mac: Option<&[u8; 6]>) -> bool {
    let (gw6, tlla) = match (gw_ip6_ll, gw_mcast_mac) {
        (Some(ll), Some(mac)) => (ll, mac),
        _ => return false,
    };
    // Minimum NS without options: 14 (Eth) + 40 (IPv6) + 4 (hdr) + 4 (reserved) + 16 (target) = 78
    if frame.len() < 78 {
        return false;
    }
    // Ethertype must be 0x86dd (IPv6).
    if frame[12] != 0x86 || frame[13] != 0xdd {
        return false;
    }
    // IPv6 next header must be 58 (ICMPv6).
    if frame[14 + 6] != 58 {
        log::trace!("nat_relay: ndp: IPv6 but not ICMPv6 (next_hdr={})", frame[14 + 6]);
        return false;
    }
    // ICMPv6 type must be 135 (Neighbor Solicitation), code must be 0.
    let icmp_off = 14 + 40; // = 54
    log::trace!("nat_relay: ndp: ICMPv6 type={} code={}", frame[icmp_off], frame[icmp_off + 1]);
    if frame[icmp_off] != 135 || frame[icmp_off + 1] != 0 {
        return false;
    }
    // Target address is at icmp_off + 8 (type + code + cksum + reserved = 8 bytes).
    let target_off = icmp_off + 8;
    let target: [u8; 16] = frame[target_off..target_off + 16].try_into().unwrap_or([0u8; 16]);
    log::trace!("nat_relay: ndp: NS target={:?}, want={:?}", target, gw6);
    if frame[target_off..target_off + 16] != *gw6 {
        return false;
    }
    log::info!("nat_relay: NS for gateway received — sending NA (TLLA=dynamic gateway multicast MAC)");

    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
    let src6: [u8; 16] = frame[14 + 8..14 + 24].try_into().unwrap();
    let gw_mac = GATEWAY_MAC.0;
    let gw6 = *gw6;

    // NA reply: 14 (Eth) + 40 (IPv6) + 32 (ICMPv6 NA with one option) = 86 bytes.
    // ICMPv6 NA breakdown: 4 (type/code/cksum) + 4 (R/S/O flags) + 16 (target) + 8 (option) = 32
    let icmpv6_len: u16 = 32;
    let mut reply = vec![0u8; 14 + 40 + 32];

    // Ethernet: dst = guest MAC, src = gateway MAC.
    reply[0..6].copy_from_slice(&src_mac);
    reply[6..12].copy_from_slice(&gw_mac);
    reply[12] = 0x86;
    reply[13] = 0xdd;

    // IPv6 header.
    reply[14] = 0x60; // version 6, TC 0, flow label 0
    reply[18] = (icmpv6_len >> 8) as u8;
    reply[19] = (icmpv6_len & 0xff) as u8;
    reply[20] = 58;  // next header = ICMPv6
    reply[21] = 255; // hop limit (NDP requires 255)
    reply[22..38].copy_from_slice(&gw6);  // src = GATEWAY_IP6_LL
    reply[38..54].copy_from_slice(&src6); // dst = guest link-local

    // ICMPv6 Neighbor Advertisement.
    let na_off = 54;
    reply[na_off]     = 136; // type = Neighbor Advertisement
    reply[na_off + 1] = 0;   // code = 0
    // reply[na_off + 2,3] = checksum (computed below)
    // Flags: S=1 (solicited), O=1 (override) → bits 30,29 of 32-bit field → 0x60000000
    reply[na_off + 4] = 0x60;
    // Target address = GATEWAY_IP6_LL
    reply[na_off + 8..na_off + 24].copy_from_slice(&gw6);
    // Option: Target Link-Layer Address (type=2, length=1 → 8 bytes).
    // We advertise the dynamic gateway multicast MAC instead of GATEWAY_MAC
    // (02:00:00:00:00:01).  VZ's virtual switch delivers IPv6 multicast to the relay
    // but silently drops IPv6 unicast.  By advertising the solicited-node multicast MAC
    // as the TLLA, the VM will use a multicast Ethernet destination for ICMPv6 frames
    // to the gateway, which VZ reliably forwards to the relay.
    reply[na_off + 24] = 2; // option type
    reply[na_off + 25] = 1; // length in units of 8 bytes
    reply[na_off + 26..na_off + 32].copy_from_slice(tlla);

    // ICMPv6 pseudo-header checksum: src IPv6 | dst IPv6 | length (4B) | 0x00 0x00 0x00 0x3a
    let mut pseudo: Vec<u8> = Vec::with_capacity(40 + 32);
    pseudo.extend_from_slice(&gw6);                              // src = gateway
    pseudo.extend_from_slice(&src6);                             // dst = guest
    pseudo.extend_from_slice(&(icmpv6_len as u32).to_be_bytes());
    pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
    pseudo.extend_from_slice(&reply[na_off..]);
    let cksum = inet_checksum(&pseudo);
    reply[na_off + 2] = (cksum >> 8) as u8;
    reply[na_off + 3] = (cksum & 0xff) as u8;

    unsafe {
        libc::send(relay_fd, reply.as_ptr() as _, reply.len(), 0);
    }
    log::info!("nat_relay: sent NDP NA for gateway link-local (TLLA=dynamic gateway multicast MAC)");
    true
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

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Build a minimal ICMPv4 Echo Request Ethernet frame.
    fn make_icmpv4_echo_request(src_mac: [u8; 6], dst_mac: [u8; 6],
                                src_ip: [u8; 4], dst_ip: [u8; 4],
                                id: u16, seq: u16) -> Vec<u8> {
        let mut f = vec![0u8; 14 + 20 + 8];
        // Ethernet
        f[..6].copy_from_slice(&dst_mac);
        f[6..12].copy_from_slice(&src_mac);
        f[12] = 0x08; f[13] = 0x00;
        // IPv4: version+IHL, TTL=64, proto=1 (ICMP)
        f[14] = 0x45;
        f[14 + 8] = 64;
        f[14 + 9] = 1;
        f[14 + 12..14 + 16].copy_from_slice(&src_ip);
        f[14 + 16..14 + 20].copy_from_slice(&dst_ip);
        let hdr_cksum = inet_checksum(&f[14..14 + 20]);
        f[14 + 10] = (hdr_cksum >> 8) as u8;
        f[14 + 11] = (hdr_cksum & 0xff) as u8;
        // ICMP: type=8 (request), code=0
        f[34] = 8;
        f[35] = 0;
        f[36] = (id >> 8) as u8; f[37] = (id & 0xff) as u8;
        f[38] = (seq >> 8) as u8; f[39] = (seq & 0xff) as u8;
        let icmp_cksum = inet_checksum(&f[34..]);
        f[36] = (icmp_cksum >> 8) as u8; // reuse id bytes since payload is empty
        // Actually put checksum in correct field:
        let mut f2 = vec![0u8; 14 + 20 + 8];
        f2[..14].copy_from_slice(&f[..14]);
        f2[14..34].copy_from_slice(&f[14..34]);
        f2[34] = 8; f2[35] = 0;
        f2[36] = (id >> 8) as u8; f2[37] = (id & 0xff) as u8;
        f2[38] = (seq >> 8) as u8; f2[39] = (seq & 0xff) as u8;
        let ck = inet_checksum(&f2[34..]);
        f2[36] = (ck >> 8) as u8;
        f2[37] = (ck & 0xff) as u8;
        f2
    }

    /// Build a minimal ICMPv6 Echo Request Ethernet frame.
    fn make_icmpv6_echo_request(src_mac: [u8; 6], dst_mac: [u8; 6],
                                src_ip: [u8; 16], dst_ip: [u8; 16],
                                id: u16, seq: u16) -> Vec<u8> {
        // 14 (Ethernet) + 40 (IPv6) + 8 (ICMPv6 header, no payload)
        let mut f = vec![0u8; 62];
        // Ethernet
        f[..6].copy_from_slice(&dst_mac);
        f[6..12].copy_from_slice(&src_mac);
        f[12] = 0x86; f[13] = 0xdd;
        // IPv6: version=6, payload length=8, next header=58 (ICMPv6), hop limit=64
        f[14] = 0x60; // version=6, traffic class=0, flow label=0
        f[14 + 4] = 0x00;
        f[14 + 5] = 0x08; // payload length = 8
        f[14 + 6] = 58;   // next header = ICMPv6
        f[14 + 7] = 64;   // hop limit
        f[14 + 8..14 + 24].copy_from_slice(&src_ip);
        f[14 + 24..14 + 40].copy_from_slice(&dst_ip);
        // ICMPv6: type=128 (echo request), code=0, checksum=0 (filled below)
        // identifier at offset 4-5, sequence at offset 6-7 within ICMPv6 header.
        f[54] = 128; // type
        f[55] = 0;   // code
        // f[56,57] = checksum — leave as 0 for now
        f[58] = (id >> 8) as u8; f[59] = (id & 0xff) as u8;   // identifier
        f[60] = (seq >> 8) as u8; f[61] = (seq & 0xff) as u8;  // sequence
        // Checksum: pseudo-header + ICMPv6 message (checksum field = 0).
        let icmpv6_len: u32 = 8;
        let mut pseudo = Vec::with_capacity(40);
        pseudo.extend_from_slice(&src_ip);
        pseudo.extend_from_slice(&dst_ip);
        pseudo.extend_from_slice(&icmpv6_len.to_be_bytes());
        pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
        pseudo.extend_from_slice(&f[54..62]);
        let ck = inet_checksum(&pseudo);
        f[56] = (ck >> 8) as u8;
        f[57] = (ck & 0xff) as u8;
        f
    }

    // ── inet_checksum ─────────────────────────────────────────────────────────

    /// RFC 1071 §3 example: checksum of the four 16-bit words
    /// 0x0001, 0xf203, 0xf4f5, 0xf6f7 should be 0x220d.
    #[test]
    fn test_inet_checksum_rfc1071_example() {
        let data: &[u8] = &[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(inet_checksum(data), 0x220d);
    }

    #[test]
    fn test_inet_checksum_all_zeros() {
        assert_eq!(inet_checksum(&[0u8; 8]), 0xffff);
    }

    #[test]
    fn test_inet_checksum_odd_length() {
        // Single 0xff byte: sum = 0xff00, complement = 0x00ff.
        assert_eq!(inet_checksum(&[0xff]), 0x00ff);
    }

    // ── ICMPv6 pseudo-header checksum ─────────────────────────────────────────

    /// Verify pseudo-header checksum construction by round-tripping: build a
    /// request frame with make_icmpv6_echo_request (which computes a checksum),
    /// then verify the checksum field is correct by recomputing independently.
    #[test]
    fn test_icmpv6_echo_request_checksum_valid() {
        let src_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2u8];
        let dst_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 0, 0, 1, 0, 0u8];
        let frame = make_icmpv6_echo_request(
            [0xaa; 6], [0xbb; 6], src_ip, dst_ip, 0x1234, 0x0001,
        );
        // Extract the checksum from the frame.
        let stored_ck = u16::from_be_bytes([frame[56], frame[57]]);
        // Zero the checksum field and recompute.
        let icmpv6_len: u32 = 8;
        let mut pseudo = Vec::with_capacity(40);
        pseudo.extend_from_slice(&src_ip);
        pseudo.extend_from_slice(&dst_ip);
        pseudo.extend_from_slice(&icmpv6_len.to_be_bytes());
        pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
        let mut msg = frame[54..].to_vec();
        msg[2] = 0; msg[3] = 0; // zero checksum field
        pseudo.extend_from_slice(&msg);
        assert_eq!(inet_checksum(&pseudo), stored_ck);
    }

    // ── icmp_echo_reply (IPv4) ────────────────────────────────────────────────

    #[test]
    fn test_icmpv4_echo_reply_swaps_addresses() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let src_mac = [0x11u8; 6];
        let dst_mac = [0x22u8; 6];
        let src_ip = [192, 168, 105, 2];
        let dst_ip = [192, 168, 105, 1];
        let frame = make_icmpv4_echo_request(src_mac, dst_mac, src_ip, dst_ip, 1, 1);
        assert!(icmp_echo_reply(fd_a, &frame));
        let mut buf = vec![0u8; 1500];
        let n = unsafe { libc::recv(fd_b, buf.as_mut_ptr() as _, buf.len(), 0) };
        assert!(n > 0);
        let reply = &buf[..n as usize];
        // Ethernet: dst should be original src, src should be original dst.
        assert_eq!(&reply[..6], &src_mac);
        assert_eq!(&reply[6..12], &dst_mac);
        // IPv4: src/dst swapped.
        assert_eq!(&reply[14 + 12..14 + 16], &dst_ip);
        assert_eq!(&reply[14 + 16..14 + 20], &src_ip);
        // ICMP type = 0 (reply).
        assert_eq!(reply[34], 0);
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    #[test]
    fn test_icmpv4_echo_reply_rejects_short_frame() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        assert!(!icmp_echo_reply(fd_a, &[0u8; 10]));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    // ── icmpv6_echo_reply ─────────────────────────────────────────────────────

    #[test]
    fn test_icmpv6_echo_reply_swaps_addresses() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let src_mac = [0xaau8; 6];
        let dst_mac = [0xbbu8; 6];
        let src_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2u8]; // fe80::2
        let dst_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 0, 0, 1, 0, 0u8]; // gateway LL
        let frame = make_icmpv6_echo_request(src_mac, dst_mac, src_ip, dst_ip, 0x42, 0x01);

        assert!(icmpv6_echo_reply(fd_a, &frame, Some(&dst_ip)));

        let mut buf = vec![0u8; 1500];
        let n = unsafe { libc::recv(fd_b, buf.as_mut_ptr() as _, buf.len(), 0) };
        assert!(n >= 62, "reply too short: {}", n);
        let reply = &buf[..n as usize];

        // Ethernet: dst ← original src, src ← original dst.
        assert_eq!(&reply[..6], &src_mac, "Ethernet dst should be original src MAC");
        assert_eq!(&reply[6..12], &GATEWAY_MAC.0, "Ethernet src should be GATEWAY_MAC");

        // IPv6: src/dst swapped.
        assert_eq!(&reply[14 + 8..14 + 24], &dst_ip, "IPv6 src should be original dst");
        assert_eq!(&reply[14 + 24..14 + 40], &src_ip, "IPv6 dst should be original src");

        // ICMPv6 type = 129 (Echo Reply).
        assert_eq!(reply[54], 129, "ICMPv6 type should be 129 (Echo Reply)");
        assert_eq!(reply[55], 0, "ICMPv6 code should be 0");

        // Checksum: recompute and verify.
        let icmpv6_len = (reply.len() - 54) as u32;
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&dst_ip); // new src
        pseudo.extend_from_slice(&src_ip); // new dst
        pseudo.extend_from_slice(&icmpv6_len.to_be_bytes());
        pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
        let mut msg = reply[54..].to_vec();
        let stored_ck = u16::from_be_bytes([msg[2], msg[3]]);
        msg[2] = 0; msg[3] = 0;
        pseudo.extend_from_slice(&msg);
        assert_eq!(inet_checksum(&pseudo), stored_ck, "ICMPv6 checksum invalid");

        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    #[test]
    fn test_icmpv6_echo_reply_rejects_ipv4_frame() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let frame = make_icmpv4_echo_request(
            [0x11; 6], [0x22; 6], [10, 0, 0, 1], [10, 0, 0, 2], 1, 1,
        );
        assert!(!icmpv6_echo_reply(fd_a, &frame, None));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    #[test]
    fn test_icmpv6_echo_reply_rejects_short_frame() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        assert!(!icmpv6_echo_reply(fd_a, &[0x86, 0xdd, 0, 0, 0, 0, 0, 0], None));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    #[test]
    fn test_icmpv6_echo_reply_rejects_non_echo_type() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let src_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2u8];
        let dst_ip = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 0, 0, 1, 0, 0u8];
        let mut frame = make_icmpv6_echo_request(
            [0xaa; 6], [0xbb; 6], src_ip, dst_ip, 1, 1,
        );
        frame[54] = 135; // Neighbor Solicitation — should not be answered here
        assert!(!icmpv6_echo_reply(fd_a, &frame, Some(&dst_ip)));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    #[test]
    fn test_icmpv6_echo_reply_rejects_external_dst() {
        // Echo requests to non-relay addresses must NOT be answered.
        // Faking replies for external destinations would hide the absence
        // of real IPv6 connectivity.
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let src_ip = [0xfd, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2u8]; // fd00::2 (VM)
        let external = [0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0,
                        0, 0, 0, 0, 0, 0, 0x88, 0x88u8]; // 2001:4860:4860::8888
        let frame = make_icmpv6_echo_request([0xaa; 6], [0xbb; 6], src_ip, external, 1, 1);
        // Pass the relay's own LL — the external dst still doesn't match.
        let own_ll = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 0, 0, 1, 0, 0u8];
        assert!(!icmpv6_echo_reply(fd_a, &frame, Some(&own_ll)),
            "must not answer ping to external IPv6 address");
        // Also with no known LL — should still reject.
        assert!(!icmpv6_echo_reply(fd_a, &frame, None),
            "must not answer ping to external IPv6 address (no LL known)");
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    // ── ndp_neighbor_advertisement ────────────────────────────────────────────

    /// Build a minimal NDP Neighbor Solicitation Ethernet frame.
    fn make_ndp_ns(src_mac: [u8; 6], src_ip: [u8; 16], target_ip: [u8; 16]) -> Vec<u8> {
        // 14 (Eth) + 40 (IPv6) + 4 (type/code/cksum) + 4 (reserved) + 16 (target) = 78
        let icmpv6_payload_len: u16 = 24; // 4 + 4 + 16
        let mut f = vec![0u8; 78];
        // Ethernet: solicited-node multicast dst, guest src.
        f[0..6].copy_from_slice(&[0x33, 0x33, 0xff,
            target_ip[13], target_ip[14], target_ip[15]]);
        f[6..12].copy_from_slice(&src_mac);
        f[12] = 0x86; f[13] = 0xdd;
        // IPv6 header.
        f[14] = 0x60;
        f[18] = (icmpv6_payload_len >> 8) as u8;
        f[19] = (icmpv6_payload_len & 0xff) as u8;
        f[20] = 58;  // ICMPv6
        f[21] = 255; // hop limit
        f[22..38].copy_from_slice(&src_ip);
        // dst = solicited-node multicast for target
        let mut snmc = [0u8; 16];
        snmc[0] = 0xff; snmc[1] = 0x02;
        snmc[11] = 0x01; snmc[12] = 0xff;
        snmc[13] = target_ip[13]; snmc[14] = target_ip[14]; snmc[15] = target_ip[15];
        f[38..54].copy_from_slice(&snmc);
        // ICMPv6 NS: type=135, code=0, cksum=0, reserved=0, target addr.
        f[54] = 135;
        f[55] = 0;
        // f[56,57] = checksum (computed below)
        // f[58..62] = reserved (zeros)
        f[62..78].copy_from_slice(&target_ip);
        // Compute checksum over pseudo-header + ICMPv6 message.
        let icmpv6_msg_len: u32 = icmpv6_payload_len as u32;
        let mut pseudo: Vec<u8> = Vec::new();
        pseudo.extend_from_slice(&src_ip);
        pseudo.extend_from_slice(&snmc);
        pseudo.extend_from_slice(&icmpv6_msg_len.to_be_bytes());
        pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
        pseudo.extend_from_slice(&f[54..]);
        let ck = inet_checksum(&pseudo);
        f[56] = (ck >> 8) as u8;
        f[57] = (ck & 0xff) as u8;
        f
    }

    /// NS targeting the gateway produces a valid NA with gateway MAC.
    #[test]
    fn test_ndp_na_responds_to_gateway_ns() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let guest_mac = [0xf2u8, 0xb9, 0xd0, 0x8c, 0x19, 0x6c];
        let guest_ll  = [0xfe, 0x80u8, 0, 0, 0, 0, 0, 0, 0xf0, 0xb9, 0xd0, 0xff, 0xfe, 0x8c, 0x19, 0x6c];
        let gw_ll = gateway_ip6_for_vm_mac(guest_mac);
        let gw_mcast_mac = gateway_ip6_mcast_mac_for_vm_mac(guest_mac);
        let gw_mac = GATEWAY_MAC.0;

        let ns = make_ndp_ns(guest_mac, guest_ll, gw_ll);
        assert!(ndp_neighbor_advertisement(fd_a, &ns, Some(&gw_ll), Some(&gw_mcast_mac)));

        let mut buf = vec![0u8; 1500];
        let n = unsafe { libc::recv(fd_b, buf.as_mut_ptr() as _, buf.len(), 0) };
        assert_eq!(n, 86, "NA reply should be exactly 86 bytes");
        let reply = &buf[..n as usize];

        // Ethernet: dst = guest MAC, src = gateway MAC.
        assert_eq!(&reply[0..6], &guest_mac, "Ethernet dst should be guest MAC");
        assert_eq!(&reply[6..12], &gw_mac, "Ethernet src should be gateway MAC");

        // IPv6: src = GATEWAY_IP6_LL, dst = guest LL.
        assert_eq!(&reply[22..38], &gw_ll, "IPv6 src should be gateway LL");
        assert_eq!(&reply[38..54], &guest_ll, "IPv6 dst should be guest LL");

        // ICMPv6 type = 136 (NA), flags S+O.
        assert_eq!(reply[54], 136, "ICMPv6 type should be 136 (NA)");
        assert_eq!(reply[55], 0, "ICMPv6 code should be 0");
        assert_eq!(reply[58], 0x60, "S+O flags should be set");

        // Target = GATEWAY_IP6_LL.
        assert_eq!(&reply[62..78], &gw_ll, "Target addr should be GATEWAY_IP6_LL");

        // Option: type=2 (Target Link-Layer), len=1, MAC=gateway.
        assert_eq!(reply[78], 2, "Option type should be 2");
        assert_eq!(reply[79], 1, "Option length should be 1");
        assert_eq!(&reply[80..86], &gw_mcast_mac, "Option MAC should be gateway multicast MAC");

        // Checksum must be valid.
        let icmpv6_len: u32 = 32;
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&gw_ll);
        pseudo.extend_from_slice(&guest_ll);
        pseudo.extend_from_slice(&icmpv6_len.to_be_bytes());
        pseudo.extend_from_slice(&[0x00, 0x00, 0x00, 58u8]);
        let stored_ck = u16::from_be_bytes([reply[56], reply[57]]);
        let mut msg = reply[54..].to_vec();
        msg[2] = 0; msg[3] = 0;
        pseudo.extend_from_slice(&msg);
        assert_eq!(inet_checksum(&pseudo), stored_ck, "ICMPv6 checksum invalid");

        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    /// NS targeting a non-gateway address is ignored.
    #[test]
    fn test_ndp_na_ignores_other_targets() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let guest_mac = [0xf2u8, 0xb9, 0xd0, 0x8c, 0x19, 0x6c];
        let guest_ll  = [0xfe, 0x80u8, 0, 0, 0, 0, 0, 0, 0xf0, 0xb9, 0xd0, 0xff, 0xfe, 0x8c, 0x19, 0x6c];
        let gw_ll = gateway_ip6_for_vm_mac(guest_mac);
        let gw_mcast_mac = gateway_ip6_mcast_mac_for_vm_mac(guest_mac);
        // Target is the guest's own address, not the gateway.
        let ns = make_ndp_ns(guest_mac, guest_ll, guest_ll);
        assert!(!ndp_neighbor_advertisement(fd_a, &ns, Some(&gw_ll), Some(&gw_mcast_mac)));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }

    /// Short frame is rejected.
    #[test]
    fn test_ndp_na_rejects_short_frame() {
        let (fd_a, fd_b) = create_socketpair().unwrap();
        let gw_ll = [0xfe, 0x80u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 1, 2, 3];
        let gw_mcast = [0x33u8, 0x33, 0xff, 1, 2, 3];
        assert!(!ndp_neighbor_advertisement(fd_a, &[0u8; 40], Some(&gw_ll), Some(&gw_mcast)));
        unsafe { libc::close(fd_a); libc::close(fd_b); }
    }
}
