#!/usr/bin/env bash
# Runs the core's tests on Linux inside an Apple `container`, and checks the Windows build.
#
#   ./scripts/test-linux.sh            # Linux (arm64) tests + clippy
#   ./scripts/test-linux.sh --windows  # also clippy + link the tests for x86_64-pc-windows-gnu
#
# Tests the working tree (tracked + untracked, not ignored files). Dev databases that are running
# (`./scripts/dev-db.sh up …`) are forwarded into the container on their usual ports and their
# integration tests are switched on. The OS keyring test runs against a throwaway GNOME Keyring.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=docker.io/library/rust:1-bookworm
NAME=dbear-linux-test
WORK="${TMPDIR:-/tmp}/dbear-linux"
WINDOWS=0
[[ "${1:-}" == "--windows" ]] && WINDOWS=1

# The container's disk lives on this Mac's disk until the container is removed (~6 GB with --windows).
free_gb=$(df -g / | awk 'NR==2 {print $4}')
if (( free_gb < 8 )); then
    echo "Only ${free_gb} GB free; the Linux build needs about 8 GB while it runs." >&2
    exit 1
fi

rm -rf "$WORK" && mkdir -p "$WORK/src"
git ls-files -z --cached --others --exclude-standard | while IFS= read -r -d '' f; do
    [[ -e "$f" ]] && printf '%s\0' "$f"
done | tar --null -T - -cf - | tar -xf - -C "$WORK/src"
[[ -f dev/libsql/dev_token ]] && cp dev/libsql/dev_token "$WORK/src/dev/libsql/"

# name host-port container-port env-flag
forwards=""
envs=""
for db in "dbear-postgres 54329 5432 DBEAR_TEST_POSTGRES" "dbear-mysql 33069 3306 DBEAR_TEST_MYSQL" \
          "dbear-libsql 18080 8080 DBEAR_TEST_LIBSQL" "dbear-sqlserver 14339 1433 DBEAR_TEST_SQLSERVER"; do
    read -r name port inner flag <<<"$db"
    ip=$(container inspect "$name" 2>/dev/null | python3 -c '
import json, sys
try:
    s = json.load(sys.stdin)[0]["status"]
    print(s["networks"][0]["ipv4Address"].split("/")[0] if s.get("state") == "running" else "")
except Exception:
    print("")' || true)
    if [[ -n "$ip" ]]; then
        forwards+="socat TCP-LISTEN:$port,fork,reuseaddr TCP:$ip:$inner & "
        envs+="$flag=1 "
        echo "forwarding $name ($ip:$inner) → localhost:$port"
    fi
done

cat >"$WORK/run.sh" <<EOF
#!/bin/sh
set -e
cd /src
export RUSTUP_TOOLCHAIN="\$(rustup default | cut -d' ' -f1)" CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
packages="socat gnome-keyring dbus"; [ $WINDOWS = 1 ] && packages="\$packages gcc-mingw-w64-x86-64"
apt-get update -qq >/dev/null && apt-get install -y -qq \$packages >/dev/null 2>&1
rustup component add clippy >/dev/null 2>&1
$forwards
sleep 1
echo "=== linux \$(uname -m): cargo test (Secret Service via gnome-keyring)"
export XDG_RUNTIME_DIR=/tmp/xdg && mkdir -p -m 700 \$XDG_RUNTIME_DIR
dbus-run-session -- sh -c 'printf dbear | gnome-keyring-daemon --unlock --components=secrets >/dev/null &&
    env $envs DBEAR_TEST_KEYRING=1 CARGO_TARGET_DIR=/t/linux cargo test -p dbcore --features os-keyring --locked'
echo "=== linux: clippy"
CARGO_TARGET_DIR=/t/linux cargo clippy -p dbcore --all-targets --features os-keyring --locked
if [ $WINDOWS = 1 ]; then
    rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1
    echo "=== windows x86_64: clippy + link tests"
    CARGO_TARGET_DIR=/t/win cargo clippy -p dbcore --all-targets --features os-keyring --locked --target x86_64-pc-windows-gnu
    CARGO_TARGET_DIR=/t/win cargo test -p dbcore --features os-keyring --no-run --locked --target x86_64-pc-windows-gnu
fi
EOF
chmod +x "$WORK/run.sh"

container rm -f "$NAME" >/dev/null 2>&1 || true
container run --rm --name "$NAME" -c 6 -m 8G \
    -v "$WORK/src:/src" -v "$WORK/run.sh:/run.sh" "$IMAGE" /run.sh
