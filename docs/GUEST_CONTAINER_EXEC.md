# Guest Container Execution — Namespace Joining

## The Core Rule

**Never use `pelagos exec <container> <cmd>` inside the guest daemon to run commands
inside a container.** It silently runs the command in the guest's root filesystem
instead.

**Always use `setns(2)` directly via `pre_exec` plus `fchdir`+`chroot` into the
container rootfs, exactly as `handle_exec_into` does.**

This is not a workaround — it is the correct approach for privileged in-VM code.
The guest daemon runs as root with `CAP_SYS_ADMIN` and should directly manipulate
namespaces rather than routing through an unprivileged CLI.

---

## Why `pelagos exec` Silently Fails Inside the VM

`pelagos exec` is designed for **rootless containers** (no root privilege, no
`CAP_SYS_ADMIN`). Its namespace-joining logic has a fundamental constraint:

**`setns(CLONE_NEWPID)` only updates `pid_for_children`.**
A subsequent `fork()` is required to actually place a new process inside the PID
namespace. `exec()` alone is insufficient.

pelagos handles this at container creation time with a double-fork (the "intermediate
process P" in container.rs). But `pelagos exec` runs its namespace joins in `pre_exec`
— the right place for USER/MOUNT/NET/UTS, but too late to redo the PID namespace
double-fork. The result: `pelagos exec` **always skips the PID namespace join**
(see `exec.rs`: `"exec: skipping PID namespace join (host PID namespace limitation)"`).

The spawned process runs in the guest's root PID and mount namespaces. The failure
is silent: exit 0, wrong filesystem, no indication anything went wrong.

### Could this be fixed in pelagos?

Yes, in principle. The pattern used by `nsenter(1)` works: in `pre_exec`, after
`setns(CLONE_NEWPID)`, fork a grandchild, exec in the grandchild, wait in the child,
exit. That would give `pelagos exec` proper PID namespace entry without root. It is
non-trivial but well-understood. Filing an upstream issue is appropriate if rootless
`exec` ever needs to be a first-class feature of pelagos.

For pelagos-guest, this is not a concern — the guest runs as root and does not have
the rootless constraint that makes this hard.

---

## Why Direct `setns` + `chroot` Is the Right Approach for pelagos-guest

The guest daemon runs as root (uid 0, `CAP_SYS_ADMIN` in the initial user namespace).
It is the privileged component whose job is to bridge the macOS host and the
container runtime. Direct namespace manipulation is correct here:

- `pre_exec` runs in the child after `fork()` — the right point in the PID namespace
  double-fork sequence. We have the capabilities; `setns(CLONE_NEWPID)` + fork works.
- Using `pelagos exec` as a subprocess would add unnecessary indirection, lose stdout
  binary data (piped through JSON text encoding), and fail anyway due to the PID ns
  limitation above.
- `ns_fds` is `[i32; 5]` — a `Copy` type. The `move` closure gets a copy; the parent
  retains the originals for cleanup after spawn.
- Namespace join order matters: `[net, uts, ipc, pid, mnt]`. Mount last so `/proc`
  remains readable until the PID join completes.
- **pelagos uses `chroot` (not `pivot_root`) for rootfs isolation.** After `setns(mnt)`
  the mount namespace "/" is still the guest root, not the container's rootfs.
  `open_root_fd(pid)` opens `/proc/<pid>/root` in the parent (before fork), then
  `pre_exec` calls `fchdir(root_fd)` + `chroot(".")` + `chdir("/")` after all `setns`
  calls to land inside the container's actual filesystem.

---

## Implementation Pattern

```rust
// 1. Get the container's PID from `pelagos ps --all`
let pid = get_container_pid(container)?;

// 2. Open namespace fds in the parent (before fork)
let ns_fds = open_ns_fds(pid)?;

// 3. Open the container's root dir via /proc/<pid>/root (before fork)
let root_fd = open_root_fd(pid)?;

// 4. Build the command; enter namespaces and chroot in the child (after fork, before exec)
unsafe {
    cmd.pre_exec(move || {
        for &ns_fd in &ns_fds {
            if call_setns(ns_fd) < 0 { return Err(std::io::Error::last_os_error()); }
            libc::close(ns_fd);
        }
        // Enter the container's rootfs (pelagos uses chroot, not pivot_root).
        if libc::fchdir(root_fd) < 0 { return Err(std::io::Error::last_os_error()); }
        if libc::chroot(b".\0".as_ptr() as *const libc::c_char) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::chdir(b"/\0".as_ptr() as *const libc::c_char);
        libc::close(root_fd);
        Ok(())
    });
}

// 5. Spawn (or .output()); close parent copies unconditionally
let result = cmd.spawn()...;
for &ns_fd in &ns_fds { unsafe { libc::close(ns_fd) }; }
unsafe { libc::close(root_fd) };
```

See `handle_exec_into`, `handle_cp_from`, `handle_cp_to` in `pelagos-guest/src/main.rs`
for complete examples.

---

## What Can and Cannot Be Done

### Can Do

| Operation | Mechanism |
|---|---|
| `docker exec` — interactive or piped | `handle_exec_into` — direct setns |
| `docker cp container→host` | `handle_cp_from` — direct setns + `tar -cC` |
| `docker cp host→container` | `handle_cp_to` — direct setns + `tar -xC` |
| Any future "run inside container" operation | Direct setns pattern |

### Will Silently Give Wrong Results

| Operation | Why |
|---|---|
| `pelagos exec` subprocess from guest code | Skips PID ns; runs in guest root filesystem |
| `spawn_and_stream` for container-internal ops | Same — calls `pelagos` CLI |

`pelagos exec <container> cat /etc/passwd` called as a subprocess inside the guest
daemon will: succeed (exit 0), return `/etc/passwd` from the **guest's own
filesystem**, not the container's — with no error or warning.

---

## Template for New Container-Internal Handlers

```rust
fn handle_foo_in_container(
    writer: &mut impl Write,
    container: &str,
    /* ... */
) -> std::io::Result<()> {
    let pid = match get_container_pid(container) {
        Ok(p) => p,
        Err(e) => {
            send_response(writer, &GuestResponse::Error { error: format!("foo: {}", e) })?;
            return Ok(());
        }
    };
    let ns_fds = match open_ns_fds(pid) {
        Ok(f) => f,
        Err(e) => {
            send_response(writer, &GuestResponse::Error { error: format!("foo: open ns fds: {}", e) })?;
            return Ok(());
        }
    };

    let mut cmd = Command::new("...");
    /* configure args, stdio */
    unsafe {
        cmd.pre_exec(move || {
            for &ns_fd in &ns_fds {
                if call_setns(ns_fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ns_fd);
            }
            Ok(())
        });
    }

    let result = cmd.spawn() /* or .output() */;
    // Close parent's copies of ns_fds — always, even on spawn error.
    for &ns_fd in &ns_fds { unsafe { libc::close(ns_fd) }; }

    /* handle result, send GuestResponse */
}
```
