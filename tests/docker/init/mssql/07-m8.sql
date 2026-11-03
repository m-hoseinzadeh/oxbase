-- M8 seed (SQL Server 2025): the structure / data sync pair, the restore target and the transfer
-- target of tests/fixtures.toml [m8] / [m8.mssql] (docs/m8-contracts.md section 9), all schemas
-- of oxbase_test:
--
--   m8_src          SOURCE of the pair (the desired state)
--   m8_tgt          TARGET of the pair (what a sync must change)
--   m8_restore      empty; restores land here
--   m8_xfer_target  empty; transfers land here
--
-- Applied by entrypoint.sh with sqlcmd (-b -I) as sa, in name order, on a fresh oxbase_test.
-- Re-runnable: each schema is emptied (foreign keys, views, routines, tables, sequences) and
-- dropped before it is created again.
--
-- The conformance harness (crates/oxbase-conformance/src/m8) builds fresh copies of the batches
-- between `-- m8:pair begin` and `-- m8:pair end` in scratch schemas (`m8_t_<token>_...`): it
-- replaces the whole words m8_src and m8_tgt and runs the batches (split on GO) in order. So
-- every object name is schema-qualified, and the schema names appear only as those words.
--
-- The file is ASCII only, so sqlcmd's input code page cannot garble it: non-ASCII values are
-- built with NCHAR (UTF-16 code units, a surrogate pair for U+1F600). Constraints are named,
-- so both sides of the pair carry the same names.

SET ANSI_NULLS ON;
SET QUOTED_IDENTIFIER ON;
SET ANSI_PADDING ON;
SET ANSI_WARNINGS ON;
SET ARITHABORT ON;
SET CONCAT_NULL_YIELDS_NULL ON;
SET NUMERIC_ROUNDABORT OFF;
GO
USE oxbase_test;
GO

DECLARE @schemas TABLE (name sysname PRIMARY KEY);
INSERT INTO @schemas (name) VALUES (N'm8_restore'), (N'm8_xfer_target');
DECLARE @steps TABLE (seq int IDENTITY(1,1) PRIMARY KEY, stmt nvarchar(max) NOT NULL);
INSERT INTO @steps (stmt)
SELECT N'ALTER TABLE ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name) + N' DROP CONSTRAINT '
       + QUOTENAME(fk.name)
FROM sys.foreign_keys fk
JOIN sys.tables t ON t.object_id = fk.parent_object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
WHERE s.name IN (SELECT name FROM @schemas);
INSERT INTO @steps (stmt)
SELECT N'DROP ' + CASE o.type WHEN 'V' THEN N'VIEW' WHEN 'P' THEN N'PROCEDURE'
                              WHEN 'U' THEN N'TABLE' WHEN 'SO' THEN N'SEQUENCE'
                              ELSE N'FUNCTION' END
       + N' ' + QUOTENAME(s.name) + N'.' + QUOTENAME(o.name)
FROM sys.objects o
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE s.name IN (SELECT name FROM @schemas) AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'U', 'SO')
ORDER BY CASE o.type WHEN 'V' THEN 1 WHEN 'P' THEN 2 WHEN 'U' THEN 4 WHEN 'SO' THEN 5 ELSE 3 END,
         o.name;
INSERT INTO @steps (stmt)
SELECT N'DROP SCHEMA ' + QUOTENAME(name) FROM @schemas WHERE SCHEMA_ID(name) IS NOT NULL;
DECLARE @sql nvarchar(max) = (SELECT STRING_AGG(stmt, N'; ') WITHIN GROUP (ORDER BY seq) FROM @steps);
IF @sql IS NOT NULL EXEC sys.sp_executesql @sql;
GO
CREATE SCHEMA m8_restore;
GO
CREATE SCHEMA m8_xfer_target;
GO

-- m8:pair begin
DECLARE @schemas TABLE (name sysname PRIMARY KEY);
INSERT INTO @schemas (name) VALUES (N'm8_src'), (N'm8_tgt');
DECLARE @steps TABLE (seq int IDENTITY(1,1) PRIMARY KEY, stmt nvarchar(max) NOT NULL);
INSERT INTO @steps (stmt)
SELECT N'ALTER TABLE ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name) + N' DROP CONSTRAINT '
       + QUOTENAME(fk.name)
FROM sys.foreign_keys fk
JOIN sys.tables t ON t.object_id = fk.parent_object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
WHERE s.name IN (SELECT name FROM @schemas);
INSERT INTO @steps (stmt)
SELECT N'DROP ' + CASE o.type WHEN 'V' THEN N'VIEW' WHEN 'P' THEN N'PROCEDURE'
                              WHEN 'U' THEN N'TABLE' WHEN 'SO' THEN N'SEQUENCE'
                              ELSE N'FUNCTION' END
       + N' ' + QUOTENAME(s.name) + N'.' + QUOTENAME(o.name)
FROM sys.objects o
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE s.name IN (SELECT name FROM @schemas) AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'U', 'SO')
ORDER BY CASE o.type WHEN 'V' THEN 1 WHEN 'P' THEN 2 WHEN 'U' THEN 4 WHEN 'SO' THEN 5 ELSE 3 END,
         o.name;
INSERT INTO @steps (stmt)
SELECT N'DROP SCHEMA ' + QUOTENAME(name) FROM @schemas WHERE SCHEMA_ID(name) IS NOT NULL;
DECLARE @sql nvarchar(max) = (SELECT STRING_AGG(stmt, N'; ') WITHIN GROUP (ORDER BY seq) FROM @steps);
IF @sql IS NOT NULL EXEC sys.sp_executesql @sql;
GO
CREATE SCHEMA m8_src;
GO
CREATE SCHEMA m8_tgt;
GO

-- --- structure: source --------------------------------------------------------------------

CREATE TABLE m8_src.customers (
    id     int            IDENTITY(1,1) NOT NULL,
    name   nvarchar(100)  NOT NULL,
    email  nvarchar(200)  NOT NULL,
    phone  nvarchar(30)   NULL,
    CONSTRAINT customers_pkey PRIMARY KEY (id),
    CONSTRAINT customers_email_uq UNIQUE (email)
);

CREATE TABLE m8_src.products (
    id     int            IDENTITY(1,1) NOT NULL,
    name   nvarchar(100)  NOT NULL,
    sku    nvarchar(40)   NOT NULL,
    price  decimal(12,2)  NOT NULL,
    stock  int            NULL CONSTRAINT products_stock_df DEFAULT 0,
    CONSTRAINT products_pkey PRIMARY KEY (id),
    CONSTRAINT products_price_ck CHECK (price >= 0)
);

CREATE TABLE m8_src.orders (
    id           int            IDENTITY(1,1) NOT NULL,
    customer_id  int            NOT NULL,
    created_at   datetime2(0)   NOT NULL,
    total        decimal(12,2)  NULL,
    CONSTRAINT orders_pkey PRIMARY KEY (id),
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES m8_src.customers (id)
);
CREATE INDEX orders_created_ix ON m8_src.orders (customer_id, created_at);

CREATE TABLE m8_src.order_audit (
    id        int           IDENTITY(1,1) NOT NULL,
    order_id  int           NOT NULL,
    action    nvarchar(20)  NOT NULL,
    CONSTRAINT order_audit_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_src.order_items (
    id          int            IDENTITY(1,1) NOT NULL,
    order_id    int            NOT NULL,
    product_id  int            NOT NULL,
    qty         int            NOT NULL,
    unit_price  decimal(12,2)  NOT NULL,
    CONSTRAINT order_items_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_src.[MixedCase] (
    id       int           NOT NULL,
    [Value]  nvarchar(50)  NULL,
    CONSTRAINT [MixedCase_pkey] PRIMARY KEY (id)
);

CREATE TABLE m8_src.[select] (
    [from]      int            NOT NULL,
    [it's]      nvarchar(max)  NULL,
    [say "hi"]  nvarchar(max)  NULL,
    CONSTRAINT select_pkey PRIMARY KEY ([from])
);

CREATE SEQUENCE m8_src.invoice_seq AS bigint START WITH 1000 INCREMENT BY 1;

INSERT INTO m8_src.customers (name, email, phone) VALUES
    (N'Ada', N'ada@example.com', N'+1 555 0100'),
    (N'Linus', N'linus@example.com', NULL);
INSERT INTO m8_src.products (name, sku, price, stock) VALUES
    (N'Widget', N'W-1', 9.50, 10),
    (N'Gadget', N'G-1', 120.00, 0);
GO

CREATE VIEW m8_src.active_customers AS
SELECT id, name, email FROM m8_src.customers WHERE phone IS NOT NULL;
GO

CREATE FUNCTION m8_src.order_total (@p_order int) RETURNS decimal(12,2)
AS
BEGIN
    RETURN (SELECT COALESCE(SUM(qty * unit_price), 0) FROM m8_src.order_items
            WHERE order_id = @p_order);
END;
GO

CREATE TRIGGER m8_src.orders_audit ON m8_src.orders AFTER INSERT
AS
BEGIN
    SET NOCOUNT ON;
    INSERT INTO m8_src.order_audit (order_id, action) SELECT id, N'insert' FROM inserted;
END;
GO

-- Two orders; the orders_audit trigger writes their two order_audit rows.
INSERT INTO m8_src.orders (customer_id, created_at, total) VALUES
    (1, '2026-01-05T10:00:00', 19.00),
    (2, '2026-01-06T11:30:00', 120.00);
GO

-- --- structure: target --------------------------------------------------------------------

CREATE TABLE m8_tgt.customers (
    id     int            IDENTITY(1,1) NOT NULL,
    name   nvarchar(100)  NOT NULL,
    email  nvarchar(200)  NOT NULL,
    fax    nvarchar(30)   NULL,
    CONSTRAINT customers_pkey PRIMARY KEY (id),
    CONSTRAINT customers_email_uq UNIQUE (email)
);

CREATE TABLE m8_tgt.products (
    id     int            IDENTITY(1,1) NOT NULL,
    name   nvarchar(100)  NOT NULL,
    sku    nvarchar(40)   NULL,
    price  decimal(10,2)  NOT NULL,
    stock  int            NULL,
    CONSTRAINT products_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_tgt.orders (
    id           int            IDENTITY(1,1) NOT NULL,
    customer_id  int            NOT NULL,
    created_at   datetime2(0)   NOT NULL,
    total        decimal(12,2)  NULL,
    CONSTRAINT orders_pkey PRIMARY KEY (id)
);
CREATE INDEX orders_created_ix ON m8_tgt.orders (created_at);

CREATE TABLE m8_tgt.order_audit (
    id        int           IDENTITY(1,1) NOT NULL,
    order_id  int           NOT NULL,
    action    nvarchar(20)  NOT NULL,
    CONSTRAINT order_audit_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_tgt.order_lines (
    id          int            IDENTITY(1,1) NOT NULL,
    order_id    int            NOT NULL,
    product_id  int            NOT NULL,
    qty         int            NOT NULL,
    unit_price  decimal(12,2)  NOT NULL,
    CONSTRAINT order_lines_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_tgt.legacy_notes (
    id    int            NOT NULL,
    note  nvarchar(max)  NULL,
    CONSTRAINT legacy_notes_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_tgt.[select] (
    [from]  int            NOT NULL,
    [it's]  nvarchar(max)  NULL,
    CONSTRAINT select_pkey PRIMARY KEY ([from])
);

INSERT INTO m8_tgt.customers (name, email, fax) VALUES
    (N'Ada', N'ada@example.com', NULL),
    (N'Linus', N'linus@example.com', N'+1 555 0199');
INSERT INTO m8_tgt.products (name, sku, price, stock) VALUES
    (N'Widget', N'W-1', 9.50, 10),
    (N'Gadget', N'G-1', 120.00, 0);
-- The same orders and audit rows (no trigger here); a sync adds orders_customer_fk over them.
INSERT INTO m8_tgt.orders (customer_id, created_at, total) VALUES
    (1, '2026-01-05T10:00:00', 19.00),
    (2, '2026-01-06T11:30:00', 120.00);
INSERT INTO m8_tgt.order_audit (order_id, action) VALUES (1, N'insert'), (2, N'insert');
GO

CREATE VIEW m8_tgt.active_customers AS
SELECT id, name, email FROM m8_tgt.customers WHERE 1 = 1;
GO

-- --- data pairs (identical structure on both sides) --------------------------------------
-- happened_at is datetimeoffset(7) with the +05:30 offset.

CREATE TABLE m8_src.m8_data (
    id           bigint             NOT NULL,
    name         nvarchar(100)      NULL,
    amount       decimal(38,10)     NULL,
    payload      varbinary(64)      NULL,
    happened_at  datetimeoffset(7)  NULL,
    note         nvarchar(max)      NULL,
    CONSTRAINT m8_data_pkey PRIMARY KEY (id)
);
CREATE TABLE m8_tgt.m8_data (
    id           bigint             NOT NULL,
    name         nvarchar(100)      NULL,
    amount       decimal(38,10)     NULL,
    payload      varbinary(64)      NULL,
    happened_at  datetimeoffset(7)  NULL,
    note         nvarchar(max)      NULL,
    CONSTRAINT m8_data_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_src.m8_big (
    id     bigint        NOT NULL,
    val    int           NULL,
    label  nvarchar(20)  NULL,
    CONSTRAINT m8_big_pkey PRIMARY KEY (id)
);
CREATE TABLE m8_tgt.m8_big (
    id     bigint        NOT NULL,
    val    int           NULL,
    label  nvarchar(20)  NULL,
    CONSTRAINT m8_big_pkey PRIMARY KEY (id)
);

CREATE TABLE m8_src.m8_composite (
    tenant  int           NOT NULL,
    code    nvarchar(20)  NOT NULL,
    qty     int           NULL,
    CONSTRAINT m8_composite_pkey PRIMARY KEY (tenant, code)
);
CREATE TABLE m8_tgt.m8_composite (
    tenant  int           NOT NULL,
    code    nvarchar(20)  NOT NULL,
    qty     int           NULL,
    CONSTRAINT m8_composite_pkey PRIMARY KEY (tenant, code)
);

CREATE TABLE m8_src.m8_nokey (
    a  int            NULL,
    b  nvarchar(max)  NULL
);
CREATE TABLE m8_tgt.m8_nokey (
    a  int            NULL,
    b  nvarchar(max)  NULL
);
GO

-- Source rows 1..20 (names: gruesse, Japanese, an astral emoji).
WITH g (i) AS (
    SELECT TOP (20) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) FROM sys.all_objects
)
INSERT INTO m8_src.m8_data (id, name, amount, payload, happened_at, note)
SELECT i,
       CASE i WHEN 1 THEN N'gr' + NCHAR(252) + NCHAR(223) + N'e'
              WHEN 2 THEN NCHAR(26085) + NCHAR(26412)
              WHEN 4 THEN NCHAR(55357) + NCHAR(56832)
              ELSE N'name-' + CAST(i AS nvarchar(20)) END,
       CASE i WHEN 1 THEN CAST(1234567890123456789012345678.0123456789 AS decimal(38,10))
              WHEN 2 THEN CAST(-1234567890123456789012345678.0123456789 AS decimal(38,10))
              WHEN 6 THEN CAST(-0.5 AS decimal(38,10))
              ELSE CAST(i * 1.25 AS decimal(38,10)) END,
       CASE i % 3 WHEN 1 THEN 0x00FF10 WHEN 2 THEN NULL ELSE 0x0A0B END,
       DATEADD(day, i - 1, CAST('2026-01-01T10:00:00+05:30' AS datetimeoffset(7))),
       CASE i WHEN 9 THEN NULL WHEN 10 THEN N'y' ELSE N'note ' + CAST(i AS nvarchar(20)) END
FROM g;
-- Values that need quoting in a script: a quote, a backslash, `;` and `--` (id 3 differs, so
-- its UPDATE writes this name) and a quote, double quotes and N'...' (id 19 is inserted).
UPDATE m8_src.m8_data SET name = N'O''Brien \ x;--' WHERE id = 3;
UPDATE m8_src.m8_data SET note = N'it''s "q"; -- N''x''' WHERE id = 19;

-- Target: ids 1..18, 3 / 2 / 4 changed (name / amount / instant; 2 and 4 are edge rows: a
-- 38-digit amount with a Japanese name, an astral emoji name), 9 and 10 swap their NULL
-- notes, 21 / 22 only here. Expected: inserts 2, updates 5, deletes 2.
INSERT INTO m8_tgt.m8_data SELECT * FROM m8_src.m8_data WHERE id <= 18;
UPDATE m8_tgt.m8_data SET name = N'name-3 changed' WHERE id = 3;
UPDATE m8_tgt.m8_data SET amount = 999.0000000001 WHERE id = 2;
UPDATE m8_tgt.m8_data SET happened_at = DATEADD(hour, 1, happened_at) WHERE id = 4;
UPDATE m8_tgt.m8_data SET note = N'x' WHERE id = 9;
UPDATE m8_tgt.m8_data SET note = NULL WHERE id = 10;
INSERT INTO m8_tgt.m8_data (id, name, amount, payload, happened_at, note) VALUES
    (21, N'extra-21', 21, NULL, CAST('2026-02-01T00:00:00+00:00' AS datetimeoffset(7)), N'extra'),
    (22, N'extra-22', 22, 0x01, CAST('2026-02-02T00:00:00+00:00' AS datetimeoffset(7)), NULL);

WITH g (i) AS (
    SELECT TOP (100000) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS bigint)
    FROM sys.all_objects a CROSS JOIN sys.all_objects b
)
INSERT INTO m8_src.m8_big (id, val, label)
SELECT i, CAST(i % 1000 AS int), N'row-' + CAST(i AS nvarchar(20)) FROM g;
-- Expected: inserts 1 (50000), updates 1 (500), deletes 1 (100001).
INSERT INTO m8_tgt.m8_big SELECT * FROM m8_src.m8_big WHERE id <> 50000;
UPDATE m8_tgt.m8_big SET val = 9999 WHERE id = 500;
INSERT INTO m8_tgt.m8_big (id, val, label) VALUES (100001, 1, N'row-100001');

INSERT INTO m8_src.m8_composite (tenant, code, qty) VALUES
    (1, N'a', 1), (1, N'b', 2), (2, N'a', 3), (2, N'b', 4), (3, N'a', 5), (3, N'b', 6);
INSERT INTO m8_tgt.m8_composite SELECT * FROM m8_src.m8_composite;
UPDATE m8_tgt.m8_composite SET qty = 40 WHERE tenant = 2 AND code = N'b';

INSERT INTO m8_src.m8_nokey (a, b) VALUES (1, N'one'), (2, N'two');
INSERT INTO m8_tgt.m8_nokey SELECT * FROM m8_src.m8_nokey;
GO
-- m8:pair end
