//! IPv6 NAT66 management via the pelagos-pfctl LaunchDaemon helper.
//!
//! The helper runs as root and owns all pfctl invocations.  This module is
//! the client side: it detects the host's global IPv6 address, sends
//! load/unload requests over the helper's Unix socket, and exposes the
//! install/uninstall/status subcommand implementations.

use std::ffi::CStr;
use std::io::{BufRead, BufReader, Write};
use std::net::Ipv6Addr;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Unix socket exposed by the pelagos-pfctl LaunchDaemon.
const SOCK_PATH: &str = "/var/run/pelagos-pfctl.sock";

/// Where the helper binary is installed by `pelagos nat66 install`.
const HELPER_INSTALL_PATH: &str = "/usr/local/lib/pelagos/pelagos-pfctl";

/// LaunchDaemon plist path.
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.pelagos.pfctl.plist";

/// LaunchDaemon label.
const LAUNCHD_LABEL: &str = "com.pelagos.pfctl";

// ---------------------------------------------------------------------------
// Wire types (must match pelagos-pfctl/src/main.rs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request<'a> {
    Load { iface: &'a str },
    Unload,
    Status,
}

#[derive(Deserialize)]
struct Response {
    ok: bool,
    error: Option<String>,
    active: Option<bool>,
}

// ---------------------------------------------------------------------------
// IPv6 host detection
// ---------------------------------------------------------------------------

/// Find the first network interface carrying a globally-routable IPv6 address
/// (not link-local, not ULA, not loopback).
///
/// Returns `(interface_name, address)` or `None` if no such interface exists.
pub fn detect_global_ipv6_iface() -> Option<(String, Ipv6Addr)> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut result = None;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family == libc::AF_INET6 as libc::sa_family_t
            {
                let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                let bytes = sin6.sin6_addr.s6_addr;
                if is_global_unicast(&bytes) {
                    let name = CStr::from_ptr(ifa.ifa_name)
                        .to_string_lossy()
                        .into_owned();
                    result = Some((name, Ipv6Addr::from(bytes)));
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        result
    }
}

/// Returns true for addresses in the global unicast range (2000::/3), i.e.
/// not loopback (::1), not link-local (fe80::/10), not ULA (fc00::/7).
fn is_global_unicast(b: &[u8; 16]) -> bool {
    if *b == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] {
        return false; // loopback ::1
    }
    if b[0] == 0xfe && (b[1] & 0xc0) == 0x80 {
        return false; // link-local fe80::/10
    }
    if (b[0] & 0xfe) == 0xfc {
        return false; // ULA fc00::/7
    }
    true
}

// ---------------------------------------------------------------------------
// Helper socket client
// ---------------------------------------------------------------------------

fn send_request<'a>(req: &Request<'a>) -> Result<Response, String> {
    let stream = UnixStream::connect(SOCK_PATH)
        .map_err(|e| format!("connect {SOCK_PATH}: {e}"))?;
    let mut writer = &stream;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;

    let r: Response = serde_json::from_str(resp.trim()).map_err(|e| e.to_string())?;
    if r.ok {
        Ok(r)
    } else {
        Err(r.error.unwrap_or_else(|| "helper returned error".into()))
    }
}

/// Ask the helper to install the NAT66 rule for `iface`.
///
/// Returns `Ok(true)` on success, `Ok(false)` if the helper is not installed
/// (non-fatal — IPv6 NAT is optional), `Err` if the helper is present but
/// the pfctl operation failed.
pub fn load(iface: &str) -> Result<bool, String> {
    match send_request(&Request::Load { iface }) {
        Ok(r) => Ok(r.active.unwrap_or(true)),
        Err(e) if e.contains("connect") => Ok(false), // helper not installed
        Err(e) => Err(e),
    }
}

/// Ask the helper to remove the NAT66 rule.  Silently succeeds if the helper
/// is not installed.
pub fn unload() -> Result<(), String> {
    match send_request(&Request::Unload) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("connect") => Ok(()), // helper not installed
        Err(e) => Err(e),
    }
}

/// Returns true if the helper daemon socket is reachable.
pub fn helper_available() -> bool {
    UnixStream::connect(SOCK_PATH).is_ok()
}

/// Query whether a NAT66 rule is currently active.  Returns None if the
/// helper is not running.
pub fn status_active() -> Option<bool> {
    send_request(&Request::Status).ok()?.active
}

// ---------------------------------------------------------------------------
// `pelagos nat66 install` / `uninstall` / `status`
// ---------------------------------------------------------------------------

/// Install the pelagos-pfctl helper binary and register it as a system
/// LaunchDaemon.  Must be called as root (sudo).
pub fn cmd_install() -> Result<(), String> {
    if unsafe { libc::getuid() } != 0 {
        return Err(
            "pelagos nat66 install must run as root.\nRun: sudo pelagos nat66 install".into(),
        );
    }

    // Locate the pelagos-pfctl binary alongside the running pelagos binary.
    let helper_src = find_helper_binary()?;
    log::debug!("helper binary source: {}", helper_src.display());

    // Create destination directory.
    let dest_dir = Path::new(HELPER_INSTALL_PATH).parent().unwrap();
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("mkdir {}: {e}", dest_dir.display()))?;

    // Copy binary.
    std::fs::copy(&helper_src, HELPER_INSTALL_PATH)
        .map_err(|e| format!("copy to {HELPER_INSTALL_PATH}: {e}"))?;

    // Ensure it is root-owned and executable.
    unsafe {
        let path = std::ffi::CString::new(HELPER_INSTALL_PATH).unwrap();
        libc::chown(path.as_ptr(), 0, 0);
        libc::chmod(path.as_ptr(), 0o755);
    }

    // Write the LaunchDaemon plist.
    let plist = plist_content();
    std::fs::write(PLIST_PATH, &plist)
        .map_err(|e| format!("write {PLIST_PATH}: {e}"))?;

    // Bootstrap the service.
    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", "system", PLIST_PATH])
        .output()
        .map_err(|e| format!("launchctl bootstrap: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // "service already loaded" is not an error.
        if !stderr.contains("already loaded") && !stderr.contains("service exists") {
            return Err(format!("launchctl bootstrap: {stderr}"));
        }
    }

    println!("pelagos-pfctl installed and running.");
    println!("  binary:  {HELPER_INSTALL_PATH}");
    println!("  plist:   {PLIST_PATH}");
    println!("  socket:  {SOCK_PATH}");
    Ok(())
}

/// Remove the LaunchDaemon and helper binary.  Must be called as root.
pub fn cmd_uninstall() -> Result<(), String> {
    if unsafe { libc::getuid() } != 0 {
        return Err("pelagos nat66 uninstall must run as root.\nRun: sudo pelagos nat66 uninstall".into());
    }

    // Unload the LaunchDaemon (ignore errors — might not be loaded).
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", "system", LAUNCHD_LABEL])
        .output();

    // Remove plist and binary.
    for path in [PLIST_PATH, HELPER_INSTALL_PATH] {
        if Path::new(path).exists() {
            std::fs::remove_file(path)
                .map_err(|e| format!("remove {path}: {e}"))?;
        }
    }

    // Remove socket if stale.
    let _ = std::fs::remove_file(SOCK_PATH);

    println!("pelagos-pfctl uninstalled.");
    Ok(())
}

/// Print current NAT66 status.
pub fn cmd_status() {
    // Host IPv6 detection.
    match detect_global_ipv6_iface() {
        Some((iface, addr)) => println!("host IPv6:  {addr} on {iface}"),
        None => println!("host IPv6:  none (IPv4-only network — NAT66 not available)"),
    }

    // Helper daemon.
    let installed = Path::new(PLIST_PATH).exists();
    let running = helper_available();
    println!(
        "helper:     {}",
        if running {
            "running"
        } else if installed {
            "installed but not responding"
        } else {
            "not installed (run: sudo pelagos nat66 install)"
        }
    );

    // Active rule.
    match status_active() {
        Some(true) => println!("nat66:      active"),
        Some(false) => println!("nat66:      inactive"),
        None => println!("nat66:      unknown (helper not running)"),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn find_helper_binary() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {e}"))?;
    let bin_dir = exe.parent()
        .ok_or("binary has no parent directory")?;

    // Development layout: target/<triple>/release/pelagos-pfctl
    // Homebrew layout: bin/pelagos and bin/pelagos-pfctl are siblings
    let candidates = [
        bin_dir.join("pelagos-pfctl"),
        bin_dir.join("../lib/pelagos/pelagos-pfctl"),
        bin_dir.join("../share/pelagos-mac/pelagos-pfctl"),
    ];

    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or(
            "pelagos-pfctl binary not found next to pelagos.\n\
             Build it with: cargo build -p pelagos-pfctl --release"
                .to_string(),
        )
}

fn plist_content() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{HELPER_INSTALL_PATH}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/pelagos-pfctl.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/pelagos-pfctl.log</string>
</dict>
</plist>
"#
    )
}
