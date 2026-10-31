-- oxbase_test fixture schema (MariaDB 11.8). Keep in step with the other engines' init scripts.
-- MariaDB supports sequences: orders.order_no defaults to NEXT VALUE FOR order_no_seq;
-- identity is AUTO_INCREMENT.
-- Runs once, as root, in MARIADB_DATABASE (oxbase_test), on an empty data volume.
-- order_no_seq is NOCACHE: MariaDB's default CACHE 1000 loses the cached block on every server
-- restart (the image restarts the server after init), which would make the next value 2000,
-- 3000, ... instead of 1005 as on the other engines.
-- Tables pin COLLATE=utf8mb4_unicode_ci explicitly: a table-level DEFAULT CHARSET alone resolves
-- to MariaDB 11.8's default collation for the charset (uca1400), ignoring --collation-server.
-- Constraint names match the other engines EXCEPT the primary keys: MySQL/MariaDB always name
-- the PK constraint `PRIMARY`, so pk_customers / pk_orders below are accepted but not kept.
-- Conformance assertions must treat `PRIMARY` as equal to pk_<table> on MySQL and MariaDB.

CREATE SEQUENCE order_no_seq START WITH 1000 INCREMENT BY 1 NOCACHE;

CREATE TABLE customers (
    id          int          NOT NULL AUTO_INCREMENT,
    email       varchar(255) NOT NULL,
    name        varchar(100) NOT NULL,
    created_at  datetime     NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  datetime     NULL,
    CONSTRAINT pk_customers PRIMARY KEY (id),
    CONSTRAINT uq_customers_email UNIQUE (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE orders (
    id           int           NOT NULL AUTO_INCREMENT,
    order_no     int           NOT NULL DEFAULT (NEXT VALUE FOR order_no_seq),
    customer_id  int           NOT NULL,
    status       varchar(20)   NOT NULL DEFAULT 'new',
    total        decimal(12,2) NOT NULL DEFAULT 0,
    ordered_at   datetime      NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT pk_orders PRIMARY KEY (id),
    CONSTRAINT uq_orders_order_no UNIQUE (order_no),
    -- Declared before the FK so InnoDB uses it instead of auto-creating its own.
    INDEX idx_orders_customer_id (customer_id),
    CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id)
        REFERENCES customers (id) ON DELETE CASCADE,
    CONSTRAINT ck_orders_total_nonneg CHECK (total >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE VIEW v_customer_order_totals AS
SELECT c.id                      AS customer_id,
       c.name                    AS name,
       COUNT(o.id)               AS order_count,
       COALESCE(SUM(o.total), 0) AS total_spent
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
GROUP BY c.id, c.name;

CREATE TRIGGER trg_customers_updated_at
    BEFORE UPDATE ON customers
    FOR EACH ROW SET NEW.updated_at = CURRENT_TIMESTAMP;
