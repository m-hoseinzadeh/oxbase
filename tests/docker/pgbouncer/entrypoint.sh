#!/bin/sh
# PgBouncer in front of the postgres fixture, for the driver's pooler tests
# (crates/oxbase-pg/tests/pgbouncer_live.rs). The PgBouncer version is the point of each
# service, so it comes from the Alpine release the service runs on:
#   pgbouncer         alpine:3.22, a current release: refuses an untracked parameter in the
#                     startup `options` ("unsupported startup parameter in options: <name>")
#   pgbouncer-legacy  alpine:3.17, a release before 1.20: refuses the startup `options`
#                     parameter altogether ("unsupported startup parameter: options")
# Env: OXBASE_POOL_MODE (session or transaction).
set -eu
apk add --no-cache pgbouncer >/dev/null
id pgbouncer >/dev/null 2>&1 || adduser -D -H -s /sbin/nologin pgbouncer
conf=/etc/oxbase-pgbouncer
mkdir -p "$conf"
cat > "$conf/pgbouncer.ini" <<INI
[databases]
oxbase_test = host=postgres port=5432 dbname=oxbase_test

[pgbouncer]
listen_addr = 0.0.0.0
listen_port = 6432
unix_socket_dir =
auth_type = scram-sha-256
auth_file = $conf/userlist.txt
pool_mode = ${OXBASE_POOL_MODE}
max_client_conn = 50
default_pool_size = 5
INI
# Test-only credentials (tests/fixtures.toml [engines.postgres]).
printf '"oxbase" "Oxbase_Fixture_Pw_2026"\n' > "$conf/userlist.txt"
chown -R pgbouncer "$conf"
pgbouncer --version
exec pgbouncer -u pgbouncer "$conf/pgbouncer.ini"
