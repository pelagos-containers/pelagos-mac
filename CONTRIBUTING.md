# Contributing to pelagos-mac

This project has two distinct compilation targets and two distinct build
environments. Understanding the split is the first thing a new contributor
needs.

---

## Two repos, two runtimes

| Repo | What it is | Where it runs |
|---|---|---|
| `pelagos-mac` (this repo) | macOS CLI, VM lifecycle, AVF bindings | macOS (aarch64-apple-darwin) |
| `pelagos` | Linux container runtime | Inside the VM (aarch64-unknown-linux-gnu) |

`pelagos-mac` boots a Linux VM and forwards container commands to `pelagos`
running inside it. Changes to the macOS side are built on macOS. Changes to
the Linux runtime must be compiled on Linux — not cross-compiled.

---

## Setting up your environment

### 1. Prerequisites

```bash
xcode-select --install
curl https://sh.rustup.rs -sSf | sh
rustup target add aarch64-unknown-linux-musl
brew install zig squashfs
```

### 2. Clone and build pelagos-mac

```bash
git clone https://github.com/pelagos-containers/pelagos-mac
cd pelagos-mac
git submodule update --init

cargo build --release -p pelagos-mac
bash scripts/sign.sh        # mandatory after every build — see below
```

**Why sign.sh is mandatory:** `cargo build` replaces the binary with a
linker-signed copy that lacks `com.apple.security.virtualization`. Without
it, macOS silently kills the VM daemon on the first AVF call — `vm status`
says "stopped", the log shows nothing. Run `bash scripts/sign.sh` after
every `cargo build`.

### 3. Build the Alpine VM image

The Alpine initramfs is the default container VM. Build it once (or after
modifying `pelagos-guest` or the initramfs):

```bash
bash scripts/build-vm-image.sh
```

Verify:

```bash
./target/aarch64-apple-darwin/release/pelagos ping   # → pong
./target/aarch64-apple-darwin/release/pelagos run alpine echo hello
```

### 4. Build the Ubuntu build VM (one-time, for hacking on pelagos)

If you are modifying `pelagos` (the Linux runtime), you need a Linux
compilation environment. Provision the Ubuntu build VM once:

```bash
bash scripts/build-build-image.sh
```

This downloads Ubuntu 24.04, installs the Rust toolchain, and writes
`out/build.img`. Takes several minutes on first run. Only needs to be
repeated if the Ubuntu image is deleted or needs a toolchain update.

---

## Daily development workflow

### Changing pelagos-mac (the macOS side)

```bash
cargo build --release -p pelagos-mac
bash scripts/sign.sh
./target/aarch64-apple-darwin/release/pelagos ping
```

### Changing pelagos (the Linux runtime)

All compilation happens inside the Ubuntu build VM.

```bash
# Start the build VM (waits until SSH is ready)
bash scripts/build-vm-start.sh

# Build — no prefix needed, cargo is in PATH via /etc/environment
pelagos --profile build vm ssh -- "cd /mnt/Projects/pelagos && cargo build --release"

# Or for iterative work, open an interactive session
pelagos --profile build vm ssh
# Inside the VM:
# cd /mnt/Projects/pelagos
# cargo build --release

# Stop when done (frees 4 GB RAM)
pelagos --profile build vm stop
```

The macOS home directory is mounted at `/mnt` inside the build VM (virtiofs,
auto-mounted by systemd). Source at `/mnt/Projects/pelagos`, build artifacts
on your macOS filesystem.

### Running tests

```bash
# pelagos-mac e2e tests
bash scripts/test-e2e.sh

# devcontainer e2e tests
bash scripts/test-devcontainer-e2e.sh

# pelagos unit tests (in build VM)
pelagos --profile build vm ssh -- "cd /mnt/Projects/pelagos && cargo test --lib"
```

---

## Git workflow

Feature branch → PR → merge commit. Never push to `main` directly, never
squash. See `CLAUDE.md` for the full workflow and PR conventions.

---

## Key documentation

| Doc | Read when |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | Full prerequisites and build steps |
| [docs/VM_PROFILES.md](docs/VM_PROFILES.md) | Alpine vs Ubuntu VM profiles, `vm shell` vs `vm ssh`, build VM detail |
| [docs/DESIGN.md](docs/DESIGN.md) | Why the architecture is the way it is |
| [docs/VM_IMAGE_DESIGN.md](docs/VM_IMAGE_DESIGN.md) | Initramfs internals |
| [docs/ARCHITECTURE_MENTAL_MODEL.md](docs/ARCHITECTURE_MENTAL_MODEL.md) | Common misconceptions and how to think about the stack |
