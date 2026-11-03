SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
DELETE FROM [dbo].[m8_t_dsync_t_data] OUTPUT DELETED.[id] INTO @k WHERE [id] = 21;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
DELETE FROM [dbo].[m8_t_dsync_t_data] OUTPUT DELETED.[id] INTO @k WHERE [id] = 22;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
UPDATE [dbo].[m8_t_dsync_t_data] SET [name] = N'grüße 3', [amount] = 1234567890123456789012345678.0123456789, [payload] = NULL, [happened_at] = N'2026-03-01T10:00:00.1234560+05:30', [note] = NULL OUTPUT INSERTED.[id] INTO @k WHERE [id] = 3;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
UPDATE [dbo].[m8_t_dsync_t_data] SET [name] = N'😀 5', [amount] = 1234567890123456789012345678.0123456789, [payload] = NULL, [happened_at] = N'2026-03-01T10:00:00.1234560+05:30', [note] = NULL OUTPUT INSERTED.[id] INTO @k WHERE [id] = 5;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
UPDATE [dbo].[m8_t_dsync_t_data] SET [name] = N'日本 7', [amount] = 1234567890123456789012345678.0123456789, [payload] = NULL, [happened_at] = N'2026-03-01T10:00:00.1234560+05:30', [note] = NULL OUTPUT INSERTED.[id] INTO @k WHERE [id] = 7;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
UPDATE [dbo].[m8_t_dsync_t_data] SET [name] = N'grüße 9', [amount] = 1234567890123456789012345678.0123456789, [payload] = NULL, [happened_at] = N'2026-03-01T10:00:00.1234560+05:30', [note] = NULL OUTPUT INSERTED.[id] INTO @k WHERE [id] = 9;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
UPDATE [dbo].[m8_t_dsync_t_data] SET [name] = N'日本 10', [amount] = 1234567890123456789012345678.0123456789, [payload] = 0x00ff10, [happened_at] = N'2026-03-01T10:00:00.1234560+05:30', [note] = N'y' OUTPUT INSERTED.[id] INTO @k WHERE [id] = 10;
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
INSERT INTO [dbo].[m8_t_dsync_t_data] ([id], [name], [amount], [payload], [happened_at], [note]) OUTPUT INSERTED.[id] INTO @k VALUES (19, N'日本 19', 1234567890123456789012345678.0123456789, NULL, N'2026-03-01T10:00:00.1234560+05:30', NULL);
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
SET NOCOUNT ON;
DECLARE @k TABLE ([id] bigint);
INSERT INTO [dbo].[m8_t_dsync_t_data] ([id], [name], [amount], [payload], [happened_at], [note]) OUTPUT INSERTED.[id] INTO @k VALUES (20, N'😀 20', -1234567890123456789012345678.0123456789, 0x00ff10, N'2026-03-01T10:00:00.1234560+05:30', NULL);
SELECT k.* FROM @k AS k;
SET NOCOUNT OFF;
GO
