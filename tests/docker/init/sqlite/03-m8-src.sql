-- M8 seed for SQLite, the SOURCE (the desired state) of the structure / data sync pair
-- (tests/fixtures.toml [m8] and [m8.sqlite], docs/m8-contracts.md section 9). SQLite has no
-- server: tests and the conformance harness apply this script to a fresh database file
-- (04-m8-tgt.sql builds the target in another one). SQLite has no sequences or routines
-- (skip_items); `id` columns are INTEGER PRIMARY KEY (the rowid). The file is ASCII only:
-- non-ASCII values are built with char(<code point>, ...).

CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    email VARCHAR(200) NOT NULL,
    phone VARCHAR(30) NULL,
    CONSTRAINT customers_email_uq UNIQUE (email)
);

CREATE TABLE products (
    id INTEGER PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    sku VARCHAR(40) NOT NULL,
    price DECIMAL(12,2) NOT NULL,
    stock INTEGER NULL DEFAULT 0,
    CONSTRAINT products_price_ck CHECK (price >= 0)
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL,
    total DECIMAL(12,2) NULL,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id)
);
CREATE INDEX orders_created_ix ON orders (customer_id, created_at);

CREATE TABLE order_audit (
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    action VARCHAR(20) NOT NULL
);

CREATE TABLE order_items (
    id INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    product_id INTEGER NOT NULL,
    qty INTEGER NOT NULL,
    unit_price DECIMAL(12,2) NOT NULL
);

CREATE TABLE "MixedCase" (
    id INTEGER NOT NULL PRIMARY KEY,
    "Value" VARCHAR(50) NULL
);

CREATE TABLE "select" (
    "from" INTEGER NOT NULL PRIMARY KEY,
    "it's" TEXT NULL,
    "say ""hi""" TEXT NULL
);

CREATE VIEW active_customers AS
SELECT id, name, email FROM customers WHERE phone IS NOT NULL;

CREATE TRIGGER orders_audit AFTER INSERT ON orders
BEGIN
    INSERT INTO order_audit (order_id, action) VALUES (NEW.id, 'insert');
END;

INSERT INTO customers (id, name, email, phone) VALUES
    (1, 'Ada', 'ada@example.com', '+1 555 0100'),
    (2, 'Linus', 'linus@example.com', NULL);
INSERT INTO products (id, name, sku, price, stock) VALUES
    (1, 'Widget', 'W-1', 9.50, 10),
    (2, 'Gadget', 'G-1', 120.00, 0);
-- Two orders; the orders_audit trigger writes their two order_audit rows.
INSERT INTO orders (customer_id, created_at, total) VALUES
    (1, '2026-01-05 10:00:00', 19.00),
    (2, '2026-01-06 11:30:00', 120.00);

-- --- data pairs (identical structure on both sides) --------------------------------------
-- happened_at is ISO text with the +05:30 offset; amount has NUMERIC affinity (both files
-- store the same values, so they compare equal).

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

-- Rows 1..20 (names: gruesse, Japanese, an astral emoji). 04-m8-tgt.sql builds the same rows.
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
