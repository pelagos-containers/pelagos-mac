# pelagos-mac — Claude Instructions

## What This Project Is

pelagos-mac is the macOS CLI for the [pelagos](https://github.com/skeptomai/pelagos)
Linux container runtime. Because pelagos uses Linux namespaces, cgroups, and seccomp —
Linux-only kernel primitives — a Linux VM is mandatory on Apple Silicon. This project
owns that VM layer and the macOS user experience.

**Relationship to pelagos:**
- `pelagos` (separate repo) — Linux container runtime library + CLI. Runs inside the VM.
- `pelagos-mac` (this repo) — boots the VM, owns the macOS CLI, forwards commands to
  the pelagos binary running inside the VM over vsock.

**Workspace layout:**
```
pelagos-vz/       Ergonomic Rust wrapper over Apple's Virtualization.framework
                  Built on objc2-virtualization (auto-generated, weekly Xcode SDK updates)
pelagos-guest/    Guest daemon: listens on AF_VSOCK, forks pelagos, streams output back
                  Cross-compiled to aarch64-unknown-linux-gnu; baked into VM disk image
pelagos-mac/      macOS CLI binary: boots VM via pelagos-vz, proxies commands over vsock
```

---

## ⚠️ CRITICAL DESIGN DECISIONS — READ BEFORE WRITING CODE ⚠️

### No Subsystem Dependencies

**pelagos-mac has no subsystem-sized external dependencies.**

This is the central architectural decision. See `docs/DESIGN.md §Revised Architecture`
for the full rationale.

The distinction: **library dependencies** are subordinate — they do what you tell them
at the call sites you choose. **Subsystem dependencies** invert that relationship — they
have their own lifecycle, conventions, and release cadence; you build *around* them.
pelagos-mac is a product, not a Lima plugin or a vfkit integration.

Consequences:
- **No Lima.** Lima is excellent; that is not the point. It would make pelagos-mac a
  Lima integration rather than a product.
- **No vfkit.** vfkit is a thin Go wrapper around AVF; we use `objc2-virtualization`
  directly, which eliminates the Go binary entirely.
- **No gRPC daemon.** That is the Docker daemon problem rebuilt. If a persistent socket
  is ever needed, the scope is minimal + mTLS mandatory from day one.
- **virtiofsd** is the sole exception: it is already written in Rust (Red Hat) and
  is a library-level dependency, not a subsystem.

### No Go

No Go binaries, no cgo, no Go subprocess dependencies. The AVF binding layer is
`objc2-virtualization` (Rust). The `Code-Hex/vz` Go bindings exist as a **readable
reference** for how to call the Objective-C API — consult them, do not depend on them.

### Pure-Rust Stack

```
pelagos-mac (Rust, aarch64-apple-darwin)
  └── pelagos-vz          ← objc2-virtualization (AVF bindings)
  └── virtiofsd           ← Rust, Red Hat, host-side virtiofs daemon
  └── vsock via UnixStream ← std::os::unix::net (AVF exposes vsock as Unix socket)

pelagos-guest (Rust, aarch64-unknown-linux-gnu, inside VM)
  └── AF_VSOCK listener
  └── pelagos binary (fetched from pelagos releases at VM image build time)
```

---

## Pilot Phase Goals

The immediate work is a proof-of-concept that validates the architecture:

1. `pelagos-vz` boots a Linux VM (kernel + initrd + disk) via `objc2-virtualization`
2. vsock round-trip works: host Rust binary sends a command, guest daemon responds
3. virtiofs file sharing works: a host directory appears inside the VM
4. The entire stack compiles and runs with zero Go

If this pilot succeeds, the architecture is validated. Port forwarding, Rosetta,
installer packaging, and multi-VM management are all post-pilot scope.

See `ONGOING_TASKS.md` for the current task plan.

---

## Build Targets

| Crate | Target | Notes |
|---|---|---|
| pelagos-vz | aarch64-apple-darwin | macOS only; won't compile on Linux |
| pelagos-mac | aarch64-apple-darwin | macOS only |
| pelagos-guest | aarch64-unknown-linux-gnu | Cross-compiled; runs inside the VM |

**Building the guest:**
```bash
cargo build --target aarch64-unknown-linux-gnu --release -p pelagos-guest
```

**Building the host (on macOS):**
```bash
cargo build --release -p pelagos-mac
```

**Note:** `cargo build` at the workspace root will fail on Linux (pelagos-vz is
macOS-only). This is intentional and expected.

---

## Required Entitlement

The `pelagos-mac` binary must be code-signed with:
```
com.apple.security.virtualization
```
For development, an ad-hoc signature with the entitlement is sufficient.
For distribution, a Developer ID signature + notarization is required.

---

## Git Workflow

**Feature branch workflow — never push directly to `master`.**

For every task, fix, or meaningful change:

1. Create a branch from `master`:
   ```bash
   git checkout master && git pull
   git checkout -b <type>/<short-description>
   # e.g. fix/vsock-fd-lifetime, feat/virtiofs-mount, chore/update-deps
   ```
2. Commit work on the branch (one or more commits).
3. Open a PR against `master` via `gh pr create`.
4. Reference any related issues in the PR body (`Closes #N`).
5. Merge via the PR (squash or merge commit — either is fine).

Branch naming: `feat/`, `fix/`, `chore/`, `docs/` prefixes.

---

## File Placement Rules

- **Documentation / design docs** → `docs/`
- **Plans and task notes** → `ONGOING_TASKS.md`
- **VM image build scripts** → `scripts/`
- **NEVER create files in `/tmp`** — they are lost on reboot

---

## Execution Style

Execute quietly — no step-by-step narration. Just do it, then give a short summary of
what was done. Reserve prose for plans, questions, and results.

All tool use is pre-approved: Bash, Read, Edit, Write, Grep, Glob, WebSearch,
WebFetch — use them freely without asking.

### Ask Before Major Decisions
- Protocol design changes (vsock message format)
- Adding new external dependencies
- Architectural changes to the VM lifecycle model
- When uncertain about the right approach

### User Macros

**"Make it so!"** — Clean up, comment, commit, and open a PR:
1. Remove any temporary debug code or dead comments
2. Ensure `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` pass
3. Commit with a descriptive message on the current feature branch
4. Push the branch and open a PR against `master` via `gh pr create`

**"So Long and Thanks for all the Fish"** — Wrap up session, document state, commit, push:
1. Update `ONGOING_TASKS.md` with current date, git SHA, what was completed, what remains
2. Commit and push
3. Confirm repo is clean and up to date

**"Engage!"** — Tag, release, monitor:
1. Create a git tag (ask user for version if unclear)
2. Push the tag
3. Monitor the release workflow; report pass/fail and release URL

---

## Key References

- `docs/DESIGN.md` — full architecture analysis, options considered, security analysis,
  and rationale for the pure-Rust AVF approach
- `pelagos` repo — the Linux runtime that runs inside the VM:
  `https://github.com/skeptomai/pelagos`
- `objc2-virtualization` — AVF bindings: `https://docs.rs/objc2-virtualization`
- `Code-Hex/vz` — Go AVF bindings, useful as API reference:
  `https://github.com/Code-Hex/vz`
- Apple WWDC 2022 — [Create macOS or Linux virtual machines](https://developer.apple.com/videos/play/wwdc2022/10002/)
- Apple WWDC 2023 — [Create seamless experiences with Virtualization](https://developer.apple.com/videos/play/wwdc2023/10007/)

---

## No Time Estimates

Never include time estimates in documentation, plans, or commit messages.
Use effort descriptors: "Quick", "Moderate Effort", "Significant Work".

---

## Coding Style

### Logging
- **Never use `eprintln!` or `println!` for diagnostic output.** Use the `log` crate macros:
  - `log::error!` — errors that require attention
  - `log::warn!`  — recoverable problems
  - `log::info!`  — normal lifecycle events (startup, shutdown, connections)
  - `log::debug!` — internal state useful during development
  - `log::trace!` — very high-frequency detail
- `println!` is permitted only for deliberate CLI output (e.g. printing "pong" to stdout).
- All crates already depend on `log`; use `env_logger` for initialization on both host and guest.
