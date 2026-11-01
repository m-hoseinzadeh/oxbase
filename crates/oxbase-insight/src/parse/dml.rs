//! `INSERT`, `UPDATE`, `DELETE` and `MERGE`. Each fills the block it is given (the statement's
//! block, or a CTE's for PostgreSQL data-modifying CTEs) with its target first.

use super::expr::ExprMode;
use super::words;
use super::{BlockKind, Parser, SlotKind, Tk};
use crate::outline::{Clause, RefRole, RelationSource, ScopeRelation};

impl Parser<'_> {
    /// `INSERT | REPLACE [modifiers] [INTO] t [AS a] [(cols)] [OUTPUT ...] VALUES ... |
    /// SELECT ... | DEFAULT VALUES | SET ... [ON CONFLICT ... | ON DUPLICATE KEY UPDATE ...]
    /// [RETURNING ...]`.
    pub(super) fn insert(&mut self, b: usize) {
        self.pos += 1;
        self.dml_modifiers();
        self.mark(
            self.pos,
            SlotKind::Keywords(&["INTO"]),
            Clause::InsertTarget,
        );
        self.eat_word("INTO");
        let target = self.in_clause(Clause::InsertTarget, |p| p.insert_target(b));
        if self.at_punct(b'(') && !self.query_starts(self.pos + 1) {
            self.in_clause(Clause::InsertColumns, |p| {
                p.column_list(target.as_ref(), Clause::InsertColumns);
            });
            self.mark(
                self.pos,
                SlotKind::Keywords(words::INSERT_SOURCE),
                Clause::InsertColumns,
            );
        }
        self.output_clause(b);
        if self.at_word("OVERRIDING") {
            // OVERRIDING SYSTEM | USER VALUE
            self.pos += 3;
        }
        if self.at_word("VALUES") || self.at_word("VALUE") {
            self.in_clause(Clause::Values, |p| {
                p.pos += 1;
                p.values_rows();
            });
        } else if self.at_word("DEFAULT") && self.is_word(self.pos + 1, "VALUES") {
            self.pos += 2;
        } else if self.at_word("SET") {
            self.in_clause(Clause::UpdateSet, |p| {
                p.pos += 1;
                p.assignments(target.as_ref());
            });
        } else if self.query_starts(self.pos) {
            self.query(Some(b), BlockKind::Isolated);
        }
        // MySQL row alias: `AS new [(a, b)]`.
        if self.eat_word("AS") {
            self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
            if self.is_any_name(self.pos) {
                self.pos += 1;
            }
            if self.at_punct(b'(') {
                self.skip_group();
            }
        }
        self.mark(
            self.pos,
            SlotKind::Keywords(words::INSERT_TAIL),
            Clause::Values,
        );
        if self.at_word("ON") {
            self.pos += 1;
            if self.eat_word("CONFLICT") {
                self.on_conflict(target.as_ref());
            } else if self.eat_word("DUPLICATE") {
                self.eat_word("KEY");
                self.eat_word("UPDATE");
                self.in_clause(Clause::UpdateSet, |p| p.assignments(target.as_ref()));
            }
        }
        self.dml_tail(b);
    }

    /// The table of an `INSERT` with an optional `AS alias` (no bare alias: the next word
    /// would be `VALUES`, `SELECT`, ...).
    fn insert_target(&mut self, b: usize) -> Option<ScopeRelation> {
        self.mark(self.pos, SlotKind::Relation, Clause::InsertTarget);
        if !self.is_name(self.pos) {
            return None;
        }
        let start = self.cur_start();
        let path = self.path(SlotKind::Relation, Clause::InsertTarget);
        if !path.complete {
            return None;
        }
        self.push_path(&path, RefRole::Relation, Clause::InsertTarget);
        self.mark(
            self.pos,
            SlotKind::Keywords(words::INSERT_AFTER_TARGET),
            Clause::InsertTarget,
        );
        let mut alias = None;
        if self.at_word("AS") && self.is_name(self.pos + 1) {
            self.pos += 1;
            self.mark(self.pos, SlotKind::Nothing, Clause::InsertTarget);
            if let Some(a) = self.single() {
                self.push_path(&a, RefRole::Definition, Clause::InsertTarget);
                alias = a.parts.into_iter().next();
            }
        }
        let end = self.prev_end();
        let relation = ScopeRelation {
            source: RelationSource::Named(path.parts),
            alias,
            column_aliases: Vec::new(),
            range: start..end,
        };
        self.queries[b].relations.push(relation.clone());
        Some(relation)
    }

    /// `ON CONFLICT [(cols) [WHERE ...] | ON CONSTRAINT c] DO NOTHING | DO UPDATE SET ...
    /// [WHERE ...]` after `CONFLICT`.
    fn on_conflict(&mut self, target: Option<&ScopeRelation>) {
        if self.at_punct(b'(') {
            self.column_list(target, Clause::Other);
        }
        if self.at_word("ON") && self.is_word(self.pos + 1, "CONSTRAINT") {
            self.pos += 3;
        }
        if self.at_word("WHERE") {
            self.in_clause(Clause::Where, |p| {
                p.pos += 1;
                p.expr(Clause::Where, ExprMode::Plain);
            });
        }
        self.mark(self.pos, SlotKind::Keywords(&["DO"]), Clause::Other);
        if !self.eat_word("DO") {
            return;
        }
        self.mark(
            self.pos,
            SlotKind::Keywords(&["NOTHING", "UPDATE"]),
            Clause::Other,
        );
        if self.eat_word("UPDATE") {
            self.mark(self.pos, SlotKind::Keywords(&["SET"]), Clause::Other);
            if self.eat_word("SET") {
                self.in_clause(Clause::UpdateSet, |p| p.assignments(target));
            }
        } else {
            self.eat_word("NOTHING");
        }
    }

    /// `UPDATE [modifiers] t [[AS] a] [JOIN ...] SET ... [FROM ...] [WHERE ...] [...]`.
    pub(super) fn update(&mut self, b: usize) {
        self.pos += 1;
        self.dml_modifiers();
        self.in_clause(Clause::UpdateTarget, |p| {
            p.from_item(b, Clause::UpdateTarget, false);
            p.joins(b);
            while p.eat_punct(b',') {
                p.from_item(b, Clause::UpdateTarget, false);
                p.joins(b);
            }
        });
        let target = self.queries[b].relations.first().cloned();
        self.mark(self.pos, SlotKind::Keywords(&["SET"]), Clause::UpdateTarget);
        if self.at_word("SET") {
            self.in_clause(Clause::UpdateSet, |p| {
                p.pos += 1;
                p.assignments(target.as_ref());
            });
        }
        self.output_clause(b);
        if self.at_word("FROM") {
            self.in_clause(Clause::From, |p| {
                p.pos += 1;
                p.from_list(b, Clause::From);
            });
            self.target_alias(b);
        }
        self.dml_tail(b);
    }

    /// `DELETE [modifiers] [FROM] t [[AS] a] [USING ...] [FROM ...] [WHERE ...] [...]`, also
    /// MySQL `DELETE a, b FROM a JOIN b ...` and T-SQL `DELETE t FROM t JOIN ...`.
    pub(super) fn delete(&mut self, b: usize) {
        self.pos += 1;
        self.dml_modifiers();
        self.mark(self.pos, SlotKind::Keywords(&["FROM"]), Clause::Target);
        self.eat_word("FROM");
        self.in_clause(Clause::Target, |p| {
            p.from_item(b, Clause::Target, false);
            while p.eat_punct(b',') {
                p.from_item(b, Clause::Target, false);
            }
        });
        self.output_clause(b);
        if self.at_word("USING") || self.at_word("FROM") {
            self.in_clause(Clause::From, |p| {
                p.pos += 1;
                p.from_list(b, Clause::From);
            });
            self.target_alias(b);
        }
        self.dml_tail(b);
    }

    /// `MERGE [INTO] t [[AS] a] USING s [[AS] b] ON ... WHEN [NOT] MATCHED [BY ...] [AND ...]
    /// THEN UPDATE SET ... | DELETE | INSERT [(cols)] VALUES (...) | DO NOTHING ...`.
    pub(super) fn merge(&mut self, b: usize) {
        self.pos += 1;
        if self.at_word("TOP") {
            self.pos += 1;
            self.top_value();
        }
        self.mark(
            self.pos,
            SlotKind::Keywords(&["INTO"]),
            Clause::InsertTarget,
        );
        self.eat_word("INTO");
        self.in_clause(Clause::InsertTarget, |p| {
            p.from_item(b, Clause::InsertTarget, false);
        });
        let target = self.queries[b].relations.first().cloned();
        if self.at_word("USING") {
            self.in_clause(Clause::From, |p| {
                p.pos += 1;
                p.from_item(b, Clause::From, false);
            });
        }
        self.mark(self.pos, SlotKind::Keywords(&["ON"]), Clause::From);
        if self.at_word("ON") {
            self.in_clause(Clause::JoinOn, |p| {
                p.pos += 1;
                p.expr(Clause::JoinOn, ExprMode::Plain);
            });
        }
        loop {
            self.mark(
                self.pos,
                SlotKind::Keywords(words::MERGE_WHEN),
                Clause::Other,
            );
            if !self.eat_word("WHEN") {
                break;
            }
            while self.word_in(self.pos, words::MERGE_MATCH) {
                self.pos += 1;
            }
            if self.eat_word("AND") {
                self.expr(Clause::Where, ExprMode::Plain);
            }
            self.mark(self.pos, SlotKind::Keywords(&["THEN"]), Clause::Other);
            self.eat_word("THEN");
            self.mark(
                self.pos,
                SlotKind::Keywords(words::MERGE_ACTIONS),
                Clause::Other,
            );
            if self.eat_word("UPDATE") {
                self.mark(self.pos, SlotKind::Keywords(&["SET"]), Clause::Other);
                if self.eat_word("SET") {
                    self.in_clause(Clause::UpdateSet, |p| p.assignments(target.as_ref()));
                }
            } else if self.eat_word("INSERT") {
                if self.at_punct(b'(') {
                    self.in_clause(Clause::InsertColumns, |p| {
                        p.column_list(target.as_ref(), Clause::InsertColumns);
                    });
                }
                if self.at_word("VALUES") {
                    self.in_clause(Clause::Values, |p| {
                        p.pos += 1;
                        p.values_rows();
                    });
                } else if self.at_word("DEFAULT") {
                    self.pos += 2;
                }
            } else if self.at_word("DELETE") || self.at_word("DO") {
                // DELETE | DO NOTHING
                self.pos += 1;
                self.eat_word("NOTHING");
            }
        }
        self.output_clause(b);
        self.dml_tail(b);
    }

    /// MySQL / SQLite / T-SQL modifiers of `INSERT`, `UPDATE`, `DELETE`.
    fn dml_modifiers(&mut self) {
        loop {
            if self.word_in(self.pos, words::DML_MODIFIERS) {
                self.pos += 1;
            } else if self.at_word("OR") {
                // SQLite `INSERT OR REPLACE`, `UPDATE OR IGNORE`.
                self.pos += 2;
            } else if self.at_word("TOP") {
                self.pos += 1;
                self.top_value();
            } else {
                break;
            }
        }
    }

    /// When the first relation of `b` (a DML target) is a bare name that a later relation
    /// declares as its alias (`UPDATE o SET ... FROM orders o`, `DELETE t FROM t JOIN ...`),
    /// that relation becomes the target.
    fn target_alias(&mut self, b: usize) {
        let Some(block) = self.queries.get(b) else {
            return;
        };
        let Some(first) = block.relations.first() else {
            return;
        };
        let RelationSource::Named(parts) = &first.source else {
            return;
        };
        let ([name], None) = (parts.as_slice(), &first.alias) else {
            return;
        };
        let case = self.case;
        let found = block
            .relations
            .iter()
            .skip(1)
            .position(|r| match (&r.alias, &r.source) {
                (Some(alias), _) => super::same_name(case, name, alias),
                (None, RelationSource::Named(p)) => p
                    .last()
                    .is_some_and(|last| super::same_name(case, name, last)),
                _ => false,
            });
        let Some(at) = found else {
            return;
        };
        let first_start = first.range.start;
        let relations = &mut self.queries[b].relations;
        let declared = relations.remove(at + 1);
        relations[0] = declared;
        if let Some(r) = self
            .refs
            .iter_mut()
            .rev()
            .find(|r| r.role == RefRole::Relation && r.range().start == first_start)
        {
            r.role = RefRole::Unknown;
        }
    }

    /// `(a, b, ...)` of column names at the current `(` (insert columns, `USING`, conflict
    /// targets, referenced columns): columns of `only` when given.
    pub(super) fn column_list(&mut self, only: Option<&ScopeRelation>, clause: Clause) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        loop {
            self.mark_only(self.pos, SlotKind::Columns, clause, only);
            if self.is_name(self.pos) {
                let path = self.path(SlotKind::Columns, clause);
                self.push_path(&path, RefRole::Column, clause);
            }
            if !self.eat_punct(b',') {
                break;
            }
        }
        self.close_paren();
        self.leave();
    }

    /// `a = expr, (b, c) = (...), ...` of `SET` (columns of `only` on the left).
    pub(super) fn assignments(&mut self, only: Option<&ScopeRelation>) {
        let clause = Clause::UpdateSet;
        loop {
            self.mark_only(self.pos, SlotKind::Columns, clause, only);
            if self.at_punct(b'(') {
                self.column_list(only, clause);
            } else if self.is_name(self.pos) {
                let path = self.path(SlotKind::Columns, clause);
                self.push_path(&path, RefRole::Column, clause);
            } else if self.peek().is_some_and(|t| t.kind == Tk::Param) {
                self.pos += 1;
            } else {
                break;
            }
            // `=`, `:=`, T-SQL compound `+=`.
            if self.peek().is_some_and(|t| t.kind == Tk::Assign) || self.at_punct(b'=') {
                self.pos += 1;
            } else if self.is_punct(self.pos + 1, b'=')
                && self.peek().is_some_and(|t| matches!(t.kind, Tk::Punct(_)))
            {
                self.pos += 2;
            }
            self.expr(clause, ExprMode::Plain);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// T-SQL `OUTPUT inserted.a, ... [INTO @t [(cols)]]`.
    pub(super) fn output_clause(&mut self, b: usize) {
        if !self.at_word("OUTPUT") {
            return;
        }
        self.in_clause(Clause::SelectList, |p| {
            p.pos += 1;
            p.select_list(b);
        });
        if self.eat_word("INTO") {
            self.mark(self.pos, SlotKind::Nothing, Clause::NewName);
            if self
                .peek()
                .is_some_and(|t| matches!(t.kind, Tk::Param | Tk::Word | Tk::Quoted))
            {
                self.pos += 1;
            }
            if self.at_punct(b'(') {
                self.skip_group();
            }
        }
    }

    /// The tail clauses of `INSERT`, `UPDATE`, `DELETE`, `MERGE`: `WHERE` (`CURRENT OF c`),
    /// `ORDER BY`, `LIMIT`, `RETURNING`, `OUTPUT`, `OPTION (...)`.
    pub(super) fn dml_tail(&mut self, b: usize) {
        loop {
            self.mark(self.pos, SlotKind::Keywords(words::DML_TAIL), Clause::Other);
            let before = self.pos;
            let word = self.upper(self.pos);
            match word.as_str() {
                "WHERE" => self.in_clause(Clause::Where, |p| {
                    p.pos += 1;
                    if p.at_word("CURRENT") && p.is_word(p.pos + 1, "OF") {
                        p.pos += 3;
                    } else {
                        p.expr(Clause::Where, ExprMode::Plain);
                    }
                }),
                "ORDER" => self.in_clause(Clause::OrderBy, |p| {
                    p.pos += 1;
                    p.expect_by(Clause::OrderBy);
                    p.order_list(Clause::OrderBy);
                }),
                "LIMIT" => self.in_clause(Clause::Limit, |p| {
                    p.pos += 1;
                    p.expr_list(Clause::Limit);
                }),
                "RETURNING" => self.in_clause(Clause::SelectList, |p| {
                    p.pos += 1;
                    p.select_list(b);
                }),
                "OUTPUT" => self.output_clause(b),
                "OPTION" if self.is_punct(self.pos + 1, b'(') => {
                    self.pos += 1;
                    self.skip_group();
                }
                _ => break,
            }
            if self.pos == before {
                break;
            }
        }
    }
}
