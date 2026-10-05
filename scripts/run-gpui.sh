#!/usr/bin/env bash
# Builds and runs the GPUI app (apps/gpui). Meant for testing it on macOS; works on Linux too.
#
#   ./scripts/run-gpui.sh                  # your saved connections (same store as the macOS app)
#   ./scripts/run-gpui.sh --samples        # a throwaway store: shows the sample connections; starts the dev Postgres
#   ./scripts/run-gpui.sh --store FILE     # another connection store (SQLite file, created if missing)
#   ./scripts/run-gpui.sh --release        # optimized build (slower to compile, much faster to scroll)
#   ./scripts/run-gpui.sh --log info       # RUST_LOG level (default: warn; e.g. info, debug, dbear=debug)
#
# Enters `nix develop` by itself when it isn't already active.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -z "${IN_NIX_SHELL:-}" ]] && command -v nix >/dev/null; then
    exec nix develop -c "$0" "$@"
fi

PROFILE=dev
TARGET_DIR=debug
STORE=""
SAMPLES=0
LOG="${RUST_LOG:-warn}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) PROFILE=release; TARGET_DIR=release ;;
        --samples) SAMPLES=1 ;;
        --store) STORE="${2:?--store needs a file}"; shift ;;
        --log) LOG="${2:?--log needs a level}"; shift ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

if [[ $SAMPLES == 1 ]]; then
    # An empty store makes the app list the samples; a fresh one each run keeps it that way.
    STORE="${TMPDIR:-/tmp}/dbear-gpui-samples.db"
    rm -f "$STORE" "$STORE-wal" "$STORE-shm"
    # app_dev, the first sample, is the dev Postgres. The other dev databases are optional.
    ./scripts/dev-db.sh up postgres >/dev/null || echo "warning: couldn't start the dev Postgres; app_dev won't connect" >&2
fi

CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}" cargo build -p dbear-gpui --profile "$PROFILE"

export RUST_LOG="$LOG"
if [[ -n "$STORE" ]]; then
    export DBEAR_CONNECTIONS_FILE="$STORE"
    echo "connections: $STORE"
fi
exec "$ROOT/target/$TARGET_DIR/dbear"
