-- M2 seed objects for the object tree and the PostgreSQL fetchers. Names come from
-- tests/fixtures.toml ([m2] and [m2.postgres]); the shape is the cross-engine contract written
-- there (columns, constraints, index, view, trigger, function, procedure, sequence).
-- Runs once, as POSTGRES_USER (oxbase) in POSTGRES_DB (oxbase_test), on an empty data volume.
-- Must be the last init script: it ends connected to the second database.

CREATE SCHEMA m2;
COMMENT ON SCHEMA m2 IS 'M2 seed objects';

CREATE TABLE m2.m2_parent (
    id    integer      NOT NULL,
    code  varchar(20)  NOT NULL,
    note  text         NULL,
    CONSTRAINT m2_parent_pkey PRIMARY KEY (id),
    CONSTRAINT m2_parent_code_key UNIQUE (code),
    CONSTRAINT m2_parent_code_chk CHECK (char_length(code) > 0)
);
COMMENT ON TABLE m2.m2_parent IS 'M2 parent table';
COMMENT ON COLUMN m2.m2_parent.code IS 'Business code';

CREATE TABLE m2.m2_child (
    id         integer NOT NULL,
    parent_id  integer NOT NULL,
    qty        integer NOT NULL DEFAULT 1,
    CONSTRAINT m2_child_pkey PRIMARY KEY (id),
    CONSTRAINT m2_child_parent_fk FOREIGN KEY (parent_id)
        REFERENCES m2.m2_parent (id) ON DELETE CASCADE,
    CONSTRAINT m2_child_qty_chk CHECK (qty >= 0)
);

CREATE INDEX m2_child_parent_idx ON m2.m2_child (parent_id);

CREATE VIEW m2.m2_view AS
SELECT c.id, c.qty, p.code
FROM m2.m2_child c
JOIN m2.m2_parent p ON p.id = c.parent_id;

CREATE MATERIALIZED VIEW m2.m2_mview AS
SELECT parent_id, count(*)
FROM m2.m2_child
GROUP BY parent_id;

-- Lets the tree show an index under a materialized view.
CREATE UNIQUE INDEX m2_mview_parent_idx ON m2.m2_mview (parent_id);

CREATE FUNCTION m2.m2_trg_fn() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.qty IS NULL OR NEW.qty < 0 THEN
        NEW.qty := 0;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER m2_trg
    BEFORE INSERT ON m2.m2_child
    FOR EACH ROW EXECUTE FUNCTION m2.m2_trg_fn();

CREATE FUNCTION m2.m2_fn(n integer) RETURNS integer
LANGUAGE sql IMMUTABLE STRICT
AS $$ SELECT n + 1 $$;
COMMENT ON FUNCTION m2.m2_fn(integer) IS 'Adds one';

-- A second overload, so routine refs need their signature (and with an argument default).
CREATE FUNCTION m2.m2_fn(t text, n integer DEFAULT 2) RETURNS text
LANGUAGE sql STABLE
AS $$ SELECT repeat(t, n) $$;

CREATE PROCEDURE m2.m2_proc(n integer)
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM n;
END;
$$;

CREATE SEQUENCE m2.m2_seq START WITH 100 INCREMENT BY 5;

-- PostgreSQL-only cases for the pg dialect live tests, in their own schema so the
-- cross-engine m2 listings stay exactly as tests/fixtures.toml declares them.
CREATE SCHEMA m2_pg;

-- A foreign key to a partitioned table: PostgreSQL 12+ adds a hidden pg_constraint row per
-- referenced partition, which must not show up as extra foreign keys.
CREATE TABLE m2_pg.part_p (
    id integer NOT NULL,
    CONSTRAINT part_p_pkey PRIMARY KEY (id)
) PARTITION BY RANGE (id);
CREATE TABLE m2_pg.part_p1 PARTITION OF m2_pg.part_p FOR VALUES FROM (0) TO (100);
CREATE TABLE m2_pg.part_p2 PARTITION OF m2_pg.part_p FOR VALUES FROM (100) TO (200);

CREATE TABLE m2_pg.part_ref (
    id   integer NOT NULL,
    pid  integer,
    CONSTRAINT part_ref_pkey PRIMARY KEY (id),
    CONSTRAINT part_ref_pid_fk FOREIGN KEY (pid) REFERENCES m2_pg.part_p (id),
    -- PostgreSQL 18+: a constraint that is declared but not checked.
    CONSTRAINT part_ref_pid_chk CHECK (pid >= 0) NOT ENFORCED
);

-- Partitioned on both sides: the partition keeps its inherited foreign key.
CREATE TABLE m2_pg.part_c (
    id   integer NOT NULL,
    pid  integer,
    CONSTRAINT part_c_pid_fk FOREIGN KEY (pid) REFERENCES m2_pg.part_p (id)
) PARTITION BY RANGE (id);
CREATE TABLE m2_pg.part_c1 PARTITION OF m2_pg.part_c FOR VALUES FROM (0) TO (100);

-- Triggers with WHEN conditions and UPDATE OF columns.
CREATE TABLE m2_pg.trg_t (
    id    integer NOT NULL,
    code  text,
    n     integer,
    r     int4range,
    CONSTRAINT trg_t_pkey PRIMARY KEY (id),
    CONSTRAINT trg_t_r_excl EXCLUDE USING gist (r WITH &&)
);

-- NULLS FIRST on an ascending column, NULLS LAST on a descending one (both non-default).
CREATE INDEX trg_t_nulls_idx ON m2_pg.trg_t (code NULLS FIRST, n DESC NULLS LAST);

CREATE FUNCTION m2_pg.trg_noop() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_case
    BEFORE UPDATE OF code, n ON m2_pg.trg_t
    FOR EACH ROW
    WHEN (CASE WHEN NEW.n > 0 THEN true ELSE false END)
    EXECUTE FUNCTION m2_pg.trg_noop();

CREATE TRIGGER trg_distinct
    AFTER UPDATE OF code ON m2_pg.trg_t
    FOR EACH ROW
    WHEN (OLD.code IS DISTINCT FROM NEW.code)
    EXECUTE FUNCTION m2_pg.trg_noop();

-- Objects an extension owns are flagged system even in a user schema.
CREATE SCHEMA m2_pg_ext;
CREATE EXTENSION citext SCHEMA m2_pg_ext;

-- A second database (owned by oxbase) with one table: the tree opens a session per database.
CREATE DATABASE oxbase_m2_other OWNER oxbase;

\connect oxbase_m2_other

CREATE TABLE public.m2_other_t (
    id integer NOT NULL,
    CONSTRAINT m2_other_t_pkey PRIMARY KEY (id)
);
