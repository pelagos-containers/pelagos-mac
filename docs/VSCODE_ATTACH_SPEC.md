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
issues this sequence of Docker CLI calls (in order):

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
6.  (docker exec -i <name> bash)    -- pipe server install script over stdin
        The install script (run INSIDE the container):
        - mkdir -p ~/.vscode-server/bin
        - curl -fsSL <VS Code CDN URL> | tar xz -C ~/.vscode-server/bin
        - mv vscode-server-linux-arm64 ~/.vscode-server/bin/<commit>
        - chmod +x ~/.vscode-server/bin/<commit>/node
7.  docker exec -d <name> \         -- start VS Code server (detached)
        ~/.vscode-server/bin/<commit>/node \
        ~/.vscode-server/bin/<commit>/out/server-main.js \
        --start-server \
        --host=127.0.0.1 \
        --port=0 \
        --connection-token=... \
        --without-browser-env-var
8.  docker exec <name> cat ~/.vscode-server/.pid-... -- wait for port
9.  (TCP connect from host to forwarded port)
10. docker exec -it <name> bash     -- open terminal in VS Code
11. docker exec -e ... <name> <postCreateCommand> (if defined)
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
| pelagos-mac exec stdin BufReader | **Applied** (not merged) | branch fix/devcontainer-suite-isolation |

---

## How to Run

```bash
bash scripts/test-vscode-attach.sh [--debug] [--layer 0..6]
```

Run this and fix every failure before opening VS Code.
Only open VS Code when the script reports 0 FAIL.
