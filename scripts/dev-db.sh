#!/usr/bin/env bash
# Local databases for development and integration tests (Apple `container` CLI for servers).
#   scripts/dev-db.sh up    [postgres|mysql|sqlite|libsql|sqlserver]   start / create (seeds on first boot); default: all
#   scripts/dev-db.sh down  [postgres|mysql|sqlite|libsql|sqlserver]   stop and delete (data is discarded)
#   scripts/dev-db.sh reset [postgres|mysql|sqlite|libsql|sqlserver]   down + up
#   scripts/dev-db.sh shell  postgres|mysql|sqlite|sqlserver           open psql / mysql / sqlite3 / sqlcmd
#   scripts/dev-db.sh logs   postgres|mysql|libsql|sqlserver           container logs
#
# "all" leaves SQL Server out: it's an amd64 image run under Rosetta in a 4 GB VM. Start it by name.
#
# Connections (non-default ports so they don't clash with servers already running):
#   postgres://postgres:postgres@localhost:54329/app_dev
#   mysql://root:mysql@localhost:33069
#   sqlite://$PWD/dev/sqlite/app.db
#   libsql://localhost:18080?tls=0&authToken=$(cat dev/libsql/dev_token)   (Turso's sqld, seeded like SQLite)
#   sqlserver://sa:Dbear_dev1@localhost:14339/app_dev?sslmode=require
set -euo pipefail
cd "$(dirname "$0")/.."

PG_NAME=dbear-postgres PG_IMAGE=postgres:17 PG_PORT=54329
MY_NAME=dbear-mysql MY_IMAGE=mysql:8.4 MY_PORT=33069
SS_NAME=dbear-sqlserver SS_IMAGE=mcr.microsoft.com/mssql/server:2022-latest SS_PORT=14339 SS_PASSWORD=Dbear_dev1
SQLITE_FILE=dev/sqlite/app.db
LIBSQL_NAME=dbear-libsql LIBSQL_IMAGE=ghcr.io/tursodatabase/libsql-server:latest LIBSQL_PORT=18080

wait_for() { # name, ready-check command, seed-error pattern, url
  local name=$1 check=$2 error_pattern=$3 url=$4
  printf 'waiting for %s' "$name"
  for _ in $(seq 1 120); do
    if eval "$check" 2>/dev/null; then
      echo ' ready'
      echo "$url"
      return 0
    fi
    # grep without -q reads the whole log: with -q it exits at the first match, `container logs`
    # dies of SIGPIPE on long logs, and pipefail turns the match into a failure.
    if container logs "$name" 2>/dev/null | grep -E "$error_pattern" >/dev/null; then
      echo ' seed failed:' >&2
      container logs "$name" | grep -E -A2 "$error_pattern" >&2
      return 1
    fi
    printf '.'
    sleep 1
  done
  echo ' timed out' >&2
  container logs "$name" | tail -20 >&2
  return 1
}

start_container() { # name, run args...
  local name=$1; shift
  if container inspect "$name" >/dev/null 2>&1; then
    container start "$name" >/dev/null 2>&1 || true
  else
    container run -d --name "$name" "$@" >/dev/null
  fi
}

remove_container() {
  container stop "$1" >/dev/null 2>&1 || true
  container rm "$1" >/dev/null 2>&1 || true
}

up_postgres() {
  start_container "$PG_NAME" \
    -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=app_dev \
    -p "127.0.0.1:$PG_PORT:5432" \
    -v "$PWD/dev/postgres:/docker-entrypoint-initdb.d:ro" \
    "$PG_IMAGE"
  # The entrypoint runs init.sql on a temporary server first; wait for the real one.
  wait_for "$PG_NAME" \
    "container logs $PG_NAME | grep 'PostgreSQL init process complete' >/dev/null && container exec $PG_NAME pg_isready -q -U postgres -d app_dev" \
    'init.sql:[0-9]*: ERROR' \
    "postgres://postgres:postgres@localhost:$PG_PORT/app_dev"
}

up_mysql() {
  start_container "$MY_NAME" \
    -e MYSQL_ROOT_PASSWORD=mysql \
    -p "127.0.0.1:$MY_PORT:3306" \
    -v "$PWD/dev/mysql:/docker-entrypoint-initdb.d:ro" \
    "$MY_IMAGE"
  wait_for "$MY_NAME" \
    "container logs $MY_NAME | grep 'MySQL init process done' >/dev/null && container exec $MY_NAME mysqladmin ping -uroot -pmysql --silent >/dev/null" \
    'ERROR [0-9]+ \(' \
    "mysql://root:mysql@localhost:$MY_PORT"
}

sqlcmd() { # args for sqlcmd inside the SQL Server container
  container exec "$SS_NAME" /opt/mssql-tools18/bin/sqlcmd -C -I -S localhost -U sa -P "$SS_PASSWORD" "$@"
}

up_sqlserver() {
  # No arm64 image: amd64 under Rosetta. The image has no init directory, so seeding is done here.
  start_container "$SS_NAME" \
    --arch amd64 --rosetta -m 4G \
    -e ACCEPT_EULA=Y -e MSSQL_PID=Developer -e "MSSQL_SA_PASSWORD=$SS_PASSWORD" \
    -p "127.0.0.1:$SS_PORT:1433" \
    -v "$PWD/dev/sqlserver:/seed:ro" \
    "$SS_IMAGE"
  wait_for "$SS_NAME" "sqlcmd -Q 'select 1' >/dev/null" 'NEVER_MATCHES' \
    "sqlserver://sa:$SS_PASSWORD@localhost:$SS_PORT/app_dev?sslmode=require" >/dev/null
  if [ "$(sqlcmd -h -1 -W -Q "set nocount on; select count(*) from sys.databases where name = 'app_dev'")" = 0 ]; then
    printf 'seeding %s\n' "$SS_NAME"
    sqlcmd -b -i /seed/init.sql >/dev/null
  fi
  echo "sqlserver://sa:$SS_PASSWORD@localhost:$SS_PORT/app_dev?sslmode=require"
}

up_sqlite() {
  if [ ! -f "$SQLITE_FILE" ]; then
    command -v sqlite3 >/dev/null || { echo 'sqlite3 not found (nix develop provides it)' >&2; return 1; }
    sqlite3 "$SQLITE_FILE" < dev/sqlite/init.sql
  fi
  echo "sqlite://$PWD/$SQLITE_FILE"
}

libsql_post() { # endpoint, JSON body
  curl -sf -H "Authorization: Bearer $(cat dev/libsql/dev_token)" -H 'Content-Type: application/json' \
    --data-binary "$2" "http://127.0.0.1:$LIBSQL_PORT/$1"
}

up_libsql() {
  start_container "$LIBSQL_NAME" \
    -e SQLD_NODE=primary -e SQLD_HTTP_LISTEN_ADDR=0.0.0.0:8080 \
    -e "SQLD_AUTH_JWT_KEY=$(cat dev/libsql/jwt_public_key)" \
    -p "127.0.0.1:$LIBSQL_PORT:8080" \
    "$LIBSQL_IMAGE"
  wait_for "$LIBSQL_NAME" "curl -sf http://127.0.0.1:$LIBSQL_PORT/v3 >/dev/null" '^$NEVER' \
    "libsql://localhost:$LIBSQL_PORT?tls=0 (token: dev/libsql/dev_token)"
  # Seed once with the SQLite seed (the JSON body is built by sqlite3 so the SQL is escaped right).
  if ! libsql_post v3/pipeline '{"requests":[{"type":"execute","stmt":{"sql":"select 1 from notes limit 1"}}]}' | grep -q '"type":"ok","response"'; then
    command -v sqlite3 >/dev/null || { echo 'sqlite3 not found (nix develop provides it)' >&2; return 1; }
    local body
    body=$(sqlite3 :memory: "select json_object('requests', json_array(json_object('type', 'sequence', 'sql', cast(readfile('dev/sqlite/init.sql') as text)), json_object('type', 'close')))")
    libsql_post v3/pipeline "$body" | grep -q '"type":"error"' && { echo 'libsql seed failed' >&2; return 1; }
  fi
  return 0
}

for_each() { # action, target
  local action=$1 target=${2:-all}
  case "$target" in
    postgres|mysql|sqlite|libsql|sqlserver) "${action}_$target" ;;
    all) "${action}_postgres"; "${action}_mysql"; "${action}_sqlite"; "${action}_libsql" ;;
    *) echo "unknown database: $target (postgres|mysql|sqlite|libsql|sqlserver)" >&2; exit 2 ;;
  esac
}

down_postgres() { remove_container "$PG_NAME"; }
down_mysql() { remove_container "$MY_NAME"; }
down_sqlite() { rm -f "$SQLITE_FILE"; }
down_libsql() { remove_container "$LIBSQL_NAME"; }
down_sqlserver() { remove_container "$SS_NAME"; }

case "${1:-up}" in
  up) for_each up "${2:-}" ;;
  down) for_each down "${2:-}" ;;
  reset) for_each down "${2:-}"; for_each up "${2:-}" ;;
  shell|psql)
    case "${2:-postgres}" in
      postgres) container exec -it "$PG_NAME" psql -U postgres -d app_dev ;;
      mysql) container exec -it "$MY_NAME" mysql -uroot -pmysql ;;
      sqlite) sqlite3 "$SQLITE_FILE" ;;
      sqlserver) container exec -it "$SS_NAME" /opt/mssql-tools18/bin/sqlcmd -C -I -S localhost -U sa -P "$SS_PASSWORD" -d app_dev ;;
    esac ;;
  logs)
    case "${2:-postgres}" in
      mysql) container logs "$MY_NAME" ;;
      libsql) container logs "$LIBSQL_NAME" ;;
      sqlserver) container logs "$SS_NAME" ;;
      *) container logs "$PG_NAME" ;;
    esac ;;
  *) echo "usage: $0 up|down|reset|shell|logs [postgres|mysql|sqlite|libsql|sqlserver]" >&2; exit 2 ;;
esac
