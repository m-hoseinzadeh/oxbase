PRAGMA foreign_keys;
PRAGMA foreign_keys = OFF;
PRAGMA legacy_alter_table;
DROP VIEW "main"."active_customers";
DROP TABLE "main"."order_lines";
DROP TABLE "main"."legacy_notes";
CREATE TABLE "main"."MixedCase" (
    "Id" INT4 NOT NULL,
    "Label" VARCHAR(20),
    PRIMARY KEY ("Id")
);
CREATE TABLE "main"."order_audit" (
    "id" INT4 NOT NULL,
    "order_id" INT4 NOT NULL,
    "at" DATETIME,
    PRIMARY KEY ("id")
);
CREATE TABLE "main"."order_items" (
    "id" INT4 NOT NULL,
    "order_id" INT4 NOT NULL,
    "product_id" INT4 NOT NULL,
    "qty" INT4 NOT NULL,
    PRIMARY KEY ("id"),
    CONSTRAINT "order_items_order_fk" FOREIGN KEY ("order_id") REFERENCES "orders" ("id") ON DELETE CASCADE,
    CONSTRAINT "order_items_product_fk" FOREIGN KEY ("product_id") REFERENCES "products" ("id")
);
ALTER TABLE "main"."customers" ADD COLUMN "email" VARCHAR(200);
SELECT name FROM "main".sqlite_master WHERE type = 'trigger' AND lower(tbl_name) = 'orders';
CREATE TABLE "main"."orders__oxbase_new" (
    "id" INT4 NOT NULL,
    "customer_id" INT4 NOT NULL,
    "total" DECIMAL(12,2),
    PRIMARY KEY ("id"),
    CONSTRAINT "orders_customer_fk" FOREIGN KEY ("customer_id") REFERENCES "customers" ("id") ON DELETE CASCADE
);
INSERT INTO "main"."orders__oxbase_new" (rowid, "id", "customer_id", "total") SELECT rowid, "id", "customer_id", "total" FROM "main"."orders";
DROP TABLE "main"."orders";
PRAGMA legacy_alter_table = ON;
ALTER TABLE "main"."orders__oxbase_new" RENAME TO "orders";
SELECT * FROM pragma_foreign_key_check(NULL, 'main') WHERE "table" = 'orders' COLLATE NOCASE OR parent = 'orders' COLLATE NOCASE;
SELECT name FROM "main".sqlite_master WHERE type = 'trigger' AND lower(tbl_name) = 'products';
CREATE TABLE "main"."products__oxbase_new" (
    "id" INT4 NOT NULL,
    "name" VARCHAR(100) NOT NULL,
    "price" DECIMAL(10,2),
    PRIMARY KEY ("id")
);
INSERT INTO "main"."products__oxbase_new" (rowid, "id", "name", "price") SELECT rowid, "id", "name", "price" FROM "main"."products";
DROP TABLE "main"."products";
PRAGMA legacy_alter_table = ON;
ALTER TABLE "main"."products__oxbase_new" RENAME TO "products";
SELECT * FROM pragma_foreign_key_check(NULL, 'main') WHERE "table" = 'products' COLLATE NOCASE OR parent = 'products' COLLATE NOCASE;
ALTER TABLE "main"."select" ADD COLUMN "order" INT4;
CREATE VIEW "main"."active_customers" AS SELECT id, name, email FROM customers;
CREATE TRIGGER orders_audit AFTER INSERT ON orders BEGIN INSERT INTO order_audit (order_id) VALUES (NEW.id); END;
-- only when fk = 1: PRAGMA foreign_keys = ON
-- only when legacy_alter = 0: PRAGMA legacy_alter_table = OFF
-- only when legacy_alter = 1: PRAGMA legacy_alter_table = ON
