#!/usr/bin/env bash
# Maps changed git paths to deployable components.
#
# Usage:
#   scripts/detect-changes.sh                       # compare HEAD vs latest tag
#   scripts/detect-changes.sh <from> [<to>]         # explicit revs (defaults: from=latest tag, to=HEAD)
#
# Output to stdout (one component per line, in dependency order):
#   dkms orr qkc sdn quditto
#
# Exit codes:
#   0  one or more components changed
#   3  no changes detected
#   2  invalid args / git failure
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

FROM="${1:-}"
TO="${2:-HEAD}"

if [[ -z "$FROM" ]]; then
    FROM="$(git describe --tags --abbrev=0 2>/dev/null || true)"
fi

if [[ -z "$FROM" ]]; then
    # No previous tag. Fall back to parent commit if it exists, else everything.
    if git rev-parse HEAD~1 >/dev/null 2>&1; then
        FROM="HEAD~1"
    else
        # Brand-new repo with one commit. Treat as "everything changed".
        printf '%s\n' dkms orr qkc sdn quditto
        exit 0
    fi
fi

if ! git rev-parse "$FROM" >/dev/null 2>&1; then
    echo "Error: revision '$FROM' does not exist" >&2
    exit 2
fi
if ! git rev-parse "$TO" >/dev/null 2>&1; then
    echo "Error: revision '$TO' does not exist" >&2
    exit 2
fi

CHANGED_FILES="$(git diff --name-only "$FROM" "$TO" 2>/dev/null || true)"
if [[ -z "$CHANGED_FILES" ]]; then
    echo "[detect-changes] no changes between $FROM and $TO" >&2
    exit 3
fi

declare -A AFFECTED=()

# Files that invalidate the whole Rust workspace.
RUST_WORKSPACE_RX='^(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|common/|etsi/|wire/|proto/|docker/Dockerfile\.workspace|\.dockerignore)'

while IFS= read -r path; do
    [[ -z "$path" ]] && continue

    if [[ "$path" =~ $RUST_WORKSPACE_RX ]]; then
        AFFECTED[dkms]=1
        AFFECTED[orr]=1
        AFFECTED[qkc]=1
        AFFECTED[sdn]=1
        AFFECTED[quditto]=1
        continue
    fi

    case "$path" in
        dkms/*)                AFFECTED[dkms]=1 ;;
        orr/*)                 AFFECTED[orr]=1 ;;
        qkc/*)                 AFFECTED[qkc]=1 ;;
        sdn/*)                 AFFECTED[sdn]=1 ;;
        quditto/*)             AFFECTED[quditto]=1 ;;
        *)
            # Unknown path: ignore (docs, .md, top-level scripts/build-*.sh, etc.)
            ;;
    esac
done <<< "$CHANGED_FILES"

if [[ "${#AFFECTED[@]}" -eq 0 ]]; then
    echo "[detect-changes] only ignored paths changed between $FROM and $TO" >&2
    exit 3
fi

# Print in canonical order.
for comp in dkms orr qkc sdn quditto; do
    [[ -n "${AFFECTED[$comp]:-}" ]] && echo "$comp"
done
