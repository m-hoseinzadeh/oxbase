DROP VIEW [m8_tgt].[active_customers]
GO
ALTER TABLE [m8_tgt].[orders] DROP CONSTRAINT [orders_customer_fk]
GO
DROP TABLE [m8_tgt].[order_lines]
GO
DROP TABLE [m8_tgt].[legacy_notes]
GO
CREATE SEQUENCE [m8_tgt].[invoice_seq] AS bigint START WITH 1000 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9999999 NO CYCLE NO CACHE
GO
CREATE TABLE [m8_tgt].[MixedCase] (
    [Id] int NOT NULL,
    [Label] nvarchar(20) NULL,
    CONSTRAINT [PK_MixedCase] PRIMARY KEY CLUSTERED ([Id])
)
GO
CREATE TABLE [m8_tgt].[order_audit] (
    [id] int NOT NULL,
    [order_id] int NOT NULL,
    [at] datetime2 NULL,
    CONSTRAINT [PK_order_audit] PRIMARY KEY CLUSTERED ([id])
)
GO
CREATE TABLE [m8_tgt].[order_items] (
    [id] int NOT NULL,
    [order_id] int NOT NULL,
    [product_id] int NOT NULL,
    [qty] int NOT NULL,
    CONSTRAINT [PK_order_items] PRIMARY KEY CLUSTERED ([id])
)
GO
ALTER TABLE [m8_tgt].[customers] ADD [email] nvarchar(200) NULL
GO
ALTER TABLE [m8_tgt].[products] ALTER COLUMN [price] decimal(10, 2) NULL
GO
ALTER TABLE [m8_tgt].[select] ADD [order] int NULL
GO
ALTER TABLE [m8_tgt].[order_items] ADD CONSTRAINT [order_items_order_fk] FOREIGN KEY ([order_id]) REFERENCES [m8_tgt].[orders] ([id]) ON DELETE CASCADE
GO
ALTER TABLE [m8_tgt].[order_items] ADD CONSTRAINT [order_items_product_fk] FOREIGN KEY ([product_id]) REFERENCES [m8_tgt].[products] ([id])
GO
ALTER TABLE [m8_tgt].[orders] ADD CONSTRAINT [orders_customer_fk] FOREIGN KEY ([customer_id]) REFERENCES [m8_tgt].[customers] ([id]) ON DELETE CASCADE
GO
CREATE FUNCTION [m8_tgt].[order_total](@p_id int) RETURNS int AS
BEGIN
    RETURN (SELECT SUM(qty) FROM [m8_tgt].[order_items] WHERE order_id = @p_id);
END
GO
CREATE VIEW [m8_tgt].[active_customers] AS
SELECT id, name, email FROM [m8_tgt].[customers]
GO
CREATE TRIGGER [m8_tgt].[orders_audit] ON [m8_tgt].[orders] AFTER INSERT AS
INSERT INTO [m8_tgt].[order_audit] (order_id) SELECT id FROM inserted
GO
