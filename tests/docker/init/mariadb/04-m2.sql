-- M2 object-tree fixture (MariaDB 11.8): the objects named in tests/fixtures.toml [m2] /
-- [m2.mariadb], read back by the oxbase-mysql dialect fetchers (crates/oxbase-mysql/tests/dialect_live.rs) and
-- the conformance harness. Shape: see the [m2] comment in tests/fixtures.toml.
-- Runs once, as root, in MARIADB_DATABASE (oxbase_test), on an empty data volume, after
-- 01-schema.sql .. 03-tls-user.sql.
--
-- No DELIMITER: the image feeds this file to the mariadb client, which splits on ';'. Every
-- routine, trigger and event body is therefore a single statement (no BEGIN ... END block).
--
-- The view, routines, trigger and event are defined by `oxbase` (the fixture user the tests log in
-- as), because information_schema shows a routine's body (ROUTINES.ROUTINE_DEFINITION) only to its
-- definer or to users with SHOW_ROUTINE / global SELECT.
--
-- Extra objects for the MySQL-family tests only (not in fixtures.toml): m2_doc (a JSON column, which
-- MariaDB stores as LONGTEXT with an automatic CHECK (json_valid(doc))) and the event m2_evt
-- (DISABLE, so it never runs).

CREATE TABLE m2_parent (
    id    int          NOT NULL,
    code  varchar(20)  NOT NULL,
    note  text         NULL,
    CONSTRAINT pk_m2_parent PRIMARY KEY (id),
    CONSTRAINT m2_parent_code_key UNIQUE (code),
    CONSTRAINT m2_parent_code_chk CHECK (char_length(code) > 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='M2 parent table';

CREATE TABLE m2_child (
    id         int  NOT NULL,
    parent_id  int  NOT NULL,
    qty        int  NOT NULL DEFAULT 1,
    CONSTRAINT pk_m2_child PRIMARY KEY (id),
    -- Declared before the FK so InnoDB uses it instead of creating its own.
    INDEX m2_child_parent_idx (parent_id),
    CONSTRAINT m2_child_parent_fk FOREIGN KEY (parent_id)
        REFERENCES m2_parent (id) ON DELETE CASCADE,
    CONSTRAINT m2_child_qty_chk CHECK (qty >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE m2_doc (
    id   int   NOT NULL,
    doc  json  NULL,
    CONSTRAINT pk_m2_doc PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE DEFINER = 'oxbase'@'%' SQL SECURITY DEFINER VIEW m2_view AS
SELECT c.id, c.qty, p.code
FROM m2_child c
JOIN m2_parent p ON p.id = c.parent_id;

CREATE DEFINER = 'oxbase'@'%' TRIGGER m2_trg
    BEFORE INSERT ON m2_child
    FOR EACH ROW SET NEW.qty = IF(NEW.qty IS NULL OR NEW.qty < 0, 0, NEW.qty);

CREATE DEFINER = 'oxbase'@'%' FUNCTION m2_fn(n int) RETURNS int
    DETERMINISTIC
    NO SQL
    COMMENT 'M2 function'
    RETURN n + 1;

CREATE DEFINER = 'oxbase'@'%' PROCEDURE m2_proc(IN n int)
    SELECT n AS n;

CREATE DEFINER = 'oxbase'@'%' EVENT m2_evt
    ON SCHEDULE EVERY 1 DAY
    ON COMPLETION PRESERVE
    DISABLE
    COMMENT 'M2 event (never runs)'
    DO DELETE FROM m2_child WHERE 1 = 0;

-- NOCACHE: see 01-schema.sql (a cached block is lost on the restart after init).
CREATE SEQUENCE m2_seq START WITH 100 INCREMENT BY 5 NOCACHE;
