#!/usr/bin/env bash
# Shared helpers for scripts/fixtures-*.sh. Source it; do not run it.
# Every compose call is pinned to this repo's compose file and the "oxbase-fixtures" project, so
# these scripts never touch other compose projects or containers.

OXBASE_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OXBASE_COMPOSE_FILE="${OXBASE_COMPOSE_FILE:-$OXBASE_REPO_ROOT/tests/docker/compose.yml}"
OXBASE_COMPOSE_PROJECT="oxbase-fixtures"

oxbase_compose() {
    docker compose --project-name "$OXBASE_COMPOSE_PROJECT" --file "$OXBASE_COMPOSE_FILE" "$@"
}

oxbase_require_docker() {
    if ! command -v docker >/dev/null 2>&1; then
        echo "error: docker is not installed or not on PATH" >&2
        exit 1
    fi
    if ! docker compose version >/dev/null 2>&1; then
        echo "error: the docker compose plugin (v2+) is required" >&2
        exit 1
    fi
}
