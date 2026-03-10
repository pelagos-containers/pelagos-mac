#!/usr/bin/env bash
# update-certs.sh — Refresh the Mozilla CA bundle used inside the VM.
#
# The pelagos binary (statically linked musl) has no built-in CA store.
# It reads /etc/ssl/certs/ca-certificates.crt at runtime, which build-vm-image.sh
# installs from certs/cacert.pem in this repo.
#
# Run this script periodically (or on demand) to pull the latest bundle from curl.se.
# Commit the result to keep builds reproducible and Homebrew-independent.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
DEST="$REPO_ROOT/certs/cacert.pem"
URL="https://curl.se/ca/cacert.pem"

echo "Downloading Mozilla CA bundle from $URL..."
curl -fsSL -o "$DEST" "$URL"
echo "Saved to $DEST ($(wc -c < "$DEST") bytes)"
