-- M3 seed objects for SQLite (tests/fixtures.toml [m3] and [m3.sqlite]): the editable seed
-- tables of the grid-editing tests.
--
-- SQLite has no server: tests apply this script to a fresh database file (on its own, or after
-- 01-m2.sql). Live edit tests never change seed rows 1-3; they insert their own rows with code
-- 't<token>-...' and delete them.
--
-- Differences from the shapes in fixtures.toml that SQLite imposes:
--   * flag is INTEGER (0/1), doc is TEXT holding JSON, data is BLOB;
--   * the INTEGER PRIMARY KEY id is the rowid, generated when an insert leaves it out;
--   * total is a STORED generated column (SQLite 3.31+).

CREATE TABLE m3_edit_pk (
    id INTEGER PRIMARY KEY,
    code VARCHAR(20) NOT NULL UNIQUE,
    name TEXT NULL,
    amount DECIMAL(12,2) NULL,
    flag INTEGER NULL,
    born DATE NULL,
    at TIMESTAMP NULL,
    doc TEXT NULL,
    data BLOB NULL,
    total DECIMAL(12,2) GENERATED ALWAYS AS (amount * 2) STORED
);

INSERT INTO m3_edit_pk (id, code, name, amount, flag, born, at, doc, data) VALUES
    (1, 'a', 'Alpha', 10.50, 1, '2024-01-02', '2024-01-02 03:04:05', '{"k": 1}', X'0102'),
    (2, 'b', 'Beta', 20.00, 0, '2024-02-03', '2024-02-03 04:05:06.250000', '[1, 2]', X'ff'),
    (3, 'c', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE m3_edit_uq (
    code VARCHAR(20) NOT NULL UNIQUE,
    val INTEGER NULL
);

INSERT INTO m3_edit_uq (code, val) VALUES ('x', 1), ('y', 2);

CREATE TABLE m3_edit_nokey (
    a INTEGER NULL,
    b TEXT NULL,
    u INTEGER NULL UNIQUE
);

INSERT INTO m3_edit_nokey (a, b, u) VALUES (1, 'one', 1);

CREATE VIEW m3_edit_view AS SELECT id, code FROM m3_edit_pk;
