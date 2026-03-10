# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-10, commit pending*

---

## Current State

**Phase 0 pilot COMPLETE.** `pelagos ping` returns `pong` end-to-end on real hardware.

The full stack has been exercised:
- macOS host boots a Linux/Alpine ARM64 VM via `objc2-virtualization`
- vsock round-trip works: host sends `{"cmd":"ping"}`, guest replies `{"pong":true}`
- No Go binary at any layer

---

## Phase 0 — Pilot: Validate the Architecture ✅

### Task 0.1 — ✅ Verify objc2-virtualization crate versions

Versions pinned: `objc2 0.6`, `objc2-foundation 0.3`, `objc2-virtualization 0.3`,
`block2 0.6`, `dispatch2 0.3`.

### Task 0.2 — ✅ Implement pelagos-vz: boot a minimal Linux VM

`pelagos-vz/src/vm.rs` — `VmConfig` / `VmConfigBuilder`, `Vm::start()`,
`Vm::connect_vsock()`, `Vm::stop()`.

Key pattern: AVF async callbacks bridged to sync Rust via
`Arc<Mutex<Option<Result>>>` + `Arc<Condvar>`, dispatched through the VM's serial
`DispatchQueue`.

**Critical bug found and fixed during pilot (see below).**

### Task 0.3 — ✅ Implement pelagos-guest: vsock listener

`pelagos-guest/src/main.rs`:
- AF_VSOCK listener via `libc` directly (Linux only; macOS stubs for `cargo check`)
- JSON command dispatch: `GuestCommand::Ping` → `{"pong":true}`, `GuestCommand::Run`
  → spawns `pelagos run`, streams stdout/stderr, returns exit code
- `FdReader` / `FdWriter` structs for direct `libc::read`/`libc::write` — avoids
  `OwnedFd::from_raw_fd` on connection sockets (irrelevant to the actual bug but
  cleaner and safer with Rust 1.84+ assertions)
- `ConnFd` RAII wrapper closes the accepted connection fd on all exit paths

### Task 0.4 — ✅ Implement vsock client in pelagos-mac

`pelagos-mac/src/main.rs` — `run_command()` and `ping_command()`.

### Task 0.5 — ✅ Wire up the CLI

clap 4 derive CLI: `pelagos --kernel K --initrd I --disk D ping|run`.

### Task 0.6 — ✅ Build VM image script

`scripts/build-vm-image.sh` (no QEMU, no ext4, no interactive install):
- Downloads Alpine 3.21 aarch64 virt ISO
- Extracts `vmlinuz-virt` + `initramfs-virt` via `bsdtar`
- Decompresses zboot kernel to raw arm64 Image (macOS 26 / VZLinuxBootLoader
  requires an uncompressed arm64 Image, not gzip or zboot format)
- Cross-compiles `pelagos-guest` for `aarch64-unknown-linux-musl`
- Extracts vsock kernel modules from the ISO's modloop squashfs
- Repacks initramfs with guest binary + vsock modules + custom `/init`
- Creates 64 MiB placeholder disk image (AVF requires at least one block device)

### Task 0.7 — ✅ Code-sign and run end-to-end ping

```bash
codesign --sign - --entitlements pelagos-mac/entitlements.plist --force \
    target/aarch64-apple-darwin/release/pelagos

RUST_LOG=info ./target/aarch64-apple-darwin/release/pelagos \
    --kernel out/vmlinuz --initrd out/initramfs-custom.gz --disk out/root.img \
    --cmdline 'console=hvc0' ping
# → pong
```

---

## Bugs Found and Fixed During Pilot

### Bug 1: Cross-compilation toolchain (cargo-zigbuild broken)

**Symptom:** `can't find crate for core/std` when building for
`aarch64-unknown-linux-musl`.

**Root cause:** Homebrew's `cargo`/`rustc` is on PATH and lacks the musl sysroot.
`cargo-zigbuild` was also broken (its managed zig cache had been deleted and
couldn't re-download).

**Fix:**
- Added `scripts/zig-aarch64-linux-musl.sh` — a direct `zig cc -target
  aarch64-linux-musl -nostartfiles` wrapper used as the musl linker
- Build command now uses explicit `RUSTC` path and `-C link-self-contained=no`:
  ```bash
  RUSTFLAGS="-C link-self-contained=no" \
  RUSTC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc" \
  cargo build -p pelagos-guest --target aarch64-unknown-linux-musl --release
  ```
- Documented in `.cargo/config.toml` and `ONGOING_TASKS.md`

### Bug 2: macOS 26 kernel format rejection

**Symptom:** VM fails to start; `VZLinuxBootLoader` rejects the kernel.

**Root cause:** Alpine 3.21's `vmlinuz-virt` is a zboot-format binary (EFI/PE stub
wrapping a gzip-compressed arm64 Image). macOS 26's `VZLinuxBootLoader` requires a
raw arm64 Image.

**Fix:** Python 3 decompression step in `build-vm-image.sh` that detects the zboot
magic bytes, extracts the gzip payload, and decompresses to a raw arm64 Image.

### Bug 3: `VZVirtioSocketConnection` fd lifetime (the pilot-blocking bug)

**Symptom:** `thread 'main' panicked at raw.rs:183: fd != -1` on the host immediately
after vsock connect. Exit code 101.

**Root cause:** `VZVirtioSocketConnection` is an Objective-C ARC object.
`fileDescriptor()` returns the raw fd, but the connection object **closes the fd when
it is deallocated**. ARC releases the connection as soon as the completion handler
block returns — before the Rust caller has a chance to use the fd.

The sequence of events:
1. AVF calls the completion handler with a valid `VZVirtioSocketConnection`
2. Handler reads `conn.fileDescriptor()` → e.g. fd 8
3. Handler stores fd 8 in the shared `Result`, signals the condvar, and **returns**
4. ARC releases the connection → its dealloc **closes fd 8**
5. Rust thread wakes, calls `libc::dup(8)` in `ping_command` → EBADF → returns -1
6. `std::fs::File::from_raw_fd(-1)` → `OwnedFd::from_raw_fd(-1)` → panic (Rust 1.84+
   assertion)

**Fix** (`pelagos-vz/src/vm.rs`): call `libc::dup(fd)` **inside** the completion
handler block, before returning, so we own a copy of the fd that the connection's
dealloc cannot close:

```rust
let fd = unsafe { (&*conn).fileDescriptor() };
if fd < 0 {
    Err(format!("invalid fileDescriptor: {}", fd))
} else {
    // dup() here — AVF closes conn's fd when the ObjC object is deallocated
    // (ARC), which happens as soon as this block returns.
    let owned = unsafe { libc::dup(fd) };
    if owned < 0 {
        Err(format!("dup failed: {}", std::io::Error::last_os_error()))
    } else {
        Ok(owned)
    }
}
```

---

## Coding Conventions Established

- **No `eprintln!`/`println!` for diagnostics** — use `log::error!`, `log::warn!`,
  `log::info!`, `log::debug!`, `log::trace!`. `println!` only for deliberate CLI
  output (e.g. `pong`).
- All crates use `env_logger` for initialization.
- Set `RUST_LOG=info` to see lifecycle messages; `RUST_LOG=debug` for connection-level
  detail.

---

## Build Reference

| Step | Command |
|---|---|
| Host binary | `cargo build -p pelagos-mac --release` |
| Guest (cross) | `RUSTFLAGS="-C link-self-contained=no" RUSTC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc" cargo build -p pelagos-guest --target aarch64-unknown-linux-musl --release` |
| VM image | `bash scripts/build-vm-image.sh` |
| Code-sign | `codesign --sign - --entitlements pelagos-mac/entitlements.plist --force target/aarch64-apple-darwin/release/pelagos` |
| Ping test | `RUST_LOG=info ./target/aarch64-apple-darwin/release/pelagos --kernel out/vmlinuz --initrd out/initramfs-custom.gz --disk out/root.img --cmdline 'console=hvc0' ping` |

---

## Phase 1 — Post-Pilot

- **Close GitHub issues 2–7** (pending: confirm issues close cleanly against pilot commit)
- PID file / persistent VM (don't reboot on every invocation)
- virtiofs bind mounts in `pelagos run -v host:container`
- Rosetta for x86_64 images
- `pelagos build`, `pelagos image pull` forwarded to guest
- `pelagos exec`, `pelagos ps`, `pelagos logs`
- Signed `.pkg` installer

---

## Notes and Risks

- `objc2-virtualization` is auto-generated from Xcode SDK headers — complete but
  not ergonomic. `pelagos-vz` provides the ergonomic wrapper.
- `com.apple.security.virtualization` entitlement required. Ad-hoc signing works
  for development.
- vsock connect: `VZVirtioSocketDevice::connectToPort_completionHandler` connects
  host→guest. The guest must be listening before the host connects — `connect_vsock()`
  includes a 30-attempt retry loop with 1-second backoff.
- virtiofsd (host side) not yet wired in — Phase 1 item.
- macOS 13.5+ required for full feature set.
- The `com.apple.security.virtualization` entitlement is required at runtime; the
  binary must be signed before execution.
