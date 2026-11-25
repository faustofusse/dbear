#!/usr/bin/env bash
# Headless tests for the Neovim plugin against a real `dbear-cli serve` and a SQLite file.
# Usage: plugins/nvim/tests/run.sh [path/to/dbear-cli]   (default: builds and uses target/debug/dbear-cli)
# Everything lives in a temp dir: its own connection store, no keychain (DBEAR_NO_KEYRING), no socket.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
bin="${1:-}"
if [[ -z "$bin" ]]; then
  (cd "$root" && cargo build -q -p dbear-cli)
  bin="$root/target/debug/dbear-cli"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
export DBEAR_CONNECTIONS_FILE="$tmp/dbear.db" DBEAR_SOCKET="$tmp/serve.sock" DBEAR_NO_KEYRING=1
export HOME="$tmp" XDG_CONFIG_HOME="$tmp" NO_COLOR=1

seed() { "$bin" --no-server "$@" > /dev/null 2> "$tmp/err" || { cat "$tmp/err"; exit 1; }; }
# The CLI never writes the store, so seed it as a plain SQLite file through the CLI's sqlite: URLs.
seed ls
: > "$tmp/app.db"
seed query "sqlite:$tmp/dbear.db" \
  "insert into connections (id, position, name, kind, database, ssl_mode) values ('app', 0, 'App', 'sqlite', '$tmp/app.db', 'prefer')"
seed query App "create table users (id integer primary key, name text, score real, note text);
  insert into users values (1, 'Ana', 1.5, null), (2, '日本語', 20, 'NULL'), (3, 'Bo', null, 'two
lines');"

DBEAR_TEST_BIN="$bin" nvim --headless -u NONE -i NONE \
  --cmd "set rtp^=$root/plugins/nvim" \
  -l "$here/spec.lua"
