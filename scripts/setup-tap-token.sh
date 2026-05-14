#!/usr/bin/env bash
# setup-tap-token.sh -- Add or update TAP_GITHUB_TOKEN in all pelagos repos.
#
# Prompts for the token value, then sets it as a GitHub Actions secret
# in pelagos, pelagos-mac, and pelagos-ui using the gh CLI.
#
# Prerequisites:
#   - gh CLI authenticated (gh auth status)
#   - Admin access to pelagos-containers org repos
#   - A fine-grained PAT already created (see docs/HOMEBREW_TAP_TOKEN.md)
#
# Usage:
#   bash scripts/setup-tap-token.sh
#
# The token itself must be created manually at:
#   https://github.com/settings/personal-access-tokens/new
#
# Required token settings:
#   Resource owner:      pelagos-containers
#   Repository access:   Only select repositories -> homebrew-tap
#   Permissions:         Contents: Read and write
#
# This script handles ONLY the secret distribution to repos.

set -euo pipefail

REPOS=(
    "pelagos-containers/pelagos"
    "pelagos-containers/pelagos-mac"
    "pelagos-containers/pelagos-ui"
)

SECRET_NAME="TAP_GITHUB_TOKEN"

echo "=== TAP_GITHUB_TOKEN setup ==="
echo ""
echo "This script sets the TAP_GITHUB_TOKEN secret in all pelagos repos."
echo "You must first create the token at:"
echo "  https://github.com/settings/personal-access-tokens/new"
echo ""
echo "Required token settings:"
echo "  Resource owner:    pelagos-containers"
echo "  Repository access: Only select repositories -> homebrew-tap"
echo "  Permissions:       Contents: Read and write"
echo ""

# Verify gh is authenticated.
if ! gh auth status >/dev/null 2>&1; then
    echo "ERROR: gh CLI is not authenticated. Run: gh auth login" >&2
    exit 1
fi

# Prompt for the token.
printf "Paste the token value: "
read -rs TOKEN
echo ""

if [[ -z "$TOKEN" ]]; then
    echo "ERROR: empty token" >&2
    exit 1
fi

# Set the secret in each repo.
for repo in "${REPOS[@]}"; do
    printf "  setting %s in %s... " "$SECRET_NAME" "$repo"
    if echo "$TOKEN" | gh secret set "$SECRET_NAME" --repo "$repo" 2>&1; then
        echo "ok"
    else
        echo "FAILED"
    fi
done

echo ""
echo "Done. To verify, re-run a failed tap update job:"
echo "  cd ~/Projects/pelagos-ui && gh run rerun <RUN_ID> --failed"
