-- oxbase_test fixture schema (SQL Server 2025). Keep in step with the other engines' init scripts:
-- customers, orders (FK), idx_orders_customer_id, v_customer_order_totals, order_no_seq,
-- IDENTITY columns and trg_customers_updated_at.
-- Applied by entrypoint.sh with sqlcmd as sa, only when oxbase_test lacks the seed-complete marker
-- (a partially seeded oxbase_test is dropped and rebuilt first).

CREATE DATABASE oxbase_test;
GO
USE oxbase_test;
GO

-- NO CACHE so an unclean stop (kill, OOM) cannot make the next value jump past 1005.
CREATE SEQUENCE dbo.order_no_seq AS int START WITH 1000 INCREMENT BY 1 NO CACHE;
GO

CREATE TABLE dbo.customers (
    id          int          IDENTITY(1,1) NOT NULL,
    email       varchar(255) NOT NULL,
    name        varchar(100) NOT NULL,
    created_at  datetime2(0) NOT NULL CONSTRAINT df_customers_created_at DEFAULT SYSDATETIME(),
    updated_at  datetime2(0) NULL,
    CONSTRAINT pk_customers PRIMARY KEY (id),
    CONSTRAINT uq_customers_email UNIQUE (email)
);
GO

CREATE TABLE dbo.orders (
    id           int           IDENTITY(1,1) NOT NULL,
    order_no     int           NOT NULL CONSTRAINT df_orders_order_no DEFAULT (NEXT VALUE FOR dbo.order_no_seq),
    customer_id  int           NOT NULL,
    status       varchar(20)   NOT NULL CONSTRAINT df_orders_status DEFAULT 'new',
    total        decimal(12,2) NOT NULL CONSTRAINT df_orders_total DEFAULT 0,
    ordered_at   datetime2(0)  NOT NULL CONSTRAINT df_orders_ordered_at DEFAULT SYSDATETIME(),
    CONSTRAINT pk_orders PRIMARY KEY (id),
    CONSTRAINT uq_orders_order_no UNIQUE (order_no),
    CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id)
        REFERENCES dbo.customers (id) ON DELETE CASCADE,
    CONSTRAINT ck_orders_total_nonneg CHECK (total >= 0)
);
GO

CREATE INDEX idx_orders_customer_id ON dbo.orders (customer_id);
GO

CREATE VIEW dbo.v_customer_order_totals AS
SELECT c.id                      AS customer_id,
       c.name                    AS name,
       COUNT(o.id)               AS order_count,
       COALESCE(SUM(o.total), 0) AS total_spent
FROM dbo.customers c
LEFT JOIN dbo.orders o ON o.customer_id = c.id
GROUP BY c.id, c.name;
GO

-- SQL Server has no BEFORE triggers: stamp updated_at after the update instead.
-- RECURSIVE_TRIGGERS is OFF by default, so the inner UPDATE does not re-fire it.
CREATE TRIGGER dbo.trg_customers_updated_at
ON dbo.customers
AFTER UPDATE
AS
BEGIN
    SET NOCOUNT ON;
    UPDATE c
       SET updated_at = SYSDATETIME()
      FROM dbo.customers c
      JOIN inserted i ON i.id = c.id;
END;
GO
