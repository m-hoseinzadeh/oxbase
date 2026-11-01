-- M3 seed objects for grid editing (SQL Server, M4). Names and shapes come from
-- tests/fixtures.toml ([m3] and [m3.mssql]): editable seed tables m3_edit_pk / m3_edit_uq, the
-- read-only m3_edit_nokey and m3_edit_view, all in schema m3, plus the audit table and the
-- trigger on m3_edit_pk that inserts one audit row per affected row WITHOUT `SET NOCOUNT ON`
-- (its row counts and the OUTPUT restriction on tables with triggers must not break the grid
-- writer's "exactly one row" check, docs/m4-contracts.md section 4.5). Round-trip scratch tables
-- (m3_rt_<kind>_<token>) are created and dropped by the tests in the same schema.
-- Applied by entrypoint.sh with sqlcmd (-b -I) as sa, in name order, on a fresh oxbase_test.
-- Live edit tests never change seed rows 1-3: they insert their own rows and delete them.

SET ANSI_NULLS ON;
SET QUOTED_IDENTIFIER ON;
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
    id      int            IDENTITY(1,1) NOT NULL,
    code    varchar(20)    NOT NULL,
    name    nvarchar(max)  NULL,
    amount  decimal(12,2)  NULL,
    flag    bit            NULL,
    born    date           NULL,
    [at]    datetime2(6)   NULL,
    doc     json           NULL,
    data    varbinary(64)  NULL,
    total   AS (amount * 2) PERSISTED,
    CONSTRAINT m3_edit_pk_pkey PRIMARY KEY (id),
    CONSTRAINT m3_edit_pk_code_key UNIQUE (code)
);
GO

-- Seed rows 1-3 in order (identity values 1, 2, 3); the next generated id is 4.
SET NOCOUNT ON;
INSERT INTO m3.m3_edit_pk (code, name, amount, flag, born, [at], doc, data) VALUES
    ('a', N'Alpha', 10.50, 1, '2020-01-02', '2020-01-02T03:04:05.123456', N'{"k": 1}', 0xCAFE);
INSERT INTO m3.m3_edit_pk (code, name, amount, flag, born, [at], doc, data) VALUES
    ('b', N'Beta', 0.00, 0, '1999-12-31', '1999-12-31T23:59:59', N'[1, 2]', 0x);
INSERT INTO m3.m3_edit_pk (code) VALUES ('c');
GO

CREATE TABLE m3.m3_edit_audit (
    id       int        IDENTITY(1,1) NOT NULL,
    edit_id  int        NULL,
    op       char(1)    NULL,
    [at]     datetime2  NULL CONSTRAINT m3_edit_audit_at_df DEFAULT (SYSDATETIME()),
    CONSTRAINT m3_edit_audit_pkey PRIMARY KEY (id)
);
GO

-- Deliberately without SET NOCOUNT ON: each audit INSERT adds its own row count.
CREATE TRIGGER m3.m3_edit_pk_audit
ON m3.m3_edit_pk
AFTER INSERT, UPDATE, DELETE
AS
BEGIN
    INSERT INTO m3.m3_edit_audit (edit_id, op)
    SELECT i.id, CASE WHEN EXISTS (SELECT 1 FROM deleted) THEN 'U' ELSE 'I' END
      FROM inserted i;
    INSERT INTO m3.m3_edit_audit (edit_id, op)
    SELECT d.id, 'D'
      FROM deleted d
     WHERE NOT EXISTS (SELECT 1 FROM inserted);
END;
GO

CREATE TABLE m3.m3_edit_uq (
    code  varchar(20)  NOT NULL,
    val   int          NULL,
    CONSTRAINT m3_edit_uq_code_key UNIQUE (code)
);
GO
INSERT INTO m3.m3_edit_uq (code, val) VALUES ('x', 1), ('y', 2);
GO

CREATE TABLE m3.m3_edit_nokey (
    a  int            NULL,
    b  nvarchar(max)  NULL,
    u  int            NULL,
    CONSTRAINT m3_edit_nokey_u_key UNIQUE (u)
);
GO
INSERT INTO m3.m3_edit_nokey (a, b, u) VALUES (1, N'one', 1);
GO

CREATE VIEW m3.m3_edit_view AS
SELECT id, code FROM m3.m3_edit_pk;
GO
