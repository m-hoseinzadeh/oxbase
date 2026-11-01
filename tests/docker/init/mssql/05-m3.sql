-- M3 seed objects for grid editing (SQL Server). Names and shapes come from tests/fixtures.toml
-- ([m3] and [m3.mssql]): editable seed tables m3_edit_pk / m3_edit_uq, the read-only
-- m3_edit_nokey and m3_edit_view, all in schema m3 of oxbase_test, plus the audit table that
-- m3_edit_pk's trigger writes to. Round-trip scratch tables (m3_rt_<kind>_<token>) are created
-- and dropped by the tests in the same schema.
-- Applied once by entrypoint.sh (sqlcmd as sa) on a fresh oxbase_test; live edit tests never
-- change seed rows 1-3: they insert their own rows and delete them.

-- sqlcmd connects with QUOTED_IDENTIFIER OFF; persisted computed columns, filtered indexes and
-- the trigger below need the ANSI settings the driver sessions use.
SET QUOTED_IDENTIFIER ON;
SET ANSI_NULLS ON;
GO
USE oxbase_test;
GO
CREATE SCHEMA m3;
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description',
    @value = N'M3 grid editing seeds and designer round-trip scratch tables',
    @level0type = N'SCHEMA', @level0name = N'm3';
GO

CREATE TABLE m3.m3_edit_pk (
    [id]      int            IDENTITY(1,1) NOT NULL,
    [code]    varchar(20)    NOT NULL,
    [name]    nvarchar(max)  NULL,
    [amount]  decimal(12,2)  NULL,
    [flag]    bit            NULL,
    [born]    date           NULL,
    [at]      datetime2(6)   NULL,
    [doc]     json           NULL,
    [data]    varbinary(64)  NULL,
    [total]   AS ([amount] * 2) PERSISTED,
    CONSTRAINT m3_edit_pk_pkey PRIMARY KEY ([id]),
    CONSTRAINT m3_edit_pk_code_key UNIQUE ([code])
);
GO

-- Inserted in order, so IDENTITY gives them ids 1, 2 and 3.
INSERT INTO m3.m3_edit_pk ([code], [name], [amount], [flag], [born], [at], [doc], [data]) VALUES
    ('a', N'Alpha', 10.50, 1, '2020-01-02', '2020-01-02T03:04:05.123456', N'{"k": 1}', 0xCAFE);
INSERT INTO m3.m3_edit_pk ([code], [name], [amount], [flag], [born], [at], [doc], [data]) VALUES
    ('b', N'Beta', 0.00, 0, '1999-12-31', '1999-12-31T23:59:59', N'[1, 2]', 0x);
INSERT INTO m3.m3_edit_pk ([code]) VALUES ('c');
GO

-- One row per affected row of m3_edit_pk, written by the trigger below.
CREATE TABLE m3.m3_edit_audit (
    [id]       int        IDENTITY(1,1) NOT NULL,
    [edit_id]  int        NULL,
    [op]       char(1)    NULL,
    [at]       datetime2  NULL CONSTRAINT df_m3_edit_audit_at DEFAULT SYSDATETIME(),
    CONSTRAINT m3_edit_audit_pkey PRIMARY KEY ([id])
);
GO

-- Deliberately WITHOUT SET NOCOUNT ON: its row counts must not change what the grid writer
-- counts, and with an enabled trigger OUTPUT needs INTO (docs/m4-contracts.md §4.5).
CREATE TRIGGER m3.m3_edit_pk_audit ON m3.m3_edit_pk
AFTER INSERT, UPDATE, DELETE
AS
BEGIN
    INSERT INTO m3.m3_edit_audit ([edit_id], [op])
    SELECT i.[id], CASE WHEN EXISTS (SELECT 1 FROM deleted) THEN 'U' ELSE 'I' END
    FROM inserted AS i;
    INSERT INTO m3.m3_edit_audit ([edit_id], [op])
    SELECT d.[id], 'D'
    FROM deleted AS d
    WHERE NOT EXISTS (SELECT 1 FROM inserted);
END;
GO

CREATE TABLE m3.m3_edit_uq (
    [code]  varchar(20)  NOT NULL,
    [val]   int          NULL,
    CONSTRAINT m3_edit_uq_code_key UNIQUE ([code])
);
GO
INSERT INTO m3.m3_edit_uq ([code], [val]) VALUES ('x', 1), ('y', 2);
GO

CREATE TABLE m3.m3_edit_nokey (
    [a]  int            NULL,
    [b]  nvarchar(max)  NULL,
    [u]  int            NULL,
    CONSTRAINT m3_edit_nokey_u_key UNIQUE ([u])
);
GO
INSERT INTO m3.m3_edit_nokey ([a], [b], [u]) VALUES (1, N'one', 1);
GO

CREATE VIEW m3.m3_edit_view AS
SELECT [id], [code] FROM m3.m3_edit_pk;
GO
