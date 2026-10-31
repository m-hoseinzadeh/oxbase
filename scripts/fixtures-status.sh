#!/usr/bin/env bash
# Show the state and health of the oxbase test-database fixtures.
# Usage: scripts/fixtures-status.sh [service...]   (default: every service in the compose file)
# Exit status: 0 if every listed service is running and healthy, 1 otherwise.
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

if [ "$#" -gt 0 ]; then
    expected="$(printf '%s\n' "$@")"
else
    expected="$(oxbase_compose config --services | sort)"
fi
rows="$(oxbase_compose ps --all --format '{{.Service}}|{{.State}}|{{.Health}}|{{.Ports}}')"

printf '%-10s %-10s %-10s %s\n' SERVICE STATE HEALTH PORTS
all_ok=0
for svc in $expected; do
    line="$(printf '%s\n' "$rows" | awk -F'|' -v s="$svc" '$1 == s' | head -n 1)"
    if [ -z "$line" ]; then
        printf '%-10s %-10s %-10s %s\n' "$svc" "absent" "-" "-"
        all_ok=1
        continue
    fi
    IFS='|' read -r _ state health ports <<<"$line"
    printf '%-10s %-10s %-10s %s\n' "$svc" "$state" "${health:--}" "$ports"
    if [ "$state" != "running" ] || [ "$health" != "healthy" ]; then
        all_ok=1
    fi
done

if [ "$all_ok" -eq 0 ]; then
    echo
    echo "All listed fixtures healthy. Connection details: tests/fixtures.toml"
fi
exit "$all_ok"
