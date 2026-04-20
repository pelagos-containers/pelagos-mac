#!/usr/bin/env bash
# sign.sh — Sign pelagos and the pelagos-pfctl privileged helper.
#
# NOTE: sign AFTER installing, not before — macOS 26 taskgated validates the
# signature at the binary's actual run path.
#
# Both binaries must be signed with the same certificate identity so that
# SMJobBless can validate the cross-signed trust chain at install time:
#   pelagos (SMPrivilegedExecutables DR) ↔ com.pelagos.pfctl (SMAuthorizedClients DR)
#
# For development: create a local "pelagos-mac Dev" certificate in Keychain Access
#   (Certificate Assistant → Create a Certificate → Name: "pelagos-mac Dev",
#    Type: Code Signing) and set PELAGOS_SIGN_IDENTITY="pelagos-mac Dev".
#
# For ad-hoc (no SMJobBless): PELAGOS_SIGN_IDENTITY="-" (default).
#   Ad-hoc signatures allow the VM to start but SMJobBless will not work
#   because ad-hoc has no stable identity for the designated requirement strings.
#
# For distribution: set PELAGOS_SIGN_IDENTITY to your Developer ID Application identity.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE="$REPO_ROOT/target/aarch64-apple-darwin/release"
IDENTITY="${PELAGOS_SIGN_IDENTITY:-"-"}"

BINARY="$RELEASE/pelagos"
PFCTL_BINARY="$RELEASE/pelagos-pfctl"
MAIN_ENTITLEMENTS="$REPO_ROOT/pelagos-mac/entitlements.plist"
PFCTL_ENTITLEMENTS="$REPO_ROOT/pelagos-pfctl/entitlements.plist"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary not found: $BINARY"
    echo "Build it first:  cargo build --release -p pelagos-mac"
    exit 1
fi
if [ ! -f "$PFCTL_BINARY" ]; then
    echo "ERROR: binary not found: $PFCTL_BINARY"
    echo "Build it first:  cargo build --release -p pelagos-pfctl"
    exit 1
fi

codesign --sign "$IDENTITY" --entitlements "$MAIN_ENTITLEMENTS" --force "$BINARY"
echo "Signed: $BINARY"

codesign --sign "$IDENTITY" --entitlements "$PFCTL_ENTITLEMENTS" --force "$PFCTL_BINARY"
echo "Signed: $PFCTL_BINARY"

# SMJobBless looks for the helper in the same directory as the calling binary,
# named exactly by its bundle identifier.  Create a signed copy named
# com.pelagos.pfctl adjacent to pelagos so the dev workflow works without
# a Homebrew install.
HELPER_COPY="$RELEASE/com.pelagos.pfctl"
cp "$PFCTL_BINARY" "$HELPER_COPY"
codesign --sign "$IDENTITY" --entitlements "$PFCTL_ENTITLEMENTS" --force "$HELPER_COPY"
echo "Signed: $HELPER_COPY"
