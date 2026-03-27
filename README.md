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

### Cross-compiling the guest

```bash
make build-guest
```

The guest is built as a static musl binary (`aarch64-unknown-linux-musl`) and baked
into the VM image by `build-vm-image.sh`.

## VM profiles

Named profiles run different VM configurations from the same binary. The
default profile runs the Alpine pelagos VM for containers. The `build` profile
runs an Ubuntu 22.04 VM for native aarch64 development:

```bash
bash scripts/build-build-image.sh   # provision Ubuntu build VM (one-time)
bash scripts/build-vm-start.sh      # start it and wait for SSH
pelagos --profile build vm ssh      # connect
pelagos --profile build vm ssh -- rustc --version
```

The key distinction: the Alpine VM uses **vsock → pelagos-guest** as its
control plane (for container commands). The Ubuntu build VM uses
**SSH → openssh-server**. `pelagos ping` handles both via `ping_mode` in
`vm.conf` — see [docs/VM_LIFECYCLE.md](docs/VM_LIFECYCLE.md#vm-profiles-and-control-planes).

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
| [docs/DESIGN.md](docs/DESIGN.md) | Architecture rationale, options evaluated, security analysis |
| [docs/NETWORK_OPTIONS.md](docs/NETWORK_OPTIONS.md) | VM networking options and smoltcp relay design |
| [docs/VM_IMAGE_DESIGN.md](docs/VM_IMAGE_DESIGN.md) | Kernel selection, initramfs, module loading |
| [docs/VM_LIFECYCLE.md](docs/VM_LIFECYCLE.md) | VM start/stop/status and daemon model |
| [docs/VM_DEBUGGING.md](docs/VM_DEBUGGING.md) | Common failures and recovery procedures |
| [docs/DEVCONTAINER_GUIDE.md](docs/DEVCONTAINER_GUIDE.md) | VS Code devcontainer setup |
| [docs/DEVCONTAINER_REQUIREMENTS.md](docs/DEVCONTAINER_REQUIREMENTS.md) | devcontainer requirements and test matrix |
| [docs/VSCODE_ATTACH_SPEC.md](docs/VSCODE_ATTACH_SPEC.md) | VS Code attach protocol — layer-by-layer spec |
| [docs/GUEST_CONTAINER_EXEC.md](docs/GUEST_CONTAINER_EXEC.md) | Container namespace joining implementation |
| [docs/ALPINE_VS_UBUNTU_KERNEL.md](docs/ALPINE_VS_UBUNTU_KERNEL.md) | Alpine vs Ubuntu kernel — RCU stall mechanism, hvc0 console buffer fix |

## License

Apache 2.0
