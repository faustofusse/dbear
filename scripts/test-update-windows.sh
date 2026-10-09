#!/usr/bin/env bash
# End-to-end test of the Windows self-update: builds dbear 8.9.0 and 9.0.0 with a throwaway update
# key, packages both, installs 8.9.0 with its installer and updates it with `dbear.exe --update`
# from a local feed. Also checks that a tampered installer, a manifest signed with another key and
# a portable copy don't install anything, and that the uninstaller cleans up.
#
#   scripts/test-update-windows.sh           # on Windows (Git Bash; CI: .github/workflows/windows.yml)
#   scripts/test-update-windows.sh --wine    # on macOS: cross-builds with cargo-xwin, runs under Wine
#                                            # in an amd64 Arch container (Apple `container`, Rosetta).
#                                            # Rosetta can't run the 32-bit installer, so this stops
#                                            # once the update is downloaded and verified.
#   … --keep                                 # keep the work folder (build/windows-update-test)
#   … --prepare-only                         # build and package, don't run (implies --keep)
#
# What it can't cover: the GUI (Restart to Update, install on quit), which uses the same installer
# call with the window open. Builds are debug builds (faster; the update path is the same).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

WINE=0
KEEP=0
PREPARE_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --wine) WINE=1 ;;
        --keep) KEEP=1 ;;
        --prepare-only) PREPARE_ONLY=1; KEEP=1 ;;
        *) echo "usage: $0 [--wine] [--keep] [--prepare-only]" >&2; exit 2 ;;
    esac
done

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) ON_WINDOWS=1 ;;
    *) ON_WINDOWS=0 ;;
esac
if [[ $ON_WINDOWS == 0 && $WINE == 0 ]]; then
    echo "not on Windows: pass --wine to run under Wine in a container" >&2; exit 2
fi
if [[ $WINE == 1 && -z "${IN_NIX_SHELL:-}" ]] && command -v nix >/dev/null; then
    exec nix develop .#windows -c "$0" "$@"
fi

# Inside the repo (build/ is ignored): `nix develop` has its own TMPDIR, removed when it exits.
WORK="$ROOT/build/windows-update-test"
mkdir -p "$WORK" && find "$WORK" -mindepth 1 -delete && mkdir -p "$WORK"/{www/good,www/tampered,www/wrongkey,pkg}
cleanup() { [[ $KEEP == 1 ]] || rm -rf "$WORK"; }
trap cleanup EXIT
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0

echo "== keys and the signing tool"
cargo build -q -p dbear-update
TOOL="$ROOT/target/debug/dbear-update"
[[ -x "$TOOL" ]] || TOOL="$TOOL.exe"
key_of() { sed -n "s/^$1 key.*: //p" "$2" | tr -d '\r'; }
"$TOOL" keygen >"$WORK/test.key"
"$TOOL" keygen >"$WORK/other.key"
PUBLIC="$(key_of public "$WORK/test.key")"

if [[ $ON_WINDOWS == 1 ]]; then
    TARGET="$ROOT/target/debug"
    build() { cargo build -q -p dbear-gpui; }
else
    TARGET="$ROOT/target/x86_64-pc-windows-msvc/debug"
    build() { cargo xwin build -q -p dbear-gpui --target x86_64-pc-windows-msvc 2>&1 | { grep -E '^error' -A20 || true; }; }
fi
for version in 8.9.0 9.0.0; do
    echo "== build $version"
    DBEAR_VERSION=$version DBEAR_UPDATE_PUBLIC_KEY="$PUBLIC" build
    cp "$TARGET/dbear.exe" "$WORK/pkg/dbear-$version.exe"
done

echo "== package"
./scripts/package-windows.sh 8.9.0 x64 "$WORK/pkg/dbear-8.9.0.exe" "$WORK/pkg" >/dev/null
./scripts/package-windows.sh 9.0.0 x64 "$WORK/pkg/dbear-9.0.0.exe" "$WORK/pkg" >/dev/null
cp "$WORK/pkg/dbear-8.9.0-windows-x64-setup.exe" "$WORK/old-setup.exe"
cp "$WORK/pkg/dbear-8.9.0.exe" "$WORK/old.exe"
manifest() { # <dir> <key file>
    cp "$WORK/pkg/dbear-9.0.0-windows-x64-setup.exe" "$WORK/www/$1/"
    DBEAR_UPDATE_TOOL="$TOOL" DBEAR_UPDATE_PRIVATE_KEY="$(key_of private "$2")" DBEAR_UPDATE_PUBLIC_KEY="$(key_of public "$2")" \
        ./scripts/write-update-manifest.sh 9.0.0 "$WORK/www/$1" "http://127.0.0.1:18732/$1" >/dev/null
}
manifest good "$WORK/test.key"
manifest tampered "$WORK/test.key"
# Same size, one byte changed: only the hash (and signature) can tell.
printf 'x' | dd of="$WORK/www/tampered/dbear-9.0.0-windows-x64-setup.exe" bs=1 seek=4096 conv=notrunc 2>/dev/null
manifest wrongkey "$WORK/other.key"
rm -rf "$WORK/pkg"
cp scripts/windows/update-e2e.sh "$WORK/"
if [[ $PREPARE_ONLY == 1 ]]; then
    echo "prepared $WORK"
    exit 0
fi

if [[ $ON_WINDOWS == 1 ]]; then
    WORK="$WORK" RUN="" INSTALL_WIN="$(cygpath -w "$WORK")\\install" PORT=18732 bash scripts/windows/update-e2e.sh
    exit
fi

echo "== run under Wine (amd64 container)"
free_gb=$(( $(df -Pk / | awk 'NR==2 {print $4}') / 1048576 ))
(( free_gb >= 6 )) || { echo "only ${free_gb} GB free; the Wine container needs about 4 GB" >&2; exit 1; }
# Rosetta only translates 64-bit x86 code, so the (32-bit) NSIS installer can't run here at all:
# NO_INSTALLER=1 tests everything up to starting it. Arch's Wine is a recent WoW64 build.
cat >"$WORK/run.sh" <<'EOF'
#!/bin/sh
set -e
sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf
pacman -Sy --noconfirm --needed wine python which >/tmp/pacman.log 2>&1 || { tail /tmp/pacman.log; exit 1; }
export WINEDEBUG=-all WINEPREFIX=/root/.wine
wineboot --init >/dev/null 2>&1 || true
WORK=/work RUN=wine NO_INSTALLER=1 INSTALL_WIN='C:\dbear-update-test' PORT=18732 bash /work/update-e2e.sh
EOF
chmod +x "$WORK/run.sh"
NAME=dbear-wine-update-test
container rm -f "$NAME" >/dev/null 2>&1 || true
container run --rm --name "$NAME" --arch amd64 --rosetta -c 4 -m 4G \
    -v "$WORK:/work" docker.io/library/archlinux:latest /work/run.sh
