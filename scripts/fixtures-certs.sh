#!/usr/bin/env bash
# Generate the TEST-ONLY PKI and SSH keys used by the Docker fixtures (M1: TLS + SSH bastion).
#
# Usage: scripts/fixtures-certs.sh [--force]
#   --force   regenerate everything, even when the existing material is still valid
#
# Output (gitignored, under target/ — never commit these files):
#   target/fixtures/tls/  ca.pem ca-key.pem server.pem server-key.pem client.pem client-key.pem
#   target/fixtures/ssh/  id_ed25519{,.pub} id_ed25519_enc{,.pub} authorized_keys
#                         ssh_host_ed25519_key{,.pub} known_hosts host_key.fingerprint
#
# Names, SANs, ports and passphrases must match the [tls] and [ssh] sections of
# tests/fixtures.toml. The script is idempotent: it only regenerates a set (TLS or SSH) when a
# file is missing or inconsistent, a certificate expires within 30 days, or the server SANs
# changed. Running containers copy this material only at start: when it is regenerated they must
# be recreated (fixtures-up.sh does that; this script warns when it sees existing containers).
# Requires: OpenSSL 1.1.1+ / 3.x (not LibreSSL) and ssh-keygen (OpenSSH 8+).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- keep in sync with tests/fixtures.toml ---------------------------------------------------
tls_dir="$repo_root/target/fixtures/tls"
ssh_dir="$repo_root/target/fixtures/ssh"
server_dns_sans=(localhost postgres mysql mariadb mssql)
server_ip_sans=(127.0.0.1)
client_cn="oxbase_tls"
valid_days=825
ca_valid_days=3650
ssh_host="127.0.0.1"
ssh_port=15422
ssh_key_passphrase="Oxbase_Fixture_Pw_2026"
# ----------------------------------------------------------------------------------------------

force=0
case "${1:-}" in
    "") ;;
    --force) force=1 ;;
    *)
        echo "usage: $0 [--force]" >&2
        exit 2
        ;;
esac

for tool in openssl ssh-keygen; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: '$tool' is required to generate the fixture PKI / SSH keys" >&2
        exit 1
    fi
done

# LibreSSL (macOS /usr/bin/openssl) and OpenSSL < 1.1.1 lack options this script relies on.
# Without this check the up-to-date test would fail on every run and regenerate the PKI each time.
openssl_version="$(openssl version 2>/dev/null || true)"
case "$openssl_version" in
    "OpenSSL 1.1.1"* | "OpenSSL 3."* | "OpenSSL 4."*) ;;
    *)
        echo "error: OpenSSL 1.1.1+ or 3.x is required, found '${openssl_version:-unknown}'." >&2
        echo "       On macOS install it (e.g. 'brew install openssl@3') and put it first on PATH." >&2
        exit 1
        ;;
esac

# Scratch directories for the generators (they hold the test CA key and SSH private keys). One
# script-level EXIT trap removes them, so a failing openssl/ssh-keygen under `set -e` cannot leave
# them behind.
work_tls=""
work_ssh=""
cleanup() {
    if [ -n "$work_tls" ]; then rm -rf "$work_tls"; fi
    if [ -n "$work_ssh" ]; then rm -rf "$work_ssh"; fi
}
trap cleanup EXIT

# Private keys are written 0600 and directories 0700. Containers read them as root (bind mounts)
# and copy them to their own location with the owner/mode each server needs.
umask 077

# The SAN line openssl prints for the server certificate, in a stable order.
expected_san_text() {
    local parts=() n
    for n in "${server_dns_sans[@]}"; do parts+=("DNS:$n"); done
    for n in "${server_ip_sans[@]}"; do parts+=("IP Address:$n"); done
    local IFS=','
    printf '%s' "${parts[*]}" | sed 's/,/, /g'
}

tls_files=(ca.pem ca-key.pem server.pem server-key.pem client.pem client-key.pem)

tls_up_to_date() {
    local f
    for f in "${tls_files[@]}"; do
        [ -s "$tls_dir/$f" ] || return 1
    done
    for f in ca.pem server.pem client.pem; do
        openssl x509 -in "$tls_dir/$f" -noout -checkend $((30 * 86400)) >/dev/null 2>&1 || return 1
    done
    local sans
    sans="$(openssl x509 -in "$tls_dir/server.pem" -noout -ext subjectAltName 2>/dev/null \
        | sed -n '2p' | sed 's/^ *//')"
    [ "$sans" = "$(expected_san_text)" ] || return 1
    openssl verify -CAfile "$tls_dir/ca.pem" "$tls_dir/server.pem" "$tls_dir/client.pem" \
        >/dev/null 2>&1 || return 1
}

generate_tls() {
    work_tls="$(mktemp -d)"
    local work="$work_tls"

    local san_cfg="" i=1 n
    for n in "${server_dns_sans[@]}"; do san_cfg+="DNS.$i = $n"$'\n'; i=$((i + 1)); done
    i=1
    for n in "${server_ip_sans[@]}"; do san_cfg+="IP.$i = $n"$'\n'; i=$((i + 1)); done

    # Self-contained config: no dependency on the distro's openssl.cnf defaults.
    cat >"$work/openssl.cnf" <<EOF
[req]
distinguished_name = dn
prompt = no
[dn]
CN = unused
[v3_ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[v3_server]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
subjectAltName = @server_sans
[v3_client]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
[server_sans]
$san_cfg
EOF

    # RSA 2048 keys in unencrypted PKCS#8 ("BEGIN PRIVATE KEY"): accepted by PostgreSQL, MySQL,
    # MariaDB and rustls alike.
    local key
    for key in ca server client; do
        openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$work/$key-key.pem" 2>/dev/null
    done

    openssl req -new -x509 -config "$work/openssl.cnf" -extensions v3_ca \
        -key "$work/ca-key.pem" -subj "/O=Oxbase fixtures/CN=Oxbase Fixture Test CA" \
        -days "$ca_valid_days" -sha256 -set_serial "0x$(openssl rand -hex 16)" \
        -out "$work/ca.pem"

    openssl req -new -config "$work/openssl.cnf" -key "$work/server-key.pem" \
        -subj "/O=Oxbase fixtures/CN=localhost" -out "$work/server.csr"
    openssl x509 -req -in "$work/server.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" \
        -set_serial "0x$(openssl rand -hex 16)" -days "$valid_days" -sha256 \
        -extfile "$work/openssl.cnf" -extensions v3_server -out "$work/server.pem" 2>/dev/null

    openssl req -new -config "$work/openssl.cnf" -key "$work/client-key.pem" \
        -subj "/O=Oxbase fixtures/CN=$client_cn" -out "$work/client.csr"
    openssl x509 -req -in "$work/client.csr" -CA "$work/ca.pem" -CAkey "$work/ca-key.pem" \
        -set_serial "0x$(openssl rand -hex 16)" -days "$valid_days" -sha256 \
        -extfile "$work/openssl.cnf" -extensions v3_client -out "$work/client.pem" 2>/dev/null

    mkdir -p "$tls_dir"
    chmod 700 "$tls_dir"
    local f
    for f in "${tls_files[@]}"; do
        install -m 0600 "$work/$f" "$tls_dir/$f"
    done
    # Certificates are public; keys stay 0600.
    chmod 644 "$tls_dir/ca.pem" "$tls_dir/server.pem" "$tls_dir/client.pem"
    rm -rf "$work_tls"
    work_tls=""
}

ssh_files=(id_ed25519 id_ed25519.pub id_ed25519_enc id_ed25519_enc.pub authorized_keys
    ssh_host_ed25519_key ssh_host_ed25519_key.pub known_hosts host_key.fingerprint)

ssh_up_to_date() {
    local f
    for f in "${ssh_files[@]}"; do
        [ -s "$ssh_dir/$f" ] || return 1
    done
    # known_hosts and host_key.fingerprint must describe the host key the container is given.
    local host_pub fp
    host_pub="$(cut -d' ' -f1,2 "$ssh_dir/ssh_host_ed25519_key.pub")"
    grep -qxF "[$ssh_host]:$ssh_port $host_pub" "$ssh_dir/known_hosts" || return 1
    fp="$(ssh-keygen -l -E sha256 -f "$ssh_dir/ssh_host_ed25519_key.pub" 2>/dev/null \
        | awk '{print $2}')" || return 1
    [ -n "$fp" ] && [ "$fp" = "$(cat "$ssh_dir/host_key.fingerprint")" ] || return 1
    # Each .pub must belong to its private key, and authorized_keys must hold both client keys.
    [ "$(ssh-keygen -y -P "" -f "$ssh_dir/ssh_host_ed25519_key" 2>/dev/null | cut -d' ' -f1,2)" \
        = "$host_pub" ] || return 1
    local pub
    for pub in id_ed25519 id_ed25519_enc; do
        grep -qF " $(cut -d' ' -f2 "$ssh_dir/$pub.pub")" "$ssh_dir/authorized_keys" || return 1
    done
    [ "$(ssh-keygen -y -P "" -f "$ssh_dir/id_ed25519" 2>/dev/null | cut -d' ' -f1,2)" \
        = "$(cut -d' ' -f1,2 "$ssh_dir/id_ed25519.pub")" ] || return 1
    # The encrypted key must still open with the documented passphrase.
    [ "$(ssh-keygen -y -P "$ssh_key_passphrase" -f "$ssh_dir/id_ed25519_enc" 2>/dev/null \
        | cut -d' ' -f1,2)" = "$(cut -d' ' -f1,2 "$ssh_dir/id_ed25519_enc.pub")" ] || return 1
}

generate_ssh() {
    work_ssh="$(mktemp -d)"
    local work="$work_ssh"

    ssh-keygen -q -t ed25519 -N "" -C "oxbase-fixture-client" -f "$work/id_ed25519"
    ssh-keygen -q -t ed25519 -N "$ssh_key_passphrase" -C "oxbase-fixture-client-encrypted" \
        -f "$work/id_ed25519_enc"
    ssh-keygen -q -t ed25519 -N "" -C "oxbase-fixture-host" -f "$work/ssh_host_ed25519_key"

    cat "$work/id_ed25519.pub" "$work/id_ed25519_enc.pub" >"$work/authorized_keys"
    # known_hosts line for the published bastion port; the trailing comment is dropped.
    printf '[%s]:%s %s\n' "$ssh_host" "$ssh_port" \
        "$(cut -d' ' -f1,2 "$work/ssh_host_ed25519_key.pub")" >"$work/known_hosts"
    ssh-keygen -l -E sha256 -f "$work/ssh_host_ed25519_key.pub" | awk '{print $2}' \
        >"$work/host_key.fingerprint"

    mkdir -p "$ssh_dir"
    chmod 700 "$ssh_dir"
    local f
    for f in "${ssh_files[@]}"; do
        install -m 0600 "$work/$f" "$ssh_dir/$f"
    done
    chmod 644 "$ssh_dir"/*.pub "$ssh_dir/authorized_keys" "$ssh_dir/known_hosts" \
        "$ssh_dir/host_key.fingerprint"
    rm -rf "$work_ssh"
    work_ssh=""
}

regenerated=0

if [ "$force" -eq 0 ] && tls_up_to_date; then
    echo "TLS test PKI up to date: ${tls_dir#"$repo_root"/}"
else
    generate_tls
    # Fresh material that fails its own check would be regenerated on every run.
    if ! tls_up_to_date; then
        echo "error: the generated TLS PKI fails its own consistency check (openssl output not as expected)" >&2
        exit 1
    fi
    regenerated=1
    echo "Generated TLS test PKI: ${tls_dir#"$repo_root"/}"
fi

if [ "$force" -eq 0 ] && ssh_up_to_date; then
    echo "SSH test keys up to date: ${ssh_dir#"$repo_root"/}"
else
    generate_ssh
    if ! ssh_up_to_date; then
        echo "error: the generated SSH keys fail their own consistency check (ssh-keygen output not as expected)" >&2
        exit 1
    fi
    regenerated=1
    echo "Generated SSH test keys: ${ssh_dir#"$repo_root"/} (host key $(cat "$ssh_dir/host_key.fingerprint"))"
fi

# Containers copy the certificates and keys only when they start, and the files above were
# replaced (new inodes), so existing containers keep serving the old material. fixtures-up.sh
# recreates them itself (and sets OXBASE_FIXTURES_CERTS_QUIET); anyone else must do it.
if [ "$regenerated" -eq 1 ] && [ -z "${OXBASE_FIXTURES_CERTS_QUIET:-}" ] \
    && command -v docker >/dev/null 2>&1 \
    && [ -n "$(docker compose --project-name oxbase-fixtures \
        --file "$repo_root/tests/docker/compose.yml" ps --all --quiet 2>/dev/null || true)" ]; then
    echo >&2
    echo "warning: fixture material was regenerated while 'oxbase-fixtures' containers exist; they" >&2
    echo "         still serve the old certificates / host key. Recreate them: scripts/fixtures-up.sh" >&2
    echo "         (recreates the affected services) or scripts/fixtures-down.sh then fixtures-up.sh." >&2
fi
