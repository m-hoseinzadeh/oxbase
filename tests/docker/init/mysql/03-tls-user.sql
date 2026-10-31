-- M1: a user that may only connect over TLS (tests/fixtures.toml [engines.mysql.tls_user]), with
-- the same grants the image gives MYSQL_USER (oxbase) on oxbase_test. The server's TLS files
-- come from the --ssl-ca/--ssl-cert/--ssl-key options in tests/docker/compose.yml.
-- Runs once, as root, on an empty data volume. TEST-ONLY credentials.

CREATE USER 'oxbase_tls'@'%' IDENTIFIED BY 'Oxbase_Fixture_Pw_2026' REQUIRE SSL;
GRANT ALL PRIVILEGES ON `oxbase_test`.* TO 'oxbase_tls'@'%';
FLUSH PRIVILEGES;
