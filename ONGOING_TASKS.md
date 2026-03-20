# pelagos-mac — Ongoing Tasks


*Last updated: 2026-03-19 — **v0.1.0 tagged and released** (SHA 2f31fa0)*

---

## Current State

**Pilot milestone reached: v0.1.0 tagged.** VS Code "Reopen in Container" works
end-to-end on Apple Silicon. All 22 devcontainer e2e tests (Suites A–E) pass.
Flask app confirmed running inside container and accessible from macOS host at
`http://192.168.105.2:<port>` (VM directly routable via socket_vmnet).

### What works today

| Feature | Status | Merged |
|---|---|---|
| VM boot via AVF | ✅ | Phase 0 |
| vsock round-trip (ping/pong) | ✅ | Phase 0 |
| `pelagos run` (pull + exec) | ✅ | PR #18 |
| Persistent daemon (warm reuse) | ✅ | PR #27 |
| virtiofs bind mounts (`-v`) | ✅ | PR #28 |
| `pelagos exec` (piped + PTY) | ✅ | PR #38 |
| `pelagos ps / logs / stop / rm` | ✅ | PR #37 |
| `pelagos run --detach --name` | ✅ | PR #37 |
| `pelagos vm shell` | ✅ | PR #45 |
| Busybox applet symlinks in VM | ✅ | PR #47 |
| Persistent OCI image cache (`/dev/vda` ext4) | ✅ | PR #50/#107 |
| ECR Public test image (no rate limits) | ✅ | PR #50 |
| devpts mount + PTY job control | ✅ | PR #38/#40 |
| `pelagos vm console` (hvc0 serial) | ✅ | PR #51 |
| `pelagos vm ssh` (dropbear + ed25519 key) | ✅ | PR #52 |
| socket_vmnet (stable NAT, no degradation) | ✅ | PR #34 |
| `devcontainer up` (VS Code devcontainer CLI) | ✅ | PR #66 |
| `docker build` | ✅ | PR #70 |
| `docker volume create/ls/rm` | ✅ | PR #70 |
| `docker network create/ls/rm` | ✅ | PR #70 |
| `docker cp` (both directions) | ✅ | PR #71 |

---

## Phase 4 — VS Code Dev Container support (Epic #67)

| Subtask | Issue | Status |
|---|---|---|
| Docker CLI shim (`pelagos-docker`) | #56 | ✅ PR #62+#63 |
| Native port forwarding | #57 | ✅ PR #59 |
| glibc/Ubuntu compat | #58 | ✅ PR #61 |
| docker exec, version, info, inspect | #64 | ✅ PR #65 |
| devcontainer up smoke test | #66 | ✅ PR #66 |
| docker build (native via pelagos) | #68 | ✅ PR #70 |
| docker cp | #69 | ✅ PR #71 |
| overlayfs / linux-lts kernel | #89 | ✅ PR #90 |

| docker build multi-stage + features test | #92 | ✅ PR #94+#100 |
| VS Code full extension integration test | #91 | ✅ verified 2026-03-19 |

---

## Remaining Work


### VS Code devcontainer — current state

T2 integration harness (`scripts/test-devcontainer-e2e.sh`) is built and running.
Current result: **Suite A/B/C/D/E: 22/22 PASS** (pelagos v0.59.0, ext4 root fs).
VS Code "Reopen in Container" verified — container opens, terminal works, Flask app
runs and is accessible from macOS host at `http://192.168.105.2:<port>`.

### VS Code full extension integration test (#91)

Run VS Code "Reopen in Container" against a project with a `.devcontainer/`
and verify: IDE attaches, extensions install, terminal opens inside container.

**All known blockers are resolved:**

1. **pelagos#120** — container `/etc/hosts` not created. **CLOSED in pelagos v0.57.0.**

2. **exec-into stdin BufReader fix** (pelagos-mac#103) — CLOSED. Applied in
   `pelagos-mac/src/main.rs`: replaced `io::stdin().read()` with `libc::read(STDIN_FILENO,...)`.

3. **exec-into `/proc/self` dangling symlink** — **FIXED (this branch).**
   Root cause: pelagos does NOT create a separate PID namespace for containers.
   The exec'd process gets a VM-level PID (e.g., 879) that doesn't exist in the
   container's filtered `/proc` view, making `/proc/self` a dangling symlink.
   VS Code's node server uses `/proc/self/exe` and `/proc/self/fd/` heavily;
   the dangling symlink caused `resolveAuthority` to fail.

   **Fix (pelagos-guest/src/main.rs `handle_exec_into`):**
   - Intermediate child calls `unshare(CLONE_NEWNS)` after joining the container's
     mnt namespace and chrooting. This creates a private copy of the mnt namespace
     so subsequent mounts don't affect the container's original `/proc`.
   - Grandchild remounts `/proc` fresh (`mount("proc", "/proc", "proc", ...)`)
     before exec. The fresh procfs reflects the current PID namespace (VM root),
     so `/proc/self` → `/proc/<grandchild-vm-pid>` is valid.
   - The `setns_pid` + double-fork is kept for future pelagos PID namespace support;
     if pelagos adds `CLONE_NEWPID` the grandchild will get a container-local PID
     and the fresh `/proc` will show that PID correctly.

   **Verified:** `/proc/self/ns/pid` resolves, `/proc/$$/exe` → `/usr/bin/dash`, exit 0.

5. **pelagos#124 — `pelagos run` must persist container PID before relaying stdout.**
   **FIXED in pelagos v0.59.0 (PR #125).** Integrated in pelagos-mac (PELAGOS_VERSION bumped
   to 0.59.0). No workaround needed; `wait_for_container_ns()` was never committed to master
   (removed before PR #107 merge). All 22 devcontainer e2e tests pass with v0.59.0.

4. **"Dev container not found" after docker run exits** — **FIXED in PR #106.**
   Root cause: pelagos removes exited containers from in-memory state immediately.
   VS Code calls `docker inspect <container>` after `docker run` exits and expects
   `State.Status="exited"`.

   **Fix (pelagos-docker/src/main.rs):**
   - `cmd_run` writes container metadata to `~/.local/share/pelagos/shim-containers.json`.
   - `cmd_inspect_container` falls back to the cache when pelagos ps --all doesn't
     list the container, returning a synthetic exited-state response.
   - `cmd_rm` removes the cache entry.

   **Verified:** `docker run exits` → `docker inspect` returns exit 0, `State.Status="exited"`.

### Epic #119 — pelagos builder VM (in progress, PR #125)

Goal: boot a standalone Ubuntu 22.04 aarch64 VM as a named profile (`--profile build`)
that can build and test pelagos natively, without a separate Linux machine.

**Completed (PR #125, branch `feat/build-vm-profile`):**
- Per-profile `vm.conf` in Rust (`state.rs`, `main.rs`): named profiles load
  `~/.local/share/pelagos/profiles/<name>/vm.conf` for disk/kernel/initrd/memory/cpus.
  Precedence: CLI flag > vm.conf > compiled default.
- `loop.ko` staged in initramfs (needed for losetup during provisioning).
- Init script external-rootfs label check: if `/dev/vda` label ≠ `pelagos-root`,
  init skips Alpine copy and pivots directly to Ubuntu's `/sbin/init`.
- `scripts/build-build-image.sh`: provisions `out/build.img` inside the running
  Alpine VM via SSH + chroot, then writes `vm.conf` for the named profile.
- `vm-ping.sh` / `vm-restart.sh` updated for profile-aware path resolution.

**Remaining steps to reach a working builder VM:**

1. **Merge PR #125** and pull to master.

2. **Rebuild the initramfs** (one-time, to bake in `loop.ko`):
   ```bash
   bash scripts/build-vm-image.sh
   ```

3. **Boot the Alpine VM** with the new image:
   ```bash
   bash scripts/vm-ping.sh
   ```

4. **Provision the Ubuntu build image** (takes several minutes):
   ```bash
   bash scripts/build-build-image.sh
   ```
   Creates `out/build.img` (20 GB sparse ext4), installs Ubuntu 22.04 base +
   build-essential + Rust stable via chroot inside the Alpine VM, writes
   `~/.local/share/pelagos/profiles/build/vm.conf`.

   > **Known risk — virtiofs loop I/O:** the image file lives on the macOS
   > virtiofs share; all `apt-get` and `tar` I/O goes virtiofs → macOS APFS →
   > virtiofs → loop → ext4. If this causes errors or hangs, see the alternative
   > design below.

5. **Boot the Ubuntu build VM:**
   ```bash
   bash scripts/vm-restart.sh --profile build
   ```

6. **Verify the build environment:**
   ```bash
   pelagos --profile build vm ssh -- rustc --version
   pelagos --profile build vm ssh -- git clone https://github.com/skeptomai/pelagos /root/pelagos
   pelagos --profile build vm ssh -- bash -c 'cd /root/pelagos && cargo build --release'
   pelagos --profile build vm ssh -- bash -c 'cd /root/pelagos && cargo test'
   ```

**virtiofs loop device — design risk and alternatives:**

The current provisioning path:
```
mke2fs → build.img (on macOS APFS, via virtiofs) → losetup /dev/loop0 →
mount ext4 → chroot → apt-get/tar write into ext4 → virtiofs → macOS
```
Every byte written during provisioning crosses virtiofs twice (once to reach
the file, once to flush through the FUSE daemon). This is a one-time cost but
could be slow or trigger FUSE/virtiofs bugs under heavy write load (millions
of small files from apt and cargo).

**Alternative A (preferred if virtiofs provisioning proves unreliable):**
Pass `build.img` directly to the Alpine VM as a second virtio-blk device.
Requires adding a `--extra-disk` flag to `daemon.rs` / `VmConfig`. The image
gets a real block device (`/dev/vdb`) — no loop, no virtiofs in the I/O path.
`build-build-image.sh` would restart the Alpine VM with `--extra-disk build.img`,
provision onto `/dev/vdb`, then restart normally.

**Alternative B (simplest, no new kernel path):**
Do the provisioning entirely on macOS using `docker run --platform linux/arm64`
with a Linux container that has the right tools. Writes ext4 into build.img from
the container. Zero virtiofs involved. Requires Docker Desktop on macOS (violates
the no-external-subsystem rule — reject this option).

**Alternative C (deferred):**
Add a dedicated NVMe or virtio-blk device attachment to pelagos-vz for secondary
disks. More general than Alternative A; the right long-term answer but Significant
Work.

**Current plan:** attempt the virtiofs provisioning path first. If it fails or is
too slow, implement Alternative A (extra virtio-blk disk for the Alpine provisioning
session only).

### Next priorities (post-v0.1.0)

- **Port forwarding** — container port → VM port → macOS `localhost`. Needed for
  VS Code Ports panel and `localhost` access. Currently workaround: direct VM IP
  (`192.168.105.2:<port>`) is routable from macOS host via socket_vmnet.
- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
- **Dynamic virtiofs shares** (#74) — current per-path shares require knowing all
  paths at VM start time; proper dynamic sharing needed for general-purpose use.
- **Signed installer** — `.pkg` for distribution. Requires Developer ID + notarization
  + `com.apple.security.virtualization` entitlement. Not yet scoped.


---

## Key Architecture Notes

- **exec-into PID namespace:** `setns(CLONE_NEWPID)` in `pre_exec` (child after fork)
  only sets `pid_for_children`; a second fork is required. Use the nsenter hybrid:
  `pre_exec` joins non-PID namespaces + chroots, then wrap with
  `nsenter --target 1 --pid -- <prog>`. See `docs/GUEST_CONTAINER_EXEC.md`.
- **socket_vmnet degradation:** if image pulls fail with "I/O error (os error 5)",
  run `sudo brew services restart socket_vmnet`, kill stale VM processes, then
  remove and recreate `~/.local/share/pelagos/vm.pid` before restarting.
  The old root.img may also become invalid (AVF: "storage device attachment invalid")
  if the VM was killed mid-write — delete `out/root.img` and rerun `build-vm-image.sh`.
- **VM networking:** socket_vmnet, subnet `192.168.105.x`, gateway `192.168.105.1`.
  Homebrew socket path: `/opt/homebrew/var/run/socket_vmnet` (no `.shared` suffix).
- **`pelagos build` uses `--network pasta`** inside the VM. `pasta` (userspace
  TCP/UDP proxy) is staged into the initramfs. Bridge/veth kernel modules are not
  required. RUN steps that need network access work via pasta.
- **`pelagos network create` requires `--subnet <CIDR>`** explicitly; the shim
  auto-generates `10.88.<hash>.0/24` from the network name.
- **Network names max 12 chars** — bridge device name is `rm-<name>`, IFNAMSIZ=15.

---

## Build Reference

| Step | Command |
|---|---|
| Host binary | `cargo build -p pelagos-mac --release` |
| Guest (cross) | `RUSTFLAGS="-C link-self-contained=no" RUSTC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc" cargo build -p pelagos-guest --target aarch64-unknown-linux-musl --release` |
| VM image | `bash scripts/build-vm-image.sh` |
| Code-sign | `codesign --sign - --entitlements pelagos-mac/entitlements.plist --force target/aarch64-apple-darwin/release/pelagos` |
| All tests | `bash scripts/test-e2e.sh` |
| Cold-start test | `bash scripts/test-e2e.sh --cold` |
| Interactive container | `bash scripts/test-interactive.sh` |
| VM shell | `bash scripts/vm-shell.sh` |
