-- M2 seed objects for the object tree and the SQL Server fetchers (docs/m4-contracts.md §5.3).
-- Names come from tests/fixtures.toml ([m2] and [m2.mssql]); the shape is the cross-engine
-- contract written there, with the SQL Server differences listed under [m2.mssql]:
-- note is nvarchar(max), the check is len(code) > 0, comments are MS_Description extended
-- properties, m2_trg is AFTER INSERT, m2_proc(@n int) runs SELECT @n AS n.
-- Applied by entrypoint.sh with sqlcmd as sa (it starts in master), on a fresh oxbase_test only.
-- Also creates the second database oxbase_m2_other with dbo.m2_other_t.

USE oxbase_test;
GO

CREATE SCHEMA m2;
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'M2 seed objects',
    @level0type = N'SCHEMA', @level0name = N'm2';
GO

CREATE TABLE m2.m2_parent (
    id    int           NOT NULL,
    code  varchar(20)   NOT NULL,
    note  nvarchar(max) NULL,
    CONSTRAINT m2_parent_pkey PRIMARY KEY (id),
    CONSTRAINT m2_parent_code_key UNIQUE (code),
    CONSTRAINT m2_parent_code_chk CHECK (len(code) > 0)
);
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'M2 parent table',
    @level0type = N'SCHEMA', @level0name = N'm2', @level1type = N'TABLE', @level1name = N'm2_parent';
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Business code',
    @level0type = N'SCHEMA', @level0name = N'm2', @level1type = N'TABLE', @level1name = N'm2_parent',
    @level2type = N'COLUMN', @level2name = N'code';
GO

CREATE TABLE m2.m2_child (
    id         int NOT NULL,
    parent_id  int NOT NULL,
    qty        int NOT NULL CONSTRAINT m2_child_qty_df DEFAULT 1,
    CONSTRAINT m2_child_pkey PRIMARY KEY (id),
    CONSTRAINT m2_child_parent_fk FOREIGN KEY (parent_id)
        REFERENCES m2.m2_parent (id) ON DELETE CASCADE,
    CONSTRAINT m2_child_qty_chk CHECK (qty >= 0)
);
GO

CREATE INDEX m2_child_parent_idx ON m2.m2_child (parent_id);
GO

CREATE VIEW m2.m2_view AS
SELECT c.id, c.qty, p.code
FROM m2.m2_child c
JOIN m2.m2_parent p ON p.id = c.parent_id;
GO

-- SQL Server has no BEFORE triggers: fix negative quantities after the insert. The check
-- constraint rejects negative rows before the trigger runs, so this only matters when the
-- check is disabled; it keeps the cross-engine intent of the seed.
CREATE TRIGGER m2.m2_trg
ON m2.m2_child
AFTER INSERT
AS
BEGIN
    SET NOCOUNT ON;
    UPDATE c
       SET qty = 0
      FROM m2.m2_child c
      JOIN inserted i ON i.id = c.id
     WHERE i.qty < 0;
END;
GO

CREATE FUNCTION m2.m2_fn(@n int) RETURNS int
AS
BEGIN
    RETURN @n + 1;
END;
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Adds one',
    @level0type = N'SCHEMA', @level0name = N'm2', @level1type = N'FUNCTION', @level1name = N'm2_fn';
GO

CREATE PROCEDURE m2.m2_proc @n int
AS
BEGIN
    SELECT @n AS n;
END;
GO

CREATE SEQUENCE m2.m2_seq AS bigint START WITH 100 INCREMENT BY 5;
GO

-- The second database: proves the tree reads another database through a session on it.
USE master;
GO
IF DB_ID(N'oxbase_m2_other') IS NOT NULL
BEGIN
    ALTER DATABASE oxbase_m2_other SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE oxbase_m2_other;
END;
GO
CREATE DATABASE oxbase_m2_other;
GO
USE oxbase_m2_other;
GO
CREATE TABLE dbo.m2_other_t (
    id int NOT NULL,
    CONSTRAINT m2_other_t_pkey PRIMARY KEY (id)
);
GO
USE oxbase_test;
GO
