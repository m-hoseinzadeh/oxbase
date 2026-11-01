//! Queries: `WITH`, `SELECT` cores, set operations, `FROM` lists with joins and derived tables,
//! and the clauses after `FROM`.

use super::expr::ExprMode;
use super::words;
use super::{BlockKind, Parser, SlotKind, Tk};
use crate::Ident;
use crate::outline::{Clause, Cte, RefRole, RelationSource, ScopeRelation};

impl Parser<'_> {
    /// A query expression (`[WITH ...] SELECT ... [UNION ...]`, `VALUES ...`, `(query)`) as a
    /// new block; returns its index.
    pub(super) fn query(&mut self, parent: Option<usize>, kind: BlockKind) -> usize {
        let b = self.new_block(parent, kind);
        let saved = self.block;
        self.block = Some(b);
        let mark = self.cte_scope.len();
        if self.at_word("WITH") && !self.is_punct(self.pos + 1, b'(') {
            self.with_clause(b);
            self.mark(
                self.pos,
                SlotKind::Keywords(words::QUERY_START),
                Clause::Start,
            );
        }
        self.query_body(b);
        self.cte_scope.truncate(mark);
        self.block = saved;
        b
    }

    /// `WITH [RECURSIVE] name [(cols)] AS [[NOT] MATERIALIZED] (body), ...`; the CTEs go to
    /// block `b` and into scope.
    pub(super) fn with_clause(&mut self, b: usize) {
        self.pos += 1;
        self.eat_word("RECURSIVE");
        loop {
            self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
            if !self.is_any_name(self.pos) {
                break;
            }
            let Some(name_path) = self.single() else {
                break;
            };
            let range_start = name_path.range().start;
            self.push_path(&name_path, RefRole::Definition, Clause::NewName);
            let Some(name) = name_path.parts.into_iter().next() else {
                break;
            };
            let columns = if self.at_punct(b'(') {
                self.definition_list()
            } else {
                Vec::new()
            };
            let index = self.queries[b].ctes.len();
            let name_end = self.prev_end();
            self.queries[b].ctes.push(Cte {
                name,
                columns,
                query: None,
                range: range_start..name_end,
            });
            self.cte_scope.push((b, index));
            self.mark(
                self.pos,
                SlotKind::Keywords(words::AFTER_CTE_NAME),
                Clause::Other,
            );
            self.eat_word("AS");
            self.eat_word("NOT");
            self.eat_word("MATERIALIZED");
            if self.at_punct(b'(') && self.enter() {
                self.pos += 1;
                let body = self.cte_body(b);
                self.close_paren();
                self.leave();
                if let Some(q) = body {
                    let output = self.queries[q].output.clone();
                    let cte = &mut self.queries[b].ctes[index];
                    cte.query = Some(q);
                    if cte.columns.is_empty() {
                        cte.columns = output;
                    }
                }
            }
            let end = self.prev_end();
            self.queries[b].ctes[index].range.end = end;
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// The body of a CTE (inside its parentheses): a query or a data-modifying statement.
    fn cte_body(&mut self, b: usize) -> Option<usize> {
        if self.query_starts(self.pos) || self.at_punct(b'(') {
            return Some(self.query(Some(b), BlockKind::Isolated));
        }
        let word = self.upper(self.pos);
        if !matches!(word.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE") {
            return None;
        }
        let q = self.new_block(Some(b), BlockKind::Isolated);
        let saved = self.block;
        self.block = Some(q);
        match word.as_str() {
            "INSERT" => self.insert(q),
            "UPDATE" => self.update(q),
            "DELETE" => self.delete(q),
            _ => self.merge(q),
        }
        self.end_block(q);
        self.block = saved;
        Some(q)
    }

    /// `(a, b, ...)` of new names (CTE columns, view columns, alias column lists), after `(`
    /// at the current token; consumes the closing `)`.
    pub(super) fn definition_list(&mut self) -> Vec<Ident> {
        let mut names = Vec::new();
        if !self.enter() {
            return names;
        }
        self.pos += 1;
        loop {
            self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
            if !self.is_any_name(self.pos) {
                break;
            }
            if let Some(path) = self.single() {
                self.push_path(&path, RefRole::Definition, Clause::NewName);
                names.extend(path.parts);
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
        self.close_paren();
        self.leave();
        names
    }

    /// Query terms joined by set operators; the first term is block `b`, later ones get blocks
    /// of their own with `b`'s parent and kind.
    pub(super) fn query_body(&mut self, b: usize) {
        let mut block = b;
        loop {
            self.query_term(block);
            if !self.set_operator() {
                break;
            }
            let parent = self.queries[b].parent;
            let kind = if self.queries[b].isolated {
                BlockKind::Isolated
            } else {
                BlockKind::Nested
            };
            block = self.new_block_with(parent, kind, Some(b));
            self.blocks[block].set_first = Some((b, self.depth));
            self.block = Some(block);
        }
        self.block = Some(b);
    }

    /// `UNION [ALL | DISTINCT]`, `EXCEPT`, `INTERSECT`, `MINUS`.
    fn set_operator(&mut self) -> bool {
        let found = self.at_word("UNION")
            || self.at_word("EXCEPT")
            || self.at_word("INTERSECT")
            || self.at_word("MINUS");
        if found {
            self.pos += 1;
            if self.at_word("ALL") || self.at_word("DISTINCT") {
                self.pos += 1;
            }
        }
        found
    }

    /// One `SELECT`, `VALUES`, `TABLE t` or parenthesised query, as block `b`.
    fn query_term(&mut self, b: usize) {
        self.block = Some(b);
        self.mark(
            self.pos,
            SlotKind::Keywords(words::QUERY_START),
            Clause::Start,
        );
        if self.at_word("SELECT") {
            self.select_core(b);
        } else if self.at_word("VALUES") {
            self.in_clause(Clause::Values, |p| {
                p.pos += 1;
                p.values_rows();
            });
            self.select_tail(b);
        } else if self.at_word("TABLE") {
            self.pos += 1;
            self.in_clause(Clause::From, |p| p.relation_item(b, Clause::From, false));
        } else if self.at_punct(b'(') && self.enter() {
            self.pos += 1;
            if self.at_word("WITH") {
                self.with_clause(b);
            }
            self.query_body(b);
            self.close_paren();
            self.leave();
            self.select_tail(b);
        }
        self.end_block(b);
    }

    /// `SELECT [modifiers] list [INTO ...] [FROM ...] tail`.
    fn select_core(&mut self, b: usize) {
        self.in_clause(Clause::SelectList, |p| {
            p.pos += 1;
            p.mark(
                p.pos,
                SlotKind::Expr(words::SELECT_START),
                Clause::SelectList,
            );
            p.select_modifiers();
            p.select_list(b);
        });
        if self.at_word("INTO") {
            self.select_into();
        }
        if self.at_word("FROM") {
            self.in_clause(Clause::From, |p| {
                p.pos += 1;
                p.relation_list(b, Clause::From);
            });
        }
        self.select_tail(b);
    }

    /// `DISTINCT [ON (...)]`, `ALL`, `TOP (n) [PERCENT] [WITH TIES]`, MySQL modifiers.
    fn select_modifiers(&mut self) {
        loop {
            if self.at_word("DISTINCT") {
                self.pos += 1;
                if self.at_word("ON") && self.is_punct(self.pos + 1, b'(') {
                    self.pos += 1;
                    self.paren_list(Clause::SelectList);
                }
            } else if self.word_in(self.pos, words::SELECT_MODIFIERS) {
                self.pos += 1;
            } else if self.at_word("TOP") {
                self.pos += 1;
                self.top_value();
            } else {
                break;
            }
        }
    }

    /// The value after `TOP` (and `PERCENT`, `WITH TIES`).
    pub(super) fn top_value(&mut self) {
        self.mark(self.pos, SlotKind::Nothing, Clause::Limit);
        if self.at_punct(b'(') {
            self.paren_list(Clause::Limit);
        } else if self
            .peek()
            .is_some_and(|t| matches!(t.kind, Tk::Num | Tk::Param))
        {
            self.pos += 1;
        }
        self.eat_word("PERCENT");
        if self.at_word("WITH") && self.is_word(self.pos + 1, "TIES") {
            self.pos += 2;
        }
    }

    /// The items of a select list (also `RETURNING` / `OUTPUT` lists), into block `b`'s
    /// output.
    pub(super) fn select_list(&mut self, b: usize) {
        loop {
            self.mark(
                self.pos,
                SlotKind::Expr(words::EXPR_START),
                Clause::SelectList,
            );
            self.select_item(b);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    fn select_item(&mut self, b: usize) {
        let clause = Clause::SelectList;
        if self.at_punct(b'*') {
            self.pos += 1;
            self.queries[b].star = true;
            self.mark(
                self.pos,
                SlotKind::Keywords(words::SELECT_ITEM_AFTER),
                clause,
            );
            return;
        }
        // T-SQL `alias = expr` and `@var = expr`.
        if self.grammar.assign_alias
            && self.is_punct(self.pos + 1, b'=')
            && !self.is_punct(self.pos + 2, b'=')
        {
            if self.peek().is_some_and(|t| t.kind == Tk::Param) {
                self.pos += 2;
                self.expr(clause, ExprMode::Plain);
                return;
            }
            if self.is_name(self.pos)
                && let Some(alias) = self.single()
            {
                self.push_path(&alias, RefRole::Definition, clause);
                self.pos += 1;
                self.expr(clause, ExprMode::Plain);
                self.queries[b].output.extend(alias.parts);
                return;
            }
        }
        let item = self.expr(clause, ExprMode::Select);
        if item.star {
            self.queries[b].star = true;
        }
        if let Some(name) = item.alias.or(item.name) {
            self.queries[b].output.push(name);
        }
    }

    /// `INTO target` of a select (T-SQL `SELECT ... INTO new_table`, MySQL `INTO @a, @b`).
    pub(super) fn select_into(&mut self) {
        self.pos += 1;
        self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
        while let Some(t) = self.peek() {
            let take = match t.kind {
                Tk::Word => !words::contains(words::STOP, self.tok_text(t)),
                Tk::Param | Tk::Str | Tk::Quoted | Tk::Punct(b',' | b'.') => true,
                _ => false,
            };
            if !take {
                break;
            }
            self.pos += 1;
        }
    }

    /// The relations of a `FROM` list (also the targets of `UPDATE` / `DELETE` and the `USING`
    /// of `DELETE`), with their joins, into block `b`.
    pub(super) fn relation_list(&mut self, b: usize, clause: Clause) {
        loop {
            self.relation_item(b, clause, false);
            self.joins(b);
            self.mark(self.pos, SlotKind::Keywords(words::FROM_TAIL), clause);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// Joins after a relation: `[NATURAL] [INNER | LEFT | RIGHT | FULL] [OUTER] JOIN x ON ... |
    /// USING (...)`, `CROSS JOIN`, `CROSS / OUTER APPLY`, `STRAIGHT_JOIN`.
    pub(super) fn joins(&mut self, b: usize) {
        while let Some(lateral) = self.join_keyword() {
            self.in_clause(Clause::Join, |p| p.relation_item(b, Clause::Join, lateral));
            self.mark(self.pos, SlotKind::Keywords(words::JOIN_TAIL), Clause::Join);
            if self.at_word("ON") {
                self.in_clause(Clause::JoinOn, |p| {
                    p.pos += 1;
                    p.expr(Clause::JoinOn, ExprMode::Plain);
                });
            } else if self.at_word("USING") && self.is_punct(self.pos + 1, b'(') {
                self.pos += 1;
                self.column_list(None, Clause::JoinOn);
            }
        }
    }

    /// Consumes the words of a join; `Some(lateral)` when there was one (`lateral` for
    /// `APPLY`).
    fn join_keyword(&mut self) -> Option<bool> {
        if !self.word_in(self.pos, words::JOIN_WORDS) {
            return None;
        }
        let start = self.pos;
        while self.word_in(self.pos, words::JOIN_MODIFIERS) {
            self.pos += 1;
        }
        if self.pos > start {
            self.mark(self.pos, SlotKind::Keywords(words::JOIN_CONT), Clause::Join);
        }
        let apply = self.at_word("APPLY");
        if apply || self.at_word("JOIN") || self.at_word("STRAIGHT_JOIN") {
            self.pos += 1;
        }
        Some(apply)
    }

    /// One relation: a (schema-qualified) table or CTE, a table function, a derived table or
    /// `VALUES` list, or a parenthesised join; with hints and an alias. Pushed to block `b`.
    pub(super) fn relation_item(&mut self, b: usize, clause: Clause, lateral: bool) {
        self.mark(self.pos, SlotKind::Relation, clause);
        let mut lateral = lateral;
        loop {
            if self.eat_word("LATERAL") {
                lateral = true;
            } else if !self.eat_word("ONLY") {
                break;
            }
            self.mark(self.pos, SlotKind::Relation, clause);
        }
        let start = self.cur_start();
        let pending = if self.at_word("ROWS")
            && self.is_word(self.pos + 1, "FROM")
            && self.is_punct(self.pos + 2, b'(')
        {
            // PostgreSQL `ROWS FROM (f(...), g(...))`: one relation of several functions.
            self.pos += 2;
            self.paren_list(clause);
            Pending::Source(RelationSource::Function(Vec::new()))
        } else if self.at_punct(b'(') {
            if !self.query_starts(self.pos + 1) {
                // A parenthesised join: its relations belong to `b`.
                if self.enter() {
                    self.pos += 1;
                    self.relation_list(b, clause);
                    self.close_paren();
                    self.leave();
                }
                return;
            }
            if !self.enter() {
                return;
            }
            self.pos += 1;
            let values = self.at_word("VALUES");
            let kind = if lateral {
                BlockKind::Nested
            } else {
                BlockKind::Isolated
            };
            let q = self.query(Some(b), kind);
            self.close_paren();
            self.leave();
            Pending::Derived { query: q, values }
        } else if self.is_name(self.pos) {
            let path = self.path(SlotKind::Relation, clause);
            if !path.complete {
                return;
            }
            if self.at_punct(b'(') {
                self.push_path(&path, RefRole::Function, clause);
                let name = path
                    .parts
                    .last()
                    .map_or_else(String::new, |p| p.text.clone());
                self.call_args(clause, &name);
                Pending::Source(RelationSource::Function(path.parts))
            } else {
                self.push_path(&path, RefRole::Relation, clause);
                let cte = match path.parts.as_slice() {
                    [one] => self.find_cte(one),
                    _ => None,
                };
                match cte {
                    Some((query, index)) => Pending::Source(RelationSource::Cte { query, index }),
                    None => Pending::Source(RelationSource::Named(path.parts)),
                }
            }
        } else if let Some(t) = self.peek().filter(|t| t.kind == Tk::Param) {
            // A T-SQL table variable.
            self.pos += 1;
            Pending::Source(RelationSource::Named(vec![Ident {
                text: self.tok_text(t).to_owned(),
                quoted: false,
            }]))
        } else {
            return;
        };
        self.table_hints();
        let (alias, column_aliases) = self.relation_alias(clause, true);
        self.table_hints();
        let source = match pending {
            Pending::Source(source) => source,
            Pending::Derived { values: true, .. } => RelationSource::Values {
                columns: column_aliases.clone(),
            },
            Pending::Derived {
                query,
                values: false,
            } => {
                let columns = if column_aliases.is_empty() {
                    self.queries[query].output.clone()
                } else {
                    column_aliases.clone()
                };
                RelationSource::Derived { query, columns }
            }
        };
        let end = self.prev_end().max(start);
        self.queries[b].relations.push(ScopeRelation {
            source,
            alias,
            column_aliases,
            range: start..end,
        });
    }

    /// Table hints and similar words around an alias: T-SQL `WITH (NOLOCK)`, MySQL index
    /// hints and `PARTITION (p)`, `TABLESAMPLE ...`, `PIVOT (...)`, SQLite `INDEXED BY i`.
    fn table_hints(&mut self) {
        loop {
            let next_is_group = self.is_punct(self.pos + 1, b'(');
            if (self.at_word("WITH")
                || self.at_word("PARTITION")
                || self.at_word("PIVOT")
                || self.at_word("UNPIVOT"))
                && next_is_group
            {
                self.pos += 1;
                self.skip_group();
            } else if (self.at_word("USE") || self.at_word("FORCE") || self.at_word("IGNORE"))
                && (self.is_word(self.pos + 1, "INDEX") || self.is_word(self.pos + 1, "KEY"))
            {
                self.pos += 2;
                if self.eat_word("FOR") {
                    self.pos += 1;
                    self.eat_word("BY");
                }
                if self.at_punct(b'(') {
                    self.skip_group();
                }
            } else if self.at_word("TABLESAMPLE") {
                self.pos += 1;
                if self.peek().is_some_and(|t| t.kind == Tk::Word) {
                    self.pos += 1;
                }
                if self.at_punct(b'(') {
                    self.skip_group();
                }
                if self.at_word("REPEATABLE") && self.is_punct(self.pos + 1, b'(') {
                    self.pos += 1;
                    self.skip_group();
                }
            } else if self.at_word("WITH") && self.is_word(self.pos + 1, "ORDINALITY") {
                // PostgreSQL `f(...) WITH ORDINALITY`.
                self.pos += 2;
            } else if self.at_word("INDEXED") && self.is_word(self.pos + 1, "BY") {
                self.pos += 3;
            } else if self.at_word("NOT") && self.is_word(self.pos + 1, "INDEXED") {
                self.pos += 2;
            } else {
                break;
            }
        }
    }

    /// `[AS] alias [(col, ...)]` after a relation. Without `AS`, only a word that can be a bare
    /// name is an alias. `column_list`: an alias may have a column alias list.
    pub(super) fn relation_alias(
        &mut self,
        clause: Clause,
        column_list: bool,
    ) -> (Option<Ident>, Vec<Ident>) {
        self.mark(
            self.pos,
            SlotKind::Keywords(words::after_relation(clause)),
            clause,
        );
        let explicit = self.eat_word("AS");
        if explicit {
            self.mark(self.pos, SlotKind::Nothing, clause);
        }
        let is_alias = if explicit {
            self.is_any_name(self.pos)
        } else {
            self.is_name(self.pos)
        };
        if !is_alias {
            return (None, Vec::new());
        }
        let Some(path) = self.single() else {
            return (None, Vec::new());
        };
        self.push_path(&path, RefRole::Definition, clause);
        let alias = path.parts.into_iter().next();
        let columns = if column_list && self.at_punct(b'(') {
            self.definition_list()
        } else {
            Vec::new()
        };
        (alias, columns)
    }

    /// `WHERE`, `GROUP BY`, `HAVING`, `WINDOW`, `QUALIFY`, `ORDER BY`, `LIMIT`, `OFFSET`,
    /// `FETCH`, `FOR ...`, `LOCK IN SHARE MODE`, `OPTION (...)`, a late `INTO`.
    pub(super) fn select_tail(&mut self, b: usize) {
        loop {
            let last = self.queries[b]
                .clauses
                .last()
                .map_or(Clause::SelectList, |(c, _)| *c);
            self.mark(self.pos, SlotKind::Keywords(words::SELECT_TAIL), last);
            let before = self.pos;
            let word = self.upper(self.pos);
            match word.as_str() {
                "WHERE" | "QUALIFY" => self.in_clause(Clause::Where, |p| {
                    p.pos += 1;
                    p.expr(Clause::Where, ExprMode::Plain);
                }),
                "GROUP" => self.in_clause(Clause::GroupBy, |p| {
                    p.pos += 1;
                    p.expect_by(Clause::GroupBy);
                    p.group_list();
                }),
                "HAVING" => self.in_clause(Clause::Having, |p| {
                    p.pos += 1;
                    p.expr(Clause::Having, ExprMode::Plain);
                }),
                "WINDOW" => self.in_clause(Clause::Window, |p| {
                    p.pos += 1;
                    p.window_definitions();
                }),
                "ORDER" => self.in_clause(Clause::OrderBy, |p| {
                    p.pos += 1;
                    // After a set operation the ORDER BY sorts the whole result: its names are
                    // the first branch's output columns.
                    let saved = p.set_order;
                    p.set_order = p.blocks[b].set_first.filter(|&(_, d)| d == p.depth);
                    p.expect_by(Clause::OrderBy);
                    p.order_list(Clause::OrderBy);
                    p.set_order = saved;
                }),
                "LIMIT" | "OFFSET" | "FETCH" => self.in_clause(Clause::Limit, |p| {
                    p.pos += 1;
                    p.limit_items();
                }),
                "FOR" => {
                    self.pos += 1;
                    if self.peek().is_some_and(|t| t.kind == Tk::Word) {
                        self.pos += 1;
                    }
                    self.skip_words();
                }
                "LOCK" => {
                    self.pos += 1;
                    self.skip_words();
                }
                "OPTION" if self.is_punct(self.pos + 1, b'(') => {
                    self.pos += 1;
                    self.skip_group();
                }
                "INTO" => self.select_into(),
                _ => break,
            }
            if self.pos == before {
                break;
            }
        }
    }

    /// Marks `BY` as expected and consumes it when present.
    pub(super) fn expect_by(&mut self, clause: Clause) {
        self.mark(self.pos, SlotKind::Keywords(&["BY"]), clause);
        self.eat_word("BY");
    }

    /// Plain words, literals and groups up to the next clause (`FOR UPDATE OF t NOWAIT`,
    /// `FOR XML PATH('')`, `LOCK IN SHARE MODE`).
    fn skip_words(&mut self) {
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Word => {
                    if words::contains(words::STOP, self.tok_text(t)) || self.strong_stop() {
                        break;
                    }
                    self.pos += 1;
                }
                Tk::Punct(b'(') => self.skip_group(),
                Tk::Str | Tk::Num | Tk::Quoted | Tk::Punct(b',' | b'.') => self.pos += 1,
                _ => break,
            }
        }
    }

    /// `GROUP BY` items, with `ROLLUP (...)`, `CUBE (...)`, `GROUPING SETS (...)`,
    /// `WITH ROLLUP`.
    fn group_list(&mut self) {
        if self.at_word("ALL") || self.at_word("DISTINCT") {
            self.pos += 1;
        }
        loop {
            if (self.at_word("ROLLUP") || self.at_word("CUBE")) && self.is_punct(self.pos + 1, b'(')
            {
                self.pos += 1;
                self.paren_list(Clause::GroupBy);
            } else if self.at_word("GROUPING") && self.is_word(self.pos + 1, "SETS") {
                self.pos += 2;
                if self.at_punct(b'(') {
                    self.paren_list(Clause::GroupBy);
                }
            } else {
                self.expr(Clause::GroupBy, ExprMode::Plain);
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
        if self.at_word("WITH") && self.is_word(self.pos + 1, "ROLLUP") {
            self.pos += 2;
        }
    }

    /// `ORDER BY` items with `ASC`, `DESC`, `NULLS FIRST | LAST`, `USING op`.
    pub(super) fn order_list(&mut self, clause: Clause) {
        loop {
            self.expr(clause, ExprMode::Plain);
            loop {
                if self.word_in(self.pos, words::ORDER_SUFFIX) {
                    self.pos += 1;
                } else if self.at_word("USING") {
                    self.pos += 1;
                    while self
                        .peek()
                        .is_some_and(|t| matches!(t.kind, Tk::Punct(c) if !matches!(c, b',' | b')' | b';' | b'(')))
                    {
                        self.pos += 1;
                    }
                } else {
                    break;
                }
            }
            self.mark(self.pos, SlotKind::Keywords(words::ORDER_AFTER), clause);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// The values and words of `LIMIT`, `OFFSET` and `FETCH`.
    fn limit_items(&mut self) {
        loop {
            let before = self.pos;
            if self.word_in(self.pos, words::LIMIT_WORDS) {
                self.pos += 1;
            } else if !self.eat_punct(b',') {
                self.expr(Clause::Limit, ExprMode::Plain);
            }
            if self.pos == before {
                break;
            }
        }
    }

    /// `WINDOW w AS (...), ...`.
    fn window_definitions(&mut self) {
        loop {
            self.mark(self.pos, SlotKind::Nothing, Clause::Window);
            if !self.is_name(self.pos) {
                break;
            }
            self.pos += 1;
            self.eat_word("AS");
            if self.at_punct(b'(') {
                self.window_spec();
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// The rows of `VALUES (...), (...)` (also MySQL `ROW(...)`).
    pub(super) fn values_rows(&mut self) {
        loop {
            self.eat_word("ROW");
            if !self.at_punct(b'(') {
                break;
            }
            self.paren_list(Clause::Values);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }
}

/// A relation whose source is known only after its alias.
enum Pending {
    Source(RelationSource),
    Derived { query: usize, values: bool },
}
