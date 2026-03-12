//! Subprocess wrapper: build pelagos argv and run it.

use std::ffi::OsString;
use std::process::{Command, Output, Stdio};

use crate::config::Config;

/// Run `pelagos <prefix_args> <sub_args>`, capture stdout+stderr, return output.
pub fn run_pelagos(cfg: &Config, sub_args: &[OsString]) -> std::io::Result<Output> {
    let mut cmd = Command::new(&cfg.pelagos_bin);
    cmd.args(cfg.pelagos_prefix_args());
    cmd.args(sub_args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.output()
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
