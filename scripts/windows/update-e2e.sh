#!/usr/bin/env bash
# The Windows half of scripts/test-update-windows.sh: installs an old build with its installer,
# then updates it with `dbear.exe --update` against a local feed. Runs on Windows (Git Bash) or
# under Wine (RUN=wine). Expects in $WORK (prepared by test-update-windows.sh):
#   old-setup.exe                       dbear 8.9.0's installer
#   old.exe                             dbear 8.9.0 itself (the portable copy)
#   www/good/…                          9.0.0's installer and zip, and a manifest signed with the test key
#   www/tampered/…                      the same manifest, with a changed installer and zip
#   www/wrongkey/…                      a manifest signed with another key
# Env: RUN (wine or empty), INSTALL_WIN (install folder, Windows path), PORT.
# NO_INSTALLER=1 (Wine under Rosetta, which can't run 32-bit code such as NSIS installers): the
# old build is "installed" by copying it next to a stand-in uninstall.exe, and the good update
# stops once the download is verified and the installer is being started.
set -euo pipefail
WORK="${WORK:?}"
RUN="${RUN:-}"
INSTALL_WIN="${INSTALL_WIN:-C:\\dbear-update-test}"
PORT="${PORT:-18732}"
OLD=8.9.0
NEW=9.0.0
FEED="http://127.0.0.1:$PORT"
# Git Bash would turn /S, /D=… and /v into paths.
export MSYS_NO_PATHCONV=1

fail() { echo "FAIL: $*" >&2; exit 1; }
run() { if [[ -n "$RUN" ]]; then "$RUN" "$@"; else "$@"; fi; }
# Windows path → one this shell can test (Git Bash: /c/…; Wine: the prefix's drive_c).
posix() {
    if [[ -n "$RUN" ]]; then winepath -u "$1" 2>/dev/null
    else cygpath -u "$1"; fi
}
dir="$(posix "$INSTALL_WIN")"
# The installed app, as this shell runs it (Wine takes the Windows path).
if [[ -n "$RUN" ]]; then APP="$INSTALL_WIN\\dbear.exe"; UNINSTALL="$INSTALL_WIN\\uninstall.exe"
else APP="$dir/dbear.exe"; UNINSTALL="$dir/uninstall.exe"; fi
version() { run "$APP" --version 2>/dev/null | tr -d '\r' | sed -n 's/^dbear //p'; }
registry_version() {
    run reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\dbear' /v DisplayVersion 2>/dev/null |
        tr -d '\r' | awk '/DisplayVersion/ {print $NF}'
}

python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$WORK/www" >"$WORK/http.log" 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
sleep 1

NO_INSTALLER="${NO_INSTALLER:-0}"
if [[ $NO_INSTALLER == 1 ]]; then
    echo "== install $OLD (copy; no 32-bit installer here)"
    mkdir -p "$dir"
    cp "$WORK/old.exe" "$dir/dbear.exe"
    : >"$dir/uninstall.exe"
    [[ "$(version)" == "$OLD" ]] || fail "installed version: '$(version)'"
else
    echo "== install $OLD (silent, per user)"
    run "$WORK/old-setup.exe" /S "/D=$INSTALL_WIN"
    for _ in $(seq 30); do [[ -f "$dir/dbear.exe" && -f "$dir/uninstall.exe" ]] && break; sleep 1; done
    [[ -f "$dir/uninstall.exe" ]] || fail "installer didn't write $INSTALL_WIN"
    [[ "$(version)" == "$OLD" ]] || fail "installed version: '$(version)'"
    [[ "$(registry_version)" == "$OLD" ]] || fail "registry DisplayVersion: '$(registry_version)'"
fi
echo "   ok: $(version) in $INSTALL_WIN"

# Runs `<exe> --update` against feed <dir>, logging to <log>; returns its exit code.
update_with() { # <exe> <feed dir> <log>
    local code
    for _ in 1 2 3 4 5 6 7 8; do
        code=0
        DBEAR_UPDATE_FEED="$FEED/$2/dbear-update-windows.json" run "$1" --update >"$3" 2>&1 || code=$?
        sed 's/^/   | /' "$3"
        # Wine under Rosetta sometimes dies on a signal (exit ≥ 128, "assertion failed … xsave");
        # that's the emulator, not dbear: try again.
        [[ -n "$RUN" && $code -ge 128 ]] || break
        echo "   (emulator crash, exit $code; retrying)"
    done
    return "$code"
}
update() { update_with "$APP" "$1" "$WORK/$1.log"; }

echo "== signed with another key"
code=0; update wrongkey || code=$?
[[ $code == 1 ]] || fail "wrong key: exit $code"
grep -q "isn’t signed with dbear’s key\|isn't signed" "$WORK/wrongkey.log" || fail "wrong key: no signature error"
sleep 3; [[ "$(version)" == "$OLD" ]] || fail "wrong key: now $(version)"
echo "   ok: rejected"

echo "== tampered installer"
code=0; update tampered || code=$?
[[ $code == 1 ]] || fail "tampered: exit $code"
grep -q "doesn’t match the signed update" "$WORK/tampered.log" || fail "tampered: no mismatch error"
sleep 3; [[ "$(version)" == "$OLD" ]] || fail "tampered: now $(version)"
echo "   ok: rejected"

echo "== portable copy (replaces its own exe)"
portable="$WORK/portable"
mkdir -p "$portable" && cp "$WORK/old.exe" "$portable/dbear.exe"
pversion() { run "$portable/dbear.exe" --version 2>/dev/null | tr -d '\r' | sed -n 's/^dbear //p'; }
portable_update() { update_with "$portable/dbear.exe" "$1" "$WORK/portable-$1.log"; }
code=0; portable_update tampered || code=$?
[[ $code == 1 && "$(pversion)" == "$OLD" ]] || fail "portable, tampered: exit $code, now $(pversion)"
code=0; portable_update good || code=$?
[[ $code == 0 ]] || fail "portable: exit $code"
[[ -f "$portable/dbear.old.exe" ]] || fail "portable: no dbear.old.exe left for the next launch"
[[ "$(pversion)" == "$NEW" ]] || fail "portable: still $(pversion)"
# That launch (--version) removed the old exe.
[[ ! -f "$portable/dbear.old.exe" ]] || fail "portable: dbear.old.exe still there after a launch"
[[ "$(find "$portable" -type f | wc -l | tr -d ' ')" == 1 ]] || fail "portable: leftovers: $(ls "$portable")"
code=0; portable_update good || code=$?
[[ $code == 3 ]] || fail "portable, up to date: exit $code"
echo "   ok: $OLD → $NEW in place"

echo "== good update"
code=0; update good || code=$?
if [[ $NO_INSTALLER == 1 ]]; then
    grep -q "^verified .*dbear-9.0.0-windows-x64-setup.exe; installing (nsis)" "$WORK/good.log" || fail "good: not verified"
    echo "   ok: downloaded and verified (the installer can't run here)"
    echo "PASS (without the installer)"
    exit 0
fi
[[ $code == 0 ]] || fail "good: exit $code"
# The installer waits for dbear.exe to exit, then replaces it.
for _ in $(seq 120); do [[ "$(version)" == "$NEW" ]] && break; sleep 1; done
[[ "$(version)" == "$NEW" ]] || fail "good: still $(version) after 120 s"
for _ in $(seq 20); do [[ "$(registry_version)" == "$NEW" ]] && break; sleep 1; done
[[ "$(registry_version)" == "$NEW" ]] || fail "good: registry still says $(registry_version)"
echo "   ok: $OLD → $NEW"

echo "== up to date"
code=0; update good || code=$?
[[ $code == 3 ]] || fail "up to date: exit $code"
echo "   ok"

echo "== uninstall"
run "$UNINSTALL" /S
for _ in $(seq 60); do [[ ! -f "$dir/dbear.exe" ]] && break; sleep 1; done
[[ ! -f "$dir/dbear.exe" ]] || fail "uninstall left dbear.exe"
[[ -z "$(registry_version)" ]] || fail "uninstall left the registry entry"
echo "   ok"

echo "PASS"
