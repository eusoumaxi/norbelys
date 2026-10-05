#!/usr/bin/env bash
# Writes .env.local from .env.example for local development: a fresh deployment key (from the
# server binary, the one place keys are made) and a database of this worktree's own,
# norbelys_<branch>. Never replaces an existing .env.local: its key seals the credentials stored
# in that database. Then load it and create the database:
#   set -a; . ./.env.local; set +a; bun run db:reset
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -e .env.local ]]; then
  echo '.env.local already exists; keeping it and its keys.'
  exit 0
fi
# The branch, or the directory's name on a detached HEAD.
branch=$(git symbolic-ref --short -q HEAD || basename "$PWD")
database=norbelys_$(printf '%s' "$branch" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9_' '_' | cut -c1-50)
# Offline: the queries are checked against the committed .sqlx/, so no database is needed yet.
key=$(SQLX_OFFLINE=true cargo run -q --locked -p norbelys-server -- admin deployment-key)
umask 077
temporary=$(mktemp .env.local.XXXXXX)
trap 'rm -f "$temporary"' EXIT
sed -e "s|/norbelys_dev\$|/$database|" -e "s|^NORBELYS_DEPLOYMENT_KEY=\$|NORBELYS_DEPLOYMENT_KEY=$key|" \
  .env.example >"$temporary"
# A hard link publishes the whole file at once and refuses to replace one created meanwhile.
ln "$temporary" .env.local
echo "Created .env.local: database $database and a fresh deployment key."
echo 'Next: set -a; . ./.env.local; set +a; bun run db:reset'
