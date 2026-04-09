# pelagos-mac

macOS CLI for the [pelagos](https://github.com/pelagos-containers/pelagos) Linux container
runtime. Runs pelagos containers on Apple Silicon by managing a lightweight Linux VM
via Apple's Virtualization Framework.

## Status

**v0.4.0 — functional.** VS Code devcontainer support works end-to-end. 27/27
devcontainer e2e tests pass (suites A–F). Build VM boots clean in ~16s via Ubuntu
6.8 HWE kernel; `pelagos vm console` replays full boot log.

## Architecture

The stack is kept deliberately minimal — library dependencies only, no subsystem
dependencies. Every component is owned or directly wrapped:

```
pelagos-mac (macOS CLI)
  │
  ├── pelagos-vz        Boots a Linux VM via Apple Virtualization Framework
  │     ├── objc2-virtualization (Rust bindings, auto-generated from Xcode SDK)
  │     └── nat_relay.rs (smoltcp userspace NAT relay)
  │
  └── vsock             Forwards commands to the guest over AVF vsock
        │
        └── pelagos-guest (inside the VM, aarch64 Alpine Linux)
              └── pelagos binary
```

Pure Rust throughout. No Go, no Lima, no gRPC daemon, no privileged helpers, no
Homebrew networking prerequisites. See [docs/DESIGN.md](docs/DESIGN.md) for the
full rationale.

## Requirements

- macOS 13.5+ (Ventura), Apple Silicon
- Xcode Command Line Tools
- Rust toolchain (`rustup`)

## Building

```bash
# 1. Build host binary
cargo build --release -p pelagos-mac

# 2. Re-sign after every build (mandatory — cargo strips the AVF entitlement)
bash scripts/sign.sh

# 3. Build VM image (first time, or after guest changes)
bash scripts/build-vm-image.sh
```

Or use `make all` to do all three in one step.

**Why sign.sh is mandatory:** `cargo build` replaces the binary with a
linker-signed copy that lacks `com.apple.security.virtualization`. Without it,
macOS silently kills the VM daemon the moment it calls into Virtualization.framework.
The log shows nothing; `vm status` says "stopped". Always re-sign after every build.

### Installing locally for testing

`cargo build` + `sign.sh` produces a working binary at
`target/aarch64-apple-darwin/release/pelagos`, but to install it as the
system `pelagos` (replacing the Homebrew-managed binary) use `build-release.sh`:

```bash
bash scripts/build-release.sh   # build, sign, pack tarballs, update local tap formula

brew uninstall pelagos-mac 2>/dev/null || true
HOMEBREW_DEVELOPER=1 HOMEBREW_NO_INSTALL_FROM_API=1 brew install skeptomai/tap/pelagos-mac
```

`build-release.sh` writes the formula to `dist/tap/Formula/pelagos-mac.rb` and
auto-syncs it to the brew tap at
`/opt/homebrew/Library/Taps/skeptomai/homebrew-tap/`. The install step then
picks it up without any manual copy. The `brew uninstall` is required when the
version number has not changed — Homebrew skips reinstall otherwise.

**Do not** use `brew reinstall pelagos-mac` or `brew install` against the remote
`pelagos-containers/tap` formula. Its checksums are pinned to GitHub release
assets and will never match a local build. `brew reinstall` uninstalls first,
then installs — if the install fails (checksum mismatch is guaranteed for local
builds), the binary is gone with no easy recovery.

### Cross-compiling the guest

```bash
make build-guest
```

The guest is built as a static musl binary (`aarch64-unknown-linux-musl`) and baked
into the VM image by `build-vm-image.sh`.

## VM profiles

pelagos-mac runs one or more Linux VMs simultaneously, each identified by a
profile name. The `default` profile is the Alpine container VM. The `build`
profile is an Ubuntu 22.04 VM for native aarch64 development.

```bash
# See all VMs and their state
pelagos vm ls

# Container VM (default) — used for all pelagos run/exec/ps commands
pelagos vm shell                           # vsock shell into Alpine VM
pelagos vm ssh                             # SSH into Alpine VM

# Build VM — native compilation environment
bash scripts/build-build-image.sh         # provision Ubuntu build VM (one-time)
bash scripts/build-vm-start.sh            # start and wait for SSH-ready
pelagos vm ssh --profile build            # SSH in
pelagos vm ssh --profile build -- rustc --version
pelagos vm stop --profile build           # stop when done (frees 4 GB RAM)
```

The Alpine VM uses **vsock → pelagos-guest** as its control plane. The Ubuntu
build VM uses **SSH → openssh-server**. `vm shell` only works for the Alpine VM;
use `vm ssh --profile build` for Ubuntu. See
[docs/VM_LIFECYCLE.md](docs/VM_LIFECYCLE.md#the-two-vm-model) for the full breakdown.

## Using with VS Code Dev Containers

Set the Docker executable in VS Code settings:

```json
{
  "dev.containers.dockerPath": "/path/to/pelagos-docker"
}
```

See [docs/DEVCONTAINER_GUIDE.md](docs/DEVCONTAINER_GUIDE.md) for the full guide.

## Testing

```bash
# Smoke test — verify VM liveness + DNS + TCP (< 10 s)
bash scripts/test-network-smoke.sh

# Full devcontainer e2e suite (27 tests)
bash scripts/test-devcontainer-e2e.sh

# Individual suites
bash scripts/test-devcontainer-e2e.sh --suite A   # pre-built images
bash scripts/test-devcontainer-e2e.sh --suite B   # custom Dockerfile
bash scripts/test-devcontainer-e2e.sh --suite C   # devcontainer features
bash scripts/test-devcontainer-e2e.sh --suite D   # postCreateCommand
```

## Codebase

| Crate | Target | Description |
|---|---|---|
| `pelagos-mac` | aarch64-apple-darwin | macOS CLI binary |
| `pelagos-vz` | aarch64-apple-darwin | AVF bindings + smoltcp NAT relay |
| `pelagos-docker` | aarch64-apple-darwin | Docker CLI compatibility shim |
| `pelagos-guest` | aarch64-unknown-linux-musl | Guest daemon (runs inside VM) |

## Documentation

| Doc | Contents |
|---|---|
| [docs/USER_GUIDE.md](docs/USER_GUIDE.md) | **Start here** — running containers, VM management, build VM, devcontainers |
| [docs/DESIGN.md](docs/DESIGN.md) | Architecture rationale, options evaluated, security analysis |
| [docs/NETWORK_OPTIONS.md](docs/NETWORK_OPTIONS.md) | VM networking options and smoltcp relay design |
| [docs/VM_IMAGE_DESIGN.md](docs/VM_IMAGE_DESIGN.md) | Kernel selection, initramfs, module loading |
| [docs/VM_LIFECYCLE.md](docs/VM_LIFECYCLE.md) | VM start/stop/status, profiles, and daemon model |
| [docs/VM_PROFILES.md](docs/VM_PROFILES.md) | Alpine vs Ubuntu profiles — dividing lines and when to use each |
| [docs/VM_DEBUGGING.md](docs/VM_DEBUGGING.md) | Common failures and recovery procedures |
| [docs/DEVCONTAINER_GUIDE.md](docs/DEVCONTAINER_GUIDE.md) | VS Code devcontainer setup |
| [docs/DEVCONTAINER_REQUIREMENTS.md](docs/DEVCONTAINER_REQUIREMENTS.md) | devcontainer requirements and test matrix |
| [docs/VSCODE_ATTACH_SPEC.md](docs/VSCODE_ATTACH_SPEC.md) | VS Code attach protocol — layer-by-layer spec |
| [docs/GUEST_CONTAINER_EXEC.md](docs/GUEST_CONTAINER_EXEC.md) | Container namespace joining implementation |
| [docs/ALPINE_VS_UBUNTU_KERNEL.md](docs/ALPINE_VS_UBUNTU_KERNEL.md) | Alpine vs Ubuntu kernel — RCU stall mechanism, hvc0 console buffer fix |

## License

Apache 2.0
