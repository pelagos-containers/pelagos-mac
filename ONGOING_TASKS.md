# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-09*

---

## Current State

Tasks 0.1–0.6 implemented. All host and guest crates compile clean (zero warnings,
clippy -D warnings). Release binaries build for both targets.

**Key implementation decisions made:**
- vsock access is in-process via `VZVirtioSocketDevice::connectToPort` (AVF API),
  not a filesystem Unix socket. `VmConfig.vsock_socket` field removed.
- Cross-compilation requires rustup's cargo (not Homebrew's) — documented in
  `.cargo/config.toml` and in `scripts/build-vm-image.sh`.

**Not yet run on macOS hardware.** The pilot phase has not been executed end-to-end.

---

## Phase 0 — Pilot: Validate the Architecture

**Goal:** prove that a pure-Rust macOS binary can boot a Linux VM via
`objc2-virtualization` and round-trip a vsock command to a Rust guest daemon.

**Success criteria:**
- `pelagos --kernel out/vmlinuz --initrd out/initramfs.gz --disk out/root.img run alpine /bin/echo hello` prints "hello" on the macOS terminal
- No Go binary involved at any layer
- virtiofsd file sharing: a host directory is visible inside the VM

### Task 0.1 — ✅ Verify objc2-virtualization crate versions

Versions pinned: `objc2 0.6`, `objc2-foundation 0.3`, `objc2-virtualization 0.3`,
`block2 0.6`, `dispatch2 0.3`.

All required AVF types confirmed present in objc2-virtualization 0.3.2:
`VZVirtualMachine`, `VZLinuxBootLoader`, `VZVirtioSocketDevice`,
`VZVirtioFileSystemDeviceConfiguration`, `VZNATNetworkDeviceAttachment`, etc.

### Task 0.2 — ✅ Implement pelagos-vz: boot a minimal Linux VM

`pelagos-vz/src/vm.rs` fully implemented:
- `VmConfig` / `VmConfigBuilder` — ergonomic configuration API
- `Vm::start()` — creates all AVF devices, validates configuration,
  instantiates `VZVirtualMachine` with a private serial dispatch queue,
  calls `startWithCompletionHandler`, blocks on a condvar until callback fires
- `Vm::connect_vsock()` — in-process vsock connect via `VZVirtioSocketDevice`,
  returns an `OwnedFd` ready for JSON I/O
- `Vm::stop()` — clean shutdown via `stopWithCompletionHandler`

Key pattern: AVF async callbacks bridged to sync Rust via `Arc<Mutex<Option<Result>>>` +
`Arc<Condvar>`, dispatched through the VM's serial `DispatchQueue`.

### Task 0.3 — ✅ Implement pelagos-guest: vsock listener

`pelagos-guest/src/main.rs` fully implemented:
- AF_VSOCK listener using `libc` directly (Linux only, with macOS stubs for cargo check)
- JSON command dispatch for `GuestCommand::Run` and `GuestCommand::Ping`
- `Run`: spawns `pelagos run <image>`, streams stdout/stderr via mpsc channel,
  writes `{"exit": N}` as terminal message
- `Ping`: responds `{"pong": true}`
- Cross-compiled to `aarch64-unknown-linux-gnu` via `cargo-zigbuild`

### Task 0.4 — ✅ Implement vsock client in pelagos-mac

Implemented in `pelagos-mac/src/main.rs` as `run_command()` and `ping_command()`:
- Calls `vm.connect_vsock()` to get an OwnedFd
- Serializes `GuestCommand` as newline-delimited JSON
- Reads `GuestResponse` stream, relays stdout/stderr to terminal
- Returns the container's exit code

### Task 0.5 — ✅ Wire up the CLI

`pelagos-mac/src/main.rs` — clap 4 derive CLI:
- `pelagos --kernel K --initrd I --disk D [--memory M] [--cpus N] run <image> [args...]`
- `pelagos ... ping`
- Boots VM on startup; no PID-file persistence yet (each invocation boots a fresh VM)

**PID-file / persistent VM deferred to Phase 1.**

### Task 0.6 — ✅ Build VM image script

`scripts/build-vm-image.sh`:
- Downloads Alpine Linux ARM64 virt ISO
- Extracts kernel + initrd via hdiutil (macOS) or 7z
- Creates 2 GiB raw disk image
- Documents manual Alpine setup steps (QEMU-based automated install also scaffolded)
- Copies pelagos-guest binary and installs OpenRC service

**Remaining manual step:** run `scripts/build-vm-image.sh` to produce `out/{vmlinuz,initramfs.gz,root.img}`.
Requires: `brew install qemu` and a completed Alpine install into the disk image.

---

## Next Steps Before Pilot Validation

1. **Run `scripts/build-vm-image.sh`** — produces the VM disk image artifacts
2. **Code-sign pelagos binary** with `com.apple.security.virtualization` entitlement
3. **Test end-to-end:**
   ```bash
   pelagos --kernel out/vmlinuz --initrd out/initramfs.gz --disk out/root.img ping
   pelagos --kernel out/vmlinuz --initrd out/initramfs.gz --disk out/root.img run alpine /bin/echo hello
   ```
4. **Fix runtime issues** (expected: boot parameters, vsock timing, device naming)

---

## Phase 1 — Post-Pilot

After Phase 0 is validated:

- PID file / persistent VM (don't reboot on every invocation)
- virtiofs bind mounts in `pelagos run -v host:container`
- Rosetta for x86_64 images
- VM lifecycle management (persistent VM, auto-boot, clean shutdown)
- `pelagos build`, `pelagos image pull` forwarded to guest
- `pelagos exec`, `pelagos ps`, `pelagos logs`
- Code signing + entitlement tooling
- Signed `.pkg` installer

---

## Notes and Risks

- `objc2-virtualization` is auto-generated from Xcode SDK headers — complete but not
  ergonomic. The `pelagos-vz` wrapper handles the boilerplate.
- The `com.apple.security.virtualization` entitlement is required. Ad-hoc signing
  works for development; check the Xcode entitlement plist format early.
- virtiofsd (host side) must be running before the VM tries to mount. Coordinate
  startup order carefully. (virtiofsd not yet wired in — Phase 1 item.)
- vsock connect: `VZVirtioSocketDevice::connectToPort_completionHandler` connects
  host→guest. The guest must already be listening when the host connects — add a
  retry/backoff loop in `connect_vsock()` after pilot validation.
- macOS 13.5+ required for full feature set (virtiofs, Rosetta, EFI boot).
- Cross-compilation note: use rustup's cargo (not Homebrew's) for the Linux guest.
  See `.cargo/config.toml` for the exact command.
