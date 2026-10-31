-- M1: a login that may only connect over TLS (tests/fixtures.toml [engines.postgres.tls_user]).
-- The TLS-only rule itself lives in tests/docker/tls/pg_hba.conf (hostssl ... / hostnossl reject).
-- Runs once, as POSTGRES_USER (oxbase) in POSTGRES_DB (oxbase_test), on an empty data volume.
-- TEST-ONLY credentials.

CREATE ROLE oxbase_tls LOGIN PASSWORD 'Oxbase_Fixture_Pw_2026';

GRANT CONNECT, CREATE, TEMPORARY ON DATABASE oxbase_test TO oxbase_tls;
GRANT USAGE, CREATE ON SCHEMA public TO oxbase_tls;
GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO oxbase_tls;
GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO oxbase_tls;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO oxbase_tls;
-- Objects the fixture owner (oxbase) creates later, e.g. in conformance round trips.
ALTER DEFAULT PRIVILEGES FOR ROLE oxbase IN SCHEMA public
    GRANT ALL PRIVILEGES ON TABLES TO oxbase_tls;
ALTER DEFAULT PRIVILEGES FOR ROLE oxbase IN SCHEMA public
    GRANT ALL PRIVILEGES ON SEQUENCES TO oxbase_tls;
