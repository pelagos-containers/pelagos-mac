# Installing pelagos-mac

Two paths: **Homebrew** (users and contributors who want a working install) and
**build from source** (contributors modifying the code).

---

## Homebrew install (recommended)

Requires macOS 13.5+ (Ventura) on Apple Silicon.

```bash
brew tap pelagos-containers/tap
brew install pelagos-containers/tap/pelagos-mac
pelagos vm init
```

`pelagos vm init` copies the VM disk image to `~/.local/share/pelagos/` and writes
`vm.conf` pointing at the installed kernel, initramfs, and disk. Run it once after
install (and after each upgrade).

Verify:

```bash
pelagos ping                       # should print: pong
pelagos run alpine echo hello      # should print: hello
```

The VM boots automatically on the first command. Cold boot takes about 2 s. Every
subsequent command reuses the running VM.

### Upgrading

```bash
brew upgrade pelagos-containers/tap/pelagos-mac
pelagos vm init --force   # stops old VM, re-inits with new kernel + initramfs
pelagos ping
```

`--force` stops any running VM, overwrites `vm.conf`, and replaces `root.img`
with the fresh placeholder from the new release. Any cached container images on
the old disk are lost; they will be re-pulled on next use.

### Uninstall

```bash
pelagos vm stop                    # stop the VM if running
brew uninstall pelagos-containers/tap/pelagos-mac
rm -rf ~/.local/share/pelagos      # removes OCI cache, vm.conf, and all state
```

`brew uninstall` removes the binaries and VM artifacts under `/opt/homebrew/`.
The state directory (`~/.local/share/pelagos/`) is not managed by Homebrew and
must be removed manually. It contains the writable disk image (OCI layer cache)
and `vm.conf` — omit the `rm` if you want to preserve cached images for a
reinstall.

---

## Build from source

For contributors who are modifying `pelagos-mac` itself.

### Prerequisites

- macOS 13.5+ (Ventura), Apple Silicon
- Xcode Command Line Tools: `xcode-select --install`
- Rust toolchain: `curl https://sh.rustup.rs -sSf | sh`
- Cross-compilation target: `rustup target add aarch64-unknown-linux-musl`
- zig (for cross-compiling the guest): `brew install zig`
- squashfs tools (for VM image build): `brew install squashfs`

### Clone and build

```bash
git clone https://github.com/pelagos-containers/pelagos-mac
cd pelagos-mac
git submodule update --init

# 1. Build the host binary
cargo build --release -p pelagos-mac

# 2. Re-sign (mandatory after every build)
bash scripts/sign.sh

# 3. Build the VM image (first time only, or after guest changes)
bash scripts/build-vm-image.sh
```

**Why sign.sh is mandatory:** `cargo build` replaces the binary with a
linker-signed copy that lacks `com.apple.security.virtualization`. Without
it, macOS silently kills the VM daemon on the first AVF call — `vm status`
says "stopped", the log shows nothing. Always run `sign.sh` after every build.

### Run without installing

The built binary runs directly from the workspace:

```bash
./target/aarch64-apple-darwin/release/pelagos ping
./target/aarch64-apple-darwin/release/pelagos run alpine echo hello
```

It auto-discovers VM artifacts in `out/` when run from the workspace root (or
when `out/` is in `../../../out` relative to the binary location).

### Ubuntu build VM (for compiling pelagos)

pelagos (the Linux container runtime that runs inside the VM) must be
compiled on Linux — not cross-compiled from macOS. The `build` profile is
a persistent Ubuntu 24.04 VM that serves as the native compilation
environment. It is a one-time setup.

**Why not cross-compile from macOS?** The Linux standard library is not
available on macOS. `cargo check --target aarch64-unknown-linux-gnu` fails
immediately on a macOS host.

**First-time provisioning** (downloads Ubuntu 24.04, installs Rust toolchain,
takes several minutes):

```bash
bash scripts/build-build-image.sh
```

This provisions `out/build.img` (a 20 GB Ubuntu disk image) and writes
`~/.local/share/pelagos/profiles/build/vm.conf`. Only needs to be run
once, or after a kernel/toolchain update.

**Daily use:**

```bash
# Start the build VM (takes ~30 s for SSH to be ready on first boot)
bash scripts/build-vm-start.sh

# Build pelagos non-interactively
pelagos --profile build vm ssh -- "cd /mnt/Projects/pelagos && cargo build --release"

# Or open an interactive shell
pelagos --profile build vm ssh

# Stop when done (frees 4 GB RAM)
pelagos --profile build vm stop
```

The macOS home directory is available at `/mnt` inside the build VM via
virtiofs (auto-mounted by systemd on boot). The pelagos source tree is at
`/mnt/Projects/pelagos`. Build artifacts land on the macOS filesystem and
persist across VM restarts.

See [VM_PROFILES.md](VM_PROFILES.md) for the full breakdown of the two VM
profiles (Alpine container VM vs Ubuntu build VM) and how they differ.

### Testing

```bash
bash scripts/test-e2e.sh           # full e2e suite
bash scripts/test-e2e.sh --cold    # cold-start variant
bash scripts/test-devcontainer-e2e.sh  # devcontainer suite (27 tests)
```

---

## Installing a local build via Homebrew

To replace a Homebrew-installed `pelagos` with a local build (for testing the
full install flow, not for normal development):

```bash
bash scripts/build-release.sh      # packs tarballs + writes local formula

brew uninstall pelagos-mac 2>/dev/null || true
HOMEBREW_DEVELOPER=1 HOMEBREW_NO_INSTALL_FROM_API=1 \
  brew install pelagos-containers/tap/pelagos-mac
pelagos vm init
```

`build-release.sh` writes the formula to `dist/tap/Formula/pelagos-mac.rb`
with `file://` URLs pointing at the local tarballs, and syncs it to the local
tap. Do **not** use `brew reinstall` — if the install fails (e.g. checksum
mismatch), the binary is gone with no recovery.

---

## Directory layout after install

| Path | Contents |
|---|---|
| `/opt/homebrew/bin/pelagos` | macOS CLI (Homebrew symlink) |
| `/opt/homebrew/bin/pelagos-docker` | Docker CLI compatibility shim |
| `/opt/homebrew/bin/pelagos-tui` | Terminal UI |
| `/opt/homebrew/share/pelagos-mac/vmlinuz` | Linux kernel (read-only, shared) |
| `/opt/homebrew/share/pelagos-mac/initramfs.gz` | Initramfs (read-only, shared) |
| `/opt/homebrew/share/pelagos-mac/root.img` | Blank disk placeholder |
| `~/.local/share/pelagos/root.img` | Writable disk (OCI image cache) |
| `~/.local/share/pelagos/vm.conf` | VM configuration (written by `vm init`) |
| `~/.local/share/pelagos/daemon.log` | VM daemon log |

The `share/` artifacts are read-only and shared across users. `vm init` copies
`root.img` to the writable state directory so each user has their own OCI cache.
