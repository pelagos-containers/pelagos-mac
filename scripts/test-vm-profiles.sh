#!/usr/bin/env bash
# test-vm-profiles.sh — Manual test suite for the --profile VM namespace feature.
#
# Profiles are SEQUENTIAL namespaces — only one VM daemon runs at a time.
# (Two profiles cannot run concurrently: same disk image + same NAT relay port.)
#
# What is tested:
#   T1  Default profile ping (no --profile flag; regression check)
#   T2  Default profile state files are in the expected root location
#   T3  Named profile state directory is correctly isolated
#   T4  Named profile starts and responds after default is stopped
#   T5  Named profile state files are under profiles/<name>/
#   T6  Named profile volumes dir is profile-specific
#   T7  Default profile restarts cleanly after named profile is stopped
#   T8  vm-ping.sh and vm-restart.sh accept --profile
#   T9  pelagos ps works under named profile (no containers yet)
#
# The default profile daemon must already be running before this script is
# invoked.  The test stops and restarts daemons as part of the test sequence.
# The default daemon is restored at the end.
#
# Usage:
#   bash scripts/test-vm-profiles.sh [--debug]
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"

DEBUG=0
[[ "${1:-}" == "--debug" ]] && DEBUG=1

PASS=0
FAIL=0
TEST_PROFILE="test-profile"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

ok()   { echo "  PASS  $1"; ((PASS++)); }
fail() { echo "  FAIL  $1"; ((FAIL++)); }

run() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" "$@"
}

run_profile() {
    local profile="$1"; shift
    "$BINARY" --profile "$profile" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" "$@"
}

profile_state_dir() {
    local name="$1"
    local base="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"
    if [[ "$name" == "default" ]]; then
        echo "$base"
    else
        echo "$base/profiles/$name"
    fi
}

stop_daemon() {
    # Stop the daemon for a given profile by sending SIGTERM to its pid.
    local name="$1"
    local pid_file
    pid_file="$(profile_state_dir "$name")/vm.pid"
    if [[ -f "$pid_file" ]]; then
        local pid
        pid="$(cat "$pid_file")"
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            local deadline=$((SECONDS + 15))
            while [[ $SECONDS -lt $deadline ]]; do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.2
            done
        fi
    fi
}

ping_with_retry() {
    # Ping with retries to allow for slow boot.
    local profile="$1"
    local attempts="${2:-3}"
    local i
    for i in $(seq 1 "$attempts"); do
        local out
        if profile="$profile"; [[ "$profile" == "default" ]]; then
            out=$(run ping 2>&1)
        else
            out=$(run_profile "$profile" ping 2>&1)
        fi
        if echo "$out" | grep -q pong; then
            echo "$out"
            return 0
        fi
        [[ $i -lt $attempts ]] && sleep 3
    done
    echo "$out"
    return 1
}

cleanup() {
    # Restore to a clean state: stop test profile, restart default.
    [[ $DEBUG -eq 1 ]] && echo "(cleanup: stopping test profile if running)"
    stop_daemon "$TEST_PROFILE"
    rm -f "$(profile_state_dir "$TEST_PROFILE")/vm.pid" \
          "$(profile_state_dir "$TEST_PROFILE")/vm.sock"
    # If default is not running, restart it.
    local default_pid_file
    default_pid_file="$(profile_state_dir default)/vm.pid"
    if [[ ! -f "$default_pid_file" ]] || ! kill -0 "$(cat "$default_pid_file")" 2>/dev/null; then
        [[ $DEBUG -eq 1 ]] && echo "(cleanup: restarting default daemon)"
        bash "$SCRIPT_DIR/vm-ping.sh" >/dev/null 2>&1 || true
    fi
}

trap cleanup EXIT

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------

echo ""
echo "=== vm-profiles test suite ==="
echo "(profiles run one-at-a-time: same disk + same NAT relay port)"
echo ""

for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [[ ! -f "$f" ]]; then
        echo "ABORT: Missing $f — run 'make all' first."
        exit 1
    fi
done

DEFAULT_DIR="$(profile_state_dir default)"
echo "--- pre-flight ---"
printf "  default daemon already running... "
if [[ -f "$DEFAULT_DIR/vm.pid" ]] && kill -0 "$(cat "$DEFAULT_DIR/vm.pid")" 2>/dev/null; then
    echo "yes (pid $(cat "$DEFAULT_DIR/vm.pid"))"
else
    echo "ABORT: default daemon not running."
    echo "       Run 'bash scripts/vm-ping.sh' first."
    exit 1
fi
echo ""

# ---------------------------------------------------------------------------
# T1: Default profile ping (regression)
# ---------------------------------------------------------------------------
echo "--- T1: default profile ping (regression) ---"
printf "  pelagos ping (no --profile)... "
if out=$(ping_with_retry default 3 2>&1) && echo "$out" | grep -q pong; then
    ok "default ping → pong"
else
    fail "default ping failed: $out"
fi

# ---------------------------------------------------------------------------
# T2: Default profile state files in expected root location
# ---------------------------------------------------------------------------
echo "--- T2: default profile state directory ---"
printf "  vm.sock at %s/vm.sock... " "$DEFAULT_DIR"
if [[ -S "$DEFAULT_DIR/vm.sock" ]]; then
    ok "present"
else
    fail "not found"
fi

printf "  profiles/default subdir NOT created... "
if [[ ! -e "$DEFAULT_DIR/profiles/default" ]]; then
    ok "correct (no profiles/default)"
else
    fail "profiles/default exists — should not be created for the default profile"
fi

# ---------------------------------------------------------------------------
# T3: Named profile state directory
# ---------------------------------------------------------------------------
echo "--- T3: named profile state directory isolation ---"
PROFILE_DIR="$(profile_state_dir "$TEST_PROFILE")"
printf "  '%s' dir is under profiles/... " "$TEST_PROFILE"
if [[ "$PROFILE_DIR" == *"/profiles/$TEST_PROFILE" ]]; then
    ok "$PROFILE_DIR"
else
    fail "unexpected: $PROFILE_DIR"
fi

printf "  named dir != default dir... "
if [[ "$PROFILE_DIR" != "$DEFAULT_DIR" ]]; then
    ok "distinct"
else
    fail "same directory"
fi

# ---------------------------------------------------------------------------
# T4: Stop default, start named profile
# ---------------------------------------------------------------------------
echo "--- T4: named profile starts after default is stopped ---"
printf "  stopping default profile... "
out=$(run vm stop 2>&1)
echo "$out"
sleep 0.5
if out=$(run vm status 2>&1); [[ $? -ne 0 ]]; then
    ok "default stopped"
else
    fail "default still running: $out"
fi

printf "  starting named profile '%s'... " "$TEST_PROFILE"
if out=$(ping_with_retry "$TEST_PROFILE" 3 2>&1) && echo "$out" | grep -q pong; then
    ok "named ping → pong"
else
    fail "named profile ping failed: $out"
    echo "  daemon.log: $(cat "$PROFILE_DIR/daemon.log" 2>/dev/null | tail -5)"
fi

printf "  vm status for named profile... "
if out=$(run_profile "$TEST_PROFILE" vm status 2>&1) && echo "$out" | grep -q running; then
    pid_named=$(echo "$out" | grep -oE '[0-9]+$')
    ok "running (pid $pid_named)"
else
    fail "not running: $out"
fi

printf "  default profile shows stopped while named is active... "
if out=$(run vm status 2>&1); [[ $? -ne 0 ]] && echo "$out" | grep -q stopped; then
    ok "default stopped (as expected)"
else
    fail "unexpected default status: $out"
fi

# ---------------------------------------------------------------------------
# T5: Named profile state files are in the correct location
# ---------------------------------------------------------------------------
echo "--- T5: named profile state files ---"
printf "  vm.sock under profiles/%s/... " "$TEST_PROFILE"
if [[ -S "$PROFILE_DIR/vm.sock" ]]; then
    ok "present"
else
    fail "not found at $PROFILE_DIR/vm.sock"
fi

printf "  no vm.sock in default dir while named is active... "
if [[ ! -S "$DEFAULT_DIR/vm.sock" ]]; then
    ok "absent (correct)"
else
    fail "default vm.sock exists — should have been cleaned up on stop"
fi

# ---------------------------------------------------------------------------
# T6: Volumes directory is profile-specific
# ---------------------------------------------------------------------------
echo "--- T6: volumes directory is profile-specific ---"
named_vol="$PROFILE_DIR/volumes"
default_vol="$DEFAULT_DIR/volumes"
printf "  named profile volumes dir created... "
if [[ -d "$named_vol" ]]; then
    ok "$named_vol"
else
    fail "missing: $named_vol"
fi
printf "  named volumes != default volumes... "
if [[ "$named_vol" != "$default_vol" ]]; then
    ok "distinct"
else
    fail "same directory: $named_vol"
fi

# ---------------------------------------------------------------------------
# T7: Stop named, restart default
# ---------------------------------------------------------------------------
echo "--- T7: default profile restarts cleanly after named profile ---"
printf "  stopping named profile... "
out=$(run_profile "$TEST_PROFILE" vm stop 2>&1)
echo "$out"
sleep 0.5
if out=$(run_profile "$TEST_PROFILE" vm status 2>&1); [[ $? -ne 0 ]]; then
    ok "named stopped"
else
    fail "named still running: $out"
fi

printf "  restarting default profile... "
if out=$(ping_with_retry default 5 2>&1) && echo "$out" | grep -q pong; then
    ok "default ping → pong"
else
    fail "default restart failed: $out"
fi

# ---------------------------------------------------------------------------
# T8: Helper scripts accept --profile
# ---------------------------------------------------------------------------
echo "--- T8: vm-ping.sh and vm-restart.sh accept --profile ---"

# Stop default first so there is a free slot.
stop_daemon default
sleep 0.5

printf "  vm-ping.sh --profile %s... " "$TEST_PROFILE"
if out=$(bash "$SCRIPT_DIR/vm-ping.sh" --profile "$TEST_PROFILE" 2>&1) && echo "$out" | grep -q pong; then
    ok "pong"
else
    fail "failed: $out"
fi

printf "  vm-restart.sh --profile %s... " "$TEST_PROFILE"
if out=$(bash "$SCRIPT_DIR/vm-restart.sh" --profile "$TEST_PROFILE" 2>&1) && echo "$out" | grep -q pong; then
    ok "pong"
else
    fail "failed: $out"
fi

# Stop named profile for T9.
stop_daemon "$TEST_PROFILE"
sleep 0.3

# Restart default for T9.
run ping >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# T9: ps under named profile returns empty
# ---------------------------------------------------------------------------
echo "--- T9: ps on named profile returns empty ---"

# Stop default to allow named to start.
stop_daemon default
sleep 0.3

run_profile "$TEST_PROFILE" ping >/dev/null 2>&1 || true

printf "  pelagos --profile %s ps (exit 0, no containers)... " "$TEST_PROFILE"
if run_profile "$TEST_PROFILE" ps 2>&1 >/dev/null; then
    ok "ps returned exit 0"
else
    fail "ps returned non-zero"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=============================="
total=$((PASS + FAIL))
echo "  $PASS/$total tests passed"
if [[ $FAIL -gt 0 ]]; then
    echo "  $FAIL FAILED"
    echo "=============================="
    exit 1
else
    echo "  PASS"
    echo "=============================="
    exit 0
fi
