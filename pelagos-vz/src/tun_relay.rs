//! Kernel-assisted NAT relay using a macOS `utun` interface.
//!
//! # Architecture
//!
//! ```text
//! VM virtio-net (raw Ethernet via SOCK_DGRAM socketpair)
//!        │
//!  [tun_relay thread — poll loop on relay_fd + utun_fd]
//!        │  strip/add 14-byte Ethernet header
//!        │  ARP replies and NDP Neighbour Advertisements (synthesised locally)
//!        │  IPv4: forward as-is — kernel handles NAT44 via pf
//!        │  IPv6: forward as-is — kernel handles NAT66 via pf
//!        │
//!   utunN (kernel L3 interface, e.g. utun5)
//!        │  fd00::1/64 assigned by pelagos-pfctl (ip6.forwarding=1)
//!        │
//!  macOS pf  ──  NAT44: 192.168.105.0/24 → egress IP
//!           ├─  NAT66: fd00::/64 → egress IPv6 (GUA)
//!           └─  RDR port-forward rules (managed by pelagos-pfctl)
//!        │
//!   egress interface (en0 / WiFi / …)
//! ```
//!
//! # VM network constants
//!
//! ```text
//! Gateway MAC:      02:00:00:00:00:01   (relay answers ARP/NDP with this)
//! Gateway IPv4:     192.168.105.1       (host-side utun address)
//! VM IPv4:          192.168.105.2/24    (configured in guest)
//! Gateway IPv6 ULA: fd00::1/64          (host-side utun address; pf NAT66 source)
//! Gateway IPv6 LL:  fe80::1             (relay answers NDP for this)
//! VM IPv6 ULA:      fd00::2/64          (configured in guest; NAT66 via pf)
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::Serialize;

// ---------------------------------------------------------------------------
// Network constants (must match the guest OS static network configuration)
// ---------------------------------------------------------------------------

const GATEWAY_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const GATEWAY_IP4: [u8; 4] = [192, 168, 105, 1];
const GATEWAY_IP6_ULA: [u8; 16] = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const GATEWAY_IP6_LL: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

// 4-byte tun packet prefix (network byte order AF value).
const TUN_HDR_IPV4: [u8; 4] = [0, 0, 0, 2]; // AF_INET  = 2
const TUN_HDR_IPV6: [u8; 4] = [0, 0, 0, 30]; // AF_INET6 = 30 on macOS

// Unix socket of the pelagos-pfctl LaunchDaemon.
const PFCTL_SOCK: &str = "/var/run/pelagos-pfctl.sock";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the utun NAT relay.
///
/// Returns `(avf_fd, relay)`:
/// - `avf_fd` is one end of a `socketpair(AF_UNIX, SOCK_DGRAM)` ready to be
///   wrapped in `NSFileHandle` and passed to `VZFileHandleNetworkDeviceAttachment`.
/// - `relay` holds the relay thread and utun fd.  Drop it to initiate shutdown.
pub fn start() -> Result<(RawFd, TunRelayHandle), crate::Error> {
    let (avf_fd, relay_fd) = create_socketpair()?;

    // 128 KB / 512 KB per AVF documentation.
    set_sock_bufs(avf_fd, 128 * 1024, 512 * 1024);
    set_sock_bufs(relay_fd, 128 * 1024, 512 * 1024);

    // Ask pelagos-pfctl (root daemon) to create the utun fd and pass it back via SCM_RIGHTS.
    // Direct creation in the pelagos daemon fails with EPERM because utun creation requires
    // root (or com.apple.private.network.interface-management, a private Apple entitlement).
    let (utun_owned, utun_iface) = pfctl_create_utun()?;
    let utun_fd = utun_owned.as_raw_fd();

    // Detect egress interface for NAT rules.
    let egress = detect_egress_iface()
        .ok_or_else(|| crate::Error::Runtime("could not detect default route interface".into()))?;

    // Ask pelagos-pfctl to assign IPs to the utun interface and load pf NAT44 rules.
    pfctl_setup_utun(&utun_iface, &egress)?;

    log::info!("tun_relay: started utun={utun_iface} egress={egress}");

    // Set both fds non-blocking for the poll loop.
    set_nonblocking(relay_fd);
    set_nonblocking(utun_fd);

    let iface_clone = utun_iface.clone();
    let thread = std::thread::Builder::new()
        .name("tun-relay".into())
        .spawn(move || run_relay(relay_fd, utun_fd, &iface_clone))
        .expect("spawn tun-relay");

    Ok((
        avf_fd,
        TunRelayHandle {
            _thread: thread,
            _utun: utun_owned,
            utun_iface,
        },
    ))
}

/// Handle to the running tun relay. When dropped, the utun fd is closed which
/// causes the relay thread to exit on the next poll iteration.
pub struct TunRelayHandle {
    _thread: std::thread::JoinHandle<()>,
    _utun: OwnedFd,
    utun_iface: String,
}

impl TunRelayHandle {
    /// Ask pelagos-pfctl to add a pf RDR port-forward rule.
    /// Accepted connections to `host_port` on the host are redirected to
    /// `vm_ip:vm_port` inside the VM.
    pub fn add_rdr(
        &self,
        proto: &str,
        host_port: u16,
        vm_ip: &str,
        vm_port: u16,
    ) -> Result<(), crate::Error> {
        #[derive(Serialize)]
        struct Req<'a> {
            action: &'static str,
            proto: &'a str,
            host_port: u16,
            vm_ip: &'a str,
            vm_port: u16,
        }
        let json = serde_json::to_string(&Req {
            action: "add_rdr",
            proto,
            host_port,
            vm_ip,
            vm_port,
        })
        .map_err(|e| crate::Error::Runtime(e.to_string()))?;
        pfctl_send(&json)
    }

    /// Remove a previously added RDR rule.
    pub fn remove_rdr(&self, proto: &str, host_port: u16) -> Result<(), crate::Error> {
        #[derive(Serialize)]
        struct Req<'a> {
            action: &'static str,
            proto: &'a str,
            host_port: u16,
        }
        let json = serde_json::to_string(&Req {
            action: "remove_rdr",
            proto,
            host_port,
        })
        .map_err(|e| crate::Error::Runtime(e.to_string()))?;
        pfctl_send(&json)
    }
}

impl Drop for TunRelayHandle {
    fn drop(&mut self) {
        // Best-effort teardown: flush pf rules when the relay is stopped.
        if let Err(e) = pfctl_teardown_utun(&self.utun_iface) {
            log::warn!("tun_relay: teardown: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// utun fd creation via pelagos-pfctl (SCM_RIGHTS fd passing)
// ---------------------------------------------------------------------------

/// Ask pelagos-pfctl (root daemon) to create a utun interface and pass back the fd.
///
/// Direct creation fails with EPERM for non-root processes.  pelagos-pfctl runs as
/// root via a LaunchDaemon, creates the utun fd, and delivers it to us via sendmsg(2)
/// with SCM_RIGHTS ancillary data.  We receive it with recvmsg(2).
fn pfctl_create_utun() -> Result<(OwnedFd, String), crate::Error> {
    let stream = UnixStream::connect(PFCTL_SOCK)
        .map_err(|e| crate::Error::Runtime(format!("pfctl connect: {e}")))?;

    (&stream)
        .write_all(b"{\"action\":\"create_utun\"}\n")
        .map_err(|e| crate::Error::Runtime(format!("pfctl write: {e}")))?;

    let stream_fd = stream.as_raw_fd();

    // Receive JSON response + utun fd via recvmsg.
    let mut json_buf = vec![0u8; 256];
    let mut iov = libc::iovec {
        iov_base: json_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: json_buf.len(),
    };

    let cmsg_space =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as libc::socklen_t;

    let n = unsafe { libc::recvmsg(stream_fd, &mut msg, 0) };
    if n <= 0 {
        return Err(crate::Error::Runtime(format!(
            "pfctl recvmsg: {}",
            std::io::Error::last_os_error()
        )));
    }

    // Parse JSON response.
    let json_str = std::str::from_utf8(&json_buf[..n as usize])
        .map_err(|_| crate::Error::Runtime("pfctl: invalid UTF-8 in create_utun response".into()))?
        .trim_matches(|c| c == '\n' || c == '\r' || c == ' ');

    #[derive(serde::Deserialize)]
    struct Resp {
        ok: bool,
        error: Option<String>,
        iface: Option<String>,
    }
    let resp: Resp = serde_json::from_str(json_str)
        .map_err(|e| crate::Error::Runtime(format!("pfctl parse {json_str:?}: {e}")))?;

    if !resp.ok {
        return Err(crate::Error::Runtime(format!(
            "pfctl create_utun: {}",
            resp.error.unwrap_or_default()
        )));
    }
    let iface = resp
        .iface
        .ok_or_else(|| crate::Error::Runtime("pfctl: create_utun response missing iface".into()))?;

    // Extract the received fd from the SCM_RIGHTS ancillary data.
    let received_fd = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(crate::Error::Runtime(
                "pfctl: no SCM_RIGHTS fd in create_utun response".into(),
            ));
        }
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(crate::Error::Runtime(format!(
                "pfctl: unexpected cmsg level={} type={} in create_utun response",
                (*cmsg).cmsg_level,
                (*cmsg).cmsg_type,
            )));
        }
        *(libc::CMSG_DATA(cmsg) as *const libc::c_int)
    };

    log::info!("tun_relay: received utun fd={received_fd} iface={iface} from pfctl");
    let owned = unsafe { OwnedFd::from_raw_fd(received_fd) };
    Ok((owned, iface))
}

// ---------------------------------------------------------------------------
// Relay loop
// ---------------------------------------------------------------------------

struct RelayState {
    /// VM MAC address, learned from the first Ethernet frame received from the VM.
    vm_mac: Option<[u8; 6]>,
}

fn run_relay(relay_fd: RawFd, utun_fd: RawFd, utun_iface: &str) {
    log::info!("tun_relay: relay loop started (relay_fd={relay_fd} utun_fd={utun_fd})");
    let mut state = RelayState { vm_mac: None };
    let mut avf_buf = vec![0u8; 65536 + 14]; // MTU + Ethernet header
    let mut tun_buf = vec![0u8; 65536 + 4]; // MTU + 4-byte tun prefix

    loop {
        let mut pollfds = [
            libc::pollfd {
                fd: relay_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: utun_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, 5000) };

        if ret < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("tun_relay: poll: {e}");
            break;
        }

        // AVF → utun (and ARP/NDP synthesis).
        if pollfds[0].revents & libc::POLLIN != 0 {
            loop {
                let n =
                    unsafe { libc::recv(relay_fd, avf_buf.as_mut_ptr() as _, avf_buf.len(), 0) };
                if n <= 0 {
                    break;
                }
                process_avf_frame(&avf_buf[..n as usize], relay_fd, utun_fd, &mut state);
            }
        }

        // Check for AVF fd close (VM stopped).
        if pollfds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            log::info!("tun_relay: AVF fd closed — relay exiting");
            break;
        }

        // utun → AVF.
        if pollfds[1].revents & libc::POLLIN != 0 {
            loop {
                let n = unsafe { libc::read(utun_fd, tun_buf.as_mut_ptr() as _, tun_buf.len()) };
                if n <= 0 {
                    break;
                }
                process_utun_packet(&tun_buf[..n as usize], relay_fd, &state);
            }
        }

        // utun fd closed means relay handle was dropped — exit cleanly.
        if pollfds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            log::info!("tun_relay: utun fd closed — relay exiting");
            break;
        }
    }

    log::info!("tun_relay: relay loop exited (iface={utun_iface})");
}

// ---------------------------------------------------------------------------
// Frame processing: AVF → utun
// ---------------------------------------------------------------------------

fn process_avf_frame(frame: &[u8], relay_fd: RawFd, utun_fd: RawFd, state: &mut RelayState) {
    if frame.len() < 14 {
        return;
    }

    // Learn VM MAC from the source field of any inbound Ethernet frame.
    if state.vm_mac.is_none() {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[6..12]);
        state.vm_mac = Some(mac);
        log::info!(
            "tun_relay: VM MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
    }

    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    match ethertype {
        0x0806 => handle_arp(frame, relay_fd, state),
        0x0800 => {
            // IPv4 — strip Ethernet header, prepend tun prefix, write to utun.
            forward_to_utun(utun_fd, &frame[14..], &TUN_HDR_IPV4);
        }
        0x86DD => {
            // IPv6 — check for NDP Neighbour Solicitation before forwarding.
            if is_ndp_ns(frame) && handle_ndp_ns(frame, relay_fd, state) {
                return; // handled locally
            }
            // Strip Ethernet header, prepend tun prefix, write to utun.
            // pf NAT66 (fd00::/64 → egress GUA) is handled by the kernel.
            forward_to_utun(utun_fd, &frame[14..], &TUN_HDR_IPV6);
        }
        _ => {
            log::trace!("tun_relay: unknown ethertype 0x{:04x} — drop", ethertype);
        }
    }
}

/// Returns true if `frame` carries an ICMPv6 Neighbour Solicitation (type 135).
fn is_ndp_ns(frame: &[u8]) -> bool {
    // IPv6 header starts at frame[14]; next_hdr at frame[20]; ICMPv6 type at frame[54].
    frame.len() > 54 && frame[20] == 58 && frame[54] == 135
}

fn handle_arp(frame: &[u8], relay_fd: RawFd, state: &RelayState) {
    // Minimum ARP over Ethernet: 42 bytes (14 eth + 28 arp).
    if frame.len() < 42 {
        return;
    }
    let op = u16::from_be_bytes([frame[20], frame[21]]);
    if op != 1 {
        return; // only reply to requests
    }
    let target_ip = &frame[38..42];
    if target_ip != GATEWAY_IP4 {
        return; // not asking for our IP
    }

    // VM MAC: use the Ethernet src field (already learned), falling back to the ARP sender field.
    let vm_mac = state
        .vm_mac
        .unwrap_or_else(|| frame[22..28].try_into().unwrap_or([0u8; 6]));
    let sender_ip: [u8; 4] = frame[28..32].try_into().unwrap_or([0u8; 4]);

    let reply = build_arp_reply(&vm_mac, &sender_ip);
    send_to_avf(relay_fd, &reply);
}

fn handle_ndp_ns(frame: &[u8], relay_fd: RawFd, state: &RelayState) -> bool {
    // NDP target address is at frame[62..78].
    if frame.len() < 78 {
        return false;
    }
    let target: [u8; 16] = match frame[62..78].try_into() {
        Ok(t) => t,
        Err(_) => return false,
    };

    // Only respond for our gateway IPv6 addresses.
    if target != GATEWAY_IP6_ULA && target != GATEWAY_IP6_LL {
        return false;
    }

    let vm_mac = match state.vm_mac {
        Some(m) => m,
        None => return false,
    };

    // NS source IPv6 at frame[22..38] → NA destination.
    let ns_src_ip: [u8; 16] = match frame[22..38].try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };

    let na = build_ndp_na(&vm_mac, &target, &ns_src_ip);
    send_to_avf(relay_fd, &na);
    true
}

fn forward_to_utun(utun_fd: RawFd, ip_packet: &[u8], tun_hdr: &[u8; 4]) {
    let mut buf = Vec::with_capacity(4 + ip_packet.len());
    buf.extend_from_slice(tun_hdr);
    buf.extend_from_slice(ip_packet);
    unsafe {
        let r = libc::write(utun_fd, buf.as_ptr() as _, buf.len());
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                log::warn!("tun_relay: write to utun: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame processing: utun → AVF
// ---------------------------------------------------------------------------

fn process_utun_packet(packet: &[u8], relay_fd: RawFd, state: &RelayState) {
    if packet.len() < 5 {
        return;
    }

    let vm_mac = match state.vm_mac {
        Some(m) => m,
        None => {
            log::debug!("tun_relay: dropping inbound packet — VM MAC not yet learned");
            return;
        }
    };

    let af = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]);
    let ip_payload = &packet[4..];

    let ethertype: [u8; 2] = match af {
        2 => [0x08, 0x00],  // AF_INET
        30 => [0x86, 0xDD], // AF_INET6
        _ => {
            log::debug!("tun_relay: unknown AF {af} in utun packet — drop");
            return;
        }
    };

    // Build Ethernet frame: dst=vm_mac, src=GATEWAY_MAC, type=ethertype, payload=ip.
    let mut frame = Vec::with_capacity(14 + ip_payload.len());
    frame.extend_from_slice(&vm_mac);
    frame.extend_from_slice(&GATEWAY_MAC);
    frame.extend_from_slice(&ethertype);
    frame.extend_from_slice(ip_payload);

    send_to_avf(relay_fd, &frame);
}

fn send_to_avf(relay_fd: RawFd, frame: &[u8]) {
    unsafe {
        let r = libc::send(relay_fd, frame.as_ptr() as _, frame.len(), 0);
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                log::warn!("tun_relay: send to AVF: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ARP and NDP packet construction
// ---------------------------------------------------------------------------

/// Build an ARP reply advertising GATEWAY_MAC as the owner of GATEWAY_IP4.
fn build_arp_reply(vm_mac: &[u8; 6], vm_ip4: &[u8; 4]) -> Vec<u8> {
    let mut f = vec![0u8; 42]; // 14 eth + 28 arp
                               // Ethernet
    f[0..6].copy_from_slice(vm_mac);
    f[6..12].copy_from_slice(&GATEWAY_MAC);
    f[12] = 0x08;
    f[13] = 0x06; // ethertype ARP
                  // ARP
    f[14] = 0x00;
    f[15] = 0x01; // Ethernet
    f[16] = 0x08;
    f[17] = 0x00; // IPv4
    f[18] = 6; // hw len
    f[19] = 4; // proto len
    f[20] = 0x00;
    f[21] = 0x02; // reply
    f[22..28].copy_from_slice(&GATEWAY_MAC);
    f[28..32].copy_from_slice(&GATEWAY_IP4);
    f[32..38].copy_from_slice(vm_mac);
    f[38..42].copy_from_slice(vm_ip4);
    f
}

/// Build an NDP Neighbour Advertisement for one of our gateway IPv6 addresses.
///
/// `target_ip6`: the IPv6 address being advertised (fd00::1 or fe80::1).
/// `dst_ip6`:    where to send the NA (the IPv6 that sent the NS).
fn build_ndp_na(vm_mac: &[u8; 6], target_ip6: &[u8; 16], dst_ip6: &[u8; 16]) -> Vec<u8> {
    let src_ip6 = target_ip6; // NA source == the target address being advertised

    // ICMPv6 NA payload (32 bytes):
    // type(1) code(1) checksum(2) flags(4) target(16) option_type(1) option_len(1) mac(6)
    let mut icmp = [0u8; 32];
    icmp[0] = 136; // NA
    icmp[1] = 0;
    // [2..4] = checksum — computed below
    icmp[4] = 0x60; // S=1 (solicited), O=1 (override)
    icmp[8..24].copy_from_slice(target_ip6);
    icmp[24] = 2; // target link-layer address option type
    icmp[25] = 1; // option length (8 bytes / 8 = 1 unit)
    icmp[26..32].copy_from_slice(&GATEWAY_MAC);

    let cksum = icmpv6_checksum(src_ip6, dst_ip6, &icmp);
    icmp[2] = (cksum >> 8) as u8;
    icmp[3] = (cksum & 0xff) as u8;

    // Full frame: 14 eth + 40 IPv6 + 32 ICMPv6 = 86 bytes
    let mut f = Vec::with_capacity(86);
    // Ethernet
    f.extend_from_slice(vm_mac);
    f.extend_from_slice(&GATEWAY_MAC);
    f.push(0x86);
    f.push(0xDD);
    // IPv6 header (40 bytes)
    f.push(0x60); // version=6, TC=0, flow=0
    f.push(0x00);
    f.push(0x00);
    f.push(0x00);
    let plen = 32u16;
    f.extend_from_slice(&plen.to_be_bytes());
    f.push(58); // next header: ICMPv6
    f.push(255); // hop limit (required 255 for NDP)
    f.extend_from_slice(src_ip6);
    f.extend_from_slice(dst_ip6);
    // ICMPv6
    f.extend_from_slice(&icmp);
    f
}

/// One's-complement checksum over the ICMPv6 pseudo-header and payload.
fn icmpv6_checksum(src: &[u8; 16], dst: &[u8; 16], payload: &[u8]) -> u16 {
    let mut sum = 0u32;
    // Pseudo-header: source address
    for i in (0..16).step_by(2) {
        sum += u16::from_be_bytes([src[i], src[i + 1]]) as u32;
    }
    // Pseudo-header: destination address
    for i in (0..16).step_by(2) {
        sum += u16::from_be_bytes([dst[i], dst[i + 1]]) as u32;
    }
    // Pseudo-header: upper-layer length
    let len = payload.len() as u32;
    sum += len >> 16;
    sum += len & 0xffff;
    // Pseudo-header: next header = 58 (ICMPv6)
    sum += 58u32;
    // Payload
    let mut i = 0;
    while i + 1 < payload.len() {
        sum += u16::from_be_bytes([payload[i], payload[i + 1]]) as u32;
        i += 2;
    }
    if i < payload.len() {
        sum += (payload[i] as u32) << 8;
    }
    // Fold to 16 bits
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// pelagos-pfctl client
// ---------------------------------------------------------------------------

fn pfctl_send(json: &str) -> Result<(), crate::Error> {
    let stream = UnixStream::connect(PFCTL_SOCK)
        .map_err(|e| crate::Error::Runtime(format!("pfctl connect: {e}")))?;
    let mut writer = &stream;
    let mut msg = json.to_string();
    msg.push('\n');
    writer
        .write_all(msg.as_bytes())
        .map_err(|e| crate::Error::Runtime(format!("pfctl write: {e}")))?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader
        .read_line(&mut resp)
        .map_err(|e| crate::Error::Runtime(format!("pfctl read: {e}")))?;

    #[derive(serde::Deserialize)]
    struct Resp {
        ok: bool,
        error: Option<String>,
    }
    let r: Resp = serde_json::from_str(resp.trim())
        .map_err(|e| crate::Error::Runtime(format!("pfctl parse: {e}")))?;

    if r.ok {
        Ok(())
    } else {
        Err(crate::Error::Runtime(format!(
            "pfctl: {}",
            r.error.unwrap_or_default()
        )))
    }
}

fn pfctl_setup_utun(iface: &str, egress: &str) -> Result<(), crate::Error> {
    #[derive(Serialize)]
    struct Req<'a> {
        action: &'static str,
        iface: &'a str,
        ipv4_addr: &'static str,
        ipv4_peer: &'static str,
        ipv4_cidr: &'static str,
        egress_iface: &'a str,
    }
    let json = serde_json::to_string(&Req {
        action: "setup_utun",
        iface,
        ipv4_addr: "192.168.105.1",
        ipv4_peer: "192.168.105.2",
        ipv4_cidr: "192.168.105.0/24",
        egress_iface: egress,
    })
    .map_err(|e| crate::Error::Runtime(e.to_string()))?;
    pfctl_send(&json)
}

fn pfctl_teardown_utun(iface: &str) -> Result<(), crate::Error> {
    #[derive(Serialize)]
    struct Req<'a> {
        action: &'static str,
        iface: &'a str,
    }
    let json = serde_json::to_string(&Req {
        action: "teardown_utun",
        iface,
    })
    .map_err(|e| crate::Error::Runtime(e.to_string()))?;
    pfctl_send(&json)
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn detect_egress_iface() -> Option<String> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("interface:")
                .map(|s| s.trim().to_string())
        })
}

fn create_socketpair() -> Result<(RawFd, RawFd), crate::Error> {
    let mut sp = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sp.as_mut_ptr()) } != 0 {
        return Err(crate::Error::Io(std::io::Error::last_os_error()));
    }
    Ok((sp[0], sp[1]))
}

fn set_sock_bufs(fd: RawFd, sndbuf: libc::c_int, rcvbuf: libc::c_int) {
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const _,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const _,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
