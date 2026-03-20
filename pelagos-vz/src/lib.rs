//! pelagos-vz — ergonomic Rust wrapper over Apple's Virtualization.framework.
//!
//! Provides VM lifecycle management (boot, stop, status) and device configuration
//! (vsock, virtiofs, NAT networking, Rosetta) for the pelagos-mac host binary.
//!
//! Built on `objc2-virtualization` (auto-generated bindings, updated weekly from
//! Xcode SDK headers). This crate adds an ergonomic API layer; the raw ObjC
//! bindings are never exposed publicly.
//!
//! # Pilot scope
//!
//! Phase 1 validates:
//! - Boot a Linux VM from kernel + initrd + disk image
//! - Expose vsock as a Unix domain socket on the host
//! - virtiofs directory sharing
//! - NAT networking + Rosetta
//!
//! See docs/DESIGN.md for full architecture and rationale.

#[cfg(not(target_os = "macos"))]
compile_error!("pelagos-vz is macOS only");

pub mod nat_relay;
pub mod socket_vmnet;
pub mod vm;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("VM configuration error: {0}")]
    Config(String),
    #[error("VM runtime error: {0}")]
    Runtime(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
