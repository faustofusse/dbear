#!/usr/bin/env bash
# Builds the Windows release files locally, without publishing anything:
#   build/windows/dbear-<version>-windows-<arch>-setup.exe   per-user installer (NSIS)
#   build/windows/dbear-<version>-windows-<arch>.zip         portable
#   build/windows/dbear-update-windows.json                  signed update manifest (with DBEAR_UPDATE_PRIVATE_KEY)
#
#   scripts/release-windows.sh <version> [--arch x64|arm64]...   on Windows (Git Bash)
#   scripts/release-windows.sh <version> --cross-debug          elsewhere: a debug build cross-compiled
#                                                               with cargo-xwin, to try the packaging
#
# Releases are normally built by .github/workflows/release-windows.yml when scripts/release-mac.sh
# pushes the tag. Release builds need Windows: GPUI compiles its shaders with the Windows SDK's
# fxc.exe there. A --cross-debug build loads them from this machine's cargo registry at runtime,
# so it only runs here (e.g. `dbear.exe --version` under Wine), not on another PC.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:?usage: scripts/release-windows.sh <version> [--arch x64|arm64]... [--cross-debug]}"
VERSION="${VERSION#v}"
shift
ARCHES=()
CROSS=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) ARCHES+=("${2:?--arch x64|arm64}"); shift ;;
        --arch=*) ARCHES+=("${1#--arch=}") ;;
        --cross-debug) CROSS=1 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done
[[ ${#ARCHES[@]} -gt 0 ]] || ARCHES=(x64)

case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ON_WINDOWS=1 ;; *) ON_WINDOWS=0 ;; esac
if [[ $ON_WINDOWS == 0 && $CROSS == 0 ]]; then
    echo "release builds need Windows (or CI: Actions ▸ Release (Windows) ▸ Run workflow)." >&2
    echo "Use --cross-debug to try the packaging with a cross-compiled debug build." >&2
    exit 2
fi
if [[ $CROSS == 1 && -z "${IN_NIX_SHELL:-}" ]] && command -v nix >/dev/null; then
    exec nix develop .#windows -c "$0" "$VERSION" "${ARCHES[@]/#/--arch=}" --cross-debug
fi

OUT="$ROOT/build/windows"
mkdir -p "$OUT"
export CARGO_INCREMENTAL=0 DBEAR_VERSION="$VERSION"
for arch in "${ARCHES[@]}"; do
    case "$arch" in x64) target=x86_64-pc-windows-msvc ;; arm64) target=aarch64-pc-windows-msvc ;; *) echo "bad arch $arch" >&2; exit 2 ;; esac
    if [[ $CROSS == 1 ]]; then
        cargo xwin build --locked -p dbear-gpui --target "$target"
        exe="target/$target/debug/dbear.exe"
    else
        rustup target add "$target" >/dev/null
        cargo build --release --locked -p dbear-gpui --target "$target"
        exe="target/$target/release/dbear.exe"
    fi
    ./scripts/package-windows.sh "$VERSION" "$arch" "$exe" "$OUT"
done

if [[ -n "${DBEAR_UPDATE_PRIVATE_KEY:-}" ]]; then
    url="https://github.com/faustofusse/dbear/releases"
    ./scripts/write-update-manifest.sh "$VERSION" "$OUT" "$url/download/v$VERSION" "" "$url/tag/v$VERSION"
else
    echo "DBEAR_UPDATE_PRIVATE_KEY isn't set: no update manifest"
fi
ls -l "$OUT"
