DELETE FROM "public"."m8_t_dsync_t_data" WHERE "id" = 21;
DELETE FROM "public"."m8_t_dsync_t_data" WHERE "id" = 22;
UPDATE "public"."m8_t_dsync_t_data" SET "name" = 'grüße 3', "amount" = 1234567890123456789012345678.0123456789, "payload" = NULL, "happened_at" = '2026-03-01T04:30:00.123456+00:00', "note" = NULL WHERE "id" = 3;
UPDATE "public"."m8_t_dsync_t_data" SET "name" = '😀 5', "amount" = 1234567890123456789012345678.0123456789, "payload" = NULL, "happened_at" = '2026-03-01T04:30:00.123456+00:00', "note" = NULL WHERE "id" = 5;
UPDATE "public"."m8_t_dsync_t_data" SET "name" = '日本 7', "amount" = 1234567890123456789012345678.0123456789, "payload" = NULL, "happened_at" = '2026-03-01T04:30:00.123456+00:00', "note" = NULL WHERE "id" = 7;
UPDATE "public"."m8_t_dsync_t_data" SET "name" = 'grüße 9', "amount" = 1234567890123456789012345678.0123456789, "payload" = NULL, "happened_at" = '2026-03-01T04:30:00.123456+00:00', "note" = NULL WHERE "id" = 9;
UPDATE "public"."m8_t_dsync_t_data" SET "name" = '日本 10', "amount" = 1234567890123456789012345678.0123456789, "payload" = '\x00ff10'::bytea, "happened_at" = '2026-03-01T04:30:00.123456+00:00', "note" = 'y' WHERE "id" = 10;
INSERT INTO "public"."m8_t_dsync_t_data" ("id", "name", "amount", "payload", "happened_at", "note") VALUES (19, '日本 19', 1234567890123456789012345678.0123456789, NULL, '2026-03-01T04:30:00.123456+00:00', NULL);
INSERT INTO "public"."m8_t_dsync_t_data" ("id", "name", "amount", "payload", "happened_at", "note") VALUES (20, '😀 20', -1234567890123456789012345678.0123456789, '\x00ff10'::bytea, '2026-03-01T04:30:00.123456+00:00', NULL);
