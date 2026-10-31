#!/usr/bin/env bash
# Show the state and health of the oxbase test-database fixtures, and whether the generated
# TLS PKI / SSH keys (scripts/fixtures-certs.sh) are present.
# Usage: scripts/fixtures-status.sh [service...]   (default: every service in the compose file)
# Exit status: 0 if every listed service is running and healthy and the TLS/SSH material is
# present, 1 otherwise.
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

# Generated test material (paths from tests/fixtures.toml [tls] / [ssh]).
tls_dir="$OXBASE_REPO_ROOT/target/fixtures/tls"
ssh_dir="$OXBASE_REPO_ROOT/target/fixtures/ssh"
echo
missing=()
for f in "$tls_dir"/{ca.pem,ca-key.pem,server.pem,server-key.pem,client.pem,client-key.pem} \
    "$ssh_dir"/{id_ed25519,id_ed25519_enc,authorized_keys,ssh_host_ed25519_key,known_hosts,host_key.fingerprint}; do
    [ -s "$f" ] || missing+=("${f#"$OXBASE_REPO_ROOT"/}")
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "TLS/SSH test material: MISSING ${missing[*]} (run scripts/fixtures-certs.sh)"
    all_ok=1
else
    expiry="-"
    if command -v openssl >/dev/null 2>&1; then
        expiry="$(openssl x509 -in "$tls_dir/server.pem" -noout -enddate 2>/dev/null | cut -d= -f2)"
    fi
    echo "TLS PKI:  ${tls_dir#"$OXBASE_REPO_ROOT"/} (server certificate valid until ${expiry:-?})"
    echo "SSH keys: ${ssh_dir#"$OXBASE_REPO_ROOT"/} (host key $(cat "$ssh_dir/host_key.fingerprint"))"
fi

if [ "$all_ok" -eq 0 ]; then
    echo
    echo "All listed fixtures healthy. Connection details: tests/fixtures.toml"
fi
exit "$all_ok"
