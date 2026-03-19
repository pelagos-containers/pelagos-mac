# pelagos-mac — Ongoing Tasks


*Last updated: 2026-03-19 (branch feat/native-pid-namespace-exec, PR #107; pelagos v0.59.0)*

---

## Current State

**Phase 4 (VS Code devcontainer support) complete.** The Docker CLI shim covers
the full devcontainer lifecycle. The exec-into PID namespace blocker
(pelagos#121) is fixed in pelagos-guest using a hybrid nsenter approach.
The "Dev container not found" blocker (shim inspect after exit) is fixed via
a local container state cache in pelagos-docker.
All 22 devcontainer e2e tests (Suites A–E) pass. VS Code "Reopen in Container"
is ready for manual verification (PR #106).

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
| Persistent OCI image cache (`/dev/vda` ext4) | ✅ | PR #50 |
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
| VS Code full extension integration test | #91 | 🔲 |

---

## Remaining Work


### VS Code devcontainer — current state

T2 integration harness (`scripts/test-devcontainer-e2e.sh`) is built and running.
Current result: **Suite A/B/C/D/E: 22/22 PASS** (with pelagos v0.58.0 + nsenter fix).

### VS Code full extension integration test (#91)

Run VS Code "Reopen in Container" against a project with a `.devcontainer/`
and verify: IDE attaches, extensions install, terminal opens inside container.

**All known blockers are resolved:**

1. **pelagos#120** — container `/etc/hosts` not created. **CLOSED in pelagos v0.57.0.**

2. **exec-into stdin BufReader fix** (pelagos-mac#103) — CLOSED. Applied in
   `pelagos-mac/src/main.rs`: replaced `io::stdin().read()` with `libc::read(STDIN_FILENO,...)`.

3. **pelagos#121 — exec-into PID namespace join.** **FIXED in PR #106.**
   Root cause: `setns(CLONE_NEWPID)` in `pre_exec` (after fork) only sets
   `pid_for_children`; a second fork is required for the process to acquire a
   namespace-local PID. Without it, `/proc/self` is a dangling symlink, causing
   VS Code `resolveAuthority` to fail.

   **Fix (pelagos-guest/src/main.rs `handle_exec_into`):**
   - `pre_exec` joins net/uts/ipc/mnt namespaces and fchdir+chroots into container rootfs.
   - The command is wrapped: `nsenter --target 1 --pid -- <prog> <args>`. After chroot,
     `/proc` is the container's procfs; nsenter performs the correct double-fork from
     a single-threaded context, giving the exec'd process a container-local PID.
   - `nsenter` (util-linux) is staged into the initramfs from Alpine's
     `util-linux-misc-2.40.4-r1.apk`.

   **Verified:** `mypid=2`, `readlink /proc/self/ns/mnt` → `mnt:[4026532138]`, exit 0.

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

### pelagos-mac — Lower priority

- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
  Bind mounts cover most real use cases so this is low priority.
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
