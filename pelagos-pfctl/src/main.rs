//! pelagos-pfctl — privileged pf/utun helper daemon.
//!
//! Runs as root via a LaunchDaemon.  Listens on a Unix socket and executes
//! ifconfig/pfctl commands on behalf of the pelagos-mac CLI and daemon.
//!
//! Protocol: newline-delimited JSON.
//!
//! utun relay requests (for `tun_relay.rs`):
//!   {"action":"create_utun"}
//!     → creates a utun fd as root, sends {"ok":true,"iface":"utunN"} + fd via SCM_RIGHTS.
//!   {"action":"setup_utun","iface":"utun5",
//!    "ipv4_addr":"192.168.105.1","ipv4_peer":"192.168.105.2",
//!    "ipv4_cidr":"192.168.105.0/24","egress_iface":"en0"}
//!     → assigns IPv4 P2P address, enables ip4/ip6 forwarding, loads NAT44 pf anchor.
//!   {"action":"assign_utun_alias","iface":"utun5","addr":"2601:x:y:z::a1b2"}
//!     → aliases a GUA to the utun so the kernel delivers inbound IPv6 to the VM.
//!       Called by the relay after observing the VM's SLAAC DAD completion.
//!       Also sends an unsolicited Neighbour Advertisement on the egress interface
//!       (e.g. en0) to populate the upstream router's NDP cache immediately.
//!   {"action":"teardown_utun","iface":"utun5"}
//!   {"action":"add_rdr","proto":"tcp","host_port":2222,
//!    "vm_ip":"192.168.105.2","vm_port":22}
//!   {"action":"remove_rdr","proto":"tcp","host_port":2222}
//!
//! Response: {"ok":true} | {"ok":false,"error":"..."} | {"ok":true,"active":true}
//!           create_utun additionally sends the utun fd as SCM_RIGHTS ancillary data.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const SOCK_PATH: &str = "/var/run/pelagos-pfctl.sock";

// NAT44 anchor used by the utun relay.
const ANCHOR_NAT: &str = "com.apple/pelagos-nat";
const ANCHOR_FILE_NAT: &str = "/etc/pf.anchors/pelagos-nat";

// ---------------------------------------------------------------------------
// macOS utun constants and C structs (used by create_utun_privileged)
// ---------------------------------------------------------------------------

const PF_SYSTEM: libc::c_int = 32; // AF_SYSTEM
const AF_SYS_CONTROL: u16 = 2;
const SYSPROTO_CONTROL: libc::c_int = 2;
const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control\0";
const UTUN_OPT_IFNAME: libc::c_int = 2;
// CTLIOCGINFO = _IOWR('N', 3, struct ctl_info)  (ctl_info = 100 bytes)
const CTLIOCGINFO: libc::c_ulong = 0xc064_4e03;
const MAX_KCTL_NAME: usize = 96;
const IFNAMSIZ: usize = 16;

#[repr(C)]
struct CtlInfo {
    ctl_id: u32,
    ctl_name: [libc::c_char; MAX_KCTL_NAME],
}

#[repr(C)]
struct SockaddrCtl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: u16,
    sc_id: u32,
    sc_unit: u32,
    sc_reserved: [u32; 5],
}

// ---------------------------------------------------------------------------
// Daemon state (shared across sequential connection handling)
// ---------------------------------------------------------------------------

/// Per-utun relay state, tracked separately for each active VM profile.
struct UtunSetup {
    egress_iface: String,
    ipv4_cidr: String,
}

struct DaemonState {
    /// Active utun relays keyed by interface name (e.g. "utun10", "utun12").
    /// Each VM profile gets its own entry so their NAT rules coexist.
    active_utuns: HashMap<String, UtunSetup>,
    /// Active port-forward rules, rebuilt into the combined anchor on every change.
    rdr_rules: Vec<RdrRule>,
    /// GUA aliases assigned to utun interfaces via assign_utun_alias.
    /// Removed on teardown_utun for the corresponding interface.
    ipv6_aliases: Vec<(String, String)>,
}

#[derive(Clone)]
struct RdrRule {
    proto: String,
    host_port: u16,
    vm_ip: String,
    vm_port: u16,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            active_utuns: HashMap::new(),
            rdr_rules: Vec::new(),
            ipv6_aliases: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (must match request senders in tun_relay.rs)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    SetupUtun {
        iface: String,
        ipv4_addr: String,
        ipv4_peer: String,
        ipv4_cidr: String,
        egress_iface: String,
    },
    TeardownUtun {
        iface: String,
    },
    AddRdr {
        proto: String,
        host_port: u16,
        vm_ip: String,
        vm_port: u16,
    },
    RemoveRdr {
        proto: String,
        host_port: u16,
    },
    /// Create a utun fd as root and pass it to the relay via SCM_RIGHTS.
    CreateUtun,
    /// Assign a GUA IPv6 alias to a utun interface so the kernel delivers
    /// inbound traffic for that address to the utun (and thus the VM).
    AssignUtunAlias {
        iface: String,
        addr: String,
    },
}

#[derive(Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<bool>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if unsafe { libc::getuid() } != 0 {
        eprintln!("pelagos-pfctl: must run as root (uid 0)");
        std::process::exit(1);
    }

    let _ = std::fs::remove_file(SOCK_PATH);

    let listener = UnixListener::bind(SOCK_PATH).unwrap_or_else(|e| {
        eprintln!("pelagos-pfctl: bind {SOCK_PATH}: {e}");
        std::process::exit(1);
    });

    // 0660 chgrp admin — any admin-group member (all normal Mac users) can send requests.
    set_socket_permissions();

    log::info!("listening on {SOCK_PATH}");

    let state = Arc::new(Mutex::new(DaemonState::new()));

    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle_connection(s, Arc::clone(&state)),
            Err(e) => log::warn!("accept: {e}"),
        }
    }
}

fn set_socket_permissions() {
    let path = CString::new(SOCK_PATH).unwrap();
    unsafe {
        libc::chmod(path.as_ptr(), 0o660);
        let grnam = CString::new("admin").unwrap();
        let grp = libc::getgrnam(grnam.as_ptr());
        if !grp.is_null() {
            // uid u32::MAX means "don't change owner"
            libc::chown(path.as_ptr(), u32::MAX, (*grp).gr_gid);
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

fn handle_connection(stream: std::os::unix::net::UnixStream, state: Arc<Mutex<DaemonState>>) {
    // Read the request line first, then release the BufReader borrow.
    let line = {
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        if let Err(e) = reader.read_line(&mut line) {
            log::warn!("read: {e}");
            return;
        }
        line
    };

    // Parse the request.
    let req = match serde_json::from_str::<Request>(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response {
                ok: false,
                error: Some(format!("parse error: {e}")),
                active: None,
            };
            if let Ok(mut out) = serde_json::to_string(&resp) {
                out.push('\n');
                let _ = (&stream).write_all(out.as_bytes());
            }
            return;
        }
    };

    // CreateUtun is handled specially: the response includes an fd passed via SCM_RIGHTS,
    // which requires sendmsg rather than the normal write_all path.
    if let Request::CreateUtun = req {
        handle_create_utun_with_fd(&stream);
        return;
    }

    let mut st = state.lock().unwrap();
    let resp = match req {
        Request::SetupUtun {
            iface,
            ipv4_addr,
            ipv4_peer,
            ipv4_cidr,
            egress_iface,
        } => handle_setup_utun(
            &iface,
            &ipv4_addr,
            &ipv4_peer,
            &ipv4_cidr,
            &egress_iface,
            &mut st,
        ),
        Request::TeardownUtun { iface } => handle_teardown_utun(&iface, &mut st),
        Request::AddRdr {
            proto,
            host_port,
            vm_ip,
            vm_port,
        } => handle_add_rdr(&proto, host_port, &vm_ip, vm_port, &mut st),
        Request::RemoveRdr { proto, host_port } => handle_remove_rdr(&proto, host_port, &mut st),
        Request::AssignUtunAlias { iface, addr } => {
            handle_assign_utun_alias(&iface, &addr, &mut st)
        }
        Request::CreateUtun => unreachable!("handled above"),
    };

    if let Ok(mut out) = serde_json::to_string(&resp) {
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// utun relay handlers
// ---------------------------------------------------------------------------

fn handle_setup_utun(
    iface: &str,
    ipv4_addr: &str,
    ipv4_peer: &str,
    ipv4_cidr: &str,
    egress_iface: &str,
    state: &mut DaemonState,
) -> Response {
    if !is_safe_iface(iface) || !is_safe_iface(egress_iface) {
        return err_resp("invalid interface name");
    }

    // 1. Assign IPv4 (point-to-point: local peer)
    //
    // macOS utun interfaces are NOT automatically destroyed when their file
    // descriptors close (unlike Linux tun/tap).  A zombie utun from a crashed
    // or unclean previous session retains its IPv4 P2P address and keeps a
    // host route for ipv4_peer pointing at itself.  Delete any lingering host
    // route for ipv4_peer before assigning; the kernel will add a fresh route
    // for the new interface when `ifconfig ... up` runs.
    //
    // NOTE: we do NOT remove IPv4 addresses from other utun interfaces here
    // because those may belong to concurrently-running VM relays on different
    // subnets.  Per-profile subnet allocation guarantees that no two running
    // VMs share the same ipv4_peer, so only stale/zombie utuns would have a
    // conflicting route — and deleting the route is sufficient.
    let _ = run_route(&["delete", "-host", ipv4_peer]);

    if let Err(e) = run_ifconfig(&[iface, "inet", ipv4_addr, ipv4_peer, "up"]) {
        return err_resp(format!("ifconfig inet: {e}"));
    }

    // Add an IPv6 anchor address (fd00::1/128) so the kernel accepts host
    // routes via this interface.  macOS silently rejects `route add -inet6
    // -host ... -interface <iface>` with ENETUNREACH when the interface has
    // no IPv6 address at all.  A /128 creates only a local (lo0) host route
    // for fd00::1 — no /64 prefix, no NDP entries on en0.
    // The VM's guest daemon configures fd00::2/64 on its side; fd00::1/128
    // here is the matching host anchor.
    let _ = run_ifconfig(&[iface, "inet6", "fd00::1", "prefixlen", "128", "alias"]);

    // 2. Enable kernel IPv4 forwarding for NAT44 (utun → egress).
    //    IPv6 forwarding is also needed so packets from utun can be routed to
    //    the egress interface (VM → internet without NAT).
    if let Err(e) = run_sysctl_set("net.inet.ip.forwarding", "1") {
        return err_resp(format!("sysctl net.inet.ip.forwarding: {e}"));
    }
    if let Err(e) = run_sysctl_set("net.inet6.ip6.forwarding", "1") {
        return err_resp(format!("sysctl net.inet6.ip6.forwarding: {e}"));
    }

    // 3. Write and load NAT44 anchor (IPv4 only; no NAT66 — VM has real GUA via SLAAC).
    //
    // The anchor file includes a `pass quick` filter rule for the VM subnet
    // so that macOS Internet Sharing's network_isolation anchor (which uses
    // `block drop quick` for subnets it manages) cannot silently drop
    // host-to-VM traffic.  The `com.apple/*` anchor (which contains this
    // anchor) is evaluated before `com.apple.internet-sharing` in the main
    // ruleset, so `pass quick` here wins.
    //
    // We also remove the VM subnet from the network_isolation_table_v4 table
    // (best-effort: the table may not exist or may not contain this subnet).
    // The `pass quick` rule above is the durable fix; the table removal
    // prevents the block from re-appearing on the same pf pass.
    let _ = run_pfctl(&[
        "-a",
        "com.apple.internet-sharing/network_isolation",
        "-t",
        "network_isolation_table_v4",
        "-T",
        "delete",
        ipv4_cidr,
    ]);

    state.active_utuns.insert(
        iface.to_string(),
        UtunSetup {
            egress_iface: egress_iface.to_string(),
            ipv4_cidr: ipv4_cidr.to_string(),
        },
    );

    let _ = Command::new("/sbin/pfctl").arg("-e").output();
    let resp = reload_nat_anchor(state);
    if !resp.ok {
        return resp;
    }
    log::info!("utun relay setup: iface={iface} egress={egress_iface}");
    ok_resp()
}

fn handle_teardown_utun(iface: &str, state: &mut DaemonState) -> Response {
    state.active_utuns.remove(iface);
    let last_relay = state.active_utuns.is_empty();

    if last_relay {
        // Flush the combined NAT/RDR/filter anchor — ignore errors (may not be active).
        let _ = run_pfctl(&["-a", ANCHOR_NAT, "-F", "all"]);
        // Disable IP forwarding only when no utun relays remain.
        let _ = run_sysctl_set("net.inet.ip.forwarding", "0");
        let _ = run_sysctl_set("net.inet6.ip6.forwarding", "0");
        state.rdr_rules.clear();
    } else {
        // Other VMs still active — rebuild anchor without this utun's subnet.
        let _ = reload_nat_anchor(state);
    }
    // Remove any GUA aliases assigned to this utun via assign_utun_alias.
    state.ipv6_aliases.retain(|(alias_iface, addr)| {
        if alias_iface == iface {
            let _ = run_route(&["delete", "-inet6", "-host", addr.as_str()]);
            false // remove from list
        } else {
            true
        }
    });
    // Remove the IPv6 anchor address added at setup time.
    let _ = run_ifconfig(&[iface, "inet6", "fd00::1", "-alias"]);
    // Bring the interface down to clear the P2P IPv4 address and associated routes.
    // macOS utun interfaces persist after their fd closes; bringing them down
    // prevents the zombie from holding a stale route for the guest IP.
    let _ = run_ifconfig(&[iface, "down"]);
    log::info!(
        "utun relay teardown: iface={iface} (remaining={})",
        state.active_utuns.len()
    );
    ok_resp()
}

fn handle_assign_utun_alias(iface: &str, addr: &str, state: &mut DaemonState) -> Response {
    if !is_safe_iface(iface) {
        return err_resp("invalid interface name");
    }
    let gua: std::net::Ipv6Addr = match addr.parse() {
        Ok(a) => a,
        Err(_) => return err_resp(format!("invalid IPv6 address: {addr:?}")),
    };
    // Add a host route for vm_gua via the utun interface instead of an ifconfig
    // alias.  An ifconfig inet6 alias on a P2P tunnel always creates a /64
    // neighbor entry regardless of the prefixlen argument; the kernel then tries
    // NDP resolution into the tunnel, gets nothing, stays INCOMPLETE, and drops
    // inbound packets.  A direct host route bypasses NDP entirely.
    if let Err(e) = run_route(&["add", "-inet6", "-host", addr, "-interface", iface]) {
        return err_resp(format!("route add -inet6 -host: {e}"));
    }
    log::info!("added host route {addr} → {iface}");
    state
        .ipv6_aliases
        .push((iface.to_string(), addr.to_string()));

    // Send an unsolicited Neighbour Advertisement on the egress interface to
    // populate the upstream router's NDP cache.  This resolves "who has
    // <vm_gua>?" before the router ever asks — or, if the router already has
    // an INCOMPLETE entry (it tried and failed), the NA with O=1 will update
    // that entry to REACHABLE immediately.
    if let Some(setup) = state.active_utuns.get(iface) {
        let egress = setup.egress_iface.clone();
        match get_iface_mac(&egress) {
            Ok(mac) => match open_bpf(&egress) {
                Ok(bpf_fd) => {
                    let gua_bytes = gua.octets();
                    match send_gratuitous_na(bpf_fd, mac, gua_bytes) {
                        Ok(()) => log::info!("sent gratuitous NA for {addr} on {egress}"),
                        Err(e) => log::warn!("gratuitous NA send failed: {e}"),
                    }
                    // Send a unicast probe NS from vm_gua to the upstream router so
                    // the router creates a cache entry for vm_gua → en0_mac.
                    // The NS is sent with Ethernet dst = router_mac (unicast), so our
                    // own en0 NIC discards the frame — preventing the host's NDP stack
                    // from creating a spurious en0 entry that would override the utun10
                    // host route.
                    match detect_ipv6_gateway(&egress) {
                        Some(router) => match get_ndp_mac(router) {
                            Some(router_mac) => {
                                match send_ndp_probe_ns(bpf_fd, mac, router_mac, gua_bytes, router)
                                {
                                    Ok(()) => log::info!(
                                        "sent unicast NDP probe NS for {addr} on {egress}"
                                    ),
                                    Err(e) => log::warn!("NDP probe NS failed: {e}"),
                                }
                            }
                            None => {
                                log::warn!("router MAC not in NDP table yet, skipping NS probe")
                            }
                        },
                        None => {
                            log::warn!("no IPv6 default gateway on {egress}, skipping NS probe")
                        }
                    }
                    unsafe { libc::close(bpf_fd) };
                }
                Err(e) => log::warn!("open_bpf({egress}): {e}"),
            },
            Err(e) => log::warn!("get_iface_mac({egress}): {e}"),
        }
    } else {
        log::warn!("assign_utun_alias: no egress_iface in state, skipping gratuitous NA");
    }

    ok_resp()
}

fn handle_add_rdr(
    proto: &str,
    host_port: u16,
    vm_ip: &str,
    vm_port: u16,
    state: &mut DaemonState,
) -> Response {
    if proto != "tcp" && proto != "udp" {
        return err_resp(format!("invalid proto: {proto:?}"));
    }
    // Overwrite any existing rule for this proto+port.
    state
        .rdr_rules
        .retain(|r| !(r.proto == proto && r.host_port == host_port));
    state.rdr_rules.push(RdrRule {
        proto: proto.to_string(),
        host_port,
        vm_ip: vm_ip.to_string(),
        vm_port,
    });
    reload_nat_anchor(state)
}

fn handle_remove_rdr(proto: &str, host_port: u16, state: &mut DaemonState) -> Response {
    state
        .rdr_rules
        .retain(|r| !(r.proto == proto && r.host_port == host_port));
    reload_nat_anchor(state)
}

/// Rebuild and reload the combined pelagos-nat anchor.
///
/// pf requires rules in section order: translation (nat, rdr) then filtering (pass/block).
/// All pelagos rules live in a single `com.apple/pelagos-nat` anchor so that the
/// `nat-anchor "com.apple/*"`, `rdr-anchor "com.apple/*"`, and `anchor "com.apple/*"`
/// wildcards in the main ruleset all traverse the same anchor file.  Using separate
/// `pelagos-nat` and `pelagos-rdr` sub-anchors does NOT work because pf's wildcard
/// expansion only finds sub-anchors that are explicitly referenced from their parent —
/// loading rules directly into `com.apple/pelagos-rdr` via pfctl does not register it
/// under the `com.apple` anchor for wildcard traversal.
fn reload_nat_anchor(state: &DaemonState) -> Response {
    let mut rules = String::new();

    // Translation section: one NAT masquerade rule per active utun relay.
    // Iterating a HashMap is non-deterministic, but pf evaluates NAT rules
    // first-match; since each rule covers a distinct CIDR there is no overlap.
    for setup in state.active_utuns.values() {
        let egress = &setup.egress_iface;
        let cidr = &setup.ipv4_cidr;
        rules.push_str(&format!(
            "nat on {egress} inet from {cidr} to any -> ({egress})\n"
        ));
    }

    // Derive a representative egress for RDR rules (all VMs share the same
    // physical egress interface, e.g. en0 — any active utun's egress will do).
    let any_egress: Option<&str> = state
        .active_utuns
        .values()
        .next()
        .map(|s| s.egress_iface.as_str());

    // Translation section: RDR port-forward rules.
    for r in &state.rdr_rules {
        // Redirect on loopback so localhost connections reach the VM.
        rules.push_str(&format!(
            "rdr pass on lo0 proto {proto} from any to 127.0.0.1 port {hp} -> {vm_ip} port {vp}\n",
            proto = r.proto,
            hp = r.host_port,
            vm_ip = r.vm_ip,
            vp = r.vm_port,
        ));
        // Also redirect on the egress interface for external inbound connections.
        if let Some(egress) = any_egress {
            rules.push_str(&format!(
                "rdr pass on {egress} proto {proto} from any to any port {hp} -> {vm_ip} port {vp}\n",
                proto = r.proto,
                hp = r.host_port,
                vm_ip = r.vm_ip,
                vp = r.vm_port,
            ));
        }
    }

    // Filter section: one pass rule per active utun so that macOS
    // internet-sharing's network_isolation anchor cannot block host<->VM traffic.
    for setup in state.active_utuns.values() {
        let cidr = &setup.ipv4_cidr;
        rules.push_str(&format!("pass quick inet from {cidr} to {cidr}\n"));
    }

    if rules.is_empty() {
        let _ = run_pfctl(&["-a", ANCHOR_NAT, "-F", "all"]);
        return ok_resp();
    }

    if let Err(e) = std::fs::write(ANCHOR_FILE_NAT, &rules) {
        return err_resp(format!("write {ANCHOR_FILE_NAT}: {e}"));
    }
    if let Err(e) = run_pfctl(&["-a", ANCHOR_NAT, "-f", ANCHOR_FILE_NAT]) {
        return err_resp(format!("pfctl nat anchor: {e}"));
    }
    ok_resp()
}

// ---------------------------------------------------------------------------
// utun fd creation (privileged — runs as root inside pelagos-pfctl)
// ---------------------------------------------------------------------------

/// Create a macOS utun interface and return the fd + interface name.
///
/// This requires root because `connect(2)` on a PF_SYSTEM/SYSPROTO_CONTROL socket
/// fails with EPERM for unprivileged processes on macOS.
fn create_utun_privileged() -> Result<(OwnedFd, String), String> {
    let io_err = |msg: &str| format!("{}: {}", msg, std::io::Error::last_os_error());

    unsafe {
        let raw_fd = libc::socket(PF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL);
        if raw_fd < 0 {
            return Err(io_err("socket(PF_SYSTEM)"));
        }
        let owned = OwnedFd::from_raw_fd(raw_fd);

        let mut ci = CtlInfo {
            ctl_id: 0,
            ctl_name: [0; MAX_KCTL_NAME],
        };
        std::ptr::copy_nonoverlapping(
            UTUN_CONTROL_NAME.as_ptr() as *const libc::c_char,
            ci.ctl_name.as_mut_ptr(),
            UTUN_CONTROL_NAME.len(),
        );
        if libc::ioctl(
            owned.as_raw_fd(),
            CTLIOCGINFO,
            &mut ci as *mut CtlInfo as *mut _,
        ) < 0
        {
            return Err(io_err("ioctl(CTLIOCGINFO)"));
        }

        let sc = SockaddrCtl {
            sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
            sc_family: PF_SYSTEM as u8,
            ss_sysaddr: AF_SYS_CONTROL,
            sc_id: ci.ctl_id,
            sc_unit: 0, // 0 = let kernel assign the next free utunN
            sc_reserved: [0; 5],
        };
        if libc::connect(
            owned.as_raw_fd(),
            &sc as *const SockaddrCtl as *const libc::sockaddr,
            std::mem::size_of::<SockaddrCtl>() as libc::socklen_t,
        ) < 0
        {
            return Err(io_err("connect(utun)"));
        }

        let mut ifname = [0u8; IFNAMSIZ];
        let mut optlen = IFNAMSIZ as libc::socklen_t;
        if libc::getsockopt(
            owned.as_raw_fd(),
            SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            ifname.as_mut_ptr() as *mut _,
            &mut optlen,
        ) < 0
        {
            return Err(io_err("getsockopt(UTUN_OPT_IFNAME)"));
        }

        let name_len = ifname
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(optlen as usize);
        let iface = String::from_utf8_lossy(&ifname[..name_len]).into_owned();
        Ok((owned, iface))
    }
}

/// Handle a `create_utun` request.
///
/// Unlike all other handlers, this one sends the response directly via `sendmsg(2)`
/// with SCM_RIGHTS ancillary data containing the utun fd — so it cannot go through
/// the normal `write_all` path.
///
/// Protocol:
///   • iov[0]   = JSON response bytes  {"ok":true,"iface":"utunN"}\n
///   • cmsg     = SOL_SOCKET / SCM_RIGHTS / [utun_raw_fd]
///
/// The kernel increments the utun fd's reference count when the message is enqueued.
/// The receiver calls recvmsg(2) to get a new fd in its process pointing to the same
/// open file description.  pelagos-pfctl's copy closes when utun_fd drops at function
/// end — safe because the kernel holds a reference until the receiver retrieves it.
fn handle_create_utun_with_fd(stream: &std::os::unix::net::UnixStream) {
    let (utun_fd, iface) = match create_utun_privileged() {
        Ok(r) => r,
        Err(e) => {
            log::error!("create_utun: {e}");
            // Error path can use the normal write path — no fd to pass.
            let resp = format!("{{\"ok\":false,\"error\":\"{e}\"}}\n");
            let _ = (&*stream).write_all(resp.as_bytes());
            return;
        }
    };

    let json = format!("{{\"ok\":true,\"iface\":\"{iface}\"}}\n");
    let json_bytes = json.as_bytes();
    let stream_raw = stream.as_raw_fd();
    let utun_raw = utun_fd.as_raw_fd();

    unsafe {
        let mut iov = libc::iovec {
            iov_base: json_bytes.as_ptr() as *mut libc::c_void,
            iov_len: json_bytes.len(),
        };

        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space as libc::socklen_t;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32);
        let fd_ptr = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
        *fd_ptr = utun_raw;

        let r = libc::sendmsg(stream_raw, &msg, 0);
        if r < 0 {
            log::error!("create_utun: sendmsg: {}", std::io::Error::last_os_error());
        } else {
            log::info!("create_utun: iface={iface} fd={utun_raw} passed to relay");
        }
        // utun_fd drops here — kernel already has a reference for the in-flight message.
    }
}

// ---------------------------------------------------------------------------
// NDP proxy helpers — gratuitous NA via BPF raw injection
// ---------------------------------------------------------------------------

// BIOCSETIF = _IOW('B', 108, struct ifreq) on macOS.
// sizeof(struct ifreq) on arm64 macOS = ifr_name[16] + union sockaddr(16) = 32.
const BIOCSETIF: libc::c_ulong = 0x8020_426c;

/// Minimal ifreq layout: only ifr_name is read by BIOCSETIF.
#[repr(C)]
struct Ifreq {
    ifr_name: [libc::c_char; 16],
    _ifr_union: [u8; 16],
}

/// Read the Ethernet MAC address of `iface` from `ifconfig` output.
fn get_iface_mac(iface: &str) -> Result<[u8; 6], String> {
    let out = Command::new("/sbin/ifconfig")
        .arg(iface)
        .output()
        .map_err(|e| format!("ifconfig {iface}: {e}"))?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if let Some(rest) = line.trim().strip_prefix("ether ") {
            let mac_str = rest.split_whitespace().next().unwrap_or("");
            let parts: Vec<u8> = mac_str
                .split(':')
                .filter_map(|x| u8::from_str_radix(x, 16).ok())
                .collect();
            if parts.len() == 6 {
                return Ok([parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]]);
            }
        }
    }
    Err(format!("no MAC address found for {iface}"))
}

/// Open the first available `/dev/bpfN` device and attach it to `iface`.
/// Returns the raw fd; caller must close it.
fn open_bpf(iface: &str) -> Result<i32, String> {
    for i in 0..10u32 {
        let path = CString::new(format!("/dev/bpf{i}")).unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            continue; // device busy or past the end
        }
        let mut ifr: Ifreq = unsafe { std::mem::zeroed() };
        let name_bytes = iface.as_bytes();
        let copy_len = name_bytes.len().min(15);
        for (dst, &src) in ifr.ifr_name[..copy_len].iter_mut().zip(name_bytes) {
            *dst = src as libc::c_char;
        }
        let r = unsafe { libc::ioctl(fd, BIOCSETIF, &ifr) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(format!("BIOCSETIF /dev/bpf{i} {iface}: {e}"));
        }
        return Ok(fd);
    }
    Err(format!("no available /dev/bpf device for {iface}"))
}

/// Compute ICMPv6 checksum over the IPv6 pseudo-header and `payload`
/// (with the checksum field zeroed).  Per RFC 4443 §2.3.
fn icmpv6_checksum(src: &[u8; 16], dst: &[u8; 16], payload: &[u8]) -> u16 {
    // Pseudo-header: src(16) + dst(16) + upper-layer length(4) + zeros(3) + next-header(1)
    let ulen = payload.len() as u32;
    let pseudo = {
        let mut p = [0u8; 40];
        p[0..16].copy_from_slice(src);
        p[16..32].copy_from_slice(dst);
        p[32..36].copy_from_slice(&ulen.to_be_bytes());
        // p[36..39] = 0 (zeroed)
        p[39] = 58; // next-header = ICMPv6
        p
    };

    let mut sum: u32 = 0;
    let mut add = |bytes: &[u8]| {
        let mut i = 0;
        while i + 1 < bytes.len() {
            sum += u16::from_be_bytes([bytes[i], bytes[i + 1]]) as u32;
            i += 2;
        }
        if i < bytes.len() {
            sum += (bytes[i] as u32) << 8;
        }
    };
    add(&pseudo);
    add(payload);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parse the default IPv6 gateway for `egress` from `netstat -rn -f inet6`.
///
/// Returns the gateway address (link-local or global) as raw bytes, without
/// any zone-id suffix.  Returns None if no default route is found.
fn detect_ipv6_gateway(egress: &str) -> Option<[u8; 16]> {
    let out = Command::new("/usr/sbin/netstat")
        .args(["-rn", "-f", "inet6"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        // e.g. "default  fe80::1afd:74ff:fec2:614f%en0  UGcg  en0"
        // Use `else { continue }` — `?` inside a loop exits the whole function.
        let mut fields = line.split_whitespace();
        let Some(dest) = fields.next() else { continue };
        let Some(gw_raw) = fields.next() else {
            continue;
        };
        let Some(_flags) = fields.next() else {
            continue;
        };
        let Some(iface) = fields.next() else { continue };
        if dest == "default" && iface == egress {
            let gw_str = gw_raw.split('%').next().unwrap_or(gw_raw);
            if let Ok(ip6) = gw_str.parse::<std::net::Ipv6Addr>() {
                return Some(ip6.octets());
            }
        }
    }
    None
}

/// Look up the link-layer address of `addr` in the local NDP table (`ndp -an`).
/// Returns None if the entry is absent or unresolved (INCOMPLETE).
fn get_ndp_mac(addr: [u8; 16]) -> Option<[u8; 6]> {
    let target = std::net::Ipv6Addr::from(addr).to_string();
    let out = Command::new("/usr/sbin/ndp").args(["-an"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let mut fields = line.split_whitespace();
        let Some(entry_raw) = fields.next() else {
            continue;
        };
        // Strip zone ID (e.g. "%en0") before comparing.
        let entry_addr = entry_raw.split('%').next().unwrap_or(entry_raw);
        if entry_addr != target {
            continue;
        }
        let Some(mac_str) = fields.next() else {
            continue;
        };
        // Skip unresolved entries (shown as "(incomplete)" etc.)
        if !mac_str.contains(':') {
            continue;
        }
        let parts: Vec<&str> = mac_str.split(':').collect();
        if parts.len() != 6 {
            continue;
        }
        let mut mac = [0u8; 6];
        for (i, p) in parts.iter().enumerate() {
            mac[i] = u8::from_str_radix(p, 16).ok()?;
        }
        return Some(mac);
    }
    None
}

/// Inject a **unicast** Neighbour Solicitation from `vm_gua` (SLLA=`en0_mac`)
/// targeting `router` via `bpf_fd`, with Ethernet dst = `router_mac`.
///
/// Sending unicast (Ethernet dst = router_mac, not a multicast address) means
/// the frame is delivered only to the router.  Our own en0 NIC discards it
/// because dst_mac ≠ en0_mac — so the host's NDP stack never processes the
/// frame and never creates a spurious cache entry for vm_gua on en0.
///
/// The router, per RFC 4861 §7.2.3, creates a Neighbor Cache entry for
/// `vm_gua → en0_mac` (STALE) on receipt, enabling it to deliver return
/// packets to vm_gua without an additional NS/NA round-trip.
///
/// Frame: Ethernet (unicast) → IPv6 → ICMPv6 NS (type 135)
///   eth dst: router_mac  (unicast — host NIC will not pass this up the stack)
///   eth src: en0_mac
///   ip6 src: vm_gua
///   ip6 dst: router (unicast)
///   target:  router
///   option:  SLLA (type 1) = en0_mac
fn send_ndp_probe_ns(
    bpf_fd: i32,
    en0_mac: [u8; 6],
    router_mac: [u8; 6],
    vm_gua: [u8; 16],
    router: [u8; 16],
) -> Result<(), String> {
    // ICMPv6 NS payload (32 bytes):
    //   type(1) code(1) checksum(2) reserved(4) target(16) SLLA-option(8)
    let mut icmp = [0u8; 32];
    icmp[0] = 135; // NS
                   // [1] code = 0, [2..4] checksum, [4..8] reserved = 0
    icmp[8..24].copy_from_slice(&router); // target
    icmp[24] = 1; // option type: Source Link-Layer Address
    icmp[25] = 1; // length in units of 8 bytes
    icmp[26..32].copy_from_slice(&en0_mac);
    // Checksum pseudo-header uses the unicast router address as dst.
    let csum = icmpv6_checksum(&vm_gua, &router, &icmp);
    icmp[2..4].copy_from_slice(&csum.to_be_bytes());

    let mut frame = [0u8; 86];
    frame[0..6].copy_from_slice(&router_mac); // Ethernet dst: unicast to router
    frame[6..12].copy_from_slice(&en0_mac);
    frame[12..14].copy_from_slice(&[0x86, 0xdd]);
    frame[14] = 0x60; // IPv6
    frame[18..20].copy_from_slice(&32u16.to_be_bytes()); // payload length
    frame[20] = 58; // ICMPv6
    frame[21] = 255; // hop limit
    frame[22..38].copy_from_slice(&vm_gua); // src
    frame[38..54].copy_from_slice(&router); // dst: unicast router address
    frame[54..86].copy_from_slice(&icmp);

    let r = unsafe { libc::write(bpf_fd, frame.as_ptr() as *const libc::c_void, frame.len()) };
    if r < 0 {
        Err(format!("BPF write NS: {}", std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

/// Build and inject an unsolicited Neighbour Advertisement for `vm_gua` onto
/// the link via `bpf_fd`.  Advertises `en0_mac` as the link-layer address so
/// the upstream router updates its NDP cache (INCOMPLETE → REACHABLE) without
/// waiting for a probe/reply cycle.
///
/// Frame: Ethernet → IPv6 → ICMPv6 NA (type 136)
///   dst:    ff02::1 / 33:33:00:00:00:01  (all-nodes multicast)
///   src:    vm_gua  / en0_mac
///   flags:  O=1 (override — forces cache update for existing entries)
///   target: vm_gua
///   option: TLLA (type 2) = en0_mac
fn send_gratuitous_na(bpf_fd: i32, en0_mac: [u8; 6], vm_gua: [u8; 16]) -> Result<(), String> {
    const ALL_NODES_MAC: [u8; 6] = [0x33, 0x33, 0x00, 0x00, 0x00, 0x01];
    const ALL_NODES_IP6: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];

    // ICMPv6 NA payload (32 bytes):
    //   type(1) code(1) checksum(2) flags(4) target(16) TLLA-option(8)
    let mut icmp = [0u8; 32];
    icmp[0] = 136; // NA
    icmp[1] = 0; // code
                 // [2..4] checksum — filled below
    icmp[4] = 0x20; // R=0 S=0 O=1 → byte 4 of the flags word = 0b0010_0000
    icmp[8..24].copy_from_slice(&vm_gua);
    icmp[24] = 2; // option type: Target Link-Layer Address
    icmp[25] = 1; // option length in units of 8 bytes
    icmp[26..32].copy_from_slice(&en0_mac);

    let csum = icmpv6_checksum(&vm_gua, &ALL_NODES_IP6, &icmp);
    icmp[2..4].copy_from_slice(&csum.to_be_bytes());

    // Full Ethernet + IPv6 + ICMPv6 frame = 14 + 40 + 32 = 86 bytes.
    let mut frame = [0u8; 86];
    // Ethernet header
    frame[0..6].copy_from_slice(&ALL_NODES_MAC);
    frame[6..12].copy_from_slice(&en0_mac);
    frame[12..14].copy_from_slice(&[0x86, 0xdd]); // ethertype IPv6
                                                  // IPv6 header (offset 14)
    frame[14] = 0x60; // version=6, TC=0, FL=0
                      // [15..18] = 0 (TC/FL continued)
    frame[18..20].copy_from_slice(&32u16.to_be_bytes()); // payload length
    frame[20] = 58; // next-header = ICMPv6
    frame[21] = 255; // hop limit
    frame[22..38].copy_from_slice(&vm_gua); // src
    frame[38..54].copy_from_slice(&ALL_NODES_IP6); // dst ff02::1
                                                   // ICMPv6 payload (offset 54)
    frame[54..86].copy_from_slice(&icmp);

    let r = unsafe { libc::write(bpf_fd, frame.as_ptr() as *const libc::c_void, frame.len()) };
    if r < 0 {
        Err(format!("BPF write: {}", std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// System command helpers
// ---------------------------------------------------------------------------

/// Returns true if `s` is a safe interface name (no shell metacharacters).
/// Required for args passed to pfctl rules where the name appears in rule text.
/// (ifconfig args are passed directly via Command::args, not a shell, so this
/// only matters for the anchor rule strings written to the anchor files.)
fn is_safe_iface(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn run_pfctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/sbin/pfctl")
        .args(args)
        .output()
        .map_err(|e| format!("exec /sbin/pfctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn run_sysctl_set(key: &str, val: &str) -> Result<(), String> {
    let out = Command::new("/usr/sbin/sysctl")
        .args(["-w", &format!("{key}={val}")])
        .output()
        .map_err(|e| format!("exec /usr/sbin/sysctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn run_route(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/sbin/route")
        .args(args)
        .output()
        .map_err(|e| format!("exec /sbin/route: {e}"))?;
    // macOS `route` often exits 0 even when the kernel rejects the route,
    // printing the error to stderr instead.  Treat any stderr output as failure.
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || !stderr.trim().is_empty() {
        return Err(format!(
            "{}{}",
            stderr,
            String::from_utf8_lossy(&out.stdout)
        ));
    }
    Ok(())
}

fn run_ifconfig(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/sbin/ifconfig")
        .args(args)
        .output()
        .map_err(|e| format!("exec /sbin/ifconfig: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Response constructors
// ---------------------------------------------------------------------------

fn ok_resp() -> Response {
    Response {
        ok: true,
        error: None,
        active: None,
    }
}

fn err_resp(msg: impl Into<String>) -> Response {
    Response {
        ok: false,
        error: Some(msg.into()),
        active: None,
    }
}
