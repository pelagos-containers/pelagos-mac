# Epic: pelagos-tui — Platform-Agnostic Terminal UI

## Background

The TUI for the pelagos container runtime currently lives inside
`pelagos-mac`. That placement is wrong in two ways:

1. **pelagos does not own UI.** pelagos is a container runtime. Its
   responsibility ends at the runtime boundary. TUI/GUI experience belongs
   elsewhere.
2. **pelagos-mac is Mac-only.** Hosting a TUI there permanently excludes
   Linux users and conflates two unrelated concerns: VM lifecycle management
   and interactive user experience.

The natural resolution is a third repository — `pelagos-tui` — that owns
all terminal UI experience for pelagos across platforms.

---

## Goal

Extract the TUI into a dedicated `pelagos-tui` repo. The TUI works on both
macOS (via the existing pelagos-mac vsock/VM transport) and Linux (via
direct pelagos invocation), without any platform-specific logic living
inside the TUI itself.

---

## Architecture

### The PelagosClient Trait

The central abstraction is a `PelagosClient` trait, published as a public
module (or feature-gated library interface) of the pelagos crate itself.
This is the right home because:

- The trait is a contract over the runtime's capabilities — versioning it
  alongside the runtime is natural
- It avoids a fourth crate (`pelagos-client`) and a fourth version number
  to track
- When pelagos ships a new capability, the trait gains a new method in the
  same release

The trait exposes the operations the TUI needs:

```
list_images() → Vec<ImageSummary>
list_containers() → Vec<ContainerSummary>
pull(ref: &str) → Result<()>
run(opts: RunOptions) → Result<ContainerId>
stop(id: &ContainerId) → Result<()>
remove(id: &ContainerId) → Result<()>
logs(id: &ContainerId) → impl Stream<Item = LogLine>
```

The TUI depends only on this trait. Platform-specific code wires up the
correct impl.

### Dependency Graph

```
pelagos-tui
  └── PelagosClient (trait, defined in pelagos)
        ├── Mac impl (in pelagos-mac)
        │     vsock → pelagos-guest → VM → pelagos binary
        └── Linux impl (in pelagos or pelagos-tui itself)
              subprocess / Unix socket → pelagos binary
```

### Version Coupling

Three repos, three version numbers:

| Repo | Version | Depends on |
|---|---|---|
| pelagos | X.Y.Z | — |
| pelagos-mac | A.B.C | pelagos ≥ X.Y |
| pelagos-tui | P.Q.R | pelagos ≥ X.Y, pelagos-mac ≥ A.B (Mac builds only) |

A breaking change to the runtime API bumps pelagos; pelagos-mac and
pelagos-tui both update their floor constraint.

---

## Component Breakdown

### What moves out of pelagos-mac → pelagos-tui

- All TUI screen code (image management screen, container screen, palette)
- Keybinding definitions and event dispatch
- TUI state model (selection, cursor, pending operations)
- Ratatui / crossterm dependency declarations
- Any TUI-specific configuration (theming, layout prefs)

### What stays in pelagos-mac

- VM boot and lifecycle management
- vsock transport layer
- A concrete `MacPelagosClient` struct implementing `PelagosClient`
  - wraps GuestCommand dispatch
  - GuestCommand remains a private transport detail, not part of the
    public interface
- The `pelagos` and `pelagos-docker` CLI binaries

### What goes into pelagos

- The `PelagosClient` trait definition (behind a `client` feature flag
  to avoid pulling it into minimal runtime builds)
- A Linux `SubprocessClient` impl that invokes `pelagos` subcommands with
  `--format json` and parses the output

---

## Platform Implementations

### Mac

`MacPelagosClient` lives in pelagos-mac. It connects to the running VM
over vsock, issues GuestCommands, and translates responses into the types
defined by `PelagosClient`. It is compiled into the TUI binary when
building for `aarch64-apple-darwin`.

### Linux

The Linux impl drives pelagos via subprocess. This is simpler than it
sounds: pelagos already has machine-readable output (`--format json`) for
all list operations. For write operations (run, pull, stop), it spawns
pelagos subcommands and streams their output. The main limitation is no
push notification from the runtime — the TUI polls on a configurable
interval, which is acceptable for a terminal UI.

A more capable Linux impl (Unix domain socket, or direct library linkage
against pelagos as a lib) is possible later but not required for an initial
working TUI on Linux.

---

## Extraction Work Required

### In pelagos-mac

1. Separate TUI state and rendering from vsock dispatch in `main.rs`
   (currently co-located)
2. Define `MacPelagosClient` as a concrete type that wraps the existing
   vsock connection
3. Expose it through the `PelagosClient` trait once that trait exists

### In pelagos

1. Add a `client` feature flag
2. Define the `PelagosClient` trait and supporting types
   (`ImageSummary`, `ContainerSummary`, `RunOptions`, `LogLine`, etc.)
3. Implement `SubprocessClient` for Linux

### In pelagos-tui (new repo)

1. Initialize the Rust workspace
2. Import the extracted TUI code from pelagos-mac
3. Add conditional compilation to wire the correct `PelagosClient` impl
   per platform
4. Add a mock/stub impl to enable TUI development without a live runtime

---

## Open Questions

- **pelagos-tui binary name**: `pelagos-tui`? Or should it eventually be a
  subcommand of a future `pelagos` CLI (`pelagos tui`)?
- **Signing on Mac**: the TUI binary on Mac connects to the VM socket —
  does it need the `com.apple.security.virtualization` entitlement, or
  does it talk to pelagos-mac which already has it? Likely no entitlement
  needed if the TUI just uses a Unix socket exposed by pelagos-mac.
- **Multiplexing**: can two clients (pelagos-mac CLI and the TUI) drive the
  same VM simultaneously? The current vsock model is single-client — this
  may need revisiting.
- **Linux privileges**: `pelagos run` requires root. Does the TUI run as
  root, or does it use sudo/polkit for privileged operations?

---

## Status

Pre-implementation. The `pelagos-tui` repository exists but contains only
the LICENSE and README. No code has been extracted. This epic documents the
intended architecture so extraction can proceed incrementally.

The immediate prerequisite work before any extraction:
1. Define `PelagosClient` trait in pelagos (agreed interface)
2. Audit current TUI code in pelagos-mac to confirm it maps cleanly to that
   interface
3. Resolve the open questions above, especially multiplexing
