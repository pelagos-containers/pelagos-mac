# VS Code Devcontainer Attach — Requirements Spec

This document specifies, layer by layer, what every component of the
pelagos-mac stack must provide for VS Code "Reopen in Container" to
succeed. Each requirement (R-IDE-NN) maps to one or more test cases
in `scripts/test-vscode-attach.sh`.

**Governing rule:** every requirement must be verifiable *without opening
VS Code*. The test script is the verification harness; VS Code is the
final smoke check after the script passes.

---

## What VS Code Does (Exact Sequence)

When the user clicks "Reopen in Container", VS Code Remote-Containers
(extension `ms-vscode-remote.remote-containers`) issues this sequence:

```
1.  docker info
2.  docker version --format {{.Server.Version}}
3.  docker inspect <name>           -- check if container already exists
4a. docker run -d --name <name> \   -- if not exists: create
        -v <workspace>:/workspaces/<name> \
        [-v <other-mounts>] \
        [-e VAR=VAL …] \
        [-p host:container …] \
        --label devcontainer.local_folder=<workspace> \
        [--entrypoint /bin/sh] \
        <image> \
        -c "while sleep 1000; do :; done"
    (or the command from devcontainer.json)
4b. docker start <name>             -- if exists-but-stopped: restart
5.  docker inspect <name>           -- re-check state after run/start
```

Steps 6–9 happen inside an **8-second `resolveAuthority` window**. If all
three exec sessions below do not complete within 8 s, VS Code aborts with
`"resolveAuthority"` error.

```
6.  docker exec -i <name> /bin/sh   -- Session 1: setup echo (exits ~40 ms)
        stdin: echo "New container started…" and exit

7.  docker exec -i -e VSCODE_REMOTE_CONTAINERS_SESSION=… \
        <name> /bin/sh              -- Session A: shell server (persists)
        stdin: sequence of sentinel-wrapped shell commands:
          echo -n ␄; ( cmd ); echo -n ␄$?␄; echo -n ␄ >&2
        stdout: ␄{output}␄{exitCode}␄ for each command
        stderr: ␄ for each command
        Used for: process scan (/proc), environment setup, writing
        vscode-remote-containers-server-N.js to /tmp

8.  docker exec -i <name> /bin/sh   -- Session B: muxrpc (persists)
        stdin:  set -e ; echo -n ␄ >&2 ;
                REMOTE_CONTAINERS_SOCKETS='[]' REMOTE_CONTAINERS_IPC='' \
                <node> <vscode-remote-containers-server-N.js> ; exit
        stderr: ␄ (sentinel signals server is alive)
        stdout/stdin: muxrpc frames (see §muxrpc Protocol below)
        Flow:
          server → client: connected() [async]
          server → client: ready()     [async]
          client → server: exec({cmd, args, env})  [async]  → processId
          client → server: stdout(processId)       [source] → stream
          client reads stdout until "Extension host agent listening on PORT"

9.  (VS Code connects to the agent via port forwarding)

10. docker exec -it <name> bash     -- open terminal in VS Code
11. docker exec -e … <name> <postCreateCommand> (if defined)
```

### Timing Budget (observed with VS Code Insiders 0.450.0, linux-arm64 server)

| Phase | Duration | Cumulative |
|---|---|---|
| Steps 1–5 (inspect, run) | ~1.7 s | 1.7 s |
| Step 6 (setup echo exec) | ~40 ms | 1.74 s |
| Step 7 (shell server, server install via dd\|tar) | ~2.82 s | 4.56 s |
| Step 8 sentinel | ~15–30 ms | 4.59 s |
| Step 8 connected()+ready() | ~30 ms | 4.62 s |
| Step 8 exec() call + response | ~10 ms | 4.63 s |
| Step 8 server-main.js startup + port report | ~60–100 ms | 4.73 s |
| **Margin before 8 s timeout** | **~3.27 s** | — |

The server install step (dd\|tar 74 MB at ~26 MB/s) is the dominant cost.
It is skipped if the server is already installed.

---

## muxrpc Wire Protocol (Session B)

Session B uses **muxrpc** over **packet-stream-codec** over the exec stdio
pipe. The protocol was reverse-engineered from
`~/.vscode-insiders/extensions/ms-vscode-remote.remote-containers-N/dist/common/remoteContainersServer.js`
and confirmed with `scripts/test-mrpc-exec.js`.

### packet-stream-codec Frame Format

Every frame is a 9-byte header followed by the body:

```
Byte 0:      flags = (stream << 3) | (end << 2) | type
               type:   0 = raw buffer  (Fr)
                       1 = UTF-8 string (_r)
                       2 = JSON         (Or)
               end:    bit 2 (0x04)  — this is the last frame for this reqId
               stream: bit 3 (0x08)  — this is a stream frame (not one-shot)
Bytes 1–4:   body length (big-endian uint32)
Bytes 5–8:   request ID  (big-endian int32)
               positive → request from sender
               negative → response to the other side's request
```

**Common flag values:**

| Hex  | Meaning |
|------|---------|
| `0x02` | JSON, non-stream, non-end → **async request or response** |
| `0x06` | JSON + end, non-stream → **async end/response** |
| `0x0A` | JSON + stream, non-end → **source subscription, stream data chunk, or demand** |
| `0x0E` | JSON + stream + end → **stream end or abort** |
| `0x08` | buffer + stream, non-end → **binary stream data chunk** |
| `0x09` | string + stream, non-end → **string stream data chunk** |

> **Common mistake:** confusing `F_STREAM=0x02` (seen in naive implementations
> that treat bit 1 as "stream") with the actual stream bit `0x08`. Using `0x02`
> for a source subscription causes the server to treat the call as async and
> return `"no async:<method>"` since the method is not in the async manifest.

### muxrpc Manifests

The server (`remoteContainersServer.js`) exposes two sets of methods:

```javascript
// server → client (server initiates these)
var yi = { rpc: "async", connected: "async", ready: "async" };

// client → server (VS Code initiates these)
var vi = {
  exec:      "async",   // spawn a process → returns processId (integer)
  stdin:     "sink",    // write to process stdin
  stdout:    "source",  // read from process stdout (streaming)
  stderr:    "source",  // read from process stderr (streaming)
  exit:      "async",   // wait for process exit → exit code
  terminate: "async",   // kill process
  dispose:   "async",
  ptyExec:   "async",
  ptyIn:     "sink",
  ptyOut:    "source",
};
```

### Frame-by-Frame Session B Flow

```
← server  reqId=+1 flags=0x02  {"name":["connected"],"args":[]}
→ client  reqId=-1 flags=0x06  [false,null]

← server  reqId=+2 flags=0x02  {"name":["ready"],"args":[]}
→ client  reqId=-2 flags=0x06  [false,null]

→ client  reqId=+1 flags=0x02  {"name":["exec"],"args":[{cmd,args,env}]}
← server  reqId=-1 flags=0x02  0                    ← processId (integer)

→ client  reqId=+2 flags=0x0A  {"name":["stdout"],"type":"source","args":[0]}
→ client  reqId=+2 flags=0x0A  null                 ← first demand

← server  reqId=-2 flags=0x08  <binary stdout chunk>
→ client  reqId=+2 flags=0x0A  null                 ← demand for next chunk

← server  reqId=-2 flags=0x08  <binary stdout chunk>
   … repeat until "Extension host agent listening on PORT" seen …
```

**Key call conventions:**

- `exec()` is **async**: send `flags=0x02` (non-stream), receive processId as
  `flags=0x02` (non-stream, non-end). The response body is the integer processId
  directly, NOT `[false, processId]`.

- `stdout()` is a **source**: send `flags=0x0A` (JSON + stream bit) with
  `type:"source"` in the body. The `type` field is **required** — the server
  reads it to dispatch the stream type; omitting it yields
  `"unsupported stream type: undefined"`.

- **Pull-stream demand**: after the subscription frame, send a demand with
  `flags=0x0A, body=null, reqId=+N` (same reqId as the subscription).
  `body=null` means "give me the next item"; `body=true` would abort the stream.
  After each received data chunk, send another demand.

- Server sends stdout chunks with `flags=0x08` (raw buffer + stream) or
  `flags=0x09` (string + stream); both decode to UTF-8 text.

### exec() Arguments

```javascript
{
  cmd:  "/root/.vscode-server-insiders/bin/<commit>-insider/node",
  args: [
    "/root/.vscode-server-insiders/bin/<commit>-insider/out/server-main.js",
    "--start-server",
    "--host=127.0.0.1",
    "--port=0",
    "--accept-server-license-terms",
    "--connection-token=<token>",
    "--without-browser-env-var",
    "--telemetry-level", "off",
  ],
  env: {
    HOME: "/root",
    PATH: "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
  },
}
```

Note: paths use the **`-insider` suffix** (`bin/<commit>-insider/`) for VS Code
Insiders. Stable VS Code uses `bin/<commit>/` without any suffix.

### Success Indicator

The server writes to stdout (not stderr):

```
Server bound to 127.0.0.1:<PORT> (IPv4)
Extension host agent listening on <PORT>
```

VS Code reads these lines from the muxrpc stdout stream and uses `<PORT>` to
establish the extension host connection.

### Test Scripts

| Script | What it tests |
|---|---|
| `scripts/test-mrpc-handshake.js` | sentinel + connected() + ready() only |
| `scripts/test-mrpc-exec.js` | full flow: sentinel → handshake → exec() → stdout() → port |

```bash
node scripts/test-mrpc-exec.js [container-name]
# Expects: "[PASS] VS Code server listening on port NNNNN"
```

---

## Layer 0: Shim Baseline (R-IDE-01)

The `pelagos-docker` shim must pass a Docker API sanity check before
VS Code will attempt anything else.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-01a | `docker info` exits 0 and returns JSON with `ServerVersion` field | TC-VS-01 |
| R-IDE-01b | `docker version --format {{.Server.Version}}` returns a bare version string | TC-VS-02 |
| R-IDE-01c | `docker ps -a` exits 0 | TC-VS-03 |

---

## Layer 1: Container Lifecycle (R-IDE-02)

VS Code creates a container, and may restart it across sessions.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-02a | `docker run -d --name X <image> sleep infinity` exits 0 | TC-VS-10 |
| R-IDE-02b | `docker inspect X` returns JSON with `State.Status = "running"` | TC-VS-11 |
| R-IDE-02c | `docker inspect X` returns `Mounts` array listing bind volumes | TC-VS-12 |
| R-IDE-02d | `docker stop X` exits 0; inspect shows `State.Status = "exited"` | TC-VS-13 |
| R-IDE-02e | `docker start X` exits 0; inspect shows `State.Status = "running"` | TC-VS-14 |
| R-IDE-02f | `docker rm X` exits 0 after stop | TC-VS-15 |

---

## Layer 2: Container Environment (R-IDE-03)

The running container must have a usable POSIX environment. These are
the specific things the VS Code server checks at startup.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-03a | `/etc/hosts` exists and contains `127.0.0.1 localhost` | TC-VS-20 |
| R-IDE-03b | `/etc/resolv.conf` exists and contains a `nameserver` line | TC-VS-21 |
| R-IDE-03c | `getent hosts localhost` resolves to `127.0.0.1` | TC-VS-22 |
| R-IDE-03d | `getent hosts google.com` resolves (external DNS works) | TC-VS-23 |
| R-IDE-03e | Outbound TCP/443 to `update.code.visualstudio.com` succeeds | TC-VS-24 |
| R-IDE-03f | `HOME` env var is set (default `/root` for root user) | TC-VS-25 |
| R-IDE-03g | `/root` is writable by the container process | TC-VS-26 |
| R-IDE-03h | `/tmp` is writable | TC-VS-27 |
| R-IDE-03i | Container can bind a TCP port on 127.0.0.1 | TC-VS-28 |

---

## Layer 3: Exec Stdin/Stdout (R-IDE-04)

VS Code's server install and several lifecycle operations pipe data
through `docker exec -i` stdin. This was broken (BufReader fix); must
be confirmed working.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-04a | `echo hello \| docker exec -i X cat` prints `hello` | TC-VS-30 |
| R-IDE-04b | 1 MB binary piped via exec -i arrives intact (byte count matches) | TC-VS-31 |
| R-IDE-04c | 64 MB binary piped via exec -i arrives intact (byte count matches) | TC-VS-32 |
| R-IDE-04d | `docker exec -i X bash` receives heredoc script over stdin, runs it | TC-VS-33 |
| R-IDE-04e | `docker exec -d X <long-running-command>` returns immediately (not blocked) | TC-VS-34 |

---

## Layer 4: VS Code Server Install (R-IDE-05)

The VS Code server tarball for `linux-arm64` must download, extract,
and be executable inside the container.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-05a | Container can `curl` the VS Code CDN (network + TLS + DNS) | TC-VS-40 |
| R-IDE-05b | VS Code server tarball downloads inside container (≈70 MB) | TC-VS-41 |
| R-IDE-05c | Tarball extracts cleanly; `node` binary is present and executable | TC-VS-42 |
| R-IDE-05d | `node --version` inside container returns a version string | TC-VS-43 |
| R-IDE-05e | glibc version inside container is ≥ 2.28 (VS Code server minimum) | TC-VS-44 |

---

## Layer 5: VS Code Server Startup (R-IDE-06)

The server must start and report its listening port.

| ID | Requirement | Test |
|---|---|---|
| R-IDE-06a | Server starts without crashing (exit code stays non-zero within 5 s) | TC-VS-50 |
| R-IDE-06b | Server writes a port number to stdout or its PID file within 10 s | TC-VS-51 |
| R-IDE-06c | `nc`/`curl` to `127.0.0.1:<port>` inside container returns HTTP | TC-VS-52 |

---

## Layer 6: Port Forwarding (R-IDE-07)

The port the server binds inside the container must be reachable from
the macOS host. (pelagos-mac handles port forwarding via `pelagos run -p`.)

| ID | Requirement | Test |
|---|---|---|
| R-IDE-07a | Port specified in `docker run -p host:container` is forwarded | TC-VS-60 |
| R-IDE-07b | `curl http://127.0.0.1:<host-port>/` from macOS returns HTTP | TC-VS-61 |

---

## Known Blockers / Fixed Issues

| Issue | Status | Fix version |
|---|---|---|
| pelagos#120 — `/etc/hosts` absent | **CLOSED** | pelagos v0.57.0 |
| pelagos-mac exec stdin BufReader | **CLOSED** | branch fix/devcontainer-suite-isolation |
| pelagos#121 — exec-into missing PID namespace join | **OPEN — CURRENT BLOCKER** | TBD |

### Current Blocker: exec-into PID namespace (pelagos#121)

VS Code's `resolveAuthority` runs `aT()` which proc-scans the container and ends with
`readlink /proc/self/ns/mnt 2>/dev/null`. This fails in pelagos containers because
exec-into processes are **not in the container's PID namespace**:

```bash
# Inside a pelagos container via exec-into:
ls -la /proc/self   # → 0-byte dangling symlink (points to non-existent /proc/<pid>)
ls /proc/[0-9]*     # → only /proc/1 (container init), exec'd process invisible
```

**Why it matters:** `aT()` uses the shell server's `exec()` method. When the command
exits with code 1, the shell server rejects the promise. This propagates through
`Ioe()` → `Rl()` → `resolveAuthority()`, which fails with
`{"code":"NotAvailable","detail":true}` at approximately T+6.8s (before the 8s timeout).

**All other layers work correctly** — the muxrpc protocol, server install, port
forwarding mechanism via `docker exec` node tunnel — none of this can be reached
because `resolveAuthority` fails first.

**Fix required in pelagos:** `exec_into` must call `setns(pid_ns_fd, CLONE_NEWPID)`
before the fork/exec, joining the container's PID namespace so exec'd processes appear
in `/proc` and `/proc/self` resolves correctly.

---

## How to Run

```bash
bash scripts/test-vscode-attach.sh [--debug] [--layer 0..6]
```

Run this and fix every failure before opening VS Code.
Only open VS Code when the script reports 0 FAIL.
