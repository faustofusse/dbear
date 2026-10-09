#!/usr/bin/env bash
# Packages a built dbear.exe for Windows: the per-user installer (NSIS) and a portable zip.
#
#   scripts/package-windows.sh <version> <x64|arm64> <dbear.exe> [out dir, default build/windows]
#
# Writes dbear-<version>-windows-<arch>-setup.exe and dbear-<version>-windows-<arch>.zip (plus
# .sha256 files). Needs makensis and zip (or 7z/powershell on Windows); `nix develop .#windows`
# has them on macOS/Linux, and CI runs it on Windows.
#
# Authenticode (optional): set DBEAR_SIGN_COMMAND to a command that signs the file passed as its
# last argument in place (e.g. a signtool or AzureSignTool invocation). dbear.exe is signed before
# packaging, the installer after. Without it, nothing is Authenticode-signed (updates are still
# verified with the ed25519 update key).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

VERSION="${1:?usage: package-windows.sh <version> <x64|arm64> <dbear.exe> [out dir]}"
VERSION="${VERSION#v}"
ARCH="${2:?arch: x64 or arm64}"
EXE="${3:?path to dbear.exe}"
OUT="${4:-$ROOT/build/windows}"
case "$ARCH" in x64|arm64) ;; *) echo "arch must be x64 or arm64" >&2; exit 2 ;; esac
[[ -f "$EXE" ]] || { echo "no such file: $EXE" >&2; exit 1; }

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp "$EXE" "$STAGE/dbear.exe"

# shellcheck disable=SC2034 # `file` is used by the eval
sign() {
    [[ -n "${DBEAR_SIGN_COMMAND:-}" ]] || return 0
    echo "signing $(basename "$1")"
    local file="$1"
    # Git Bash: hand Windows tools a Windows path, and keep MSYS from rewriting /f, /p… into paths.
    if command -v cygpath >/dev/null; then file="$(cygpath -w "$1")"; fi
    (export MSYS_NO_PATHCONV=1; eval "$DBEAR_SIGN_COMMAND \"\$file\"")
}
sha() {
    if command -v sha256sum >/dev/null; then (cd "$(dirname "$1")" && sha256sum "$(basename "$1")" >"$1.sha256")
    else (cd "$(dirname "$1")" && shasum -a 256 "$(basename "$1")" >"$1.sha256"); fi
}
sign "$STAGE/dbear.exe"

BASE="dbear-$VERSION-windows-$ARCH"
# Portable zip: just the app. With no uninstall.exe beside it, it updates by replacing itself
# (plain Deflate: what the updater unpacks).
ZIP="$OUT/$BASE.zip"
rm -f "$ZIP"
if command -v zip >/dev/null; then
    (cd "$STAGE" && zip -q -9 -X "$ZIP" dbear.exe)
elif command -v 7z >/dev/null; then
    (cd "$STAGE" && 7z a -tzip -mm=Deflate -mx=9 "$ZIP" dbear.exe >/dev/null)
else
    powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$STAGE/dbear.exe")' -DestinationPath '$(cygpath -w "$ZIP")'"
fi
sha "$ZIP"

# makensis takes -D options as `-D` on POSIX and `/D` on Windows; both accept `-`.
NUMERIC="$(sed -E 's/[-+].*//' <<<"$VERSION")"
IFS=. read -r MAJ MIN PAT <<<"$NUMERIC"
NUMVERSION="${MAJ:-0}.${MIN:-0}.${PAT:-0}.0"
SETUP="$OUT/$BASE-setup.exe"
SRCDIR="$STAGE"
if command -v cygpath >/dev/null; then SRCDIR="$(cygpath -w "$STAGE")"; SETUP_ARG="$(cygpath -w "$SETUP")"; else SETUP_ARG="$SETUP"; fi
makensis -V2 -INPUTCHARSET UTF8 \
    "-DVERSION=$VERSION" "-DNUMVERSION=$NUMVERSION" "-DARCH=$ARCH" \
    "-DSRCDIR=$SRCDIR" "-DOUTFILE=$SETUP_ARG" \
    "$ROOT/packaging/windows/dbear.nsi"
sign "$SETUP"
sha "$SETUP"

ls -l "$ZIP" "$SETUP"
