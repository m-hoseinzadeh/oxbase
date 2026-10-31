#!/usr/bin/env bash
# Stop the oxbase test-database fixtures and delete their containers, network and named volumes.
# Only the "oxbase-fixtures" compose project is affected.
set -euo pipefail

# shellcheck source=scripts/fixtures-common.sh
. "$(dirname "${BASH_SOURCE[0]}")/fixtures-common.sh"
oxbase_require_docker

echo "Removing compose project '$OXBASE_COMPOSE_PROJECT' (containers, network, volumes)..."
oxbase_compose down --volumes --remove-orphans --timeout 20
echo "Done."
