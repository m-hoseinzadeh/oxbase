#!/usr/bin/env bash
# Brings the Docker database fixtures (tests/docker/compose.yml) up or down for the
# conformance job. Ports/credentials live in tests/fixtures.toml.
#
#   scripts/ci-fixtures.sh up      # start all engines and wait until healthy
#   scripts/ci-fixtures.sh logs    # print container status and logs
#   scripts/ci-fixtures.sh down    # stop and remove containers, networks and volumes
#
# Env: OXBASE_COMPOSE_FILE (default tests/docker/compose.yml),
#      OXBASE_FIXTURES_WAIT_SECS (default 600; SQL Server is slow on cold runners).
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

case "${1:-}" in
  up)
    compose pull --quiet
    compose up --detach --wait --wait-timeout "$wait_secs"
    compose ps
    ;;
  logs)
    compose ps --all || true
    compose logs --no-color --timestamps || true
    ;;
  down)
    compose down --volumes --remove-orphans --timeout 30
    ;;
  *)
    echo "usage: $0 up|logs|down" >&2
    exit 2
    ;;
esac
