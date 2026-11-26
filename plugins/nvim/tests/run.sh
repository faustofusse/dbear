#!/usr/bin/env bash
# Headless tests for the Neovim plugin against a real `dbear-cli serve` and a SQLite file.
# Usage: plugins/nvim/tests/run.sh [path/to/dbear-cli]   (default: builds and uses target/debug/dbear-cli)
# Everything lives in a temp dir: its own connection store, no keychain (DBEAR_NO_KEYRING), no socket.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
# oil.nvim, telescope.nvim and plenary.nvim for tests/tree_plugins_spec.lua (used read-only; the
# suite is skipped when they aren't there).
plugins="${DBEAR_TEST_PLUGINS-$HOME/.local/share/nvim/site/pack/core/opt}"
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
# The CLI doesn't add connections, so seed the store as a plain SQLite file through the CLI's sqlite: URLs.
seed ls
: > "$tmp/app.db"
seed query "sqlite:$tmp/dbear.db" \
  "insert into connections (id, position, name, kind, database, ssl_mode) values ('app', 0, 'App', 'sqlite', '$tmp/app.db', 'prefer')"
seed query App "create table users (id integer primary key, name text, score real, note text);
  insert into users values (1, 'Ana', 1.5, null), (2, '日本語', 20, 'NULL'), (3, 'Bo', null, 'two
lines');"

status=0
DBEAR_TEST_BIN="$bin" nvim --headless -u NONE -i NONE \
  --cmd "set rtp^=$root/plugins/nvim" \
  -l "$here/spec.lua" || status=1

# The browser (tests/browse_spec.lua) in a store of its own, with "Shop": keys (single, composite,
# none), a view, a foreign key, NULLs, '', 'NULL', unicode, a pipe, a newline, a long value, blobs.
dir="$tmp/browse"
mkdir -p "$dir"
export DBEAR_CONNECTIONS_FILE="$dir/dbear.db" DBEAR_SOCKET="$dir/serve.sock" HOME="$dir" XDG_CONFIG_HOME="$dir"
q() { seed query "$@"; }
seed ls
: > "$dir/shop.db"
q "sqlite:$dir/dbear.db" "insert into connections (id, position, name, kind, database, ssl_mode) values ('shop', 1, 'Shop', 'sqlite', '$dir/shop.db', 'prefer')"
q Shop "create table users (id integer primary key, name text not null, email text unique, note text, score real, data blob);
insert into users values
  (1, 'Ana', 'ana@x.io', null, 1.5, x'0102'),
  (2, '日本語', 'jp@x.io', '', 20, null),
  (3, 'Bo', 'bo@x.io', 'NULL', null, null),
  (4, 'Pipe', 'p@x.io', 'a|b\\c', 2, null),
  (5, 'Multi', 'm@x.io', 'two
lines', 3, null),
  (6, 'Long', 'l@x.io', 'Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore', 4, null),
  (9, 'Gone', 'g@x.io', null, null, null);
create table orders (id integer primary key, user_id integer references users(id), total numeric not null default 0, status text default 'new');
insert into orders values (10, 1, 5, 'paid'), (11, 1, 7, 'new'), (12, 2, 9, 'paid');
create table pairs (a text, b integer, v text, primary key (a, b));
insert into pairs values ('x', 1, 'one'), ('x', 2, 'two'), ('y', 1, 'three');
create table files (id integer primary key, data blob);
insert into files values (1, x'00ff');
create table logs (msg text);
insert into logs values ('hello');
create view named as select id, name from users;
create table big (id integer primary key, v text);
with recursive c(x) as (select 1 union all select x + 1 from c where x < 250) insert into big select x, 'v' || x from c;"

DBEAR_TEST_BIN="$bin" nvim --headless -u NONE -i NONE \
  --cmd "set rtp^=$root/plugins/nvim" \
  -l "$here/browse_spec.lua" || status=1

# The mirror (tests/tree_spec.lua) in a store of its own: "Shop" (id shop) and three connections
# on another file ("shop", whose name collides with "Shop" where case doesn't count; "a/b: c";
# ".hidden"), the mock Postgres "Staging" (several databases) and "Replica" (unreachable).
dir="$tmp/tree"
mkdir -p "$dir/outside"
export DBEAR_CONNECTIONS_FILE="$dir/dbear.db" DBEAR_SOCKET="$dir/serve.sock" HOME="$dir" XDG_CONFIG_HOME="$dir/config"
export XDG_CACHE_HOME="$dir/cache" XDG_DATA_HOME="$dir/data" XDG_STATE_HOME="$dir/state"
seed ls
: > "$dir/shop.db"
: > "$dir/other.db"
q "sqlite:$dir/dbear.db" "insert into connections (id, position, name, kind, database, ssl_mode) values
  ('shop', 1, 'Shop', 'sqlite', '$dir/shop.db', 'prefer'),
  ('other', 2, 'shop', 'sqlite', '$dir/other.db', 'prefer'),
  ('odd', 3, 'a/b: c', 'sqlite', '$dir/other.db', 'prefer'),
  ('dot', 4, '.hidden', 'sqlite', '$dir/other.db', 'prefer');
insert into connections (id, position, name, kind, host, port, database, user, ssl_mode, show_all_databases) values
  ('staging-pg', 5, 'Staging', 'postgres', 'staging-db.internal', 5432, 'app', 'readonly', 'prefer', 1),
  ('prod-replica', 6, 'Replica', 'postgres', 'replica-db.internal', 5432, 'app', 'readonly', 'prefer', 1)"
q shop "create table users (id integer primary key, name text not null, email text);
insert into users values (1, 'Ana', 'ana@x.io'), (2, 'Bo', null);
create table orders (id integer primary key, user_id integer references users(id), total numeric);
insert into orders values (10, 1, 5);
create view named as select id, name from users;
create table big (id integer primary key);"

# The mirror's root is made by the plugin; the tree is read-only, so make it writable to clean up.
trap 'chmod -R u+w "$tmp" 2>/dev/null; rm -rf "$tmp"' EXIT
DBEAR_TEST_BIN="$bin" DBEAR_TEST_MIRROR="$dir/mirror" DBEAR_TEST_OUTSIDE="$dir/outside" DBEAR_TEST_SHOP="$dir/shop.db" \
  nvim --headless -u NONE -i NONE \
  --cmd "set rtp^=$root/plugins/nvim" \
  -l "$here/tree_spec.lua" || status=1

if [[ -d "$plugins/oil.nvim" && -d "$plugins/telescope.nvim" && -d "$plugins/plenary.nvim" ]]; then
  DBEAR_TEST_BIN="$bin" DBEAR_TEST_MIRROR="$dir/mirror-plugins" DBEAR_TEST_PLUGINS="$plugins" \
    nvim --headless -u NONE -i NONE \
    --cmd "set rtp^=$root/plugins/nvim" \
    -l "$here/tree_plugins_spec.lua" || status=1
else
  echo "skipped tree_plugins_spec.lua: no oil.nvim, telescope.nvim and plenary.nvim in $plugins"
fi
exit $status
