#!/usr/bin/env bash
# Writes the signed update manifest for a Windows release: <dir>/dbear-update-windows.json.
#
#   DBEAR_UPDATE_PRIVATE_KEY=… scripts/write-update-manifest.sh <version> <dir> <base url> [notes.md] [notes url]
#
# Lists every dbear-<version>-windows-<x64|arm64>-setup.exe in <dir> as an `nsis` artifact
# downloadable at <base url>/<file name> (for a release: …/releases/download/v<version>).
# The private key is the base64 ed25519 seed from `dbear-update keygen`; its public half must be
# the one the installed apps were built with (packaging/update-public-key), and is checked
# against that file unless DBEAR_UPDATE_PUBLIC_KEY overrides it (tests use throwaway keys).
# DBEAR_UPDATE_TOOL: a built `dbear-update` binary (default: cargo run).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

VERSION="${1:?usage: write-update-manifest.sh <version> <dir> <base url> [notes.md] [notes url]}"
VERSION="${VERSION#v}"
DIR="${2:?dir}"
BASE_URL="${3:?base url}"
BASE_URL="${BASE_URL%/}"
NOTES="${4:-}"
NOTES_URL="${5:-}"
: "${DBEAR_UPDATE_PRIVATE_KEY:?DBEAR_UPDATE_PRIVATE_KEY isn’t set}"
export DBEAR_UPDATE_PRIVATE_KEY

tool() {
    if [[ -n "${DBEAR_UPDATE_TOOL:-}" ]]; then "$DBEAR_UPDATE_TOOL" "$@"
    else cargo run -q --release -p dbear-update --manifest-path "$ROOT/Cargo.toml" -- "$@"; fi
}

trusted() { { grep -v '^[[:space:]]*#' "$ROOT/packaging/update-public-key" 2>/dev/null || true; } | tr -s ' \n' ',' | sed 's/^,//; s/,$//'; }
expected="${DBEAR_UPDATE_PUBLIC_KEY:-$(trusted)}"
[[ -n "$expected" ]] || { echo "no public key in packaging/update-public-key: installed apps couldn't verify this" >&2; exit 1; }
actual="$(tool public-key | tr -d '\r')"
[[ ",$expected," == *",$actual,"* ]] || {
    echo "DBEAR_UPDATE_PRIVATE_KEY's public key ($actual) isn't one the app trusts ($expected)" >&2; exit 1; }

args=(sign --version "$VERSION" --pub-date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --out "$DIR/dbear-update-windows.json")
[[ -n "$NOTES" && -s "$NOTES" ]] && args+=(--notes "$NOTES")
[[ -n "$NOTES_URL" ]] && args+=(--notes-url "$NOTES_URL")
found=0
for arch in x64 arm64; do
    file="$DIR/dbear-$VERSION-windows-$arch-setup.exe"
    [[ -f "$file" ]] || continue
    case "$arch" in x64) target=windows-x86_64 ;; arm64) target=windows-aarch64 ;; esac
    args+=(--artifact "$target" nsis "$file" "$BASE_URL/$(basename "$file")")
    found=1
done
[[ $found == 1 ]] || { echo "no dbear-$VERSION-windows-*-setup.exe in $DIR" >&2; exit 1; }
tool "${args[@]}"
tool verify "$DIR/dbear-update-windows.json" --key "$actual" >/dev/null
echo "wrote $DIR/dbear-update-windows.json"
