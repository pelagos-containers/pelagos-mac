# VS Code devcontainer with pelagos-mac

This guide explains how to use VS Code's Dev Containers extension with pelagos-mac as the container backend.

## Prerequisites

- pelagos-mac built and signed (`cargo build --release -p pelagos-mac && bash scripts/sign.sh`)
- VM image built (`bash scripts/build-vm-image.sh`)
- VS Code with the [Dev Containers](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) extension
- `pelagos-docker` on your PATH

## Configure VS Code to use pelagos-docker

Set the Docker executable path in VS Code settings:

```json
{
  "dev.containers.dockerPath": "/path/to/pelagos-docker"
}
```

Or set the environment variable before launching VS Code:

```sh
export DOCKER_HOST=""
export PATH="/path/to/pelagos-mac/bin:$PATH"
```

## Minimal devcontainer.json

```json
{
  "name": "My Project",
  "image": "alpine:latest",
  "mounts": [
    {
      "source": "vscode-server-${devcontainerId}",
      "target": "/root/.vscode-server",
      "type": "volume"
    }
  ]
}
```

The `vscode-server-${devcontainerId}` named volume persists the VS Code server installation
across container restarts. Because pelagos-mac uses an always-on virtiofs share for
`/var/lib/pelagos/volumes/`, this volume also survives VM restarts — the data lives on
your Mac at `~/.local/share/pelagos/volumes/vscode-server-<id>/`.

## Named volumes and persistence

pelagos named volumes are stored at:

| Location | Description |
|---|---|
| VM: `/var/lib/pelagos/volumes/<name>/` | Runtime mount point |
| Mac host: `~/.local/share/pelagos/volumes/<name>/` | Persistent backing store (virtiofs) |

The virtiofs share `pelagos-volumes` is always-on: it is configured at VM boot time and
does not require any `-v` flag. You do not need to stop and restart the VM to use volumes.

## Using container labels

devcontainer uses Docker labels to identify containers it manages:

```json
{
  "name": "My Project",
  "image": "alpine:latest",
  "runArgs": ["--label", "devcontainer.local_folder=/Users/you/myproject"]
}
```

Labels are stored natively in pelagos container state — no sidecar required.
You can inspect them with:

```sh
pelagos container inspect my-container
```

And filter containers by label:

```sh
pelagos ps --filter label=devcontainer.local_folder=/Users/you/myproject
```

## Full devcontainer.json example

```json
{
  "name": "My Rust Project",
  "image": "ghcr.io/pelagos-containers/pelagos-dev:latest",
  "mounts": [
    {
      "source": "vscode-server-${devcontainerId}",
      "target": "/root/.vscode-server",
      "type": "volume"
    },
    {
      "source": "cargo-registry",
      "target": "/root/.cargo/registry",
      "type": "volume"
    }
  ],
  "customizations": {
    "vscode": {
      "extensions": [
        "rust-lang.rust-analyzer"
      ]
    }
  }
}
```

## How container restart works

When VS Code reconnects to a stopped container, it calls `docker start <name>`.
pelagos-docker maps this to `pelagos start <name>`, which re-launches the container
using the same image, volumes, networks, and command that were used originally.

The restart uses a fresh overlay upper layer — writable changes from the previous
run are not preserved. Use named volumes for any state you want to persist.

## Limitations

- Bridge networking (`--network bridge`) is not supported. Use the default networking
  (smoltcp NAT relay) or loopback.
- Port forwarding is configured at VM boot time. If you need different ports, run
  `pelagos vm stop` first, then restart with the new `-p` flags.
- Interactive TTY (`docker run -it`) works via the vsock relay. Very large terminal
  resizes may be slightly delayed.
