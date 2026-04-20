//! Privileged helper installation via osascript.
//!
//! On first `pelagos vm start`, if `/var/run/pelagos-pfctl.sock` is absent,
//! `ensure_pfctl_blessed()` runs `install-pfctl-daemon.sh` as root.  macOS
//! shows a one-time admin credential dialog; after that the helper runs
//! permanently as a LaunchDaemon and restarts automatically on reboot.
//!
//! # Why osascript and not AuthorizationExecuteWithPrivileges?
//!
//! `AuthorizationExecuteWithPrivileges` (Security framework) is the textbook
//! API for this use case and is what we tried first.  On macOS 26 it has been
//! silently neutered for CLI tools: `AuthorizationCopyRights` succeeds and
//! returns `errAuthorizationSuccess`, but the tool is launched with the
//! calling user's UID/EUID (501/501) rather than root.
//! `security_authtrampoline` (the SUID-root relay) is not invoked at all.
//!
//! `osascript do shell script ... with administrator privileges` is the only
//! automated privilege-escalation path that still works for unsigned CLI
//! binaries on macOS 26.  It is a documented, stable macOS API — not a
//! hack — but it does require a GUI session.  If the process is running
//! headless (SSH, launchd non-GUI context), it fails with a clear error and
//! we surface manual-install instructions.
//!
//! # Why not SMJobBless?
//!
//! SMJobBless (deprecated macOS 13) requires the calling binary to be part of
//! a proper `.app` bundle.  `smd` on macOS 26 rejects CLI tool callers with
//! `CFErrorDomainLaunchd error 2` before copying the helper binary.
//!
//! # Helper binary location
//!
//! - Dev builds: `<exe_dir>/Contents/Library/LaunchServices/com.pelagos.pfctl`
//!   (created by `scripts/sign.sh`)
//! - Homebrew: `<exe_dir>/../share/pelagos-mac/com.pelagos.pfctl`

use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Socket path for the pfctl helper daemon.
const PFCTL_SOCK: &str = "/var/run/pelagos-pfctl.sock";

/// Bundle identifier / LaunchDaemon label for the privileged helper.
const HELPER_LABEL: &str = "com.pelagos.pfctl";

/// Install destination for the helper binary (PrivilegedHelperTools convention).
const HELPER_DST: &str = "/Library/PrivilegedHelperTools/com.pelagos.pfctl";

/// LaunchDaemon plist — embedded so the binary is self-contained; written to
/// /tmp at install time and passed as an argument to the install script.
const LAUNCHD_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.pelagos.pfctl</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Library/PrivilegedHelperTools/com.pelagos.pfctl</string>
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
"#;

/// Install script — embedded so the binary is self-contained regardless of
/// install location.  Written to /tmp at install time and run as root.
const INSTALL_SCRIPT: &str = include_str!("../../scripts/install-pfctl-daemon.sh");

/// Ensure the pelagos-pfctl privileged helper is installed and running.
///
/// Fast path: if the socket already exists, return `Ok(())` immediately.
///
/// Install path: uses `osascript` to run the install script as root.
/// macOS prompts for admin credentials exactly once.
pub fn ensure_pfctl_blessed() -> io::Result<()> {
    if pfctl_socket_present() {
        return Ok(());
    }
    install_helper()
}

fn pfctl_socket_present() -> bool {
    std::fs::metadata(PFCTL_SOCK)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

fn install_helper() -> io::Result<()> {
    let helper_src = find_helper_binary()?;

    // Write plist and install script to /tmp (not $TMPDIR — the per-user
    // sandbox at /var/folders — which macOS prevents from being exec'd with
    // elevated privileges).
    use std::os::unix::fs::PermissionsExt;

    let plist_tmp = PathBuf::from("/tmp/com.pelagos.pfctl.plist");
    std::fs::write(&plist_tmp, LAUNCHD_PLIST)?;

    let script_tmp = PathBuf::from("/tmp/pelagos-install-pfctl.sh");
    std::fs::write(&script_tmp, INSTALL_SCRIPT)?;
    std::fs::set_permissions(&script_tmp, std::fs::Permissions::from_mode(0o755))?;

    log::info!(
        "bless: installing privileged helper — macOS will prompt for admin credentials"
    );

    // Build the shell command.  All three paths are in /tmp or a standard
    // Homebrew/release directory; none contain single quotes or spaces, so
    // single-quoting each token is sufficient.
    let shell_cmd = format!(
        "'{script}' '{helper}' '{plist}'",
        script = script_tmp.display(),
        helper = helper_src.display(),
        plist  = plist_tmp.display(),
    );
    // osascript runs the shell command synchronously as root and returns only
    // after the script exits — no async race with temp-file cleanup.
    let applescript = format!(
        "do shell script {shell_cmd:?} with administrator privileges"
    );

    let output = std::process::Command::new("osascript")
        .args(["-e", &applescript])
        .output()
        .map_err(|e| io::Error::other(format!("osascript: {e}")))?;

    let _ = std::fs::remove_file(&plist_tmp);
    let _ = std::fs::remove_file(&script_tmp);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "privileged install failed (user may have cancelled, or no GUI session).\n\
                 osascript: {stderr}\n\
                 \n\
                 To install manually:\n\
                   sudo bash scripts/install-pfctl-daemon.sh\n\
                 Or (Homebrew):\n\
                   sudo bash \"$(brew --prefix)/share/pelagos-mac/install-pfctl-daemon.sh\"\n\
                 \n\
                 The daemon installs to {HELPER_DST} and runs as a system LaunchDaemon."
            ),
        ));
    }

    // osascript is synchronous; the install script checks for the socket
    // before exiting, so it should already be present.
    wait_for_socket(Duration::from_secs(5))?;
    log::info!("bless: com.pelagos.pfctl installed successfully");
    Ok(())
}

/// Locate the helper binary to install.
///
/// Search order:
/// 1. Dev: `<exe_dir>/Contents/Library/LaunchServices/com.pelagos.pfctl` (sign.sh)
/// 2. Homebrew: `<exe_dir>/../share/pelagos-mac/com.pelagos.pfctl`
fn find_helper_binary() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "cannot determine exe directory")
    })?;

    let dev_path = exe_dir
        .join("Contents/Library/LaunchServices")
        .join(HELPER_LABEL);
    if dev_path.exists() {
        return Ok(dev_path);
    }

    let brew_path = exe_dir.join("../share/pelagos-mac").join(HELPER_LABEL);
    if let Ok(p) = brew_path.canonicalize() {
        if p.exists() {
            return Ok(p);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "privileged helper not found.\n\
             Expected at {dev} (dev) or {brew} (Homebrew).\n\
             If using Homebrew: brew reinstall pelagos-containers/tap/pelagos-mac\n\
             If developing locally: run scripts/sign.sh",
            dev = dev_path.display(),
            brew = brew_path.display(),
        ),
    ))
}

fn wait_for_socket(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if pfctl_socket_present() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "helper installed but {PFCTL_SOCK} did not appear within {}s\n\
                     Check /var/log/pelagos-pfctl.log for errors.",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
