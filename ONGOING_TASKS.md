# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-13, SHA (chore/nat-diagnostics branch)*

---

## Current State

**Phase 4 (VS Code devcontainer support) COMPLETE.** All 36 e2e tests pass
(`bash scripts/test-e2e.sh`). The Docker CLI shim covers the full devcontainer
lifecycle including `docker build`, `docker cp`, volumes, and networks.

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
| Persistent OCI image cache (`/dev/vda` ext2) | ✅ | PR #50 |
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

## Phase 4 — VS Code Dev Container support (Epic #67) ✅ COMPLETE

All sub-issues resolved:

| Subtask | Issue | Status |
|---|---|---|
| Docker CLI shim (`pelagos-docker`) | #56 | ✅ PR #62+#63 |
| Native port forwarding | #57 | ✅ PR #59 |
| glibc/Ubuntu compat | #58 | ✅ PR #61 |
| docker exec, version, info, inspect | #64 | ✅ PR #65 |
| devcontainer up smoke test | #66 | ✅ PR #66 |
| docker build, volume, network | #68 | ✅ PR #70 |
| docker cp | #69 | ✅ PR #71 |

**DNS workaround note:** pelagos-guest bind-mounts `/etc/resolv.conf` into every
container as a workaround for the runtime not doing it automatically. The proper
fix is tracked upstream: https://github.com/skeptomai/pelagos/issues/87

---

## Remaining Work

### pelagos runtime — DNS (pelagos/issues#87) — Next up

**Context:** pelagos does not automatically bind-mount `/etc/resolv.conf` into
containers. The `auto_dns` block in `src/container.rs` (~line 2781) only runs for
bridge and pasta network modes; `--network none` (the VM default) gets nothing.

**Code location:** `pelagos/src/container.rs`
- `auto_dns` population: ~line 2781
- DNS temp-file write + bind-mount: ~lines 2823–2843, 3416–3434
- `host_upstream_dns()` helper: ~line 386

**Fix strategy:** After the existing `auto_dns` block, add a fallback: when
`auto_dns` is empty and a mount namespace + chroot are available, bind-mount
`/etc/resolv.conf` directly from the host (no temp file needed — it's already the
right format). A `host_resolv_bind: Option<CString>` field parallel to
`dns_temp_file_cstring` is the cleanest approach.

**Cleanup after fix:** Remove the explicit `--mount type=bind,source=/etc/resolv.conf`
from `run_container()` in `pelagos-guest/src/main.rs` and close pelagos-mac issue #60.

### pelagos-mac — Lower priority

- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
  Bind mounts cover most real use cases so this is low priority.
- **VS Code extension end-to-end test** — BLOCKED on pelagos runtime fix.
  The shim layer (docker version/ps/run/inspect/label filtering) all work.
  `docker exec` into a running container lands in the wrong filesystem because
  pelagos uses `chroot` instead of `pivot_root` for container rootfs isolation.
  After `setns(mnt_fd)`, `/` is still the guest Alpine root — the container's
  actual rootfs is only reachable via the `chroot` pelagos called at startup.
  A workaround (`fchdir(/proc/<pid>/root)` + `chroot(".")` after `setns`) exists
  in branch `fix/exec-into-chroot` (PR #85, closed), but the correct fix is
  upstream. **Resume after pelagos#96 lands** (switch `with_chroot` → `with_pivot_root`
  in `src/cli/run.rs`). Once pelagos uses `pivot_root`, `setns(mnt)` gives the
  correct container rootfs directly and PR #85 can be reopened/superseded.
- **Signed installer** — `.pkg` for distribution. Requires Developer ID + notarization
  + `com.apple.security.virtualization` entitlement. Not yet scoped.

---

## Key Architecture Notes

- **`pelagos exec` subprocess cannot enter container namespaces** from inside the
  guest daemon — it silently runs in the guest root instead. Always use direct
  `setns(2)` via `pre_exec`. See `docs/GUEST_CONTAINER_EXEC.md`.
- **VM networking:** socket_vmnet, subnet `192.168.105.x`, gateway `192.168.105.1`.
  Homebrew socket path: `/opt/homebrew/var/run/socket_vmnet` (no `.shared` suffix).
- **`pelagos build` requires `--network none`** in the VM (no bridge/veth kernel
  modules). Build steps that need network access require a kernel extension.
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
