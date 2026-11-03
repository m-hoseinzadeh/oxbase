ALTER TABLE "m8_tgt"."orders" DROP CONSTRAINT "orders_customer_fk";
ALTER TABLE "m8_tgt"."customers" ALTER COLUMN "id" TYPE bigint USING "id"::bigint;
ALTER TABLE "m8_tgt"."orders" ALTER COLUMN "customer_id" TYPE bigint USING "customer_id"::bigint;
ALTER TABLE "m8_tgt"."orders" ADD CONSTRAINT "orders_customer_fk" FOREIGN KEY ("customer_id") REFERENCES "m8_tgt"."customers" ("id");
