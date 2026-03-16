# pelagos-mac — devcontainer Support Requirements

*Authoritative requirements for VS Code Remote-Containers support via `pelagos-docker`.*

---

## Governing Rule — No VS Code Dependency in Tests

**Every devcontainer requirement must be verifiable outside VS Code.**

VS Code is the ultimate consumer, but it is not a test tool. Its output is opaque,
its failure messages are vague, and it cannot be scripted. Every R-DC-* requirement
must have a corresponding test case in `scripts/test-devcontainer-e2e.sh` that uses
only the `devcontainer` CLI directly — no IDE, no extension, no GUI.

Corollary: if a requirement cannot be tested without VS Code, it is not well-defined.
Rewrite it until it can be expressed as a `devcontainer` CLI assertion.

The only exception is R-DC-05 (IDE-specific extension behavior) which is inherently
manual. All other requirements are automatable and must be automated before being
declared "met".

---

## How to Use This Document

Every requirement has an ID (`R-VM-*`, `R-SH-*`, `R-DC-*`). Each maps to one or
more test cases in `scripts/test-devcontainer-shim.sh` (prefix `TC-*`). A requirement
is **met** only when all its test cases pass and have been verified against a live VM.

---

## Layer Terminology

| Layer | What | Owned By |
|---|---|---|
| **VM layer** | AVF virtual machine, kernel, init, networking | pelagos-mac (this repo) |
| **Runtime layer** | pelagos guest daemon, container lifecycle | pelagos (separate repo) |
| **Shim layer** | pelagos-docker Docker CLI compatibility shim | pelagos-mac (this repo) |
| **devcontainer** | devcontainer CLI + VS Code Remote-Containers | Third-party |

---

## Part 1 — VM/Runtime Requirements

These are contracts that the VM and the pelagos runtime must uphold for the shim to work.

### R-VM-01 — VM stays up between commands; never auto-shuts-down

**Description:** The VM daemon must not exit on its own. It runs until explicit
`pelagos vm stop` or system restart. This eliminates any race between consecutive
devcontainer CLI commands.

**Rationale:** devcontainer CLI sends `docker run` then, milliseconds later, `docker ps`
and `docker inspect`. If the VM could shut down spontaneously, those calls would fail
silently.

**Acceptance Criteria:**
- VM daemon is running after `pelagos run`, `pelagos stop`, and `pelagos rm`
- Consecutive commands within a devcontainer session all succeed without any sleep
  between them
- Explicit `pelagos vm stop` is the only way to stop the VM

**Current State:** MEETS — resolved by design decision in pelagos (issue #91, gap 3,
closed 2026-03-13). The VM never auto-shuts-down. The keepalive hack that previously
existed in the shim has been removed.

**Removed Hack:** `while sleep 1000; do :; done` was appended to probe container
commands to keep the VM alive. No longer present or needed.

---

### R-VM-02 — Container state persists after exit; discoverable via `ps --all`

**Description:** After a container exits (any exit code), its metadata (name, image,
labels, mounts, status) must be readable via `pelagos ps --all` and
`pelagos inspect <name>` until explicitly removed.

**Rationale:** devcontainer calls `docker ps -q -a --filter label=...` after the probe
run exits. If the container record is gone, devcontainer cannot find the container it
just created and reports failure.

**Acceptance Criteria:**
- `pelagos ps --all` includes exited containers
- `pelagos inspect <name>` succeeds for exited containers
- `inspect` output includes `spawn_config.labels` (or top-level `labels`) with the
  original labels passed to `run --label`
- State persists across a VM restart (state file is on disk, not in-memory only)

**Current State:** MEETS — pelagos persists container state in `~/.local/share/pelagos/`
state files. Labels present in `pelagos inspect` JSON output.

---

### R-VM-03 — Label filtering round-trip

**Description:** Labels passed to `pelagos run --label KEY=VALUE` must be readable back
via `pelagos inspect` (at key `labels` or `spawn_config.labels`) and must match exactly
(key and value, case-sensitive).

**Rationale:** devcontainer uses two labels to identify its container:
- `devcontainer.local_folder=<absolute_workspace_path>`
- `devcontainer.config_file=<absolute_devcontainer_json_path>`

The shim client-side filters `pelagos ps --all` output using these labels. If a label
is lost or transformed, the filter returns empty.

**Acceptance Criteria:**
- `pelagos run --label foo=bar ... && pelagos inspect <name>` → `labels.foo == "bar"`
- Labels survive container restart (`pelagos start`)
- Label values containing `/` and `.` (path-like values) are stored verbatim
- Multiple labels on the same container all round-trip correctly

**Current State:** MEETS — confirmed via live inspect of `test-label` container.

---

### R-VM-04 — exec-into enters container namespaces

**Description:** `pelagos exec-into <name> <cmd>` must run `<cmd>` inside the
container's mount namespace (minimum). The process must see the container's rootfs,
not the host/VM root.

**Acceptance Criteria:**
- `exec-into <name> cat /etc/os-release` returns the container's OS, not Alpine
- `exec-into <name> uname -s` returns `Linux`
- Interactive stdin/stdout work (exec with `-i` flag passes stdin to the process)
- Non-zero exit code from the command is propagated as the exit code of exec-into

**Current State:** MEETS — verified in test phase 6.

**Known Limitation:** exec-into does NOT enter the PID namespace. `/proc/self/environ`
is therefore unavailable inside exec'd processes. devcontainer CLI's fallback
(`printenv`) must work instead.

---

### R-VM-05 — Named volumes backed by persistent storage

**Description:** `pelagos volume create <name>` must create a named volume whose
contents persist across VM restarts. VS Code stores its server installation and
extensions in named volumes (`vscode`, `vscode-server-<hash>`).

**Acceptance Criteria:**
- `volume create vscode` → exists after `pelagos vm stop && pelagos run ...`
- Files written to `/root/.vscode-server` (mounted from named volume) survive VM restart
- `volume ls` includes the volume name
- `volume rm <name>` removes it permanently

**Current State:** PARTIAL — `volume create/ls/rm` delegate to pelagos. Backing store
persistence across VM restart is UNVERIFIED (issue #93).

---

### R-VM-06 — Concurrent VM command handling

**Description:** The VM daemon must handle at least 2 concurrent vsock connections
without deadlock. devcontainer CLI sometimes issues background `events` polling
concurrently with foreground `ps`/`inspect` calls.

**Acceptance Criteria:**
- `docker events` (blocking, polling) can run in background while `docker ps` succeeds
- No vsock connection backlog stalls under normal devcontainer CLI load

**Current State:** MEETS — guest daemon uses thread-per-connection accept loop.

---

## Part 2 — Shim (pelagos-docker) Requirements

### R-SH-01 — Pre-flight commands return expected formats

| Command | Expected output |
|---|---|
| `docker -v` | Any string with "docker", "version", or "pelagos" |
| `docker version` | JSON with `"Client"` and `"Server"` keys |
| `docker version --format '{{.Server.Version}}'` | Bare version matching `[0-9]+\.[0-9]+` |
| `docker context ls --format '{{json .}}'` | JSON object with `"Name": "default"` |
| `docker context show` | `default\n` |
| `docker buildx version` | Exit non-zero (signals no BuildKit; triggers fallback) |
| `docker info` | JSON with `"ServerVersion"` and `"OSType": "linux"` |

**Acceptance Criteria:** Each command returns the expected format and exit code.

---

### R-SH-02 — Probe run runs normally; container restartable after exit

**Description:** When devcontainer CLI sends:
```
docker run --sig-proxy=false ... <image> /bin/sh -c "echo Container started"
```
the shim must run it as a normal foreground container. The container prints
`Container started`, exits, and its state is preserved. devcontainer CLI then:
1. Calls `docker ps -a --filter label=...` to find the exited container
2. Calls `docker inspect` (shows `State.Running=false`)
3. Calls `docker start` to restart it
4. Calls `docker exec` to enter the running container

**Rationale:** The VM never auto-shuts-down (R-VM-01) and pelagos persists exited
container state (R-VM-02). No keepalive injection needed; the correct fix is in the
runtime, not the shim.

**Acceptance Criteria:**
- `docker run ... sh -c "echo Container started"` prints `Container started` and exits 0
- Container appears in `docker ps -q -a --filter label=...` immediately after exit
- `docker inspect` shows `State.Running=false`, `State.Status=exited`
- `docker start <name>` restarts the container successfully
- `docker exec` into the restarted container enters the correct rootfs

**Note:** `--sig-proxy` is accepted and silently ignored by the shim (no special
handling).

---

### R-SH-03 — `docker ps` label filtering

**Description:** `docker ps [-q] [-a] --filter label=KEY=VALUE [--filter label=K2=V2]`
must return only containers whose labels satisfy ALL filters (AND semantics).

**Acceptance Criteria:**
- A container run with `--label k=v` appears when filtered with `--filter label=k=v`
- A container without that label does NOT appear
- Multiple `--filter label=` clauses are ANDed
- Label values containing `/` (path separators) and `.` (dots) match exactly
- Works for running AND exited containers when `-a` is given

---

### R-SH-04 — `docker inspect` container format

**Description:** `docker inspect [--type container] <name>` must return a JSON array
with one element containing at minimum:

```json
[{
  "Id": "<name>",
  "Name": "/<name>",
  "Created": "<ISO-8601>",
  "State": {
    "Running": <bool>,
    "Status": "<running|exited|...>",
    "StartedAt": "<ISO-8601>"
  },
  "Config": {
    "Image": "<image-ref>",
    "Labels": { "key": "value", ... },
    "User": "",
    "Env": ["K=V", ...],
    "Cmd": [],
    "WorkingDir": "",
    "Entrypoint": null
  },
  "HostConfig": { "Binds": ["<host>:<container>", ...] },
  "Mounts": [
    { "Type": "bind", "Source": "<host-path>", "Destination": "<container-path>",
      "Mode": "", "RW": true, "Propagation": "rprivate" }
  ],
  "NetworkSettings": { "Ports": {} }
}]
```

**Critical fields devcontainer reads:**
- `State.Running` — used to decide whether to `start` or `exec`
- `Config.Labels` — used to identify the container across sessions
- `Mounts[].Source` — must be **host** paths (not VM-internal `/mnt/share0/...`)
- `Created` — used as a lifecycle marker for idempotent setup scripts
- `Config.Env` — must be an array, not an object

**Acceptance Criteria:**
- All required fields present with correct types
- Mount `Source` values are host paths (translation via `vm.mounts` applied)
- Labels match what was passed to `run --label`
- `Config.Env` is a JSON array of `"K=V"` strings

---

### R-SH-05 — Mount path translation

**Description:** When pelagos stores a bind mount as `/mnt/share0/Projects/foo:/workspace`,
the shim must translate it to `/Users/cb/Projects/foo:/workspace` in all inspect output.

**Acceptance Criteria:**
- `docker inspect` bind mounts show host paths
- Translation uses `~/.local/share/pelagos/vm.mounts` (written at VM boot)
- Unmapped paths are returned as-is (not silently dropped)

---

### R-SH-06 — `docker exec` interactive mode

**Description:** `docker exec [-i] [-t] [-u <user>] [-e K=V] <name> <cmd> [args...]`
must run `<cmd>` inside the container.

**Acceptance Criteria:**
- Non-interactive: stdout of command returned on stdout, exit code propagated
- Interactive (`-i`): stdin is forwarded to the subprocess
- Exit code of the command is the exit code of `docker exec`
- `-u root` flag accepted (silently passed through or enforced)
- `-e K=V` sets env vars in the exec'd process

---

### R-SH-07 — `docker build` delegates to pelagos build

**Description:** `docker build -t <tag> [-f <Dockerfile>] <context>` must:
1. Archive the build context and transfer it to the VM
2. Pre-pull all `FROM` base images that are not prior-stage aliases
3. Call `pelagos build -t <tag> -f <Dockerfile> <context-in-vm>`
4. Stream build output to stderr
5. Return exit 0 on success, non-zero on failure

**Acceptance Criteria:**
- Single-stage Dockerfile: image available for `docker run` after build
- Multi-stage Dockerfile (`FROM x AS stage1` + `COPY --from=stage1`): succeeds
- Correct base images are pre-pulled (stage aliases are NOT pulled as images)
- Build output is streamed in real time (not buffered)

**Current State:** PARTIAL — implemented in `pelagos-guest/src/main.rs handle_build`.
Multi-stage pre-pull logic is code-complete but END-TO-END UNTESTED (issue #92).

---

### R-SH-08 — `docker volume` CRUD

| Command | Expected |
|---|---|
| `volume create <name>` | Prints `<name>`, exit 0 |
| `volume ls` | Lists volume names, one per line |
| `volume ls -q` | Same, quiet (no header) |
| `volume rm <name>` | Exit 0 on success |
| `volume inspect <name>` | JSON with `Name`, `Mountpoint` — LOW PRIORITY |

---

### R-SH-09 — `docker network` CRUD

| Command | Expected |
|---|---|
| `network create <name>` | Prints network ID, exit 0 |
| `network ls` | Lists networks |
| `network rm <name>` | Exit 0 on success |
| `network inspect <name>` | JSON — LOW PRIORITY |

---

### R-SH-10 — `docker events` polling

**Description:** `docker events [--filter event=start]` must block and emit synthetic
Docker event JSON as new containers appear. devcontainer CLI runs this in the background
as a liveness check.

**Acceptance Criteria:**
- Does not exit immediately
- Emits a `start` event JSON when a new container starts
- JSON format: `{"Type":"container","Action":"start","Actor":{"ID":"<name>","Attributes":{...}}}`
- Does not emit events for containers that existed before the call
- Terminates cleanly when the caller sends SIGTERM/SIGKILL

---

## Part 3 — devcontainer CLI Integration Requirements

These are end-to-end requirements for a complete devcontainer session.

### R-DC-01 — `devcontainer up` succeeds with a pre-built image

**devcontainer.json:**
```json
{ "image": "mcr.microsoft.com/devcontainers/base:ubuntu" }
```

**Acceptance Criteria:**
- `devcontainer up --workspace-folder <path>` exits 0
- `outcome` in the JSON result is `"success"`
- `devcontainer exec` works inside the container afterwards
- Container has correct labels and mounts

---

### R-DC-02 — `devcontainer up` succeeds with a custom Dockerfile

**devcontainer.json:**
```json
{
  "build": { "dockerfile": "Dockerfile", "context": ".." },
  "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
}
```

**Acceptance Criteria:**
- `docker build` is called with the devcontainer.json's `dockerfile`/`context`
- Image is built successfully inside the VM
- Container is started from the built image
- `devcontainer up` exits 0 with `"outcome": "success"`

**Current State:** UNTESTED (issue #92).

---

### R-DC-03 — `devcontainer up` succeeds with devcontainer features

**devcontainer.json:**
```json
{
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "features": {
    "ghcr.io/devcontainers/features/node:1": {}
  }
}
```

**Acceptance Criteria:**
- devcontainer CLI builds the feature layer Dockerfile
- Multi-stage or layered build completes inside the VM
- `node --version` works inside the container after `devcontainer up`

**Current State:** UNTESTED (issue #92).

---

### R-DC-04 — Container survives `devcontainer exec` after `devcontainer up`

**Acceptance Criteria:**
- `devcontainer exec --workspace-folder <path> -- node --version` works after R-DC-03
- exec enters the correct container (not a different namespace)
- Exit code of the exec'd command is propagated

---

### R-DC-05 — VS Code Remote-Containers full IDE flow

**Acceptance Criteria:**
- "Reopen in Container" opens the container
- VS Code Server installs into the `vscode` named volume
- Terminal opens inside the container
- Extensions listed in `devcontainer.json` install
- `postCreateCommand` runs and output is visible in VS Code terminal

**Current State:** UNTESTED (issue #91). Requires manual VS Code interaction.

---

## Part 4 — Test Plan

### Automation Strategy

Tests are organized into three tiers:

| Tier | Scope | Runner | CI? |
|---|---|---|---|
| **T1** — Shim unit | Individual `pelagos-docker` commands in isolation | `scripts/test-devcontainer-shim.sh` | Yes |
| **T2** — Integration | Full devcontainer CLI command sequence without VS Code | `scripts/test-devcontainer-e2e.sh` (to build) | Yes |
| **T3** — IDE | VS Code "Reopen in Container" full IDE flow | Manual (issue #91) | No |

T1 and T2 must be scriptable, non-interactive, and runnable with a single command.

---

### T1 — Shim Command Tests (`test-devcontainer-shim.sh`)

Fully rewritten to test the correct lifecycle (probe exits → start → exec) and to
produce clear diagnostic output on failures. Run with `--debug` for full output.

| Phase | Tests | Requirements |
|---|---|---|
| 1  — pre-flight | 7 | R-SH-01 |
| 2  — volume CRUD | 3 | R-SH-08 |
| 3  — ps baseline (no containers) | 2 | R-SH-03 |
| 4  — probe run + post-exit ps | 3 | R-SH-02, R-VM-02, R-SH-03 |
| 5  — inspect exited | 3 | R-SH-04, R-SH-05 |
| 6  — docker start | 2 | R-VM-01 (pelagos start) |
| 7  — inspect running | 2 | R-SH-04 |
| 8  — exec | 4 | R-SH-06 |
| 9  — shell server pattern | 5 | R-SH-06 |
| 10 — system-config patching | 3 | R-SH-06 |
| 11 — timing: ps with no sleep | 1 | R-VM-02, R-SH-03 |
| 12 — multi-label AND filter | 2 | R-SH-03 |
| 13 — label path values | 2 | R-VM-03, R-SH-03 |
| 14 — inspect field types | 5 | R-SH-04 |
| 15 — stop + rm | 3 | — |

**Remaining gaps (T2 or manual):**

| TC-ID | Test case | Requirement |
|---|---|---|
| TC-T1-37 | Named volume data survives VM restart | R-VM-05 |
| TC-T1-38 | `docker events` emits start event | R-SH-10 |
| TC-T1-39 | `docker build` single-stage Dockerfile | R-SH-07 |
| TC-T1-40 | `docker build` multi-stage Dockerfile | R-SH-07 |

---

### T2 — Integration Test (`test-devcontainer-e2e.sh`)

Drives the actual `devcontainer` CLI (not just `pelagos-docker` directly). Requires
`devcontainer` CLI installed (`npm install -g @devcontainers/cli`).

Run: `bash scripts/test-devcontainer-e2e.sh [--debug] [--suite A|B|C|D]`

**Test cases:**

| TC-ID | Suite | Scenario | Requirements |
|---|---|---|---|
| TC-T2-01 | A | Pre-built image: `devcontainer up` exits 0, outcome=success | R-DC-01 |
| TC-T2-02 | A | Pre-built image: `devcontainer exec -- uname -s` = `Linux` | R-DC-01, R-DC-04 |
| TC-T2-03 | A | Pre-built image: `devcontainer exec -- cat /etc/os-release` contains image distro | R-DC-04 |
| TC-T2-04 | A | Pre-built image: `devcontainer.local_folder` label present after up | R-VM-03, R-SH-03 |
| TC-T2-05 | A | Second `devcontainer up` on same workspace reuses same container | R-DC-01 |
| TC-T2-06 | B | Custom Dockerfile: `devcontainer up` builds image then starts container | R-DC-02 |
| TC-T2-07 | B | Custom Dockerfile: marker file from RUN step present in container | R-DC-02, R-DC-04 |
| TC-T2-07b | B | Custom Dockerfile: `curl` installed by `apt-get` in Dockerfile works | R-DC-02 |
| TC-T2-08 | D | `devcontainer up` with `postCreateCommand` runs and exits 0 | R-DC-01 |
| TC-T2-08b | D | `postCreateCommand` ran: marker file exists inside container | R-DC-01 |
| TC-T2-09 | D | `devcontainer down` stops the container cleanly | R-DC-01 |
| TC-T2-10 | C | Features: `devcontainer up` with node feature, exits 0 | R-DC-03 |
| TC-T2-10b | C | Features: `node --version` works inside container | R-DC-03, R-DC-04 |
| TC-T2-10c | C | Features: `npm --version` works inside container | R-DC-03, R-DC-04 |

**Fixture projects** (`test/fixtures/`):

```
test/fixtures/
  dc-prebuilt/    .devcontainer/devcontainer.json  { "image": "ubuntu:22.04" }
  dc-dockerfile/  .devcontainer/{devcontainer.json,Dockerfile}  (installs curl, marker file)
  dc-features/    .devcontainer/devcontainer.json  { features: { node:lts } }
  dc-postcreate/  .devcontainer/devcontainer.json  { postCreateCommand: "touch /tmp/..." }
```

---

### T3 — IDE Integration (Manual)

See issue #91. Must be performed by a human with VS Code installed. Not automatable.

---

## Part 5 — Open Issues

| # | Title | Requirement | Priority |
|---|---|---|---|
| #91 | VS Code Remote-Containers full IDE integration test | R-DC-05 | Medium |
| #92 | docker build end-to-end with features + custom Dockerfile | R-SH-07, R-DC-02, R-DC-03 | High |
| #93 | Named volume backing store persistence across VM restart | R-VM-05 | High |
| #74 | Dynamic virtiofs host-directory sharing | R-SH-05 (path translation) | Low |
| #95 | T1 gap tests — ✅ **Done** (TC-T1-30..TC-T1-41 incorporated into rewritten harness) | All T1 gaps | — |
| #96 | Build `test-devcontainer-e2e.sh` (T2 integration harness) | R-DC-01 through R-DC-04 | High |
