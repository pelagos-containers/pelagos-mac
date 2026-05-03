#!/usr/bin/env bash
# setup-kubernetes-tls.sh -- One-time TLS setup for the rusternetes api-server.
#
# What this script does:
#   1. Generates a local CA (ca.key + ca.crt) and a server certificate
#      (server.key + server.crt) signed by that CA.
#   2. Stores both in ~/Projects/pelagos-mac/tls/ on the macOS host.
#      This directory is exposed to the build VM via virtiofs at
#      /mnt/Projects/pelagos-mac/tls/, so the api-server can read the
#      cert files without any extra copying.
#   3. Adds the CA certificate to the macOS System keychain as a trusted
#      root, so WKWebView (Tauri/pelagos-ui) and browsers trust the
#      api-server TLS certificate without errors.
#
# Run this once.  Re-run with --force to regenerate (e.g. if the VM IP
# changes or the cert expires).  After re-running, restart the rusternetes
# stack so the api-server picks up the new cert.
#
# Certificate location (macOS host):
#   ~/Projects/pelagos-mac/tls/ca.crt      CA certificate (added to keychain)
#   ~/Projects/pelagos-mac/tls/ca.key      CA private key  (keep secret)
#   ~/Projects/pelagos-mac/tls/server.crt  api-server certificate
#   ~/Projects/pelagos-mac/tls/server.key  api-server private key
#
# Same files, as seen from inside the build VM (via virtiofs):
#   /mnt/Projects/pelagos-mac/tls/server.crt
#   /mnt/Projects/pelagos-mac/tls/server.key
#
# The pelagos-guest kubernetes start handler automatically uses these
# files when they exist, falling back to --tls-self-signed otherwise.
#
# Usage:
#   bash scripts/setup-kubernetes-tls.sh [--force]

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TLS_DIR="$REPO/tls"
FORCE=0

for arg in "$@"; do
    case $arg in
        --force) FORCE=1 ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

# Build VM IP -- must match the vm_ip in ~/.local/share/pelagos/profiles/build/vm.conf
# and the SAN embedded in the certificate.
VM_IP="192.168.106.2"

if [[ -f "$TLS_DIR/server.crt" && $FORCE -eq 0 ]]; then
    echo "[tls-setup] certificates already exist at $TLS_DIR"
    echo "            Use --force to regenerate."
    exit 0
fi

if ! command -v openssl >/dev/null 2>&1; then
    echo "[tls-setup] ERROR: openssl not found -- install it with: brew install openssl"
    exit 1
fi

mkdir -p "$TLS_DIR"

echo "[tls-setup] generating CA key and certificate..."
openssl genrsa -out "$TLS_DIR/ca.key" 4096 2>/dev/null
openssl req -new -x509 -days 3650 -key "$TLS_DIR/ca.key" \
    -out "$TLS_DIR/ca.crt" \
    -subj "/CN=Pelagos Rusternetes Local CA/O=pelagos-dev"

echo "[tls-setup] generating server key and CSR..."
openssl genrsa -out "$TLS_DIR/server.key" 4096 2>/dev/null
openssl req -new -key "$TLS_DIR/server.key" \
    -out "$TLS_DIR/server.csr" \
    -subj "/CN=$VM_IP/O=pelagos-dev"

echo "[tls-setup] signing server certificate (SANs: localhost, 127.0.0.1, $VM_IP)..."
cat > "$TLS_DIR/server.ext" <<EOF
subjectAltName=DNS:localhost,IP:127.0.0.1,IP:$VM_IP
EOF

openssl x509 -req -days 3650 \
    -in "$TLS_DIR/server.csr" \
    -CA "$TLS_DIR/ca.crt" \
    -CAkey "$TLS_DIR/ca.key" \
    -CAcreateserial \
    -out "$TLS_DIR/server.crt" \
    -extfile "$TLS_DIR/server.ext" 2>/dev/null

rm -f "$TLS_DIR/server.csr" "$TLS_DIR/server.ext"

chmod 600 "$TLS_DIR/ca.key" "$TLS_DIR/server.key"

echo "[tls-setup] adding CA to macOS System keychain (requires sudo)..."
sudo security add-trusted-cert \
    -d -r trustRoot \
    -k /Library/Keychains/System.keychain \
    "$TLS_DIR/ca.crt"

echo ""
echo "[tls-setup] done."
echo ""
echo "  CA cert:     $TLS_DIR/ca.crt  (trusted in System keychain)"
echo "  Server cert: $TLS_DIR/server.crt"
echo "  Server key:  $TLS_DIR/server.key"
echo ""
echo "  In build VM: /mnt/Projects/pelagos-mac/tls/server.{crt,key}"
echo ""
echo "  Restart the rusternetes stack for the new cert to take effect:"
echo "    pelagos --profile build kubernetes stop"
echo "    pelagos --profile build kubernetes start"
