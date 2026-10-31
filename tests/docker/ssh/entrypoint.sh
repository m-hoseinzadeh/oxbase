#!/bin/sh
# Entrypoint wrapper for the SSH bastion fixture (lscr.io/linuxserver/openssh-server). TEST-ONLY.
#
# Runs as root before the image's s6 /init and prepares /config so the image's own init
# (init-openssh-server-config) picks it up:
#   * the fixed ed25519 host key from target/fixtures/ssh, so tests can pin it (known_hosts /
#     SHA256 fingerprint). Because /config/ssh_host_keys already exists, the image generates no
#     other host keys, and sshd is started with this key only.
#   * authorized_keys with both test client public keys (plain + passphrase-protected).
#   * /config/sshd/sshd_config.d/50-oxbase.conf, which the image Includes when the directory
#     exists. The include sits above the image's "AllowTcpForwarding no" and sshd keeps the first
#     value it reads, so forwarding is enabled; without it every tunnel fails with
#     "administratively prohibited".
# Host files are bind-mounted read-only at /oxbase-ssh-keys; the image's init then fixes owners
# (/config to the login user, /config/sshd to root:<user>) and modes.
set -eu

src=/oxbase-ssh-keys
for f in ssh_host_ed25519_key ssh_host_ed25519_key.pub authorized_keys; do
    if [ ! -s "$src/$f" ]; then
        echo "oxbase-ssh: missing $src/$f - run scripts/fixtures-certs.sh (fixtures-up.sh does)" >&2
        exit 1
    fi
done

mkdir -p /config/ssh_host_keys /config/.ssh /config/sshd/sshd_config.d
rm -f /config/ssh_host_keys/ssh_host_*
cp "$src/ssh_host_ed25519_key" "$src/ssh_host_ed25519_key.pub" /config/ssh_host_keys/
chmod 0600 /config/ssh_host_keys/ssh_host_ed25519_key
chmod 0644 /config/ssh_host_keys/ssh_host_ed25519_key.pub

cp "$src/authorized_keys" /config/.ssh/authorized_keys

cat >/config/sshd/sshd_config.d/50-oxbase.conf <<'EOF'
# oxbase fixture: SSH tunnels (direct-tcpip) to the database services on the compose network.
AllowTcpForwarding yes
PermitOpen any
EOF
chmod 0640 /config/sshd/sshd_config.d/50-oxbase.conf

exec /init "$@"
