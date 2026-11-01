//! Statements: the loop over the statements of a range, the dispatch on the leading keyword,
//! and the DDL, routine, session and control-flow statements.

use super::expr::ExprMode;
use super::words;
use super::{BlockKind, Parser, SlotKind, StmtInfo, Tk};
use crate::outline::{Clause, RefRole, RelationSource, ScopeRelation, StatementClass};

impl Parser<'_> {
    /// Parses the statements of the range one after another.
    pub(super) fn script(&mut self) {
        while self.pos < self.toks.len() {
            if self.at_punct(b';') {
                self.pos += 1;
                if self.at_end() {
                    self.block = None;
                    self.stmt = None;
                    self.mark(self.pos, SlotKind::Start, Clause::Start);
                }
                continue;
            }
            let before = self.pos;
            self.statement();
            if self.pos == before {
                self.pos += 1;
            }
        }
    }

    /// One statement from the current token.
    fn statement(&mut self) {
        let Some(first) = self.peek() else {
            return;
        };
        let begin = self.pos;
        let index = self.statements.len();
        self.statements.push(StmtInfo {
            range: first.start..first.end,
            class: StatementClass::Other,
        });
        self.stmt = Some(index);
        self.block = None;
        self.mark(self.pos, SlotKind::Start, Clause::Start);
        let class = self.statement_body();
        let end = if self.pos > begin {
            self.prev_end()
        } else {
            first.end
        };
        self.statements[index] = StmtInfo {
            range: first.start..end.max(first.end),
            class,
        };
        self.block = None;
        self.stmt = None;
    }

    /// Dispatches on the leading keyword (after `EXPLAIN` options and a label).
    fn statement_body(&mut self) -> StatementClass {
        loop {
            if self.at_word("EXPLAIN") {
                self.pos += 1;
                self.explain_options();
            } else if self.is_name(self.pos)
                && self.is_punct(self.pos + 1, b':')
                && !self.is_punct(self.pos + 2, b'=')
            {
                // A label: `lbl: LOOP`.
                self.pos += 2;
            } else {
                break;
            }
        }
        let Some(t) = self.peek() else {
            return StatementClass::Empty;
        };
        if t.kind == Tk::Punct(b'(') {
            self.query(None, BlockKind::Nested);
            return StatementClass::Select;
        }
        if t.kind != Tk::Word {
            self.skip_statement(true);
            return StatementClass::Other;
        }
        let word = self.upper(self.pos);
        match word.as_str() {
            "SELECT" | "VALUES" | "TABLE" => {
                self.query(None, BlockKind::Nested);
                StatementClass::Select
            }
            "WITH" => self.with_statement(),
            "INSERT" | "REPLACE" | "UPSERT" => {
                self.dml_statement(Self::insert);
                StatementClass::Insert
            }
            "UPDATE" => {
                self.dml_statement(Self::update);
                StatementClass::Update
            }
            "DELETE" => {
                self.dml_statement(Self::delete);
                StatementClass::Delete
            }
            "MERGE" => {
                self.dml_statement(Self::merge);
                StatementClass::Merge
            }
            "CREATE" => self.create(),
            "ALTER" => self.alter(),
            "DROP" => self.drop_statement(),
            "TRUNCATE" => {
                self.truncate();
                StatementClass::Other
            }
            "CALL" | "EXEC" | "EXECUTE" => {
                self.call();
                StatementClass::Call
            }
            "USE" => {
                self.pos += 1;
                self.object_names(None, SlotKind::Database, RefRole::Database);
                StatementClass::Use
            }
            "BEGIN" => self.begin(),
            "END" => {
                self.pos += 1;
                if self.word_in(self.pos, words::END_SUFFIX) {
                    self.pos += 1;
                }
                StatementClass::Other
            }
            "ELSE" => {
                self.pos += 1;
                StatementClass::Other
            }
            "DECLARE" => {
                self.declare();
                StatementClass::Other
            }
            "RETURN" => {
                self.pos += 1;
                if !self.at_end() && !self.at_punct(b';') && !self.strong_stop() {
                    self.expr(Clause::Other, ExprMode::Plain);
                }
                StatementClass::Other
            }
            "GRANT" | "REVOKE" | "DENY" => {
                self.grant();
                StatementClass::Other
            }
            "COMMENT" => {
                self.comment_on();
                StatementClass::Other
            }
            "DESCRIBE" | "DESC" => {
                self.pos += 1;
                self.object_names(None, SlotKind::Relation, RefRole::Relation);
                StatementClass::Other
            }
            "LOCK" => {
                self.pos += 1;
                if self.at_word("TABLE") || self.at_word("TABLES") {
                    self.pos += 1;
                }
                self.object_names(None, SlotKind::Relation, RefRole::Relation);
                self.skip_statement(true);
                StatementClass::Other
            }
            _ => self.other_statement(&word),
        }
    }

    /// Statements told apart by word lists: conditions, transactions, session statements and
    /// the ones skipped whole.
    fn other_statement(&mut self, word: &str) -> StatementClass {
        if words::contains(words::CONDITIONS, word) {
            self.pos += 1;
            self.expr(Clause::Other, ExprMode::Plain);
            if self.at_word("THEN") || self.at_word("DO") || self.at_word("LOOP") {
                self.pos += 1;
            }
            return StatementClass::Other;
        }
        if words::contains(words::TRANSACTIONS, word) {
            self.pos += 1;
            self.skip_statement(true);
            return StatementClass::Transaction;
        }
        if words::contains(words::SESSION, word) {
            self.pos += 1;
            self.skip_statement(true);
            return StatementClass::Set;
        }
        if words::contains(words::SKIPPED_STATEMENTS, word) {
            self.pos += 1;
        }
        self.skip_statement(true);
        StatementClass::Other
    }

    /// `EXPLAIN [ANALYZE] [VERBOSE] [(options)] [QUERY PLAN] [FORMAT = x]` options.
    fn explain_options(&mut self) {
        loop {
            if self.at_punct(b'(') {
                self.skip_group();
            } else if self.at_word("ANALYZE")
                || self.at_word("VERBOSE")
                || self.at_word("QUERY")
                || self.at_word("PLAN")
                || self.at_word("EXTENDED")
                || self.at_word("PARTITIONS")
            {
                self.pos += 1;
            } else if self.at_word("FORMAT") {
                self.pos += 1;
                self.eat_punct(b'=');
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// A statement with a block of its own (DML): `f` parses it into the block.
    fn dml_statement(&mut self, f: impl FnOnce(&mut Self, usize)) {
        let b = self.new_block(None, BlockKind::Nested);
        self.block = Some(b);
        f(self, b);
        self.end_block(b);
    }

    /// `WITH ... SELECT | INSERT | UPDATE | DELETE | MERGE`.
    fn with_statement(&mut self) -> StatementClass {
        if self.is_punct(self.pos + 1, b'(') {
            self.skip_statement(true);
            return StatementClass::Other;
        }
        let b = self.new_block(None, BlockKind::Nested);
        self.block = Some(b);
        let mark = self.cte_scope.len();
        self.with_clause(b);
        self.mark(
            self.pos,
            SlotKind::Keywords(words::AFTER_WITH),
            Clause::Start,
        );
        let word = self.upper(self.pos);
        let class = match word.as_str() {
            "INSERT" | "REPLACE" => {
                self.insert(b);
                StatementClass::Insert
            }
            "UPDATE" => {
                self.update(b);
                StatementClass::Update
            }
            "DELETE" => {
                self.delete(b);
                StatementClass::Delete
            }
            "MERGE" => {
                self.merge(b);
                StatementClass::Merge
            }
            _ => {
                self.query_body(b);
                StatementClass::Select
            }
        };
        self.end_block(b);
        self.cte_scope.truncate(mark);
        class
    }

    /// Skips the rest of a statement: to `;`, or (`stop`, in batches without terminators) to
    /// the next statement keyword. Subqueries on the way are still parsed.
    pub(super) fn skip_statement(&mut self, stop: bool) {
        let start = self.pos;
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b';') => break,
                Tk::Punct(b'(') => {
                    if self.query_starts(self.pos + 1) {
                        self.paren_operand(Clause::Other);
                    } else {
                        self.skip_group();
                    }
                }
                _ => {
                    if stop && self.pos > start && self.strong_stop() {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }
    }

    /// `IF [NOT] EXISTS`.
    fn if_exists(&mut self) {
        if self.at_word("IF")
            && (self.is_word(self.pos + 1, "NOT") || self.is_word(self.pos + 1, "EXISTS"))
        {
            self.pos += 1;
            self.eat_word("NOT");
            self.eat_word("EXISTS");
        }
    }

    /// Names of existing objects separated by commas (`DROP TABLE a, b`, `USE db`), each
    /// recorded with `role` (relations also into block `b`).
    fn object_names(&mut self, b: Option<usize>, slot: SlotKind, role: RefRole) {
        loop {
            self.mark(self.pos, slot, Clause::Target);
            if !self.is_name(self.pos) {
                break;
            }
            let path = self.path(slot, Clause::Target);
            self.push_path(&path, role, Clause::Target);
            if let Some(b) = b
                && path.complete
            {
                let range = path.range();
                self.queries[b].relations.push(ScopeRelation {
                    source: RelationSource::Named(path.parts),
                    alias: None,
                    column_aliases: Vec::new(),
                    range,
                });
            }
            if matches!(slot, SlotKind::Function | SlotKind::Procedure) && self.at_punct(b'(') {
                // Argument types of `DROP FUNCTION f(int)`.
                self.skip_group();
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// The new object's name after `CREATE ... TABLE | VIEW | ...`.
    fn new_name(&mut self) {
        self.if_exists();
        self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
        if self.is_name(self.pos) {
            let path = self.path(SlotKind::Nothing, Clause::NewName);
            self.push_path(&path, RefRole::Definition, Clause::NewName);
        }
    }

    // --- CREATE --------------------------------------------------------------------------------

    fn create(&mut self) -> StatementClass {
        self.pos += 1;
        loop {
            self.mark(
                self.pos,
                SlotKind::Keywords(words::CREATE_OBJECTS),
                Clause::Start,
            );
            if self.at_word("OR") {
                // OR REPLACE | OR ALTER
                self.pos += 2;
            } else if self.word_in(self.pos, words::CREATE_MODIFIERS) {
                self.pos += 1;
            } else if self.at_word("DEFINER") {
                self.definer();
            } else if self.at_word("ALGORITHM") || self.at_word("SQL") {
                // ALGORITHM = x, SQL SECURITY x
                self.pos += 1;
                self.eat_punct(b'=');
                self.eat_word("SECURITY");
                self.pos += 1;
            } else {
                break;
            }
        }
        let object = self.upper(self.pos);
        match object.as_str() {
            "TABLE" => {
                self.pos += 1;
                self.create_table();
                StatementClass::CreateTable
            }
            "VIEW" => {
                self.pos += 1;
                self.create_view();
                StatementClass::CreateView
            }
            "INDEX" => {
                self.pos += 1;
                self.create_index();
                StatementClass::CreateIndex
            }
            "FUNCTION" | "PROCEDURE" | "PROC" | "TRIGGER" | "EVENT" => {
                self.pos += 1;
                self.routine_header(&object);
                StatementClass::CreateRoutine
            }
            _ => {
                if !object.is_empty() {
                    self.pos += 1;
                }
                self.new_name();
                self.skip_statement(false);
                StatementClass::CreateOther
            }
        }
    }

    /// MySQL `DEFINER = user@host | CURRENT_USER[()]`.
    fn definer(&mut self) {
        self.pos += 1;
        self.eat_punct(b'=');
        let mut taken = 0;
        while taken < 8
            && !self.at_end()
            && !self.word_in(self.pos, words::CREATE_MODIFIERS)
            && !self.is_object_word()
        {
            self.pos += 1;
            taken += 1;
        }
    }

    /// At the object word of a `CREATE` (after a `DEFINER`).
    fn is_object_word(&self) -> bool {
        [
            "TABLE",
            "VIEW",
            "FUNCTION",
            "PROCEDURE",
            "TRIGGER",
            "EVENT",
            "SQL",
            "ALGORITHM",
        ]
        .iter()
        .any(|w| self.at_word(w))
    }

    fn create_table(&mut self) {
        self.new_name();
        if self.eat_word("LIKE") {
            self.object_names(None, SlotKind::Relation, RefRole::Relation);
        }
        if self.at_punct(b'(') {
            self.table_elements();
        }
        while let Some(t) = self.peek() {
            if t.kind == Tk::Punct(b';') || self.strong_stop() {
                break;
            }
            if self.at_word("AS") || self.query_starts(self.pos) {
                self.eat_word("AS");
                self.mark(
                    self.pos,
                    SlotKind::Keywords(words::QUERY_START),
                    Clause::Start,
                );
                self.query(None, BlockKind::Nested);
                break;
            }
            if self.at_punct(b'(') {
                self.skip_group();
            } else {
                self.pos += 1;
            }
        }
    }

    /// The `(...)` of `CREATE TABLE`: column definitions and table constraints.
    fn table_elements(&mut self) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        loop {
            self.mark(self.pos, SlotKind::Nothing, Clause::ColumnDefinitions);
            if self.word_in(self.pos, words::TABLE_CONSTRAINTS) || self.is_key_definition() {
                self.constraint_definition();
            } else if self.is_name(self.pos) {
                self.column_definition();
            }
            self.skip_to_element_end();
            if !self.eat_punct(b',') {
                break;
            }
        }
        self.close_paren();
        self.leave();
    }

    /// MySQL `KEY name (cols)` / `INDEX name (cols)` (not a column named `key`).
    fn is_key_definition(&self) -> bool {
        (self.at_word("KEY") || self.at_word("INDEX"))
            && (self.is_punct(self.pos + 1, b'(') || self.is_punct(self.pos + 2, b'('))
    }

    /// `name type [constraints]` at the current token.
    fn column_definition(&mut self) {
        if let Some(name) = self.single() {
            self.push_path(&name, RefRole::Definition, Clause::ColumnDefinitions);
        }
        self.type_name(Clause::TypeName);
        self.column_constraints();
    }

    /// Column constraints up to the next `,` or `)`: `NOT NULL`, `DEFAULT expr`,
    /// `REFERENCES t (c)`, `CHECK (...)`, `GENERATED ... AS (...)`, `COLLATE x`, ...
    fn column_constraints(&mut self) {
        loop {
            self.mark(
                self.pos,
                SlotKind::Keywords(words::COLUMN_CONSTRAINTS),
                Clause::ColumnDefinitions,
            );
            let Some(t) = self.peek() else {
                break;
            };
            match t.kind {
                Tk::Punct(b',' | b')' | b';') => break,
                Tk::Punct(b'(') => self.skip_group(),
                Tk::Word => {
                    let word = self.upper(self.pos);
                    self.pos += 1;
                    match word.as_str() {
                        "REFERENCES" => self.references(),
                        "CHECK" | "AS" if self.at_punct(b'(') => {
                            self.paren_list(Clause::ColumnDefinitions);
                        }
                        "DEFAULT" => {
                            self.expr(Clause::ColumnDefinitions, ExprMode::Plain);
                        }
                        "COLLATE" => {
                            if !self.at_end() {
                                self.pos += 1;
                            }
                        }
                        _ => {}
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    /// `REFERENCES t [(cols)]` after `REFERENCES`.
    fn references(&mut self) {
        self.mark(self.pos, SlotKind::Relation, Clause::Other);
        if !self.is_name(self.pos) {
            return;
        }
        let path = self.path(SlotKind::Relation, Clause::Other);
        if !path.complete {
            return;
        }
        self.push_path(&path, RefRole::Relation, Clause::Other);
        let range = path.range();
        let relation = ScopeRelation {
            source: RelationSource::Named(path.parts),
            alias: None,
            column_aliases: Vec::new(),
            range,
        };
        if self.at_punct(b'(') {
            self.column_list(Some(&relation), Clause::Other);
        }
    }

    /// A table constraint (`CONSTRAINT c PRIMARY KEY (a)`, `FOREIGN KEY (a) REFERENCES t (b)`,
    /// `CHECK (...)`, `UNIQUE (a)`, MySQL `KEY k (a)`), up to the next `,` or `)`.
    fn constraint_definition(&mut self) {
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b',' | b')' | b';') => break,
                Tk::Punct(b'(') => self.skip_group(),
                _ => {
                    let word = self.upper(self.pos);
                    self.pos += 1;
                    if word == "REFERENCES" {
                        self.references();
                    } else if word == "CHECK" && self.at_punct(b'(') {
                        self.paren_list(Clause::ColumnDefinitions);
                    }
                }
            }
        }
    }

    /// Skips to the `,` or `)` that ends a table element.
    fn skip_to_element_end(&mut self) {
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b',' | b')' | b';') => break,
                Tk::Punct(b'(') => self.skip_group(),
                _ => self.pos += 1,
            }
        }
    }

    fn create_view(&mut self) {
        self.new_name();
        if self.at_punct(b'(') {
            self.definition_list();
        }
        while let Some(t) = self.peek() {
            if t.kind == Tk::Punct(b';') || self.at_word("AS") || self.strong_stop() {
                break;
            }
            if self.at_punct(b'(') {
                self.skip_group();
            } else {
                self.pos += 1;
            }
        }
        if self.eat_word("AS") {
            self.mark(
                self.pos,
                SlotKind::Keywords(words::QUERY_START),
                Clause::Start,
            );
            self.query(None, BlockKind::Nested);
        }
        self.skip_statement(true);
    }

    /// `[CONCURRENTLY] [IF NOT EXISTS] [name] ON t [USING m] (cols) [INCLUDE (...)]
    /// [WHERE ...]`.
    fn create_index(&mut self) {
        self.eat_word("CONCURRENTLY");
        self.if_exists();
        if !self.at_word("ON") {
            self.new_name();
        }
        self.mark(self.pos, SlotKind::Keywords(&["ON"]), Clause::Other);
        if !self.eat_word("ON") {
            self.skip_statement(true);
            return;
        }
        let b = self.new_block(None, BlockKind::Nested);
        self.block = Some(b);
        self.eat_word("ONLY");
        self.in_clause(Clause::Target, |p| {
            p.object_names(Some(b), SlotKind::Relation, RefRole::Relation);
        });
        if self.eat_word("USING") && !self.at_punct(b'(') {
            self.pos += 1;
        }
        if self.at_punct(b'(') && self.enter() {
            self.pos += 1;
            loop {
                self.expr(Clause::Other, ExprMode::Plain);
                while self.word_in(self.pos, words::ORDER_SUFFIX)
                    || (self.is_name(self.pos) && !self.is_punct(self.pos + 1, b'('))
                {
                    // ASC, DESC, NULLS LAST, an operator class.
                    self.pos += 1;
                }
                if !self.eat_punct(b',') {
                    break;
                }
            }
            self.close_paren();
            self.leave();
        }
        if self.eat_word("INCLUDE") && self.at_punct(b'(') {
            self.paren_list(Clause::Other);
        }
        while let Some(t) = self.peek() {
            if t.kind == Tk::Punct(b';') || self.strong_stop() {
                break;
            }
            if self.at_word("WHERE") {
                self.in_clause(Clause::Where, |p| {
                    p.pos += 1;
                    p.expr(Clause::Where, ExprMode::Plain);
                });
            } else if self.at_punct(b'(') {
                self.skip_group();
            } else {
                self.pos += 1;
            }
        }
        self.end_block(b);
    }

    /// The head of `CREATE | ALTER FUNCTION | PROCEDURE | TRIGGER | EVENT` up to its body; the
    /// body's statements are parsed by [`Parser::script`] as statements of their own.
    fn routine_header(&mut self, object: &str) {
        self.new_name();
        match object {
            "TRIGGER" => self.trigger_header(),
            "EVENT" => {
                while let Some(t) = self.peek() {
                    if t.kind == Tk::Punct(b';') {
                        break;
                    }
                    self.pos += 1;
                    if self.is_word(self.pos - 1, "DO") {
                        break;
                    }
                }
            }
            _ => {
                if self.at_punct(b'(') {
                    self.parameters(true);
                } else if self.peek().is_some_and(|t| t.kind == Tk::Param) {
                    self.parameters(false);
                }
                self.routine_options();
            }
        }
    }

    /// Routine parameters: in `(...)` when `parens`, else a T-SQL list ending at `AS`.
    fn parameters(&mut self, parens: bool) {
        if parens {
            if !self.enter() {
                return;
            }
            self.pos += 1;
        }
        loop {
            while self.word_in(self.pos, words::PARAM_MODES) {
                self.pos += 1;
            }
            let Some(t) = self.peek() else {
                break;
            };
            if t.kind == Tk::Punct(b')') {
                break;
            }
            let named = t.kind == Tk::Param
                || (matches!(t.kind, Tk::Word | Tk::Quoted)
                    && self
                        .tok(self.pos + 1)
                        .is_some_and(|n| matches!(n.kind, Tk::Word | Tk::Quoted)));
            if named {
                self.pos += 1;
                self.eat_word("AS");
            }
            self.type_name(Clause::TypeName);
            // Defaults, OUT, READONLY up to the next parameter.
            while let Some(n) = self.peek() {
                let end = match n.kind {
                    Tk::Punct(b',' | b')' | b';') => true,
                    Tk::Word => {
                        !parens
                            && (self.at_word("AS") || self.at_word("WITH") || self.at_word("FOR"))
                    }
                    _ => false,
                };
                if end {
                    break;
                }
                if n.kind == Tk::Punct(b'(') {
                    self.skip_group();
                } else {
                    self.pos += 1;
                }
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
        if parens {
            self.close_paren();
            self.leave();
        }
    }

    /// `RETURNS type`, `LANGUAGE x`, characteristics, `AS 'body'` up to the body.
    fn routine_options(&mut self) {
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b';') => break,
                Tk::Punct(b'(') => self.skip_group(),
                Tk::Word => {
                    if self.word_in(self.pos, words::BODY_START) {
                        break;
                    }
                    let word = self.upper(self.pos);
                    self.pos += 1;
                    if word == "RETURNS" {
                        self.returns();
                    } else if word == "AS" && !self.peek().is_some_and(|n| n.kind == Tk::Str) {
                        // T-SQL: the body follows.
                        break;
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    /// The type after `RETURNS` (`TABLE (...)`, T-SQL `@t TABLE (...)`, `SETOF t`).
    fn returns(&mut self) {
        if self.peek().is_some_and(|t| t.kind == Tk::Param) {
            self.pos += 1;
        }
        self.eat_word("SETOF");
        if self.eat_word("TABLE") {
            if self.at_punct(b'(') {
                self.skip_group();
            }
            return;
        }
        self.type_name(Clause::TypeName);
    }

    /// `{BEFORE | AFTER | INSTEAD OF} events ON t [FOR EACH ROW] [WHEN ...]` up to the body
    /// (also T-SQL `ON t {AFTER | FOR} events AS`).
    fn trigger_header(&mut self) {
        while let Some(t) = self.peek() {
            if t.kind == Tk::Punct(b';') {
                return;
            }
            if self.eat_word("ON") {
                self.object_names(None, SlotKind::Relation, RefRole::Relation);
                break;
            }
            self.pos += 1;
        }
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b';') => return,
                Tk::Punct(b'(') => self.skip_group(),
                _ => {
                    if self.at_word("AS") {
                        self.pos += 1;
                        return;
                    }
                    if self.at_word("EXECUTE") {
                        self.pos += 1;
                        if self.at_word("FUNCTION") || self.at_word("PROCEDURE") {
                            self.pos += 1;
                        }
                        self.object_names(None, SlotKind::Function, RefRole::Function);
                        return;
                    }
                    let event = self.is_word(self.pos.wrapping_sub(1), "AFTER")
                        || self.is_word(self.pos.wrapping_sub(1), "BEFORE")
                        || self.is_word(self.pos.wrapping_sub(1), "FOR")
                        || self.is_word(self.pos.wrapping_sub(1), "OF")
                        || self.is_word(self.pos.wrapping_sub(1), "OR")
                        || self.is_punct(self.pos.wrapping_sub(1), b',');
                    if !event && self.word_in(self.pos, words::BODY_START) {
                        return;
                    }
                    self.pos += 1;
                }
            }
        }
    }

    // --- ALTER, DROP, TRUNCATE -----------------------------------------------------------------

    fn alter(&mut self) -> StatementClass {
        self.pos += 1;
        self.mark(
            self.pos,
            SlotKind::Keywords(words::ALTER_OBJECTS),
            Clause::Start,
        );
        while self.at_word("ONLINE") || self.at_word("IGNORE") {
            self.pos += 1;
        }
        let object = self.upper(self.pos);
        if !object.is_empty() {
            self.pos += 1;
        }
        match object.as_str() {
            "TABLE" => self.alter_table(),
            "VIEW" => self.create_view(),
            "FUNCTION" | "PROCEDURE" | "PROC" | "TRIGGER" | "EVENT" => {
                self.routine_header(&object);
            }
            "SCHEMA" => {
                self.object_names(None, SlotKind::Schema, RefRole::Schema);
                self.skip_statement(true);
            }
            "DATABASE" => {
                self.object_names(None, SlotKind::Database, RefRole::Database);
                self.skip_statement(true);
            }
            "SEQUENCE" => {
                self.object_names(None, SlotKind::Sequence, RefRole::Unknown);
                self.skip_statement(true);
            }
            _ => self.skip_statement(true),
        }
        StatementClass::Alter
    }

    fn alter_table(&mut self) {
        self.if_exists();
        self.eat_word("ONLY");
        let b = self.new_block(None, BlockKind::Nested);
        self.block = Some(b);
        self.in_clause(Clause::Target, |p| {
            p.object_names(Some(b), SlotKind::Relation, RefRole::Relation);
        });
        let target = self.queries[b].relations.first().cloned();
        loop {
            self.mark(
                self.pos,
                SlotKind::Keywords(words::ALTER_TABLE_ACTIONS),
                Clause::Other,
            );
            if self.at_end() || self.at_punct(b';') || self.strong_stop() {
                break;
            }
            self.alter_action(target.as_ref());
            if !self.skip_to_comma() {
                break;
            }
        }
        self.end_block(b);
    }

    /// Skips to just after the next `,`; `false` when a `;`, the end or a statement keyword
    /// came first.
    fn skip_to_comma(&mut self) -> bool {
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b';') => return false,
                Tk::Punct(b',') => {
                    self.pos += 1;
                    return true;
                }
                Tk::Punct(b'(') => self.skip_group(),
                _ => {
                    if self.strong_stop() {
                        return false;
                    }
                    self.pos += 1;
                }
            }
        }
        false
    }

    /// One action of `ALTER TABLE t`.
    fn alter_action(&mut self, target: Option<&ScopeRelation>) {
        let action = self.upper(self.pos);
        match action.as_str() {
            "ADD" => {
                self.pos += 1;
                self.mark(self.pos, SlotKind::Keywords(words::ADD_WHAT), Clause::Other);
                if self.word_in(self.pos, words::TABLE_CONSTRAINTS) || self.is_key_definition() {
                    self.constraint_definition();
                } else {
                    self.eat_word("COLUMN");
                    self.if_exists();
                    self.mark(self.pos, SlotKind::Nothing, Clause::ColumnDefinitions);
                    if self.is_name(self.pos) {
                        self.column_definition();
                    }
                }
            }
            "DROP" => {
                self.pos += 1;
                self.mark(
                    self.pos,
                    SlotKind::Keywords(words::DROP_WHAT),
                    Clause::Other,
                );
                let column = self.eat_word("COLUMN");
                if column
                    || (self.is_name(self.pos) && !self.word_in(self.pos, words::DROP_NON_COLUMNS))
                {
                    self.if_exists();
                    self.existing_column(target);
                }
            }
            "ALTER" | "MODIFY" | "CHANGE" => {
                self.pos += 1;
                self.eat_word("COLUMN");
                self.existing_column(target);
                if action == "CHANGE" {
                    self.mark(self.pos, SlotKind::Nothing, Clause::ColumnDefinitions);
                    if let Some(name) = self.single() {
                        self.push_path(&name, RefRole::Definition, Clause::ColumnDefinitions);
                    }
                }
                if self.eat_word("TYPE") {
                    self.type_name(Clause::TypeName);
                } else if self.at_word("SET") && self.is_word(self.pos + 1, "DATA") {
                    self.pos += 2;
                    self.eat_word("TYPE");
                    self.type_name(Clause::TypeName);
                } else if !self.word_in(self.pos, words::ALTER_COLUMN_ACTIONS) {
                    self.type_name(Clause::TypeName);
                }
            }
            "RENAME" => {
                self.pos += 1;
                if self.at_word("TO") || self.at_word("AS") {
                    self.pos += 1;
                    self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
                } else if !(self.at_word("CONSTRAINT") || self.at_word("INDEX")) {
                    self.eat_word("COLUMN");
                    self.existing_column(target);
                    self.mark(self.pos, SlotKind::Keywords(&["TO"]), Clause::Other);
                    if self.eat_word("TO") {
                        self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
                    }
                }
            }
            _ => {}
        }
    }

    /// A column of `target` (`DROP COLUMN c`, `ALTER COLUMN c`, `RENAME COLUMN c`).
    fn existing_column(&mut self, target: Option<&ScopeRelation>) {
        self.mark_only(self.pos, SlotKind::Columns, Clause::Other, target);
        if self.is_name(self.pos) {
            let path = self.path(SlotKind::Columns, Clause::Other);
            self.push_path(&path, RefRole::Column, Clause::Other);
        }
    }

    fn drop_statement(&mut self) -> StatementClass {
        self.pos += 1;
        self.mark(
            self.pos,
            SlotKind::Keywords(words::DROP_OBJECTS),
            Clause::Start,
        );
        while self.at_word("TEMPORARY") || self.at_word("MATERIALIZED") || self.at_word("FOREIGN") {
            self.pos += 1;
        }
        let object = self.upper(self.pos);
        if !object.is_empty() {
            self.pos += 1;
        }
        self.eat_word("CONCURRENTLY");
        self.if_exists();
        let (slot, role) = match object.as_str() {
            "TABLE" | "VIEW" => (SlotKind::Relation, RefRole::Relation),
            "FUNCTION" | "AGGREGATE" => (SlotKind::Function, RefRole::Function),
            "PROCEDURE" | "PROC" => (SlotKind::Procedure, RefRole::Procedure),
            "SCHEMA" => (SlotKind::Schema, RefRole::Schema),
            "DATABASE" => (SlotKind::Database, RefRole::Database),
            "SEQUENCE" => (SlotKind::Sequence, RefRole::Unknown),
            "TYPE" | "DOMAIN" => (SlotKind::Type, RefRole::Type),
            _ => (SlotKind::Nothing, RefRole::Unknown),
        };
        let b = (role == RefRole::Relation).then(|| {
            let b = self.new_block(None, BlockKind::Nested);
            self.block = Some(b);
            b
        });
        self.in_clause(Clause::Target, |p| p.object_names(b, slot, role));
        if self.eat_word("ON") {
            // MySQL `DROP INDEX i ON t`, `DROP TRIGGER t ON tbl`.
            self.object_names(b, SlotKind::Relation, RefRole::Relation);
        }
        self.skip_statement(true);
        if let Some(b) = b {
            self.end_block(b);
        }
        StatementClass::Drop
    }

    fn truncate(&mut self) {
        self.pos += 1;
        self.eat_word("TABLE");
        self.eat_word("ONLY");
        let b = self.new_block(None, BlockKind::Nested);
        self.block = Some(b);
        self.in_clause(Clause::Target, |p| {
            p.object_names(Some(b), SlotKind::Relation, RefRole::Relation);
        });
        self.skip_statement(true);
        self.end_block(b);
    }

    // --- other statements ----------------------------------------------------------------------

    /// `CALL p(...)`, `EXEC[UTE] [@rc =] p args`, `EXEC ('sql')`.
    fn call(&mut self) {
        self.pos += 1;
        self.mark(self.pos, SlotKind::Procedure, Clause::RoutineName);
        if self.peek().is_some_and(|t| t.kind == Tk::Param) && self.is_punct(self.pos + 1, b'=') {
            self.pos += 2;
            self.mark(self.pos, SlotKind::Procedure, Clause::RoutineName);
        }
        if self.at_punct(b'(') {
            self.skip_group();
            return;
        }
        if !self.is_name(self.pos) {
            self.skip_statement(true);
            return;
        }
        let path = self.path(SlotKind::Procedure, Clause::RoutineName);
        self.push_path(&path, RefRole::Procedure, Clause::RoutineName);
        if self.at_punct(b'(') {
            self.paren_list(Clause::Other);
            return;
        }
        // T-SQL arguments: `@p = value [OUTPUT], value, ...`.
        loop {
            if self.peek().is_some_and(|t| t.kind == Tk::Param) && self.is_punct(self.pos + 1, b'=')
            {
                self.pos += 2;
            }
            let before = self.pos;
            self.expr(Clause::Other, ExprMode::Plain);
            if self.at_word("OUTPUT") || self.at_word("OUT") {
                self.pos += 1;
            }
            if self.pos == before || !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// `BEGIN` of a transaction or of a block (`BEGIN ... END`, `BEGIN TRY`, `BEGIN ATOMIC`).
    fn begin(&mut self) -> StatementClass {
        self.pos += 1;
        if self.at_end() || self.at_punct(b';') || self.word_in(self.pos, words::TRANSACTION_NEXT) {
            self.skip_statement(true);
            return StatementClass::Transaction;
        }
        if self.at_word("TRY") || self.at_word("CATCH") || self.at_word("ATOMIC") {
            self.pos += 1;
        }
        StatementClass::Other
    }

    /// `DECLARE @x type [= expr], ...`, `DECLARE c CURSOR FOR query`, `DECLARE @t TABLE (...)`,
    /// MySQL `DECLARE x INT DEFAULT 0`.
    fn declare(&mut self) {
        self.pos += 1;
        loop {
            let Some(t) = self.peek() else {
                break;
            };
            if !matches!(t.kind, Tk::Param | Tk::Word | Tk::Quoted) {
                break;
            }
            self.pos += 1;
            if self.at_word("CONDITION") || self.at_word("HANDLER") || self.at_word("CONTINUE") {
                self.skip_statement(true);
                return;
            }
            if self.at_word("CURSOR") || self.at_word("SCROLL") || self.at_word("INSENSITIVE") {
                while !self.at_end() && !self.at_punct(b';') && !self.at_word("FOR") {
                    self.pos += 1;
                }
                if self.eat_word("FOR") && self.query_starts(self.pos) {
                    self.query(None, BlockKind::Nested);
                }
                return;
            }
            self.eat_word("AS");
            if self.eat_word("TABLE") {
                if self.at_punct(b'(') {
                    self.table_elements();
                }
            } else {
                self.type_name(Clause::TypeName);
            }
            if self.eat_word("DEFAULT") || self.eat_punct(b'=') {
                self.expr(Clause::Other, ExprMode::Plain);
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// `GRANT ... ON [TABLE] t TO ...` and `REVOKE` / `DENY`.
    fn grant(&mut self) {
        self.pos += 1;
        while let Some(t) = self.peek() {
            if t.kind == Tk::Punct(b';') {
                return;
            }
            if self.eat_word("ON") {
                break;
            }
            self.pos += 1;
        }
        self.eat_word("TABLE");
        if self.word_in(
            self.pos,
            &["DATABASE", "FUNCTION", "PROCEDURE", "SCHEMA", "SEQUENCE"],
        ) || self.at_word("ALL")
        {
            self.skip_statement(false);
            return;
        }
        self.object_names(None, SlotKind::Relation, RefRole::Relation);
        self.skip_statement(false);
    }

    /// `COMMENT ON TABLE t | COLUMN t.c | ... IS '...'`.
    fn comment_on(&mut self) {
        self.pos += 1;
        self.eat_word("ON");
        let object = self.upper(self.pos);
        if !object.is_empty() {
            self.pos += 1;
        }
        match object.as_str() {
            "TABLE" | "VIEW" => self.object_names(None, SlotKind::Relation, RefRole::Relation),
            "COLUMN" => self.object_names(None, SlotKind::Relation, RefRole::Column),
            _ => {}
        }
        self.skip_statement(false);
    }
}
