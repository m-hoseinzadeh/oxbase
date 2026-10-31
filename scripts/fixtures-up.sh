#!/usr/bin/env bash
# Start the oxbase test-database fixtures and wait until every engine is healthy and seeded.
# Usage: scripts/fixtures-up.sh [service...]     (default: all services)
# Env:   OXBASE_FIXTURES_TIMEOUT  seconds to wait for health (default 300)
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

timeout="${OXBASE_FIXTURES_TIMEOUT:-300}"

echo "Starting compose project '$OXBASE_COMPOSE_PROJECT' ($OXBASE_COMPOSE_FILE)..."
if ! oxbase_compose up --detach --wait --wait-timeout "$timeout" "$@"; then
    echo >&2
    echo "error: fixtures did not become healthy within ${timeout}s. Current state:" >&2
    oxbase_compose ps --all >&2 || true
    echo >&2
    echo "Recent logs of unhealthy/exited services:" >&2
    for svc in $(oxbase_compose ps --all --format '{{.Service}} {{.Health}} {{.State}}' \
        | awk '$2 != "healthy" { print $1 }'); do
        echo "--- $svc ---" >&2
        oxbase_compose logs --tail 40 "$svc" >&2 || true
    done
    exit 1
fi

echo
# Check only the services that were asked for, so `fixtures-up.sh redis` succeeds even when other
# services are stopped.
"$(dirname "${BASH_SOURCE[0]}")/fixtures-status.sh" "$@"
