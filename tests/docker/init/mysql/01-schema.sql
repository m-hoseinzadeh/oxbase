-- oxbase_test fixture schema (MySQL 8.4). Keep in step with the other engines' init scripts.
-- MySQL has no sequences: orders.order_no is a plain column filled explicitly by the seed, and
-- identity is AUTO_INCREMENT.
-- Runs once, as root, in MYSQL_DATABASE (oxbase_test), on an empty data volume.
-- Tables pin COLLATE=utf8mb4_0900_ai_ci explicitly (same as --collation-server in compose.yml).
-- Constraint names match the other engines EXCEPT the primary keys: MySQL/MariaDB always name
-- the PK constraint `PRIMARY`, so pk_customers / pk_orders below are accepted but not kept.
-- Conformance assertions must treat `PRIMARY` as equal to pk_<table> on MySQL and MariaDB.

CREATE TABLE customers (
    id          int          NOT NULL AUTO_INCREMENT,
    email       varchar(255) NOT NULL,
    name        varchar(100) NOT NULL,
    created_at  datetime     NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  datetime     NULL,
    CONSTRAINT pk_customers PRIMARY KEY (id),
    CONSTRAINT uq_customers_email UNIQUE (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

CREATE TABLE orders (
    id           int           NOT NULL AUTO_INCREMENT,
    order_no     int           NOT NULL,
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
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

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
