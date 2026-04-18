//! pelagos-pfctl — privileged pf NAT66 helper daemon.
//!
//! Runs as root via a LaunchDaemon.  Listens on a Unix socket and executes
//! pfctl commands on behalf of the pelagos-mac CLI and daemon.
//!
//! Protocol: newline-delimited JSON.
//!   Request:  {"action":"load","iface":"en0"} | {"action":"unload"} | {"action":"status"}
//!   Response: {"ok":true} | {"ok":false,"error":"..."}

use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

use serde::{Deserialize, Serialize};

const SOCK_PATH: &str = "/var/run/pelagos-pfctl.sock";

/// Named anchor under com.apple/* so no /etc/pf.conf modification is needed —
/// the wildcard `nat-anchor "com.apple/*"` in the default macOS pf.conf picks
/// it up automatically.
const ANCHOR: &str = "com.apple/pelagos-nat66";
const ANCHOR_FILE: &str = "/etc/pf.anchors/pelagos-nat66";

/// ULA source prefix used by the pelagos VM.
const VM_PREFIX: &str = "fd00::/64";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Load { iface: String },
    Unload,
    Status,
}

#[derive(Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<bool>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if unsafe { libc::getuid() } != 0 {
        eprintln!("pelagos-pfctl: must run as root (uid 0)");
        std::process::exit(1);
    }

    // Remove stale socket from a previous run.
    let _ = std::fs::remove_file(SOCK_PATH);

    let listener = UnixListener::bind(SOCK_PATH).unwrap_or_else(|e| {
        eprintln!("pelagos-pfctl: bind {SOCK_PATH}: {e}");
        std::process::exit(1);
    });

    // 0660 chgrp admin — any admin-group member (i.e. all normal Mac users
    // via the default %admin sudoers membership) can send requests.
    set_socket_permissions();

    log::info!("listening on {SOCK_PATH}");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle_connection(s),
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
            // uid -1 (u32::MAX) means "don't change owner"
            libc::chown(path.as_ptr(), u32::MAX, (*grp).gr_gid);
        }
    }
}

fn handle_connection(stream: std::os::unix::net::UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut writer = &stream;
    let mut line = String::new();

    if let Err(e) = reader.read_line(&mut line) {
        log::warn!("read: {e}");
        return;
    }

    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::Load { iface }) => handle_load(&iface),
        Ok(Request::Unload) => handle_unload(),
        Ok(Request::Status) => handle_status(),
        Err(e) => Response {
            ok: false,
            error: Some(format!("parse error: {e}")),
            active: None,
        },
    };

    if let Ok(mut out) = serde_json::to_string(&resp) {
        out.push('\n');
        let _ = writer.write_all(out.as_bytes());
    }
}

fn handle_load(iface: &str) -> Response {
    // Reject interface names that could be used for shell injection.
    if !iface.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Response {
            ok: false,
            error: Some(format!("invalid interface name: {iface:?}")),
            active: None,
        };
    }

    // Write the anchor rule file.
    let rule = format!("nat on {iface} inet6 from {VM_PREFIX} to any -> ({iface})\n");
    if let Err(e) = std::fs::write(ANCHOR_FILE, &rule) {
        return Response {
            ok: false,
            error: Some(format!("write {ANCHOR_FILE}: {e}")),
            active: None,
        };
    }

    // Ensure pf is enabled.  The exit code is 1 when already enabled ("pf
    // already enabled"), which is fine — ignore that.
    let _ = Command::new("/sbin/pfctl").arg("-e").output();

    // Load the anchor.
    let out = Command::new("/sbin/pfctl")
        .args(["-a", ANCHOR, "-f", ANCHOR_FILE])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            log::info!("nat66 loaded on {iface}: {}", rule.trim());
            Response { ok: true, error: None, active: Some(true) }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            log::warn!("pfctl load failed: {stderr}");
            Response {
                ok: false,
                error: Some(format!("pfctl: {stderr}")),
                active: None,
            }
        }
        Err(e) => Response {
            ok: false,
            error: Some(format!("exec /sbin/pfctl: {e}")),
            active: None,
        },
    }
}

fn handle_unload() -> Response {
    let out = Command::new("/sbin/pfctl")
        .args(["-a", ANCHOR, "-F", "all"])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            log::info!("nat66 unloaded");
            Response { ok: true, error: None, active: Some(false) }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            // "Anchor does not exist" is fine — means the anchor was never loaded.
            if stderr.contains("Anchor does not exist") || stderr.contains("pfctl: ") {
                log::info!("nat66: anchor was not active (ok)");
                Response { ok: true, error: None, active: Some(false) }
            } else {
                log::warn!("pfctl unload: {stderr}");
                Response {
                    ok: false,
                    error: Some(format!("pfctl: {stderr}")),
                    active: None,
                }
            }
        }
        Err(e) => Response {
            ok: false,
            error: Some(format!("exec /sbin/pfctl: {e}")),
            active: None,
        },
    }
}

fn handle_status() -> Response {
    let out = Command::new("/sbin/pfctl")
        .args(["-a", ANCHOR, "-s", "nat"])
        .output();

    let active = match out {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => false,
    };
    Response { ok: true, error: None, active: Some(active) }
}
