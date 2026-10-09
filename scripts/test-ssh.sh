#!/usr/bin/env bash
# Starts the dev Postgres, MySQL and SSH server if needed and runs the SSH tunnel tests: the
# databases are reached through the SSH server, at their address on the containers' network.
# A private ssh-agent holding the dev key covers agent sign-in (your own agent isn't used).
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/dev-db.sh up postgres >/dev/null
./scripts/dev-db.sh up mysql >/dev/null
./scripts/dev-db.sh up ssh >/dev/null

agent_dir=$(mktemp -d)
trap 'ssh-agent -k >/dev/null 2>&1 || true; rm -rf "$agent_dir"' EXIT
eval "$(ssh-agent -a "$agent_dir/agent.sock" -s)" >/dev/null
ssh-add -q dev/ssh/id_ed25519 2>/dev/null

DBEAR_TEST_SSH=1 \
DBEAR_TEST_SSH_POSTGRES_HOST="$(./scripts/dev-db.sh ip postgres)" \
DBEAR_TEST_SSH_MYSQL_HOST="$(./scripts/dev-db.sh ip mysql)" \
  cargo test -p dbcore --test ssh "$@"
