DROP VIEW "m8_tgt"."active_customers";
ALTER TABLE "m8_tgt"."orders" DROP CONSTRAINT "orders_customer_fk";
DROP TABLE "m8_tgt"."order_lines";
DROP TABLE "m8_tgt"."legacy_notes";
CREATE SEQUENCE "m8_tgt"."invoice_seq" AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1000 CACHE 1 NO CYCLE;
CREATE TABLE "m8_tgt"."MixedCase" (
    "Id" integer NOT NULL,
    "Label" character varying(20),
    PRIMARY KEY ("Id")
);
CREATE TABLE "m8_tgt"."order_audit" (
    "id" integer NOT NULL,
    "order_id" integer NOT NULL,
    "at" timestamp without time zone,
    PRIMARY KEY ("id")
);
CREATE TABLE "m8_tgt"."order_items" (
    "id" integer NOT NULL,
    "order_id" integer NOT NULL,
    "product_id" integer NOT NULL,
    "qty" integer NOT NULL,
    PRIMARY KEY ("id")
);
ALTER TABLE "m8_tgt"."customers" ADD COLUMN "email" character varying(200);
ALTER TABLE "m8_tgt"."products" ALTER COLUMN "price" TYPE numeric(10,2) USING "price"::numeric(10,2);
ALTER TABLE "m8_tgt"."select" ADD COLUMN "order" integer;
ALTER TABLE "m8_tgt"."order_items" ADD CONSTRAINT "order_items_order_fk" FOREIGN KEY ("order_id") REFERENCES "m8_tgt"."orders" ("id") ON DELETE CASCADE;
ALTER TABLE "m8_tgt"."order_items" ADD CONSTRAINT "order_items_product_fk" FOREIGN KEY ("product_id") REFERENCES "m8_tgt"."products" ("id");
ALTER TABLE "m8_tgt"."orders" ADD CONSTRAINT "orders_customer_fk" FOREIGN KEY ("customer_id") REFERENCES "m8_tgt"."customers" ("id") ON DELETE CASCADE;
CREATE FUNCTION "m8_tgt"."order_total"("p_id" integer)
 RETURNS numeric
 LANGUAGE plpgsql STABLE PARALLEL UNSAFE
AS $body$
BEGIN
  RETURN (SELECT sum(qty) FROM m8_tgt.order_items WHERE order_id = p_id);
END
$body$;
CREATE VIEW "m8_tgt"."active_customers" AS
SELECT customers.id,
    customers.name,
    customers.email
   FROM m8_tgt.customers;
CREATE TRIGGER orders_audit AFTER INSERT ON m8_tgt.orders FOR EACH ROW EXECUTE FUNCTION m8_tgt.audit_order();
