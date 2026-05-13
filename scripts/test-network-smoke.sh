#!/usr/bin/env bash
# test-network-smoke.sh -- Quick DNS + TCP smoke test from inside a container.
#
# Tests: DNS resolution + outbound TCP connection via pasta networking.
# Should complete in under 30 seconds.
#
# Depends on: a running VM, ubuntu:22.04 image cached.
set -uo pipefail

IMG="ubuntu:22.04"
DNS_NAME="nsdns-$$"
TCP_NAME="nstcp-$$"

cleanup() {
    pelagos stop "$DNS_NAME" >/dev/null 2>&1 || true
    pelagos rm "$DNS_NAME" >/dev/null 2>&1 || true
    pelagos stop "$TCP_NAME" >/dev/null 2>&1 || true
    pelagos rm "$TCP_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== network smoke test ==="

# 1. VM responsive
printf "  ping... "
if ! pelagos ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 2. DNS resolution via getent (uses glibc resolver)
printf "  DNS (getent google.com)... "
pelagos run -d --name "$DNS_NAME" "$IMG" \
    bash -c 'getent hosts google.com | head -1' >/dev/null 2>&1
sleep 5
dns_result=$(pelagos logs "$DNS_NAME" 2>&1)
if echo "$dns_result" | grep -qE '^[0-9]'; then
    echo "ok ($dns_result)"
else
    echo "FAIL"
    echo "  output: $dns_result"
    exit 1
fi

# 3. TCP connection via bash /dev/tcp (no curl/wget needed)
printf "  TCP (example.com:80)... "
pelagos run -d --name "$TCP_NAME" "$IMG" \
    bash -c 'exec 3<>/dev/tcp/example.com/80 2>/dev/null &&
     printf "GET / HTTP/1.0\r\nHost: example.com\r\n\r\n" >&3 &&
     head -1 <&3 &&
     exec 3>&-' >/dev/null 2>&1
sleep 5
tcp_result=$(pelagos logs "$TCP_NAME" 2>&1)
if echo "$tcp_result" | grep -q "HTTP/"; then
    echo "ok ($tcp_result)"
else
    echo "FAIL"
    echo "  output: $tcp_result"
    exit 1
fi

echo ""
echo "PASS  network smoke test complete"
