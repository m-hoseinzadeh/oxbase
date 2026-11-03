#!/usr/bin/env bash
# The fixtures of the webview smoke test (ui/e2e/README.md): PostgreSQL, and since M7 MongoDB
# and Redis (step 11). Same compose file and project as scripts/ci-fixtures.sh, so
# `scripts/ci-fixtures.sh down` also removes them.
#
#   scripts/e2e-fixtures.sh up      # TLS/SSH material, then start them and wait until healthy
#   scripts/e2e-fixtures.sh logs    # print their status and logs
#   scripts/e2e-fixtures.sh down    # stop and remove the containers, network and volumes
#
# Env: OXBASE_FIXTURES_WAIT_SECS (default 300).
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

wait_secs="${OXBASE_FIXTURES_WAIT_SECS:-300}"
services=(postgres mongodb redis)

case "${1:-}" in
  up)
    # The services bind-mount the fixture TLS files and refuse to start without them.
    OXBASE_FIXTURES_CERTS_QUIET=1 bash "$OXBASE_REPO_ROOT/scripts/fixtures-certs.sh"
    oxbase_compose pull --quiet "${services[@]}"
    oxbase_compose up --detach --wait --wait-timeout "$wait_secs" "${services[@]}"
    oxbase_compose ps
    ;;
  logs)
    oxbase_compose ps --all || true
    oxbase_compose logs --no-color --timestamps "${services[@]}" || true
    ;;
  down)
    oxbase_compose down --volumes --remove-orphans --timeout 30
    ;;
  *)
    echo "usage: $0 up|logs|down" >&2
    exit 2
    ;;
esac
