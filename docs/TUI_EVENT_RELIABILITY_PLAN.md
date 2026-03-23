# TUI Container Event Reliability Plan

**Status:** IMPLEMENTED — all fixes shipped in PR #161
**Related issues:** [#156 (Epic: async guest daemon + push-based TUI events)](https://github.com/skeptomai/pelagos-mac/issues/156), [#158 (guest Subscribe)](https://github.com/skeptomai/pelagos-mac/issues/158), [#159 (TUI subscribe runner)](https://github.com/skeptomai/pelagos-mac/issues/159), [#149 (TUI Epic)](https://github.com/skeptomai/pelagos-mac/issues/149)

---

## Problem Statement

The TUI's subscribe-based event stream is unreliable. Containers — especially short-lived ones — fail to appear. The root cause is not one bug but a compounding set of architectural gaps across a 5-hop, 14-step communication chain that was never validated end-to-end before integration into the TUI.

We have been fixing symptoms. This document lays out the full chain, every known failure mode, what we do not yet know, a test harness to close the knowledge gaps, and the architectural changes required to make delivery reliable.

---

## 1. Architecture: All Players and Communication Paths

### 1.1 Players

| # | Player | Location | Role |
|---|--------|----------|------|
| P1 | **pelagos** (Linux runtime) | Inside VM | Creates/updates container state files; runs containers |
| P2 | **state files** | `/run/pelagos/containers/*/state.json` inside VM | Ground truth for container state |
| P3 | **event\_poller\_loop** | `pelagos-guest`, spawned thread | Reads P2 every 250 ms, diffs against previous snapshot, calls broadcast |
| P4 | **subscriber registry** | `pelagos-guest`, global `OnceLock<Arc<Mutex<Vec<SyncSender<GuestEvent>>>>>` | Holds one sender per active Subscribe connection |
| P5 | **handle\_subscribe** | `pelagos-guest`, per-connection thread | Registers in P4, sends initial Snapshot, loops forwarding from channel to vsock |
| P6 | **vsock fd** | `pelagos-guest` ↔ Apple Virtualization framework | Transport between VM and host |
| P7 | **proxy thread** | `pelagos-mac daemon` | Accepts Unix socket connection, calls `vm.connect_vsock()`, bidirectionally copies bytes between Unix socket and vsock fd via two `std::io::copy` threads |
| P8 | **vm.sock** | macOS filesystem | Unix domain socket served by the daemon; one connection per CLI invocation |
| P9 | **`pelagos subscribe`** | macOS, subprocess of TUI subscription thread | Sends `GuestCommand::Subscribe`, reads NDJSON from vsock, prints to stdout |
| P10 | **TUI subscription thread** | `pelagos-tui` | Spawns P9, reads its stdout line by line, sends parsed `SubscriptionMsg` to TUI main loop via `mpsc::Sender` |
| P11 | **TUI main loop** | `pelagos-tui` | Drains the mpsc channel every 250 ms, calls `apply_subscription`, calls `terminal.draw` |

### 1.2 Event Delivery Path (happy path)

```
P1 pelagos runtime
  └─ writes/updates state.json (P2)
       └─ P3 event_poller_loop reads dir every 250 ms
            └─ diff detects change
                 └─ broadcast_events: P4 registry locked, try_send to each P4 sender
                      └─ P5 handle_subscribe: rx.recv() unblocks
                           └─ P5 send_event: libc::write to P6 vsock fd
                                └─ P7 daemon proxy (vsock→Unix copy thread): io::copy
                                     └─ P8 vm.sock → P9 pelagos subscribe: reader.read_line
                                          └─ P9 print! + stdout.flush()
                                               └─ P10 subscription thread: reader.read_line [BLOCKING]
                                                    └─ serde_json::from_str::<SubscriptionMsg>
                                                         └─ tx.send(msg)
                                                              └─ P11 main loop: sub_rx.try_recv()
                                                                   └─ apply_subscription
                                                                        └─ terminal.draw
```

**14 distinct steps.** A silent failure at any step produces the same symptom: container does not appear in the TUI.

### 1.3 Subscription Lifecycle Path

```
TUI starts
  └─ start_subscription_thread spawned
       └─ run_subscription: spawn P9 (pelagos subscribe subprocess)
            └─ P9: is_daemon_alive() check → connect_or_exit → subscribe_command
                 └─ sends {"cmd":"subscribe"}\n to P7
                      └─ P7 proxy thread spawned, vm.connect_vsock()
                           └─ guest accepts vsock, handle_connection dispatches Subscribe
                                └─ P5 handle_subscribe:
                                     (a) push tx to P4 registry
                                     (b) read_container_states() → send Snapshot
                                     (c) loop: rx.recv() → send_event
```

---

## 2. Known Failure Modes

### F1: State File Semantics Are Unknown

**What we do not know:** Does `pelagos` (the Linux runtime, P1) delete `state.json` when a container exits, or does it keep it with `"status": "exited"`? Does it write the file before the container process starts, or after? What fields are stable across pelagos versions?

**Why it matters:** The entire event poller (P3) depends on state files existing and being stable. If pelagos deletes state files on exit, containers that live less than 250 ms will never be seen by the poller — they appear and disappear between polls. If the file is written after the container is already running, P3 may see `"running"` only briefly before `"exited"` (or miss it entirely).

**Risk level: CRITICAL — unverified assumption.**

### F2: 250 ms Poll Gap for Short-Lived Containers

**Mechanism:** P3 polls every 250 ms. A container that starts and exits in < 250 ms will be seen by P3 only if its state file still exists at the next poll cycle. If F1's answer is "state files are deleted on exit," such a container is invisible. If the answer is "kept as exited," then `diff_container_states` generates `ContainerStarted{status:"exited"}`.

**TUI handling:** With `show_all=true`, `ContainerStarted{status:"exited"}` IS added to the container list. With `show_all=false`, it is silently dropped (filter: `container.status == "running"`). So even if the event is delivered, it may not be displayed.

**Risk level: HIGH** if containers shorter than 250 ms are common.

### F3: `read_line` Blocks — Generation Check Is Delayed

**Mechanism:** In `run_subscription`, the subscription thread calls `reader.read_line(&mut line)` which blocks until P9 produces output. If no container events are happening, this blocks indefinitely. When the user presses `a` (toggling show\_all) or switches profiles, the generation counter is bumped — but `run_subscription` will not notice until the *next event arrives from the guest*. The reconnect is delayed by however long it takes for the next event.

**During this delay:** The new generation is set but the old `pelagos subscribe` process is still running with the old generation's view of the world. No reconnect happens. The TUI is effectively frozen for show\_all changes until an unrelated container event triggers the generation check.

**Note:** The `child.wait()` deadlock (where the pipe read end was still open, so SIGPIPE never arrived) was a separate but related bug that has been partially fixed. The generation check delay is the remaining issue.

**Risk level: HIGH** — directly causes "nothing shows up after pressing 'a'."

### F4: Subscriber Permanently Dropped on Full Channel

**Mechanism in `broadcast_events`:**
```rust
subs.retain(|tx| events.iter().all(|e| tx.try_send(e.clone()).is_ok()));
```
`try_send` returns `Err(Full)` when the 256-slot channel is at capacity. `retain` removes the subscriber permanently. Once removed, `tx` is dropped. `rx.iter()` in P5 returns `None`. P5's loop exits. P5 returns. The vsock fd is closed. P9 reads EOF. P9 exits. P10's `read_line` gets EOF. P10 exits `run_subscription`. P10 sends `Disconnected`. TUI reconnects.

**Triggering condition:** P5 is blocked writing to the vsock (P6) — e.g., because the daemon proxy or P9 is slow to consume — while P3 keeps broadcasting. With 250 ms interval and 256 capacity, this requires P5 to be blocked for ≥ 64 seconds. Unlikely under normal operation but **silent** when it does occur.

**Secondary bug:** `all()` short-circuits. If the first `try_send` fails, subsequent events in the same broadcast batch are never sent to the subscriber (even if it could have accepted them). The subscriber is removed based on one failed send.

**Risk level: MEDIUM** — unlikely but undetectable when it fires.

### F5: No Subscription Liveness Detection

**Mechanism:** There is no heartbeat on the subscription stream. If the vsock connection drops silently (e.g., due to VM network stack reset, macOS socket GC, or a daemon bug), P10 will block in `read_line` indefinitely. The TUI appears live (renders normally) but no events arrive. The modeline's "↻ Xs ago" counter climbs without limit but there is no automatic recovery.

A related case: if P3 (`event_poller_loop`) crashes (panic, OOM), no events are ever broadcast. P5's `rx.recv()` blocks forever. The connection appears alive. No reconnect.

**Risk level: MEDIUM** — manifests as "TUI appears to work but shows stale data."

### F6: Reconnect Gap Event Loss

**Mechanism:** When a reconnect is triggered (generation change, or P9 exiting), there is a window between:
- (a) the old subscriber's `tx` being removed from P4, and
- (b) the new subscriber's `tx` being registered in P4 (in the next P5 `handle_subscribe` call)

During this window, events broadcast by P3 go to nobody. If a container starts and exits entirely within this window, no `ContainerStarted` or `ContainerExited` event is ever delivered.

**Mitigation in current design:** The new `handle_subscribe` sends a fresh Snapshot that reflects the state at reconnect time. If the container's state file still exists (F1 question), it appears in the Snapshot even if the events were missed.

**Risk level: MEDIUM** — window is typically < 500 ms but cannot be eliminated in a polling design.

### F7: `show_all=false` Silently Drops Exited Containers from Events

**Mechanism:** In `apply_subscription`, `ContainerStarted` is only added to the container list if `self.show_all || container.status == "running"`. A fast container that arrives as `ContainerStarted{status:"exited"}` is silently ignored when `show_all=false`. This is intentional for the "running-only" view but means the user may not realize a container ran and exited.

**Risk level: LOW** — by design, but worth documenting.

---

## 3. What We Do Not Know (Open Questions)

| Q# | Question | Why It Matters |
|----|----------|----------------|
| Q1 | Does `pelagos` keep state files after container exit? | If no, sub-250ms containers are invisible. Determines whether F2 is fundamental or fixable at our level. |
| Q2 | When exactly does `pelagos` write the state file? Before the container process starts, after, or asynchronously? | Determines the minimum latency before P3 can detect a new container. |
| Q3 | What is the format and field set of `state.json`? Are fields stable across pelagos versions? | We are parsing it with a manually crafted field extraction; any field rename breaks silently. |
| Q4 | Is there a `pelagos events` or equivalent command for real-time container lifecycle events? | Would allow the guest to subscribe to pelagos natively rather than polling. |
| Q5 | Does `pelagos ps --all` hold a global runtime lock? | Determines whether we can safely call it from P3 for ground-truth verification. |

---

## 4. Test Harness

**Goal:** Answer Q1–Q5 definitively, and verify the full communication chain independently of the TUI. No TUI code should be modified during the testing phase. Tests must be reproducible, scripted, and produce a clear pass/fail for each failure mode.

### 4.1 Phase 0: State File Audit (answers Q1–Q3)

**Script: `scripts/test-state-files.sh`**

Run directly inside the VM via `pelagos exec` or `pelagos shell`:

```bash
#!/bin/bash
# Inside the VM:

STATE_DIR=/run/pelagos/containers

echo "=== State files before run ==="
ls -la $STATE_DIR/

echo "=== Starting container ==="
pelagos run alpine sh -c 'echo STARTED; sleep 2; echo EXITED' &
RUN_PID=$!

# Poll state files every 100ms for 5 seconds
for i in $(seq 1 50); do
    sleep 0.1
    echo "--- T=$(echo "scale=1; $i / 10" | bc)s ---"
    ls -la $STATE_DIR/ 2>/dev/null
    for f in $STATE_DIR/*/state.json; do
        [ -f "$f" ] && echo "$f: $(cat $f | python3 -m json.tool --no-indent 2>/dev/null || cat $f)"
    done
done

wait $RUN_PID
echo "=== State files after run exited ==="
ls -la $STATE_DIR/
for f in $STATE_DIR/*/state.json; do
    [ -f "$f" ] && echo "$f: $(cat $f)"
done

echo "=== Repeat with instant container ==="
pelagos run alpine echo hello
sleep 0.5
echo "State files after instant container:"
ls -la $STATE_DIR/
```

**Expected outputs to capture:**
- Does a state file appear? In which sub-directory?
- What is the `status` value at each 100ms step?
- Does the file remain after the container exits?
- What fields are present?

### 4.2 Phase 1: Subscription Event Delivery (standalone, no TUI)

**Script: `scripts/test-subscribe-events.sh`**

This test runs entirely on macOS and exercises the full chain from P9 down to P1, without the TUI.

```bash
#!/bin/bash
# On macOS: test subscription event delivery for containers of varying lifetimes.

set -euo pipefail

PASS=0
FAIL=0
LOG=/tmp/subscribe-events-$$.log

echo "Starting pelagos subscribe in background..."
pelagos subscribe > "$LOG" &
SUB_PID=$!

# Give subscribe time to connect and receive the initial Snapshot
sleep 2

echo "Initial snapshot:"
head -1 "$LOG"

run_test() {
    local label="$1"
    local cmd="$2"        # container command (e.g., "sleep 5")
    local expect_start=$3 # 1 = expect ContainerStarted event
    local expect_exit=$4  # 1 = expect ContainerExited event
    local timeout_s=$5    # seconds to wait after container exits

    local before_lines
    before_lines=$(wc -l < "$LOG")

    echo ""
    echo "=== $label ==="
    pelagos run alpine $cmd
    sleep "$timeout_s"

    local after_lines
    after_lines=$(wc -l < "$LOG")
    local new_events
    new_events=$(tail -n "+$((before_lines + 1))" "$LOG")

    local got_start=0
    local got_exit=0
    echo "$new_events" | grep -q '"type":"container_started"' && got_start=1
    echo "$new_events" | grep -q '"type":"container_exited"'  && got_exit=1

    local result="PASS"
    if [ "$expect_start" -eq 1 ] && [ "$got_start" -eq 0 ]; then result="FAIL (missing container_started)"; fi
    if [ "$expect_exit"  -eq 1 ] && [ "$got_exit"  -eq 0 ]; then result="FAIL (missing container_exited)"; fi

    echo "Events received: start=$got_start exit=$got_exit → $result"
    echo "New events:"
    echo "$new_events" | python3 -m json.tool --no-indent 2>/dev/null || echo "$new_events"

    if [ "$result" = "PASS" ]; then ((PASS++)); else ((FAIL++)); fi
}

# T1: Long-lived container (5s) — should definitely generate both events
run_test "T1: sleep 5 (5s)" "sleep 5" 1 1 6

# T2: Medium container (1s) — should generate both events
run_test "T2: sleep 1 (1s)" "sleep 1" 1 1 3

# T3: Short container (500ms) — tests 250ms poll gap
run_test "T3: sh -c 'sleep 0.5'" "sh -c 'sleep 0.5'" 1 1 2

# T4: Very short container (100ms) — likely to be missed by 250ms poller
run_test "T4: sh -c 'sleep 0.1'" "sh -c 'sleep 0.1'" 1 1 1

# T5: Instant container (echo) — tests state file persistence
run_test "T5: echo hello (instant)" "echo hello" 1 1 1

echo ""
echo "=== Summary ==="
echo "PASS: $PASS  FAIL: $FAIL"

kill "$SUB_PID" 2>/dev/null || true
wait "$SUB_PID" 2>/dev/null || true
rm -f "$LOG"

[ "$FAIL" -eq 0 ]
```

**Pass criteria:** All 5 tests pass. If T4/T5 fail, we have confirmed the sub-250ms gap and can proceed to the architectural fix.

### 4.3 Phase 2: Reconnect Reliability

**Script: `scripts/test-subscribe-reconnect.sh`**

Tests that a new subscription receives an accurate Snapshot (covers F6 and F3).

```bash
#!/bin/bash
# Test: disconnect and reconnect subscription; verify snapshot accuracy.

# Start a container that runs for 30s
pelagos run --detach --name test-reconnect alpine sleep 30

# Connect subscribe, get initial snapshot, verify container is present
echo "=== First connection ==="
timeout 3 pelagos subscribe | head -1 | python3 -c "
import json,sys
snap=json.load(sys.stdin)
names=[c['name'] for c in snap.get('containers',[])]
found='test-reconnect' in names
print(f'Container in snapshot: {found}')
assert found, 'FAIL: container missing from snapshot'
print('PASS')
"

# Kill the container while subscribe is disconnected
pelagos stop test-reconnect
pelagos rm test-reconnect

# Reconnect: snapshot should NOT include the (now removed) container
echo "=== Second connection (after rm) ==="
timeout 3 pelagos subscribe | head -1 | python3 -c "
import json,sys
snap=json.load(sys.stdin)
names=[c['name'] for c in snap.get('containers',[])]
found='test-reconnect' in names
print(f'Container in snapshot: {found}')
assert not found, 'FAIL: removed container still in snapshot'
print('PASS')
"
```

### 4.4 Phase 3: Continuous Stress Test

A 5-minute soak test that starts 60 containers (1 per 5s), each with a 2s lifetime, and verifies ≥ 95% event delivery. Acceptable because we expect some events to be lost during the brief reconnect gaps (which are themselves a known limitation of the polling design). The goal is ≥ 95%, not 100%.

If the pass rate is < 95%, the polling approach needs to be replaced with inotify (see Section 6.2).

---

## 5. Proposed Architecture Changes

### 5.1 Fix F3: Prompt Generation-Change Detection

**Problem:** `read_line` blocks until data arrives. Generation changes are not noticed until the next event.

**Proposed fix:** Replace the blocking `read_line` loop with a loop that uses `libc::poll` with a 100 ms timeout on the child stdout fd. When the timeout fires and no data is available, check the generation and break if changed.

```
loop {
    if generation_changed → break
    if buffer has data → skip poll
    poll(stdout_fd, POLLIN, 100ms)
    if timeout → continue (recheck generation)
    if data → read_line → process
}
drop(reader); child.kill().ok(); child.wait();
```

This bounds the generation check latency to 100 ms (one poll cycle) regardless of event volume. Requires adding `libc = "0.2"` to `pelagos-tui/Cargo.toml`.

**Alternative (no libc dependency):** Spawn a dedicated reader thread that sends lines over an `mpsc::channel`. The generation-check thread uses `recv_timeout(100ms)` on that channel. Cleaner Rust, slightly more overhead.

**Self-criticism of this fix:** It does not address F2 (short-lived container gap). It only makes the reconnect prompt. The underlying problem — missed events — requires the inotify change (5.2).

### 5.2 Fix F1/F2: Replace File Polling with inotify (Guest-Side)

**Current approach:** P3 polls `/run/pelagos/containers/` every 250 ms, diffs results.

**Problem:** Any container that starts and exits between two poll cycles may be missed (F2). If F1 confirms state files are deleted on exit, this is certain for any < 250 ms container.

**Proposed replacement:** Use Linux `inotify` to watch `/run/pelagos/containers/` for file-system events. The kernel delivers events synchronously on:
- `IN_CREATE` — new container directory created → immediately read its `state.json`
- `IN_MODIFY` or `IN_CLOSE_WRITE` — `state.json` updated → re-read and diff
- `IN_DELETE` or `IN_MOVED_FROM` — container directory removed → generate ContainerExited

**Key benefit:** Even a 1 ms container generates `IN_CREATE` → `IN_DELETE` events. Nothing is missed.

**Implementation sketch in `pelagos-guest`:**

```
fn event_watcher_loop(registry: SubscriberList) {
    let ifd = inotify_init1(IN_CLOEXEC | IN_NONBLOCK);
    inotify_add_watch(ifd, "/run/pelagos/containers", IN_CREATE | IN_DELETE | IN_ONLYDIR);

    let mut watched_dirs: HashMap<WatchDescriptor, String> = HashMap::new();
    let mut known_state: HashMap<String, ContainerSnapshot> = HashMap::new();

    // Seed initial state
    for each container in read_container_states() {
        let wd = inotify_add_watch(ifd, "/run/pelagos/containers/<name>", IN_CLOSE_WRITE | IN_DELETE_SELF);
        watched_dirs.insert(wd, container.name.clone());
        known_state.insert(container.name.clone(), container);
    }

    loop {
        // Block on inotify fd with poll(500ms) timeout for heartbeat
        let events = read_inotify_events(ifd);
        for event in events {
            match event {
                IN_CREATE for container dir → add watch, read state, emit ContainerStarted
                IN_CLOSE_WRITE for state.json → re-read, emit diff
                IN_DELETE_SELF for container dir → emit ContainerExited, remove watch
            }
        }
        // Every ~2s (via poll timeout accumulation), emit Heartbeat event
    }
}
```

**Self-criticism of this approach:**
- inotify requires Linux (fine: guest IS Linux)
- inotify watches are per-process; if `pelagos-guest` is restarted, watches are lost (acceptable: restart = new subscribers anyway)
- `IN_CLOSE_WRITE` fires when the write end is closed; if `pelagos` writes state atomically via `rename(2)`, we need `IN_MOVED_TO` instead of `IN_CLOSE_WRITE`
- Race condition: if the container directory is created but `state.json` is not yet written when we read it, we get an empty read. We must handle this (retry after a short delay, or watch for `IN_CLOSE_WRITE` within the subdirectory after `IN_CREATE` on the parent)
- We do not know yet whether `pelagos` uses rename-atomicity for state file writes (Q3 above). Phase 0 testing must answer this before inotify is implemented

**Fallback if inotify proves fragile:** Combine inotify for prompt detection with a 1s poll for ground-truth reconciliation. Hybrid approach: inotify triggers immediate events, periodic poll catches anything inotify missed (e.g., during a watch gap).

### 5.3 Fix F5: Add Heartbeat

**Proposed:** P5 (`handle_subscribe`) sends a `GuestEvent::Heartbeat` every 5 seconds if no other event has been sent. P10 tracks the last received message time. If > 15 seconds pass without any message, P10 forces a reconnect.

This detects silent death of the vsock connection, daemon proxy failure, or P3 crash.

**Wire protocol addition:**
```rust
GuestEvent::Heartbeat { ts: u64 }  // Unix seconds
```

**TUI handling:** `SubscriptionMsg::Heartbeat` updates `last_refresh` only (no container list change). Unrecognized fields are silently ignored by serde (already via unknown field handling).

**Self-criticism:** A heartbeat extends the protocol. The existing `SubscriptionMsg` enum uses `#[serde(tag = "type")]`. A `heartbeat` type will be silently dropped by `serde_json::from_str` if not added to `SubscriptionMsg`. This is a minor schema sync requirement.

### 5.4 Fix F4: Don't Drop Subscriber on Full Channel

**Proposed:** In `broadcast_events`, distinguish between `Err(Full)` (transient, keep subscriber, drop this event batch) and `Err(Disconnected)` (subscriber is gone, remove from list):

```rust
fn broadcast_events(events: &[GuestEvent]) {
    let mut subs = subscriber_list().lock().unwrap();
    subs.retain(|tx| {
        for e in events {
            match tx.try_send(e.clone()) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    // Channel temporarily full: skip this event batch for this subscriber.
                    // The subscriber is still alive; do not remove it.
                    log::warn!("subscriber channel full — dropping event batch");
                    return true; // retain
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return false; // subscriber gone; remove
                }
            }
        }
        true // all events sent; retain
    });
}
```

**Self-criticism:** Dropping event batches silently means the subscriber's view can become inconsistent. The TUI would show stale state until the next Snapshot (which requires a reconnect). A better fix is to increase channel capacity or use a blocking send with a timeout. But blocking `broadcast_events` while holding the subscriber list mutex would cause head-of-line blocking. The cleanest fix is per-subscriber ring buffers with overflow = drop oldest. That is a larger change; the `Disconnected`-only removal is the right minimum fix.

---

## 6. Self-Criticism of This Plan

**C1: F1 (state file semantics) should have been verified before any subscription code was written.** The entire event poller is built on an unverified assumption about pelagos internals. If `pelagos run alpine echo hello` deletes its state file before the first 250ms poll, no amount of TUI or guest fixes will make that container visible. Phase 0 (the state file audit) should have been step one of the original implementation.

**C2: The 14-step communication chain was never tested as a unit.** End-to-end testing (subscription → event delivery → TUI render) was done only via manual observation of the TUI. A scripted harness (the Phase 1 test above) takes 30 minutes to write and would have identified all of F1–F7 within the first day.

**C3: The inotify proposal (5.2) depends on Q3 (pelagos write semantics).** If `pelagos` uses `rename(2)` for atomic state file updates, we need `IN_MOVED_TO` watches. If it uses direct `write(2)`, we need `IN_CLOSE_WRITE`. Getting this wrong causes inotify events to fire on partially-written files. Phase 0 must precede Phase 2 implementation.

**C4: The heartbeat (5.3) addresses a symptom, not a cause.** If the subscription can silently die, the right fix is to make it unkillable: use kernel keepalives on the vsock connection, or have the daemon detect and recover dead proxies. Heartbeat is a workaround for the undetected-dead-connection problem. We should also investigate whether `SO_KEEPALIVE` can be set on vsock fds via the Virtualization framework.

**C5: The test harness proposed in Section 4 tests pelagos-mac, not pelagos-tui.** It validates the subscription stream independently of the TUI. This is intentional and correct. But it means TUI-specific bugs (e.g., `apply_subscription` not adding containers correctly, `show_all` filter edge cases) require a separate set of TUI unit tests that mock the subscription stream. Those are not written.

**C6: We are not testing the daemon proxy (P7) independently.** The proxy is a critical player (two `io::copy` threads, a `dup`'d vsock fd). A slow or stuck proxy causes event backpressure that can fill the subscriber channel (F4). No test currently exercises P7 under load.

---

## 7. Implementation Sequence (after plan approval)

1. **Phase 0: Run state-file audit script inside VM** — 30 min. Answers Q1–Q3. If state files are deleted on exit, decide whether to pivot to inotify immediately before any other work.

2. **Phase 1: Run subscribe event delivery script** — 30 min. Establishes baseline: which container lifetimes are reliably detected today? Documents pass/fail for T1–T5.

3. **Fix F4 (subscriber drop on full channel)** — 30 min code, 10 min test. Low risk, independent of other changes.

4. **Fix F3 (read_line blocking)** — 1 hr code + test. Use reader thread + `recv_timeout(100ms)`. Requires adding no new dependencies.

5. **Add heartbeat (F5)** — 1 hr. Guest emits `Heartbeat` every 5s. TUI detects silence > 15s and reconnects.

6. **Implement inotify (F2, if Phase 0 confirms it's needed)** — 3 hr code + test. Replaces `event_poller_loop`. Requires careful handling of write atomicity discovered in Phase 0.

7. **Re-run Phase 1 + Phase 3 stress test** — Confirm ≥ 95% event delivery for all container lifetimes.

8. **TUI-specific integration test** — Mock subscription stream, verify `apply_subscription` handles all event types correctly for both `show_all=true` and `show_all=false`.

---

## 8. Acceptance Criteria

The subscription system is considered reliable when:

- [ ] Phase 0 audit documents `pelagos` state file behavior (fields, write timing, persistence)
- [ ] Phase 1 T1–T3 (5s, 1s, 500ms containers) pass 100% of the time in 10 consecutive runs
- [ ] Phase 1 T4–T5 (100ms, instant) pass ≥ 90% in 10 consecutive runs (or inotify is implemented, raising this to 100%)
- [ ] Phase 3 stress test: ≥ 95% event delivery over 5 minutes
- [ ] Reconnect after pressing `a` completes within 500 ms (measured from keypress to new Snapshot rendered)
- [ ] Heartbeat: TUI auto-recovers within 20 s if subscription is silently killed (`kill -9 <subscribe_pid>`)
- [ ] No memory growth in the guest (subscriber registry cleaned up correctly on disconnect)

---

## 9. What Is Out of Scope for This Plan

- Running containers *from* the TUI (command palette). This is explicitly deferred; the focus is receive-only reliability.
- Linux-native TUI (`LinuxRunner`). All testing is macOS + VM.
- pelagos-side fixes (e.g., adding `pelagos events` command). We treat pelagos as a black box with the state file interface.
