# Guest Container Execution — Namespace Joining

## The Core Rule

**Always use direct `setns(2)` via `pre_exec` in the guest daemon, exactly as
`handle_exec_into`, `handle_cp_from`, and `handle_cp_to` do.**

Do not call `pelagos exec <container> <cmd>` as a subprocess from guest code.

---

## Why Not `pelagos exec`?

### The one remaining reason: PID namespace

`pelagos exec` explicitly skips joining the container's PID namespace (see
`src/cli/exec.rs`: `"exec: skipping PID namespace join (host PID namespace
limitation)"`). The exec'd process runs in the host (Alpine VM) PID namespace.

This is not fixable without a double-fork inside `pre_exec`: `setns(CLONE_NEWPID)`
only updates `pid_for_children`; the exec'd process itself is not inside the
new PID namespace until a subsequent `fork()`. Neither `pelagos exec` nor our
direct `setns` code currently implements this double-fork. Both have the same
PID namespace limitation for now.

For VS Code devcontainer operations (file access, script execution), running in
the host PID namespace is acceptable — these operations don't depend on the
container's PID space.

### What is NOT a reason

**The root filesystem bug is fixed in pelagos.** As of `fix(exec): join
namespaces in correct order` (`f41c212`), `pelagos exec` correctly does
`fchdir(/proc/<pid>/root) + chroot(".")` after `setns(CLONE_NEWNS)`, and
handles the P→C intermediate process case via `find_root_pid()`. The
GUEST_CONTAINER_EXEC.md claim that "pelagos exec runs in the guest root
filesystem" is no longer true.

### Why direct setns is still preferred

For `handle_cp_from` and `handle_cp_to`, we need raw binary tar data. Using
`pelagos exec tar ...` as a subprocess and capturing its stdout works
mechanically, but direct setns + spawn avoids the extra process layer in the
chain. For a privileged root component that owns the namespace-bridge role,
direct kernel API manipulation is more appropriate than routing through a
userspace CLI.

---

## Critical: `find_root_pid` for the P→C Case

`pelagos ps` shows `state.pid = P`, the intermediate process spawned by
pelagos. When a PID namespace is active, P never calls `pivot_root` — that is
done by C, P's only child (PID 1 inside the container). `/proc/P/root`
therefore points to Alpine's root, not the container's overlay.

**Always call `open_root_fd(pid)` which internally resolves P → C via
`find_root_pid()`.** Never open `/proc/<pid>/root` directly with the raw PID
from `get_container_pid()`.

```
P (state.pid from pelagos ps)
└── C (P's only child — this process called pivot_root)
      └── /proc/C/root = container's overlay rootfs  ← correct
/proc/P/root = Alpine initramfs root                  ← wrong
```

---

## Correct Namespace Join Order

After `fork()` (in `pre_exec`), before `exec()`:

1. `setns(net_fd, 0)` — network namespace
2. `setns(uts_fd, 0)` — UTS namespace
3. `setns(ipc_fd, 0)` — IPC namespace
4. `setns(pid_fd, 0)` — PID namespace (sets `pid_for_children` only; no actual effect on the exec'd process)
5. `setns(mnt_fd, 0)` — mount namespace (last: so `/proc` remains readable until here)
6. `fchdir(root_fd)` — step into container's rootfs via fd opened before fork
7. `chroot(".")` — re-anchor root dentry to container rootfs
8. `chdir("/")` — normalize CWD
9. `close(root_fd)`

Steps 6–9 are required because `setns(CLONE_NEWNS)` changes the mount
namespace but does NOT update the calling process's root dentry. Without them,
absolute paths resolve through Alpine's root dentry regardless of which mount
namespace is active.

---

## Implementation Pattern

```rust
fn handle_foo_in_container(
    writer: &mut impl Write,
    container: &str,
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
    // open_root_fd internally calls find_root_pid to resolve P → C
    let root_fd = match open_root_fd(pid) {
        Ok(f) => f,
        Err(e) => {
            for &nfd in &ns_fds { unsafe { libc::close(nfd) }; }
            send_response(writer, &GuestResponse::Error { error: format!("foo: open root fd: {}", e) })?;
            return Ok(());
        }
    };

    let mut cmd = Command::new("...");
    unsafe {
        cmd.pre_exec(move || {
            for &ns_fd in &ns_fds {
                if call_setns(ns_fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ns_fd);
            }
            if libc::fchdir(root_fd) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chroot(b".\0".as_ptr() as *const libc::c_char) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(b"/\0".as_ptr() as *const libc::c_char) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(root_fd);
            Ok(())
        });
    }

    let result = cmd.spawn();
    // Close parent's copies — always, even on spawn error.
    for &ns_fd in &ns_fds { unsafe { libc::close(ns_fd) }; }
    unsafe { libc::close(root_fd) };

    /* handle result, send GuestResponse */
    Ok(())
}
```

See `handle_exec_into`, `handle_cp_from`, `handle_cp_to` in
`pelagos-guest/src/main.rs` for complete examples.
