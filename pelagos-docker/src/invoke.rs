//! Subprocess wrapper: build pelagos argv and run it.

use std::ffi::OsString;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::config::Config;

/// Run `pelagos <prefix_args> <sub_args>`, capture stdout+stderr, return output.
pub fn run_pelagos(cfg: &Config, sub_args: &[OsString]) -> std::io::Result<Output> {
    let mut cmd = Command::new(&cfg.pelagos_bin);
    cmd.args(cfg.pelagos_prefix_args());
    cmd.args(sub_args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.output()
}

/// Run `pelagos <sub_args>`, but kill the process and return None if it does
/// not complete within `timeout`.  Used for inspect calls on containers that
/// may be in a broken state — `pelagos inspect` must never block `docker ps`.
pub fn run_pelagos_timeout(
    cfg: &Config,
    sub_args: &[OsString],
    timeout: Duration,
) -> Option<Output> {
    let mut cmd = Command::new(&cfg.pelagos_bin);
    cmd.args(cfg.pelagos_prefix_args());
    cmd.args(sub_args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    // Reap thread: wait for the child and send the result.
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Some(output),
        _ => None, // timed out or error — caller treats as empty/not-found
    }
}

/// Run `pelagos <prefix_args> <sub_args>`, inheriting all stdio (for interactive use).
pub fn run_pelagos_inherited(
    cfg: &Config,
    sub_args: &[OsString],
) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = Command::new(&cfg.pelagos_bin);
    cmd.args(cfg.pelagos_prefix_args());
    cmd.args(sub_args);
    cmd.status()
}

/// Convenience: build an OsString vec from string slices.
pub fn args(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}
