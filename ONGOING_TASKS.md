# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-16, branch chore/nat-diagnostics*

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
| overlayfs / linux-lts kernel | #89 | ✅ PR #90 |
| docker build multi-stage + features test | #92 | 🔶 blocked on pelagos#114 (image ENV not applied) |
| VS Code full extension integration test | #91 | 🔲 |

---

## Remaining Work

### VS Code devcontainer — current state

T2 integration harness (`scripts/test-devcontainer-e2e.sh`) is built and running.
Current result: **Suite A (7/7), B (3/3), C (1/3), D (3/3) pass.**

Suite C TC-T2-10 (`devcontainer up` with node feature) now passes with pelagos v0.52.0
and the host-clock-sync fix (VM clock injected via `clock.utc=` in kernel cmdline).

Suite C TC-T2-10b/10c (`node --version`, `npm --version`) fail with
`exec spawn failed: No such file or directory` because `pelagos exec-into` does not
apply the image's Dockerfile `ENV` layer to the exec'd process environment.
Node is installed but `PATH` does not include `/usr/local/share/nvm/current/bin`.

**Blocked on pelagos#115** — filed 2026-03-16.
No workaround in the shim (CLAUDE.md: fix belongs in pelagos).
(pelagos#114 fixed `run` PATH; #115 is the same fix needed for `exec-into`.)

Changes in this branch (chore/nat-diagnostics):
- `exec-into` `-w`/`--workdir` support (container working directory)
- `devcontainer up` probe detection fix for CLI 0.84+ built-in keepalive
- `RUST_LOG=debug` removed from VM init script (was causing output noise)
- Default VM memory increased to 2048 MiB (OOM fix for Node.js nvm install)
- Persistent disk increased to 8192 MiB
- pelagos v0.52.0 integrated
- VM clock synced from host at boot via `clock.utc=` kernel cmdline parameter
  (removes NTP from the critical startup path)

### VS Code full extension integration test (#91)

Run VS Code "Reopen in Container" against a project with a `.devcontainer/`
and verify: IDE attaches, extensions install, terminal opens inside container.

### pelagos-mac — Lower priority

- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
  Bind mounts cover most real use cases so this is low priority.
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
