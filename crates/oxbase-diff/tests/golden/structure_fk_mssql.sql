ALTER TABLE [m8_tgt].[orders] DROP CONSTRAINT [orders_customer_fk]
GO
ALTER TABLE [m8_tgt].[customers] DROP CONSTRAINT [pk_customers]
GO
ALTER TABLE [m8_tgt].[customers] ALTER COLUMN [id] bigint NOT NULL
GO
ALTER TABLE [m8_tgt].[customers] ADD CONSTRAINT [pk_customers] PRIMARY KEY CLUSTERED ([id])
GO
ALTER TABLE [m8_tgt].[orders] ALTER COLUMN [customer_id] bigint NOT NULL
GO
ALTER TABLE [m8_tgt].[orders] ADD CONSTRAINT [orders_customer_fk] FOREIGN KEY ([customer_id]) REFERENCES [m8_tgt].[customers] ([id])
GO
