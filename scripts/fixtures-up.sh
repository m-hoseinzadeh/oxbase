#!/usr/bin/env bash
# Start the oxbase test-database fixtures and wait until every engine is healthy and seeded.
# Usage: scripts/fixtures-up.sh [service...]     (default: all services)
# Env:   OXBASE_FIXTURES_TIMEOUT  seconds to wait for health (default 300)
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

timeout="${OXBASE_FIXTURES_TIMEOUT:-300}"

# TLS PKI and SSH keys must exist before `up`: compose bind-mounts them file by file and refuses
# to start when one is missing. Idempotent; keeps existing, still-valid material.
#
# The containers copy this material only when they start, the bind mounts keep pointing at the
# replaced files (new inodes), and `up` does not recreate a container whose config is unchanged.
# So when the certs script regenerates anything, the services that mount it are recreated below;
# otherwise they would keep serving the old certificate / host key.
mounted_material=(
    "$OXBASE_REPO_ROOT/target/fixtures/tls/ca.pem"
    "$OXBASE_REPO_ROOT/target/fixtures/tls/server.pem"
    "$OXBASE_REPO_ROOT/target/fixtures/tls/server-key.pem"
    "$OXBASE_REPO_ROOT/target/fixtures/ssh/ssh_host_ed25519_key"
    "$OXBASE_REPO_ROOT/target/fixtures/ssh/ssh_host_ed25519_key.pub"
    "$OXBASE_REPO_ROOT/target/fixtures/ssh/authorized_keys"
)
material_services=(postgres mysql mariadb mssql ssh)
material_digest() {
    # cksum is POSIX (no sha256sum/shasum split between Linux and macOS); this only detects change.
    cksum "${mounted_material[@]}" 2>/dev/null || true
}

digest_before="$(material_digest)"
OXBASE_FIXTURES_CERTS_QUIET=1 "$(dirname "${BASH_SOURCE[0]}")/fixtures-certs.sh"
digest_after="$(material_digest)"
echo

if [ "$digest_before" != "$digest_after" ]; then
    # Recreate only the affected services that already have a container (running or stopped) and
    # were asked for; new containers pick up the fresh material anyway. Named data volumes are kept.
    recreate=()
    for svc in "${material_services[@]}"; do
        if [ "$#" -gt 0 ] && [[ " $* " != *" $svc "* ]]; then
            continue
        fi
        if [ -n "$(oxbase_compose ps --all --quiet "$svc" 2>/dev/null || true)" ]; then
            recreate+=("$svc")
        fi
    done
    if [ "${#recreate[@]}" -gt 0 ]; then
        echo "Fixture TLS/SSH material changed; recreating: ${recreate[*]}"
        if ! oxbase_compose up --detach --no-deps --force-recreate "${recreate[@]}"; then
            echo "error: could not recreate ${recreate[*]}; run scripts/fixtures-down.sh and retry" >&2
            exit 1
        fi
    fi
fi

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
