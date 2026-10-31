#!/usr/bin/env bash
# The PostgreSQL fixture only, for the webview smoke test (ui/e2e/README.md). Same compose file
# and project as scripts/ci-fixtures.sh, so `scripts/ci-fixtures.sh down` also removes it.
#
#   scripts/e2e-fixtures.sh up      # TLS/SSH material, then start postgres and wait until healthy
#   scripts/e2e-fixtures.sh logs    # print its status and logs
#   scripts/e2e-fixtures.sh down    # stop and remove the containers, network and volumes
#
# Env: OXBASE_FIXTURES_WAIT_SECS (default 300).
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

wait_secs="${OXBASE_FIXTURES_WAIT_SECS:-300}"

case "${1:-}" in
  up)
    # The postgres service bind-mounts the fixture TLS files and refuses to start without them.
    OXBASE_FIXTURES_CERTS_QUIET=1 bash "$OXBASE_REPO_ROOT/scripts/fixtures-certs.sh"
    oxbase_compose pull --quiet postgres
    oxbase_compose up --detach --wait --wait-timeout "$wait_secs" postgres
    oxbase_compose ps
    ;;
  logs)
    oxbase_compose ps --all || true
    oxbase_compose logs --no-color --timestamps postgres || true
    ;;
  down)
    oxbase_compose down --volumes --remove-orphans --timeout 30
    ;;
  *)
    echo "usage: $0 up|logs|down" >&2
    exit 2
    ;;
esac
