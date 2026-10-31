-- M3 grid-editing fixture (MariaDB 11.8): the editable seed tables named in tests/fixtures.toml [m3]
-- (shapes in the [m3] comment there), used by the oxbase-mysql DML live tests
-- (crates/oxbase-mysql/tests/m3_live.rs), the services edit tests and the conformance harness.
-- Runs once, as root, in MARIADB_DATABASE (oxbase_test), on an empty data volume, after
-- 01-schema.sql .. 04-m2.sql.
--
-- Live tests never change seed rows 1-3 (m3_edit_pk) or 'x' / 'y' (m3_edit_uq): they insert their
-- own rows with code 't<token>-...' and delete them. Round-trip scratch tables (m3_rt_*) are
-- created and dropped by the tests themselves.
--
-- MariaDB's json is an alias of longtext with an automatic CHECK (json_valid(doc)); tables pin
-- COLLATE=utf8mb4_unicode_ci (see 01-schema.sql).

CREATE TABLE m3_edit_pk (
    id      int            NOT NULL AUTO_INCREMENT,
    code    varchar(20)    NOT NULL,
    name    text           NULL,
    amount  decimal(12,2)  NULL,
    flag    tinyint(1)     NULL,
    born    date           NULL,
    at      datetime(6)    NULL,
    doc     json           NULL,
    data    varbinary(64)  NULL,
    total   decimal(14,2)  GENERATED ALWAYS AS (amount * 2) STORED,
    CONSTRAINT pk_m3_edit_pk PRIMARY KEY (id),
    CONSTRAINT m3_edit_pk_code_key UNIQUE (code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO m3_edit_pk (id, code, name, amount, flag, born, at, doc, data) VALUES
    (1, 'a', 'Alpha', 10.50, 1, '2024-01-02', '2024-01-02 03:04:05.123456', '{"k": 1}', X'CAFE'),
    (2, 'b', '', 0.00, 0, '2024-02-03', '2024-02-03 00:00:00', '[]', X''),
    (3, 'c', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE m3_edit_uq (
    code  varchar(20)  NOT NULL,
    val   int          NULL,
    CONSTRAINT m3_edit_uq_code_key UNIQUE (code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO m3_edit_uq (code, val) VALUES ('x', 1), ('y', 2);

CREATE TABLE m3_edit_nokey (
    a  int   NULL,
    b  text  NULL,
    u  int   NULL,
    CONSTRAINT m3_edit_nokey_u_key UNIQUE (u)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO m3_edit_nokey (a, b, u) VALUES (1, 'one', 1);

CREATE DEFINER = 'oxbase'@'%' SQL SECURITY DEFINER VIEW m3_edit_view AS
SELECT id, code FROM m3_edit_pk;
