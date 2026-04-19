//! pelagos-pfctl — privileged pf/utun helper daemon.
//!
//! Runs as root via a LaunchDaemon.  Listens on a Unix socket and executes
//! ifconfig/pfctl commands on behalf of the pelagos-mac CLI and daemon.
//!
//! Protocol: newline-delimited JSON.
//!
//! Legacy NAT66-only requests (for `pelagos nat66` subcommand):
//!   {"action":"load","iface":"en0"}
//!   {"action":"unload"}
//!   {"action":"status"}
//!
//! utun relay requests (for `tun_relay.rs`):
//!   {"action":"create_utun"}
//!     → creates a utun fd as root, sends {"ok":true,"iface":"utunN"} + fd via SCM_RIGHTS.
//!       The relay process receives the fd via recvmsg and holds it for the relay lifetime.
//!   {"action":"setup_utun","iface":"utun5",
//!    "ipv4_addr":"192.168.105.1","ipv4_peer":"192.168.105.2",
//!    "ipv4_cidr":"192.168.105.0/24","ipv6_addr":"fd00::1",
//!    "ipv6_prefix":64,"ipv6_cidr":"fd00::/64","egress_iface":"en0"}
//!   {"action":"teardown_utun","iface":"utun5"}
//!   {"action":"add_rdr","proto":"tcp","host_port":2222,
//!    "vm_ip":"192.168.105.2","vm_port":22}
//!   {"action":"remove_rdr","proto":"tcp","host_port":2222}
//!
//! Response: {"ok":true} | {"ok":false,"error":"..."} | {"ok":true,"active":true}
//!           create_utun additionally sends the utun fd as SCM_RIGHTS ancillary data.

use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const SOCK_PATH: &str = "/var/run/pelagos-pfctl.sock";

// Legacy NAT66-only anchor (kept for backward compat with `pelagos nat66` subcommand).
const ANCHOR_NAT66: &str = "com.apple/pelagos-nat66";
const ANCHOR_FILE_NAT66: &str = "/etc/pf.anchors/pelagos-nat66";

// Combined NAT44+NAT66 anchor used by the utun relay.
const ANCHOR_NAT: &str = "com.apple/pelagos-nat";
const ANCHOR_FILE_NAT: &str = "/etc/pf.anchors/pelagos-nat";

// RDR anchor used by the utun relay for port forwarding.
const ANCHOR_RDR: &str = "com.apple/pelagos-rdr";
const ANCHOR_FILE_RDR: &str = "/etc/pf.anchors/pelagos-rdr";

// ULA source prefix for the legacy NAT66-only rule.
const VM_PREFIX_V6: &str = "fd00::/64";

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

struct DaemonState {
    /// utun interface currently active (set by setup_utun, cleared by teardown_utun).
    utun_iface: Option<String>,
    /// Egress interface used by the active utun setup — needed for RDR rules.
    egress_iface: Option<String>,
    /// Active port-forward rules, rebuilt into the RDR anchor on every change.
    rdr_rules: Vec<RdrRule>,
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
            utun_iface: None,
            egress_iface: None,
            rdr_rules: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (must match request senders in nat66.rs and tun_relay.rs)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    // Legacy NAT66-only operations.
    Load {
        iface: String,
    },
    Unload,
    Status,
    // utun relay operations.
    SetupUtun {
        iface: String,
        ipv4_addr: String,
        ipv4_peer: String,
        ipv4_cidr: String,
        ipv6_addr: String,
        ipv6_prefix: u8,
        ipv6_cidr: String,
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
        Request::Load { iface } => handle_load(&iface),
        Request::Unload => handle_unload(),
        Request::Status => handle_status(),
        Request::SetupUtun {
            iface,
            ipv4_addr,
            ipv4_peer,
            ipv4_cidr,
            ipv6_addr,
            ipv6_prefix,
            ipv6_cidr,
            egress_iface,
        } => handle_setup_utun(
            &iface,
            &ipv4_addr,
            &ipv4_peer,
            &ipv4_cidr,
            &ipv6_addr,
            ipv6_prefix,
            &ipv6_cidr,
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
        Request::CreateUtun => unreachable!("handled above"),
    };

    if let Ok(mut out) = serde_json::to_string(&resp) {
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Legacy NAT66 handlers (load / unload / status)
// ---------------------------------------------------------------------------

fn handle_load(iface: &str) -> Response {
    if !is_safe_iface(iface) {
        return err_resp(format!("invalid interface name: {iface:?}"));
    }
    let rule = format!("nat on {iface} inet6 from {VM_PREFIX_V6} to any -> ({iface})\n");
    if let Err(e) = std::fs::write(ANCHOR_FILE_NAT66, &rule) {
        return err_resp(format!("write {ANCHOR_FILE_NAT66}: {e}"));
    }
    // Ensure pf is enabled (exit code 1 when already enabled is fine — ignore it).
    let _ = Command::new("/sbin/pfctl").arg("-e").output();
    match run_pfctl(&["-a", ANCHOR_NAT66, "-f", ANCHOR_FILE_NAT66]) {
        Ok(_) => {
            log::info!("nat66 loaded on {iface}: {}", rule.trim());
            ok_active(true)
        }
        Err(e) => err_resp(format!("pfctl: {e}")),
    }
}

fn handle_unload() -> Response {
    match run_pfctl(&["-a", ANCHOR_NAT66, "-F", "all"]) {
        Ok(_) => ok_active(false),
        Err(e) if e.contains("Anchor does not exist") => ok_active(false),
        Err(e) => err_resp(format!("pfctl: {e}")),
    }
}

fn handle_status() -> Response {
    let out = Command::new("/sbin/pfctl")
        .args(["-a", ANCHOR_NAT66, "-s", "nat"])
        .output();
    let active = match out {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => false,
    };
    ok_active(active)
}

// ---------------------------------------------------------------------------
// utun relay handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn handle_setup_utun(
    iface: &str,
    ipv4_addr: &str,
    ipv4_peer: &str,
    ipv4_cidr: &str,
    ipv6_addr: &str,
    ipv6_prefix: u8,
    ipv6_cidr: &str,
    egress_iface: &str,
    state: &mut DaemonState,
) -> Response {
    if !is_safe_iface(iface) || !is_safe_iface(egress_iface) {
        return err_resp("invalid interface name");
    }

    // 1. Assign IPv4 (point-to-point: local peer)
    if let Err(e) = run_ifconfig(&[iface, "inet", ipv4_addr, ipv4_peer, "up"]) {
        return err_resp(format!("ifconfig inet: {e}"));
    }

    // 2. Assign IPv6
    let prefix_str = ipv6_prefix.to_string();
    if let Err(e) = run_ifconfig(&[iface, "inet6", ipv6_addr, "prefixlen", &prefix_str]) {
        return err_resp(format!("ifconfig inet6: {e}"));
    }

    // 3. Enable kernel IP forwarding — the kernel must route packets that arrive on
    //    the utun interface out through the egress interface so pf NAT can fire.
    //    The smoltcp relay never needed this (pure userspace), but the utun relay does.
    if let Err(e) = run_sysctl_set("net.inet.ip.forwarding", "1") {
        return err_resp(format!("sysctl net.inet.ip.forwarding: {e}"));
    }
    if let Err(e) = run_sysctl_set("net.inet6.ip6.forwarding", "1") {
        log::warn!("sysctl net.inet6.ip6.forwarding: {e}"); // non-fatal — IPv6 may not be available
    }

    // 4. Write and load combined NAT44+NAT66 anchor.
    let nat_rules = format!(
        "nat on {egress_iface} inet from {ipv4_cidr} to any -> ({egress_iface})\n\
         nat on {egress_iface} inet6 from {ipv6_cidr} to any -> ({egress_iface})\n"
    );
    if let Err(e) = std::fs::write(ANCHOR_FILE_NAT, &nat_rules) {
        return err_resp(format!("write {ANCHOR_FILE_NAT}: {e}"));
    }
    let _ = Command::new("/sbin/pfctl").arg("-e").output();
    if let Err(e) = run_pfctl(&["-a", ANCHOR_NAT, "-f", ANCHOR_FILE_NAT]) {
        return err_resp(format!("pfctl nat anchor: {e}"));
    }

    log::info!("utun relay setup: iface={iface} egress={egress_iface}");
    state.utun_iface = Some(iface.to_string());
    state.egress_iface = Some(egress_iface.to_string());
    ok_resp()
}

fn handle_teardown_utun(iface: &str, state: &mut DaemonState) -> Response {
    // Flush NAT and RDR anchors — ignore errors (anchor may not be active).
    let _ = run_pfctl(&["-a", ANCHOR_NAT, "-F", "all"]);
    let _ = run_pfctl(&["-a", ANCHOR_RDR, "-F", "all"]);
    // Disable IP forwarding that was enabled by setup_utun.
    let _ = run_sysctl_set("net.inet.ip.forwarding", "0");
    let _ = run_sysctl_set("net.inet6.ip6.forwarding", "0");
    // Try to bring down the interface (it's destroyed when the relay fd closes anyway).
    let _ = run_ifconfig(&[iface, "down"]);
    state.utun_iface = None;
    state.egress_iface = None;
    state.rdr_rules.clear();
    log::info!("utun relay teardown: iface={iface}");
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
    reload_rdr_anchor(state)
}

fn handle_remove_rdr(proto: &str, host_port: u16, state: &mut DaemonState) -> Response {
    state
        .rdr_rules
        .retain(|r| !(r.proto == proto && r.host_port == host_port));
    reload_rdr_anchor(state)
}

fn reload_rdr_anchor(state: &DaemonState) -> Response {
    if state.rdr_rules.is_empty() {
        let _ = run_pfctl(&["-a", ANCHOR_RDR, "-F", "all"]);
        return ok_resp();
    }

    let mut rules = String::new();
    for r in &state.rdr_rules {
        // Redirect on loopback for local connections (e.g. `pelagos vm ssh`).
        rules.push_str(&format!(
            "rdr pass on lo0 proto {proto} from any to 127.0.0.1 port {hp} -> {vm_ip} port {vp}\n",
            proto = r.proto,
            hp = r.host_port,
            vm_ip = r.vm_ip,
            vp = r.vm_port,
        ));
        // Also redirect on the egress interface for external connections.
        if let Some(egress) = &state.egress_iface {
            rules.push_str(&format!(
                "rdr pass on {egress} proto {proto} from any to any port {hp} -> {vm_ip} port {vp}\n",
                proto = r.proto,
                hp = r.host_port,
                vm_ip = r.vm_ip,
                vp = r.vm_port,
            ));
        }
    }

    if let Err(e) = std::fs::write(ANCHOR_FILE_RDR, &rules) {
        return err_resp(format!("write {ANCHOR_FILE_RDR}: {e}"));
    }
    if let Err(e) = run_pfctl(&["-a", ANCHOR_RDR, "-f", ANCHOR_FILE_RDR]) {
        return err_resp(format!("pfctl rdr anchor: {e}"));
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

fn ok_active(active: bool) -> Response {
    Response {
        ok: true,
        error: None,
        active: Some(active),
    }
}

fn err_resp(msg: impl Into<String>) -> Response {
    Response {
        ok: false,
        error: Some(msg.into()),
        active: None,
    }
}
