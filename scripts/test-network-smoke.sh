#!/usr/bin/env bash
# test-network-smoke.sh — Quick DNS + TCP smoke test from inside a container.
#
# Uses the already-cached ubuntu:22.04 image (no apt-get, no downloads).
# Tests: DNS resolution + outbound TCP connection via the smoltcp NAT relay.
# Should complete in under 10 seconds.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
IMG="public.ecr.aws/docker/library/ubuntu:22.04"

RUN="$BINARY --kernel $KERNEL --initrd $INITRD --disk $DISK run $IMG"

echo "=== network smoke test ==="

# 1. VM responsive
printf "  ping... "
if ! "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 2. DNS resolution via getent (uses glibc resolver → smoltcp UDP NAT)
printf "  DNS (getent google.com)... "
dns_result=$($RUN bash -c 'getent hosts google.com | head -1' 2>&1)
if echo "$dns_result" | grep -qE '^[0-9]'; then
    echo "ok ($dns_result)"
else
    echo "FAIL"
    echo "  output: $dns_result"
    exit 1
fi

# 3. TCP connection via bash /dev/tcp (no curl/wget needed)
printf "  TCP (example.com:80)... "
tcp_result=$($RUN bash -c \
    'exec 3<>/dev/tcp/example.com/80 2>/dev/null &&
     printf "GET / HTTP/1.0\r\nHost: example.com\r\n\r\n" >&3 &&
     head -1 <&3 &&
     exec 3>&-' 2>&1)
if echo "$tcp_result" | grep -q "HTTP/"; then
    echo "ok ($tcp_result)"
else
    echo "FAIL"
    echo "  output: $tcp_result"
    exit 1
fi

echo ""
echo "PASS  network smoke test complete"
