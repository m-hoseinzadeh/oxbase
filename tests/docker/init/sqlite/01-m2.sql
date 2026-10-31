-- M2 seed objects for SQLite (tests/fixtures.toml [m2] and [m2.sqlite]).
--
-- SQLite has no server: tests apply this script to a fresh database file, then ATTACH a second
-- file as `m2aux` holding `m2_aux_t` themselves. SQLite has no functions, procedures, sequences
-- or comments, so `m2_fn`, `m2_proc`, `m2_seq` and the table comment do not exist here.
--
-- Differences from the shape in fixtures.toml that SQLite imposes:
--   * length(code) instead of char_length(code);
--   * a BEFORE trigger cannot assign NEW.qty, so m2_trg rejects a negative qty instead of
--     setting it to 0 (NULL is already refused by NOT NULL).

CREATE TABLE m2_parent (
    id INTEGER PRIMARY KEY,
    code VARCHAR(20) NOT NULL UNIQUE,
    note TEXT NULL,
    CONSTRAINT m2_parent_code_chk CHECK (length(code) > 0)
);

CREATE TABLE m2_child (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER NOT NULL,
    qty INTEGER NOT NULL DEFAULT 1,
    CONSTRAINT m2_child_parent_fk FOREIGN KEY (parent_id)
        REFERENCES m2_parent (id) ON DELETE CASCADE,
    CONSTRAINT m2_child_qty_chk CHECK (qty >= 0)
);

CREATE INDEX m2_child_parent_idx ON m2_child (parent_id);

CREATE VIEW m2_view AS
    SELECT c.id, c.qty, p.code FROM m2_child c JOIN m2_parent p ON p.id = c.parent_id;

CREATE TRIGGER m2_trg BEFORE INSERT ON m2_child FOR EACH ROW WHEN NEW.qty < 0
BEGIN
    SELECT RAISE(ABORT, 'm2_child.qty must not be negative');
END;
