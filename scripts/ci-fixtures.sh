#!/usr/bin/env bash
# Brings the Docker database fixtures (tests/docker/compose.yml) up or down for the
# conformance job. Ports/credentials live in tests/fixtures.toml.
#
#   scripts/ci-fixtures.sh up [service...]    # start the engines (default: all) and wait
#                                             # until they are healthy
#   scripts/ci-fixtures.sh logs [service...]  # print container status and logs
#   scripts/ci-fixtures.sh down               # stop and remove containers, networks and volumes
#
# Services are the compose service names (postgres, mysql, mariadb, mssql, mongodb, redis,
# ssh). The import-perf job starts only `postgres mysql` (docs/m5-contracts.md §9.3).
#
# Env: OXBASE_COMPOSE_FILE (default tests/docker/compose.yml),
#      OXBASE_FIXTURES_WAIT_SECS (default 600; SQL Server is slow on cold runners),
#      OXBASE_FIXTURES_UP_ATTEMPTS (default 3; containers that exit during startup are
#      started again).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
compose_file="${OXBASE_COMPOSE_FILE:-$repo_root/tests/docker/compose.yml}"
wait_secs="${OXBASE_FIXTURES_WAIT_SECS:-600}"

if [[ ! -f "$compose_file" ]]; then
  echo "ci-fixtures: compose file not found: $compose_file" >&2
  exit 1
fi

# Same project name as scripts/fixtures-common.sh, so both script sets always address the
# same containers whatever OXBASE_COMPOSE_FILE points at.
compose_project="oxbase-fixtures"

compose() {
  docker compose --project-name "$compose_project" --file "$compose_file" "$@"
}

action="${1:-}"
if [[ $# -gt 0 ]]; then
  shift
fi
# The remaining arguments name services; anything else is refused before Docker runs.
for service in "$@"; do
  if [[ ! "$service" =~ ^[a-z][a-z0-9_-]*$ ]]; then
    echo "ci-fixtures: not a service name: $service" >&2
    exit 2
  fi
done

case "$action" in
  up)
    compose pull --quiet "$@"
    # A container that exits during startup is started again, up to `attempts` times in all:
    # the SQL Server image sometimes stops at once on a fresh runner ("Failed to load LSA:
    # 0xc0070102") and starts fine the next time. A health-check timeout is not retried.
    attempts="${OXBASE_FIXTURES_UP_ATTEMPTS:-3}"
    attempt=1
    until compose up --detach --wait --wait-timeout "$wait_secs" "$@"; do
      mapfile -t exited < <(compose ps --all --status exited --services "$@" || true)
      if (( attempt >= attempts || ${#exited[@]} == 0 )); then
        echo "ci-fixtures: the fixtures did not start (attempt $attempt of $attempts)" >&2
        exit 1
      fi
      echo "ci-fixtures: attempt $attempt of $attempts: exited during startup: ${exited[*]}" >&2
      for service in "${exited[@]}"; do
        compose logs --no-color --timestamps --tail 30 "$service" >&2 || true
      done
      compose rm --force --stop "${exited[@]}"
      attempt=$((attempt + 1))
    done
    compose ps
    ;;
  logs)
    compose ps --all || true
    compose logs --no-color --timestamps "$@" || true
    ;;
  down)
    compose down --volumes --remove-orphans --timeout 30
    ;;
  *)
    echo "usage: $0 up [service...] | logs [service...] | down" >&2
    exit 2
    ;;
esac
