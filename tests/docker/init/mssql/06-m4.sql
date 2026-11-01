-- M4 SQL Server specifics (docs/m4-contracts.md section 5.3). Names and shapes come from
-- tests/fixtures.toml [m4] and [m4.mssql]: the type table for the Arrow mapping, a table with
-- identity, computed, rowversion, named defaults, filtered / included-column indexes and
-- comments, a system-versioned table, an indexed view with an INSTEAD OF trigger, table-valued
-- functions, alias and table types, a procedure with several result sets and a database-level
-- DDL trigger (which must not show up under any table).
-- Applied by entrypoint.sh with sqlcmd (-b -I) as sa, in name order, on a fresh oxbase_test.
--
-- The file is ASCII only, so sqlcmd's input code page cannot garble it: non-ASCII values are
-- built with CHAR (code page 1252 of the database collation) and NCHAR (UTF-16 code units, a
-- surrogate pair for U+1F600).
--
-- Indexed views and filtered indexes need these SET options when they are created (and on
-- every session that changes the tables they cover); sqlcmd's defaults differ (QUOTED_IDENTIFIER
-- OFF without -I), so they are set explicitly.

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

CREATE SCHEMA m4;
GO

-- One column per type family. Row 1: the literals of section 5.3; row 2: everything but id NULL.
CREATE TABLE m4.m4_types (
    id               int               NOT NULL,
    c_tinyint        tinyint           NULL,
    c_smallint       smallint          NULL,
    c_int            int               NULL,
    c_bigint         bigint            NULL,
    c_bit            bit               NULL,
    c_decimal        decimal(18,4)     NULL,
    c_numeric        numeric(38,10)    NULL,
    c_money          money             NULL,
    c_smallmoney     smallmoney        NULL,
    c_float          float             NULL,
    c_real           real              NULL,
    c_date           date              NULL,
    c_time           time(7)           NULL,
    c_datetime       datetime          NULL,
    c_datetime2      datetime2(7)      NULL,
    c_smalldatetime  smalldatetime     NULL,
    c_dto            datetimeoffset(7) NULL,
    c_char           char(10)          NULL,
    c_varchar        varchar(50)       NULL,
    c_varchar_max    varchar(max)      NULL,
    c_nchar          nchar(10)         NULL,
    c_nvarchar       nvarchar(50)      NULL,
    c_nvarchar_max   nvarchar(max)     NULL,
    c_text           text              NULL,
    c_ntext          ntext             NULL,
    c_binary         binary(16)        NULL,
    c_varbinary      varbinary(64)     NULL,
    c_varbinary_max  varbinary(max)    NULL,
    c_image          image             NULL,
    c_guid           uniqueidentifier  NULL,
    c_xml            xml               NULL,
    c_json           json              NULL,
    c_vector         vector(3)         NULL,
    c_variant        sql_variant       NULL,
    c_hierarchyid    hierarchyid       NULL,
    c_geometry       geometry          NULL,
    c_geography      geography         NULL,
    c_rowversion     rowversion,
    CONSTRAINT pk_m4_types PRIMARY KEY (id)
);
GO

SET NOCOUNT ON;
INSERT INTO m4.m4_types (
    id, c_tinyint, c_smallint, c_int, c_bigint, c_bit, c_decimal, c_numeric, c_money,
    c_smallmoney, c_float, c_real, c_date, c_time, c_datetime, c_datetime2, c_smalldatetime,
    c_dto, c_char, c_varchar, c_varchar_max, c_nchar, c_nvarchar, c_nvarchar_max, c_text,
    c_ntext, c_binary, c_varbinary, c_varbinary_max, c_image, c_guid, c_xml, c_json, c_vector,
    c_variant, c_hierarchyid, c_geometry, c_geography)
VALUES (
    1, 255, -32768, 2147483647, -9223372036854775808, 1, 12345678901234.5678,
    1234567890123456789012345678.0123456789, 922337203685477.5807, -214748.3648, 1.5E300, 3.25,
    '0001-01-01', '23:59:59.1234567', '2026-01-15T10:20:30.997', '9999-12-31T23:59:59.9999999',
    '2026-01-15T10:21:00', '2026-01-15T10:20:30.1234567+03:30', 'abc', 'caf' + CHAR(233),
    REPLICATE(CAST('x' AS varchar(max)), 9000), NCHAR(9731),
    N'Gr' + NCHAR(252) + NCHAR(223) + N'e ' + NCHAR(28450) + NCHAR(23383) + N' '
        + NCHAR(55357) + NCHAR(56832),
    REPLICATE(CAST(N'ab' AS nvarchar(max)), 5000), 'old text', N'old ntext', 0x0102,
    0xCAFEBABE, CAST(REPLICATE(CAST(0xAB AS varbinary(max)), 10000) AS varbinary(max)),
    0xFFD8FF, '6F9619FF-8B86-D011-B42D-00C04FC964FF', N'<a b="1"/>',
    N'{"a":1,"b":[true,null]}', '[1,2,3]', CAST(42 AS int), hierarchyid::Parse('/1/2/'),
    geometry::STGeomFromText('POINT(1 2)', 0), geography::Point(47.65, -122.34, 4326));
INSERT INTO m4.m4_types (id) VALUES (2);
GO

CREATE TABLE m4.m4_features (
    id      int            IDENTITY(100, 5) NOT NULL,
    code    varchar(20)    NULL,
    qty     int            NOT NULL CONSTRAINT df_m4_features_qty DEFAULT (1),
    price   decimal(10,2)  NOT NULL,
    total   AS (qty * price) PERSISTED,
    label   AS (upper(code)),
    rv      rowversion,
    status  nvarchar(10)   NOT NULL CONSTRAINT df_m4_features_status DEFAULT (N'new'),
    CONSTRAINT pk_m4_features PRIMARY KEY CLUSTERED (id),
    CONSTRAINT ck_m4_features_qty CHECK (qty >= 0)
);
GO
CREATE UNIQUE NONCLUSTERED INDEX m4_features_code_uq ON m4.m4_features (code)
    WHERE code IS NOT NULL;
GO
CREATE NONCLUSTERED INDEX m4_features_qty_ix ON m4.m4_features (qty DESC) INCLUDE (price);
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'M4 features',
    @level0type = N'SCHEMA', @level0name = N'm4',
    @level1type = N'TABLE', @level1name = N'm4_features';
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'how many',
    @level0type = N'SCHEMA', @level0name = N'm4',
    @level1type = N'TABLE', @level1name = N'm4_features',
    @level2type = N'COLUMN', @level2name = N'qty';
GO
-- Ids 100 and 105; two rows with a NULL code (the filtered unique index allows them).
INSERT INTO m4.m4_features (code, qty, price) VALUES ('a', 2, 1.50);
INSERT INTO m4.m4_features (code, qty, price) VALUES (NULL, 0, 3.00);
INSERT INTO m4.m4_features (code, qty, price) VALUES (NULL, 4, 0.25);
GO

CREATE TABLE m4.m4_temporal (
    id          int           NOT NULL,
    val         nvarchar(20)  NULL,
    valid_from  datetime2     GENERATED ALWAYS AS ROW START HIDDEN NOT NULL,
    valid_to    datetime2     GENERATED ALWAYS AS ROW END HIDDEN NOT NULL,
    PERIOD FOR SYSTEM_TIME (valid_from, valid_to),
    CONSTRAINT pk_m4_temporal PRIMARY KEY (id)
)
WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = m4.m4_temporal_history));
GO
INSERT INTO m4.m4_temporal (id, val) VALUES (1, N'first');
UPDATE m4.m4_temporal SET val = N'second' WHERE id = 1;
GO

CREATE VIEW m4.m4_iview
WITH SCHEMABINDING
AS
SELECT id, qty
FROM m4.m4_features
WHERE qty > 0;
GO
CREATE UNIQUE CLUSTERED INDEX m4_iview_ix ON m4.m4_iview (id);
GO

-- Listed under the view's Triggers folder.
CREATE TRIGGER m4.m4_view_trg
ON m4.m4_iview
INSTEAD OF INSERT
AS
BEGIN
    SET NOCOUNT ON;
END;
GO

CREATE FUNCTION m4.m4_itvf(@n int)
RETURNS TABLE
AS
RETURN SELECT id, qty FROM m4.m4_features WHERE id > @n;
GO

CREATE FUNCTION m4.m4_mtvf(@n int)
RETURNS @t TABLE (n int)
AS
BEGIN
    INSERT INTO @t (n) VALUES (@n);
    RETURN;
END;
GO

CREATE TYPE m4.m4_code_t FROM varchar(20) NOT NULL;
GO
CREATE TYPE m4.m4_rows_t AS TABLE (
    id    int           NULL,
    name  nvarchar(50)  NULL
);
GO

CREATE PROCEDURE m4.m4_proc_tvp @rows m4.m4_rows_t READONLY
AS
SELECT COUNT(*) AS n FROM @rows;
GO

-- Several result sets; the PRINT / severity-10 messages are a known gap (section 11).
CREATE PROCEDURE m4.m4_messages
AS
BEGIN
    PRINT N'm4 print';
    RAISERROR(N'm4 info', 10, 1);
    SELECT 1 AS a;
    SELECT 2 AS b;
END;
GO

-- A database-level DDL trigger: not a table's trigger, so no Triggers folder may list it.
CREATE TRIGGER m4_ddl_trg
ON DATABASE
FOR CREATE_TABLE
AS
BEGIN
    SET NOCOUNT ON;
END;
GO
