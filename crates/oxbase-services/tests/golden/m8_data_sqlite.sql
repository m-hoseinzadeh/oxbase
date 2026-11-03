DELETE FROM "main"."m8_t_dsync_t_data" WHERE "id" = 21;
DELETE FROM "main"."m8_t_dsync_t_data" WHERE "id" = 22;
UPDATE "main"."m8_t_dsync_t_data" SET "name" = 'grüße 3', "amount" = '1234567890123456789012345678.0123456789', "payload" = NULL, "happened_at" = '2026-03-01T10:00:00.123456+05:30', "note" = NULL WHERE "id" = 3;
UPDATE "main"."m8_t_dsync_t_data" SET "name" = '😀 5', "amount" = '1234567890123456789012345678.0123456789', "payload" = NULL, "happened_at" = '2026-03-01T10:00:00.123456+05:30', "note" = NULL WHERE "id" = 5;
UPDATE "main"."m8_t_dsync_t_data" SET "name" = '日本 7', "amount" = '1234567890123456789012345678.0123456789', "payload" = NULL, "happened_at" = '2026-03-01T10:00:00.123456+05:30', "note" = NULL WHERE "id" = 7;
UPDATE "main"."m8_t_dsync_t_data" SET "name" = 'grüße 9', "amount" = '1234567890123456789012345678.0123456789', "payload" = NULL, "happened_at" = '2026-03-01T10:00:00.123456+05:30', "note" = NULL WHERE "id" = 9;
UPDATE "main"."m8_t_dsync_t_data" SET "name" = '日本 10', "amount" = '1234567890123456789012345678.0123456789', "payload" = X'00ff10', "happened_at" = '2026-03-01T10:00:00.123456+05:30', "note" = 'y' WHERE "id" = 10;
INSERT INTO "main"."m8_t_dsync_t_data" ("id", "name", "amount", "payload", "happened_at", "note") VALUES (19, '日本 19', '1234567890123456789012345678.0123456789', NULL, '2026-03-01T10:00:00.123456+05:30', NULL);
INSERT INTO "main"."m8_t_dsync_t_data" ("id", "name", "amount", "payload", "happened_at", "note") VALUES (20, '😀 20', '-1234567890123456789012345678.0123456789', X'00ff10', '2026-03-01T10:00:00.123456+05:30', NULL);
