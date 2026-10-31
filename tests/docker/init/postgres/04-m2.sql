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

-- A second database (owned by oxbase) with one table: the tree opens a session per database.
CREATE DATABASE oxbase_m2_other OWNER oxbase;

\connect oxbase_m2_other

CREATE TABLE public.m2_other_t (
    id integer NOT NULL,
    CONSTRAINT m2_other_t_pkey PRIMARY KEY (id)
);
