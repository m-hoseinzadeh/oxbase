-- M8 seed (MySQL 8.4): the structure / data sync pair, the restore target and the transfer
-- target of tests/fixtures.toml [m8] / [m8.mysql] (docs/m8-contracts.md section 9).
-- Runs once, as root, on an empty data volume (after 01-05). Re-runnable: every database is
-- dropped and created again.
--
--   m8_src          SOURCE of the pair (the desired state)
--   m8_tgt          TARGET of the pair (what a sync must change)
--   m8_restore      empty; restores land here
--   m8_xfer_target  empty; the PostgreSQL transfer set lands here
--
-- The fixture user `oxbase` gets every privilege on these databases and on the scratch
-- databases `m8_t_...` the tests and the conformance harness create. The harness
-- (crates/oxbase-conformance/src/m8) builds fresh copies of the section between
-- `-- m8:pair begin` and `-- m8:pair end` as `oxbase`: it replaces the whole words m8_src and
-- m8_tgt with scratch names and runs the statements in order (USE switches its session too).
-- No DELIMITER: every routine and trigger body is a single statement. Views, routines and
-- triggers are defined by `oxbase` (information_schema shows routine bodies to their definer).
-- The file is ASCII only: non-ASCII values are built from their UTF-8 bytes.

DROP DATABASE IF EXISTS m8_restore;
CREATE DATABASE m8_restore DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;
DROP DATABASE IF EXISTS m8_xfer_target;
CREATE DATABASE m8_xfer_target DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;

GRANT ALL PRIVILEGES ON `m8_src`.* TO 'oxbase'@'%';
GRANT ALL PRIVILEGES ON `m8_tgt`.* TO 'oxbase'@'%';
GRANT ALL PRIVILEGES ON `m8_restore`.* TO 'oxbase'@'%';
GRANT ALL PRIVILEGES ON `m8_xfer_target`.* TO 'oxbase'@'%';
-- Scratch databases (tests/fixtures.toml [m8] scratch_prefix); `\_` is a literal underscore.
GRANT ALL PRIVILEGES ON `m8\_t\_%`.* TO 'oxbase'@'%';

-- m8:pair begin
DROP DATABASE IF EXISTS m8_src;
DROP DATABASE IF EXISTS m8_tgt;
CREATE DATABASE m8_src DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;
CREATE DATABASE m8_tgt DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;

-- --- structure: source --------------------------------------------------------------------
USE m8_src;

CREATE TABLE customers (
    id     int           NOT NULL AUTO_INCREMENT,
    name   varchar(100)  NOT NULL,
    email  varchar(200)  NOT NULL,
    phone  varchar(30)   NULL,
    CONSTRAINT PRIMARY KEY (id),
    CONSTRAINT customers_email_uq UNIQUE (email)
) ENGINE=InnoDB;

CREATE TABLE products (
    id     int            NOT NULL AUTO_INCREMENT,
    name   varchar(100)   NOT NULL,
    sku    varchar(40)    NOT NULL,
    price  decimal(12,2)  NOT NULL,
    stock  int            NULL DEFAULT 0,
    CONSTRAINT PRIMARY KEY (id),
    CONSTRAINT products_price_ck CHECK (price >= 0)
) ENGINE=InnoDB;

CREATE TABLE orders (
    id           int            NOT NULL AUTO_INCREMENT,
    customer_id  int            NOT NULL,
    created_at   datetime       NOT NULL,
    total        decimal(12,2)  NULL,
    CONSTRAINT PRIMARY KEY (id),
    INDEX orders_created_ix (customer_id, created_at),
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id)
) ENGINE=InnoDB;

CREATE TABLE order_audit (
    id        int          NOT NULL AUTO_INCREMENT,
    order_id  int          NOT NULL,
    action    varchar(20)  NOT NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE order_items (
    id          int            NOT NULL AUTO_INCREMENT,
    order_id    int            NOT NULL,
    product_id  int            NOT NULL,
    qty         int            NOT NULL,
    unit_price  decimal(12,2)  NOT NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE `MixedCase` (
    id       int          NOT NULL,
    `Value`  varchar(50)  NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE `select` (
    `from`      int   NOT NULL,
    `it's`      text  NULL,
    `say "hi"`  text  NULL,
    CONSTRAINT PRIMARY KEY (`from`)
) ENGINE=InnoDB;

CREATE DEFINER = 'oxbase'@'%' SQL SECURITY DEFINER VIEW active_customers AS
SELECT id, name, email FROM customers WHERE phone IS NOT NULL;

CREATE DEFINER = 'oxbase'@'%' FUNCTION order_total(p_order int) RETURNS decimal(12,2)
    READS SQL DATA
    RETURN (SELECT COALESCE(SUM(qty * unit_price), 0) FROM order_items WHERE order_id = p_order);

CREATE DEFINER = 'oxbase'@'%' TRIGGER orders_audit AFTER INSERT ON orders
    FOR EACH ROW INSERT INTO order_audit (order_id, action) VALUES (NEW.id, 'insert');

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

-- --- structure: target --------------------------------------------------------------------
USE m8_tgt;

CREATE TABLE customers (
    id     int           NOT NULL AUTO_INCREMENT,
    name   varchar(100)  NOT NULL,
    email  varchar(200)  NOT NULL,
    fax    varchar(30)   NULL,
    CONSTRAINT PRIMARY KEY (id),
    CONSTRAINT customers_email_uq UNIQUE (email)
) ENGINE=InnoDB;

CREATE TABLE products (
    id     int            NOT NULL AUTO_INCREMENT,
    name   varchar(100)   NOT NULL,
    sku    varchar(40)    NULL,
    price  decimal(10,2)  NOT NULL,
    stock  int            NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE orders (
    id           int            NOT NULL AUTO_INCREMENT,
    customer_id  int            NOT NULL,
    created_at   datetime       NOT NULL,
    total        decimal(12,2)  NULL,
    CONSTRAINT PRIMARY KEY (id),
    INDEX orders_created_ix (created_at)
) ENGINE=InnoDB;

CREATE TABLE order_audit (
    id        int          NOT NULL AUTO_INCREMENT,
    order_id  int          NOT NULL,
    action    varchar(20)  NOT NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE order_lines (
    id          int            NOT NULL AUTO_INCREMENT,
    order_id    int            NOT NULL,
    product_id  int            NOT NULL,
    qty         int            NOT NULL,
    unit_price  decimal(12,2)  NOT NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE legacy_notes (
    id    int   NOT NULL,
    note  text  NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE `select` (
    `from`  int   NOT NULL,
    `it's`  text  NULL,
    CONSTRAINT PRIMARY KEY (`from`)
) ENGINE=InnoDB;

CREATE DEFINER = 'oxbase'@'%' SQL SECURITY DEFINER VIEW active_customers AS
SELECT id, name, email FROM customers WHERE 1 = 1;

INSERT INTO customers (id, name, email, fax) VALUES
    (1, 'Ada', 'ada@example.com', NULL),
    (2, 'Linus', 'linus@example.com', '+1 555 0199');
INSERT INTO products (id, name, sku, price, stock) VALUES
    (1, 'Widget', 'W-1', 9.50, 10),
    (2, 'Gadget', 'G-1', 120.00, 0);
-- The same orders and audit rows (no trigger here); a sync adds orders_customer_fk over them.
INSERT INTO orders (customer_id, created_at, total) VALUES
    (1, '2026-01-05 10:00:00', 19.00),
    (2, '2026-01-06 11:30:00', 120.00);
INSERT INTO order_audit (order_id, action) VALUES (1, 'insert'), (2, 'insert');

-- --- data pairs (identical structure on both sides) --------------------------------------
-- happened_at is datetime(6) holding the UTC instant (+05:30 already applied).

CREATE TABLE m8_src.m8_data (
    id           bigint          NOT NULL,
    name         varchar(100)    NULL,
    amount       decimal(38,10)  NULL,
    payload      varbinary(64)   NULL,
    happened_at  datetime(6)     NULL,
    note         text            NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;
CREATE TABLE m8_tgt.m8_data LIKE m8_src.m8_data;

CREATE TABLE m8_src.m8_big (
    id     bigint       NOT NULL,
    val    int          NULL,
    label  varchar(20)  NULL,
    CONSTRAINT PRIMARY KEY (id)
) ENGINE=InnoDB;
CREATE TABLE m8_tgt.m8_big LIKE m8_src.m8_big;

CREATE TABLE m8_src.m8_composite (
    tenant  int          NOT NULL,
    code    varchar(20)  NOT NULL,
    qty     int          NULL,
    CONSTRAINT PRIMARY KEY (tenant, code)
) ENGINE=InnoDB;
CREATE TABLE m8_tgt.m8_composite LIKE m8_src.m8_composite;

CREATE TABLE m8_src.m8_nokey (
    a  int   NULL,
    b  text  NULL
) ENGINE=InnoDB;
CREATE TABLE m8_tgt.m8_nokey LIKE m8_src.m8_nokey;

-- Source rows 1..20 (names: gruesse, Japanese, an astral emoji from their UTF-8 bytes).
SET SESSION cte_max_recursion_depth = 200000;
INSERT INTO m8_src.m8_data (id, name, amount, payload, happened_at, note)
SELECT i,
       CASE i WHEN 1 THEN CONVERT(X'677275C3BCC39F65' USING utf8mb4)
              WHEN 2 THEN CONVERT(X'E697A5E69CAC' USING utf8mb4)
              WHEN 4 THEN CONVERT(X'F09F9880' USING utf8mb4)
              ELSE CONCAT('name-', i) END,
       CASE i WHEN 1 THEN 1234567890123456789012345678.0123456789
              WHEN 2 THEN -1234567890123456789012345678.0123456789
              WHEN 6 THEN -0.5
              ELSE i * 1.25 END,
       CASE i % 3 WHEN 1 THEN X'00FF10' WHEN 2 THEN NULL ELSE X'0A0B' END,
       TIMESTAMP '2026-01-01 04:30:00' + INTERVAL (i - 1) DAY,
       CASE i WHEN 9 THEN NULL WHEN 10 THEN 'y' ELSE CONCAT('note ', i) END
FROM (WITH RECURSIVE s (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 20)
      SELECT i FROM s) AS g;
-- Values that need quoting in a script: a quote, a backslash (CHAR(92): the same text with
-- or without NO_BACKSLASH_ESCAPES), `;` and `--` (id 3 differs, so its UPDATE writes this
-- name) and a quote, double quotes and N'...' (id 19 is inserted).
UPDATE m8_src.m8_data SET name = CONCAT('O''Brien ', CHAR(92 USING utf8mb4), ' x;--') WHERE id = 3;
UPDATE m8_src.m8_data SET note = 'it''s "q"; -- N''x''' WHERE id = 19;

-- Target: ids 1..18, 3 / 2 / 4 changed (name / amount / instant; 2 and 4 are edge rows: a
-- 38-digit amount with a Japanese name, an astral emoji name), 9 and 10 swap their NULL
-- notes, 21 / 22 only here. Expected: inserts 2, updates 5, deletes 2.
INSERT INTO m8_tgt.m8_data SELECT * FROM m8_src.m8_data WHERE id <= 18;
UPDATE m8_tgt.m8_data SET name = 'name-3 changed' WHERE id = 3;
UPDATE m8_tgt.m8_data SET amount = 999.0000000001 WHERE id = 2;
UPDATE m8_tgt.m8_data SET happened_at = happened_at + INTERVAL 1 HOUR WHERE id = 4;
UPDATE m8_tgt.m8_data SET note = 'x' WHERE id = 9;
UPDATE m8_tgt.m8_data SET note = NULL WHERE id = 10;
INSERT INTO m8_tgt.m8_data (id, name, amount, payload, happened_at, note) VALUES
    (21, 'extra-21', 21, NULL, '2026-02-01 00:00:00', 'extra'),
    (22, 'extra-22', 22, X'01', '2026-02-02 00:00:00', NULL);

INSERT INTO m8_src.m8_big (id, val, label)
SELECT i, i % 1000, CONCAT('row-', i)
FROM (WITH RECURSIVE s (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 100000)
      SELECT i FROM s) AS g;
-- Expected: inserts 1 (50000), updates 1 (500), deletes 1 (100001).
INSERT INTO m8_tgt.m8_big SELECT * FROM m8_src.m8_big WHERE id <> 50000;
UPDATE m8_tgt.m8_big SET val = 9999 WHERE id = 500;
INSERT INTO m8_tgt.m8_big (id, val, label) VALUES (100001, 1, 'row-100001');

INSERT INTO m8_src.m8_composite (tenant, code, qty) VALUES
    (1, 'a', 1), (1, 'b', 2), (2, 'a', 3), (2, 'b', 4), (3, 'a', 5), (3, 'b', 6);
INSERT INTO m8_tgt.m8_composite SELECT * FROM m8_src.m8_composite;
UPDATE m8_tgt.m8_composite SET qty = 40 WHERE tenant = 2 AND code = 'b';

INSERT INTO m8_src.m8_nokey (a, b) VALUES (1, 'one'), (2, 'two');
INSERT INTO m8_tgt.m8_nokey SELECT * FROM m8_src.m8_nokey;
-- m8:pair end
