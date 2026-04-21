#!/usr/bin/env bash
# sync-claude-context.sh - sync Claude memory and plans to Christophers-Laptop.local.
#
# Syncs only:
#   - per-project memory directories (pelagos-mac, pelagos)
#   - ~/.claude/plans/
#
# Does NOT touch settings.json, CLAUDE.md, keybindings.json or any other
# machine-specific config on the target.
#
# Usage: bash scripts/sync-claude-context.sh [--dry-run]

set -euo pipefail

TARGET_HOST="cb@Christophers-Laptop.local"
TARGET_HOME="/Users/cb"
DRY_RUN=""
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN="--dry-run"
    echo "==> dry run - no files will be transferred"
fi

RSYNC="rsync -av --no-perms ${DRY_RUN}"

# Source paths (this machine)
SRC_PELAGOS_MAC="$HOME/.claude/projects/-Users-christopherbrown-Projects-pelagos-mac/memory/"
SRC_PELAGOS="$HOME/.claude/projects/-Users-christopherbrown-Projects-pelagos/memory/"
SRC_PLANS="$HOME/.claude/plans/"

# Destination paths (target machine - different username and project path encoding)
DST_PELAGOS_MAC="${TARGET_HOME}/.claude/projects/-Users-cb-Projects-pelagos-mac/memory/"
DST_PELAGOS="${TARGET_HOME}/.claude/projects/-Users-cb-Projects-pelagos/memory/"
DST_PLANS="${TARGET_HOME}/.claude/plans/"

echo "==> syncing pelagos-mac memory"
$RSYNC "$SRC_PELAGOS_MAC" "${TARGET_HOST}:${DST_PELAGOS_MAC}"

if [[ -d "$SRC_PELAGOS" ]]; then
    echo "==> syncing pelagos memory"
    $RSYNC "$SRC_PELAGOS" "${TARGET_HOST}:${DST_PELAGOS}"
else
    echo "==> skipping pelagos memory (no memory dir yet)"
fi

echo "==> syncing plans"
$RSYNC "$SRC_PLANS" "${TARGET_HOST}:${DST_PLANS}"

echo "==> done"
