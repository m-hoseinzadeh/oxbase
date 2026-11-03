-- M8 seed for SQLite, the TARGET (what a sync must change) of the structure / data sync pair
-- (tests/fixtures.toml [m8] and [m8.sqlite], docs/m8-contracts.md section 9). SQLite has no
-- server: tests and the conformance harness apply this script to a fresh database file
-- (03-m8-src.sql builds the source in another one). The file is ASCII only.

CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    email VARCHAR(200) NOT NULL,
    fax VARCHAR(30) NULL,
    CONSTRAINT customers_email_uq UNIQUE (email)
);

CREATE TABLE products (
    id INTEGER PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    sku VARCHAR(40) NULL,
    price DECIMAL(10,2) NOT NULL,
    stock INTEGER NULL
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL,
    total DECIMAL(12,2) NULL
);
CREATE INDEX orders_created_ix ON orders (created_at);

CREATE TABLE order_audit (
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    action VARCHAR(20) NOT NULL
);

CREATE TABLE order_lines (
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    product_id INTEGER NOT NULL,
    qty INTEGER NOT NULL,
    unit_price DECIMAL(12,2) NOT NULL
);

CREATE TABLE legacy_notes (
    id INTEGER NOT NULL PRIMARY KEY,
    note TEXT NULL
);

CREATE TABLE "select" (
    "from" INTEGER NOT NULL PRIMARY KEY,
    "it's" TEXT NULL
);

CREATE VIEW active_customers AS
SELECT id, name, email FROM customers WHERE 1 = 1;

INSERT INTO customers (id, name, email, fax) VALUES
    (1, 'Ada', 'ada@example.com', NULL),
    (2, 'Linus', 'linus@example.com', '+1 555 0199');
INSERT INTO products (id, name, sku, price, stock) VALUES
    (1, 'Widget', 'W-1', 9.50, 10),
    (2, 'Gadget', 'G-1', 120.00, 0);
-- The same orders and audit rows as 03-m8-src.sql (no trigger here); a sync adds
-- orders_customer_fk over them.
INSERT INTO orders (customer_id, created_at, total) VALUES
    (1, '2026-01-05 10:00:00', 19.00),
    (2, '2026-01-06 11:30:00', 120.00);
INSERT INTO order_audit (order_id, action) VALUES (1, 'insert'), (2, 'insert');

-- --- data pairs (identical structure on both sides) --------------------------------------

CREATE TABLE m8_data (
    id BIGINT NOT NULL,
    name VARCHAR(100) NULL,
    amount DECIMAL(38,10) NULL,
    payload BLOB NULL,
    happened_at TEXT NULL,
    note TEXT NULL,
    CONSTRAINT m8_data_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_big (
    id BIGINT NOT NULL,
    val INTEGER NULL,
    label VARCHAR(20) NULL,
    CONSTRAINT m8_big_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_composite (
    tenant INTEGER NOT NULL,
    code VARCHAR(20) NOT NULL,
    qty INTEGER NULL,
    CONSTRAINT m8_composite_pkey PRIMARY KEY (tenant, code)
);

CREATE TABLE m8_nokey (
    a INTEGER NULL,
    b TEXT NULL
);

-- The source rows first (the same statements as 03-m8-src.sql), then the target's edits.
WITH RECURSIVE g (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < 20)
INSERT INTO m8_data (id, name, amount, payload, happened_at, note)
SELECT i,
       CASE i WHEN 1 THEN char(103, 114, 252, 223, 101) WHEN 2 THEN char(26085, 26412)
              WHEN 4 THEN char(128512) ELSE 'name-' || i END,
       CASE i WHEN 1 THEN 1234567890123456789012345678.0123456789
              WHEN 2 THEN -1234567890123456789012345678.0123456789
              WHEN 6 THEN -0.5
              ELSE i * 1.25 END,
       CASE i % 3 WHEN 1 THEN X'00FF10' WHEN 2 THEN NULL ELSE X'0A0B' END,
       '2026-01-' || printf('%02d', i) || 'T10:00:00+05:30',
       CASE i WHEN 9 THEN NULL WHEN 10 THEN 'y' ELSE 'note ' || i END
FROM g;
-- Values that need quoting in a script: a quote, a backslash, `;` and `--` (id 3 differs, so
-- its UPDATE writes this name) and a quote, double quotes and N'...' (id 19 is inserted).
UPDATE m8_data SET name = 'O''Brien \ x;--' WHERE id = 3;
UPDATE m8_data SET note = 'it''s "q"; -- N''x''' WHERE id = 19;

WITH RECURSIVE g (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < 100000)
INSERT INTO m8_big (id, val, label) SELECT i, i % 1000, 'row-' || i FROM g;

INSERT INTO m8_composite (tenant, code, qty) VALUES
    (1, 'a', 1), (1, 'b', 2), (2, 'a', 3), (2, 'b', 4), (3, 'a', 5), (3, 'b', 6);

INSERT INTO m8_nokey (a, b) VALUES (1, 'one'), (2, 'two');

-- m8_data: 19 / 20 missing, 3 / 2 / 4 changed (name / amount / instant; 2 and 4 are edge
-- rows), 9 and 10 swap their NULL notes, 21 / 22 only here (expected: inserts 2, updates 5,
-- deletes 2).
DELETE FROM m8_data WHERE id IN (19, 20);
UPDATE m8_data SET name = 'name-3 changed' WHERE id = 3;
UPDATE m8_data SET amount = 999.0000000001 WHERE id = 2;
UPDATE m8_data SET happened_at = '2026-01-04T11:00:00+05:30' WHERE id = 4;
UPDATE m8_data SET note = 'x' WHERE id = 9;
UPDATE m8_data SET note = NULL WHERE id = 10;
INSERT INTO m8_data (id, name, amount, payload, happened_at, note) VALUES
    (21, 'extra-21', 21, NULL, '2026-02-01T00:00:00+00:00', 'extra'),
    (22, 'extra-22', 22, X'01', '2026-02-02T00:00:00+00:00', NULL);

-- m8_big: inserts 1 (50000), updates 1 (500), deletes 1 (100001).
DELETE FROM m8_big WHERE id = 50000;
UPDATE m8_big SET val = 9999 WHERE id = 500;
INSERT INTO m8_big (id, val, label) VALUES (100001, 1, 'row-100001');

-- m8_composite: updates 1.
UPDATE m8_composite SET qty = 40 WHERE tenant = 2 AND code = 'b';
