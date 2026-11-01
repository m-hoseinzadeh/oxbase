//! The sources of completion items, one per expected kind.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use oxbase_dialect::{FunctionInfo, TypeParams};
use oxbase_schema_cache::{NameHit, NameKind, RelationId, SchemaCache, Target};
use oxbase_types::{
    CatalogColumn, CatalogRelation, IdentCase, RelationKind, Result, RoutineKind, Snippet,
};

use super::CompletionKind;
use super::rank::{Collector, Pass, Rank, Tier, adapt_case, escape_snippet, quote_with, sub};
use crate::outline::Clause;
use crate::{Cte, Expected, Ident, RelationSource, ScopeRelation};

/// Most join conditions offered after `JOIN`.
const MAX_JOINS: usize = 50;
/// How deep `*` is expanded through CTEs and derived tables (also stops a recursive CTE).
const MAX_STAR_DEPTH: usize = 8;

/// Columns a scope relation contributes.
enum Columns {
    /// A catalog relation.
    Catalog(RelationId),
    /// Names known from the statement (CTE, derived table, `VALUES`, alias list), with a
    /// detail when the column could be traced to a catalog column.
    Listed(Vec<(String, Option<String>)>),
}

/// A column that matched, before it becomes an item.
struct Found {
    name: String,
    detail: Option<String>,
    doc: Option<String>,
    tier: Tier,
    score: u32,
}

/// One relation columns come from.
struct Source {
    /// What qualifies its columns when a name is ambiguous (alias, else the name as written).
    handle: Option<String>,
    columns: Columns,
}

fn relation_word(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MatView => "materialized view",
        RelationKind::Foreign => "foreign table",
        RelationKind::Temporary => "temporary table",
    }
}

fn relation_kind(kind: RelationKind) -> CompletionKind {
    match kind {
        RelationKind::View | RelationKind::MatView => CompletionKind::View,
        RelationKind::Table | RelationKind::Foreign | RelationKind::Temporary => {
            CompletionKind::Table
        }
    }
}

/// `type · nullable / not null · PK / FK → table` of a catalog column.
fn column_detail(rel: &CatalogRelation, col: &CatalogColumn) -> String {
    let mut out = col.native_type.clone();
    out.push_str(if col.nullable {
        " · nullable"
    } else {
        " · not null"
    });
    if col.primary_key {
        out.push_str(" · PK");
    }
    if let Some(fk) = rel
        .foreign_keys
        .iter()
        .find(|fk| fk.columns.contains(&col.name))
    {
        let _ = write!(out, " · FK → {}", fk.ref_table);
    }
    out
}

/// A signature line: `name(args) → returns`.
fn signature(name: &str, args: &str, returns: Option<&str>) -> String {
    match returns {
        Some(r) if !r.is_empty() => format!("{name}({args}) → {r}"),
        _ => format!("{name}({args})"),
    }
}

/// The schemas a name typed after the qualifier may live in.
#[derive(Debug, Clone, Copy)]
enum Scope {
    /// No qualifier: every schema (names outside the search path are inserted qualified).
    Any,
    /// `schema.` or `database.schema.`.
    Schema(u32),
    /// The qualifier names no schema of the cache (or there is no cache).
    Nowhere,
}

/// The first catalog relation a name in the statement refers to (search path first, then any
/// schema for a single part).
fn resolve_named(cache: &SchemaCache, parts: &[Ident]) -> Option<RelationId> {
    cache
        .resolve_relation(parts)
        .first()
        .copied()
        .or_else(|| match parts {
            [one] => cache.relations_named(one).first().copied(),
            _ => None,
        })
}

/// `true` when `a` and `b` are the same names (ignoring case).
fn same_names(a: &[Ident], b: &[Ident]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.text.to_lowercase() == y.text.to_lowercase())
}

fn part(text: &str) -> Ident {
    Ident {
        text: text.to_owned(),
        quoted: true,
    }
}

impl Collector<'_> {
    /// The CTE a one-part relation name refers to (a CTE hides a table of the same name),
    /// except the CTE whose body is block `inside` (a non-recursive CTE does not see itself)
    /// and a CTE whose definition contains the cursor.
    fn cte_for(&self, parts: &[Ident], inside: Option<usize>) -> Option<&Cte> {
        let [one] = parts else {
            return None;
        };
        let case = self.rules.relations;
        let offset = self.ctx.offset;
        self.ctx.ctes.iter().find(|c| {
            case.matches(&one.text, one.quoted, &c.name.text)
                && (inside.is_none() || c.query != inside)
                && !c.range.contains(&offset)
        })
    }

    /// An identifier of the statement as it was written (quoted when it was).
    fn as_written(&self, ident: &Ident) -> String {
        if ident.quoted {
            self.dialect.quoting.quote_ident(&ident.text)
        } else {
            ident.text.clone()
        }
    }

    /// Which schemas the qualifier before the cursor allows.
    fn qualifier_scope(&self) -> Scope {
        let Some(cache) = self.cache else {
            return Scope::Nowhere;
        };
        let found = match self.ctx.qualifier.as_slice() {
            [] => return Scope::Any,
            [schema] => cache.resolve_schema(schema),
            [database, schema] => {
                let db = &cache.snapshot().database;
                if self
                    .rules
                    .schemas
                    .matches(&database.text, database.quoted, db)
                {
                    cache.resolve_schema(schema)
                } else {
                    None
                }
            }
            _ => None,
        };
        found.map_or(Scope::Nowhere, Scope::Schema)
    }

    /// `name` of schema `s`, qualified with the schema when no qualifier was typed and `s` is
    /// not on the search path.
    fn schema_level_insert(
        &self,
        cache: &SchemaCache,
        s: u32,
        name: &str,
        case: IdentCase,
    ) -> String {
        let name = self.ins_name(name, case);
        if self.ctx.qualifier.is_empty() && !self.in_path(s) {
            let schema = cache.schema(s).map_or("", |x| x.name.as_str());
            format!(
                "{}.{name}",
                self.dialect.quote_name(schema, self.rules.schemas)
            )
        } else {
            name
        }
    }

    // --- keywords and snippets -------------------------------------------------------------

    pub(super) fn keywords(&mut self, pass: Pass) -> Result<()> {
        if self.pattern.quoted || !self.ctx.qualifier.is_empty() {
            return Ok(());
        }
        let ctx = self.ctx;
        let listed = &ctx.keywords;
        for (i, kw) in listed.iter().enumerate() {
            let Some((tier, _)) = self.pattern.matches(pass, kw) else {
                continue;
            };
            let label = adapt_case(kw, &self.pattern.raw);
            let mut item = self.item(label.clone(), CompletionKind::Keyword, label);
            item.detail = Some("keyword".to_owned());
            let rank = Rank {
                tier,
                group: self.group(Expected::Keyword, sub::MAIN),
                score: 999_u32.saturating_sub(u32::try_from(i).unwrap_or(999)),
                recent: None,
            };
            self.push(item, rank);
        }
        // Every other keyword of the dialect only at statement level (§4.3); inside a clause
        // the context's list is all there is.
        if ctx.clause != Clause::Start {
            return Ok(());
        }
        for kw in self.dialect.language.keywords {
            self.tick()?;
            if listed.iter().any(|l| l.eq_ignore_ascii_case(kw)) {
                continue;
            }
            let Some((tier, score)) = self.pattern.matches(pass, kw) else {
                continue;
            };
            let label = adapt_case(kw, &self.pattern.raw);
            let mut item = self.item(label.clone(), CompletionKind::Keyword, label);
            item.detail = Some("keyword".to_owned());
            let group = self.group(Expected::Keyword, sub::OTHER);
            self.push(
                item,
                Rank {
                    tier,
                    group,
                    score,
                    recent: None,
                },
            );
        }
        Ok(())
    }

    pub(super) fn snippets(&mut self, pass: Pass, snippets: &[Snippet]) {
        if pass != Pass::Prefix || self.pattern.quoted || !self.ctx.qualifier.is_empty() {
            return;
        }
        for s in snippets
            .iter()
            .filter(|s| s.applies_to(self.dialect.engine))
        {
            let Some((_, score)) = self.pattern.prefix_match(&s.prefix) else {
                continue;
            };
            let mut item = self.item(s.prefix.clone(), CompletionKind::Snippet, s.body.clone());
            item.is_snippet = true;
            item.detail = Some(s.name.clone());
            item.documentation = (!s.description.is_empty()).then(|| s.description.clone());
            // A snippet never outranks a keyword spelled the same: no exact tier.
            let rank = Rank {
                tier: Tier::Prefix,
                group: self.group(Expected::Snippet, sub::MAIN),
                score,
                recent: None,
            };
            self.push(item, rank);
        }
    }

    // --- functions, procedures, types, sequences ------------------------------------------

    pub(super) fn functions(&mut self, pass: Pass) -> Result<()> {
        if !self.pattern.quoted && self.ctx.qualifier.is_empty() {
            self.builtin_functions(pass)?;
        }
        self.routines(pass, false)
    }

    fn builtin_functions(&mut self, pass: Pass) -> Result<()> {
        let functions = self.dialect.language.functions;
        let group = self.group(Expected::Function, sub::MAIN);
        let mut i = 0;
        while i < functions.len() {
            self.tick()?;
            let name = functions[i].name;
            let run = functions[i..]
                .iter()
                .take_while(|f| f.name.eq_ignore_ascii_case(name))
                .count()
                .max(1);
            let overloads = &functions[i..i + run];
            i += run;
            let available: Vec<&FunctionInfo> = overloads
                .iter()
                .filter(|f| {
                    self.dialect
                        .version
                        .as_ref()
                        .is_none_or(|v| f.available_on(v))
                })
                .collect();
            let Some(first) = available.first() else {
                continue;
            };
            let Some((tier, score)) = self.pattern.matches(pass, name) else {
                continue;
            };
            let label = adapt_case(name, &self.pattern.raw);
            let no_args = available.iter().all(|f| f.max_args == Some(0));
            let (insert, is_snippet) = if no_args {
                (format!("{label}()"), false)
            } else {
                (format!("{}($0)", escape_snippet(&label)), true)
            };
            let mut item = self.item(label.clone(), CompletionKind::Function, insert);
            item.is_snippet = is_snippet;
            item.detail = Some(signature(&label, first.args, Some(first.returns)));
            let mut doc = first.summary.to_owned();
            if available.len() > 1 {
                doc.push_str("\n\n");
                for f in &available {
                    let _ = writeln!(doc, "- `{}`", signature(&label, f.args, Some(f.returns)));
                }
            }
            item.documentation = Some(doc);
            item.filter_text = (label != name).then(|| name.to_owned());
            self.push(
                item,
                Rank {
                    tier,
                    group,
                    score,
                    recent: Some(name),
                },
            );
        }
        Ok(())
    }

    /// Cached routines: procedures for `CALL` / `EXEC` (`procedures`), else functions and
    /// aggregates.
    fn routines(&mut self, pass: Pass, procedures: bool) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        let schema = match self.qualifier_scope() {
            Scope::Nowhere => return Ok(()),
            Scope::Any => None,
            Scope::Schema(s) => Some(s),
        };
        let expected = if procedures {
            Expected::Procedure
        } else {
            Expected::Function
        };
        let group = self.group(expected, sub::MAIN);
        for (hit, tier) in self.search(pass, &[NameKind::Routine], schema, None, 0) {
            self.tick()?;
            let Target::Routine(id) = hit.target else {
                continue;
            };
            let Some(routine) = cache.routine(id) else {
                continue;
            };
            if (routine.kind == RoutineKind::Procedure) != procedures {
                continue;
            }
            let name = routine.name.as_str();
            let base = self.schema_level_insert(cache, id.schema, name, self.rules.routines);
            let (kind, insert, is_snippet) = if procedures {
                (CompletionKind::Procedure, base, false)
            } else if routine.args.trim().is_empty() {
                (CompletionKind::Function, format!("{base}()"), false)
            } else {
                (
                    CompletionKind::Function,
                    format!("{}($0)", escape_snippet(&base)),
                    true,
                )
            };
            let filter = self.name_filter(name, &insert);
            let mut item = self.item(name.to_owned(), kind, insert);
            item.is_snippet = is_snippet;
            item.detail = Some(signature(name, &routine.args, routine.returns.as_deref()));
            let schema_name = cache.schema(id.schema).map_or("", |s| s.name.as_str());
            item.documentation = Some(format!(
                "{} in schema `{schema_name}`",
                if procedures { "Procedure" } else { "Function" }
            ));
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: Some(name),
            };
            self.push(item, rank);
        }
        Ok(())
    }

    pub(super) fn procedures(&mut self, pass: Pass) -> Result<()> {
        self.routines(pass, true)
    }

    pub(super) fn types(&mut self, pass: Pass) -> Result<()> {
        let group = self.group(Expected::Type, sub::MAIN);
        if !self.pattern.quoted && self.ctx.qualifier.is_empty() {
            for t in self.dialect.language.types {
                self.tick()?;
                let Some((tier, score)) = self.pattern.matches(pass, t.name) else {
                    continue;
                };
                let label = adapt_case(t.name, &self.pattern.raw);
                let params = match t.params {
                    TypeParams::None => None,
                    TypeParams::Length => Some("${1:n}"),
                    TypeParams::Precision => Some("${1:p}"),
                    TypeParams::PrecisionScale => Some("${1:p}, ${2:s}"),
                };
                let insert = match params {
                    Some(p) => format!("{}({p})", escape_snippet(&label)),
                    None => label.clone(),
                };
                let mut item = self.item(label.clone(), CompletionKind::Type, insert);
                item.is_snippet = params.is_some();
                item.detail = Some("type".to_owned());
                item.documentation = Some(t.summary.to_owned());
                item.filter_text = (label != t.name).then(|| t.name.to_owned());
                self.push(
                    item,
                    Rank {
                        tier,
                        group,
                        score,
                        recent: None,
                    },
                );
            }
        }
        self.schema_names(
            pass,
            NameKind::Type,
            CompletionKind::Type,
            Expected::Type,
            "type",
        )
    }

    pub(super) fn sequences(&mut self, pass: Pass) -> Result<()> {
        self.schema_names(
            pass,
            NameKind::Sequence,
            CompletionKind::Sequence,
            Expected::Sequence,
            "sequence",
        )
    }

    /// Cached types or sequences.
    fn schema_names(
        &mut self,
        pass: Pass,
        name_kind: NameKind,
        kind: CompletionKind,
        expected: Expected,
        word: &str,
    ) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        let schema = match self.qualifier_scope() {
            Scope::Nowhere => return Ok(()),
            Scope::Any => None,
            Scope::Schema(s) => Some(s),
        };
        let group = self.group(expected, sub::MAIN);
        for (hit, tier) in self.search(pass, &[name_kind], schema, None, 0) {
            self.tick()?;
            let Some(s) = hit.target.schema() else {
                continue;
            };
            let name = cache.name_of(hit.target);
            let insert = self.schema_level_insert(cache, s, name, self.rules.relations);
            let filter = self.name_filter(name, &insert);
            let mut item = self.item(name.to_owned(), kind, insert);
            let schema_name = cache.schema(s).map_or("", |x| x.name.as_str());
            item.detail = Some(format!("{word} · {schema_name}"));
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: None,
            };
            self.push(item, rank);
        }
        Ok(())
    }

    // --- schemas and databases ------------------------------------------------------------

    pub(super) fn schemas(&mut self, pass: Pass) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        let allowed = match self.ctx.qualifier.as_slice() {
            [] => true,
            [db] => self
                .rules
                .schemas
                .matches(&db.text, db.quoted, &cache.snapshot().database),
            _ => false,
        };
        if !allowed {
            return Ok(());
        }
        let group = self.group(Expected::Schema, sub::MAIN);
        for (hit, tier) in self.search(pass, &[NameKind::Schema], None, None, 0) {
            self.tick()?;
            let Target::Schema(s) = hit.target else {
                continue;
            };
            let Some(schema) = cache.schema(s) else {
                continue;
            };
            let insert = self.ins_name(&schema.name, self.rules.schemas);
            let filter = self.name_filter(&schema.name, &insert);
            let mut item = self.item(schema.name.clone(), CompletionKind::Schema, insert);
            item.detail = Some(
                if schema.system {
                    "system schema"
                } else {
                    "schema"
                }
                .to_owned(),
            );
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: None,
            };
            self.push(item, rank);
        }
        Ok(())
    }

    pub(super) fn databases(&mut self, pass: Pass) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        if !self.ctx.qualifier.is_empty() {
            return Ok(());
        }
        let group = self.group(Expected::Database, sub::MAIN);
        for (hit, tier) in self.search(pass, &[NameKind::Database], None, None, 0) {
            self.tick()?;
            let name = cache.name_of(hit.target);
            let insert = self.ins_name(name, self.rules.schemas);
            let filter = self.name_filter(name, &insert);
            let mut item = self.item(name.to_owned(), CompletionKind::Database, insert);
            let current = name == cache.snapshot().database;
            item.detail = Some(
                if current {
                    "current database"
                } else {
                    "database"
                }
                .to_owned(),
            );
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: None,
            };
            self.push(item, rank);
        }
        Ok(())
    }

    // --- relations ------------------------------------------------------------------------

    pub(super) fn relations(&mut self, pass: Pass) -> Result<()> {
        if self.ctx.qualifier.is_empty() {
            if self.ctx.clause == Clause::Join && !self.pattern.quoted {
                self.joins(pass)?;
            }
            self.cte_names(pass);
        }
        let Some(cache) = self.cache else {
            return Ok(());
        };
        match self.qualifier_scope() {
            Scope::Nowhere => Ok(()),
            Scope::Schema(s) => {
                let hits = self.search(pass, &[NameKind::Relation], Some(s), None, 0);
                self.relation_items(cache, hits, sub::MAIN)
            }
            Scope::Any => {
                let path = self.path.clone();
                let mut on_path = 0;
                for s in &path {
                    let hits = self.search(pass, &[NameKind::Relation], Some(*s), None, 0);
                    on_path += hits.len();
                    self.relation_items(cache, hits, sub::MAIN)?;
                }
                let schemas = cache.snapshot().schemas.len();
                if path.len() < schemas {
                    let hits: Vec<_> = self
                        .search(pass, &[NameKind::Relation], None, None, on_path)
                        .into_iter()
                        .filter(|(h, _)| h.target.schema().is_some_and(|s| !path.contains(&s)))
                        .collect();
                    self.relation_items(cache, hits, sub::OTHER)?;
                }
                Ok(())
            }
        }
    }

    fn relation_items(
        &mut self,
        cache: &SchemaCache,
        hits: Vec<(NameHit, Tier)>,
        position: u16,
    ) -> Result<()> {
        let group = self.group(Expected::Relation, position);
        for (hit, tier) in hits {
            self.tick()?;
            let Target::Relation(id) = hit.target else {
                continue;
            };
            let (Some(rel), Some(schema)) = (cache.relation(id), cache.schema(id.schema)) else {
                continue;
            };
            let insert =
                self.schema_level_insert(cache, id.schema, &rel.name, self.rules.relations);
            let filter = self.name_filter(&rel.name, &insert);
            let mut item = self.item(rel.name.clone(), relation_kind(rel.kind), insert);
            item.detail = Some(format!("{} · {}", relation_word(rel.kind), schema.name));
            item.documentation.clone_from(&rel.comment);
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: Some(&rel.name),
            };
            self.push(item, rank);
        }
        Ok(())
    }

    /// CTE names in scope, offered as relations.
    fn cte_names(&mut self, pass: Pass) {
        let ctx = self.ctx;
        let group = self.group(Expected::Relation, sub::ALIAS);
        for cte in &ctx.ctes {
            let Some((tier, score)) = self.pattern.matches(pass, &cte.name.text) else {
                continue;
            };
            let insert = match ctx.quote {
                Some(open) => quote_with(open, &cte.name.text),
                None => self.as_written(&cte.name),
            };
            let filter = self.name_filter(&cte.name.text, &insert);
            let mut item = self.item(cte.name.text.clone(), CompletionKind::Alias, insert);
            item.detail = Some("CTE".to_owned());
            item.filter_text = filter;
            self.push(
                item,
                Rank {
                    tier,
                    group,
                    score,
                    recent: None,
                },
            );
        }
    }

    /// Join conditions from foreign keys between the relations in scope and other relations
    /// (`customers c ON c.id = o.customer_id`).
    fn joins(&mut self, pass: Pass) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        let ctx = self.ctx;
        let mut taken: HashSet<String> = HashSet::new();
        let mut anchors: Vec<(RelationId, String)> = Vec::new();
        for r in &ctx.scope {
            let written = match (&r.alias, &r.source) {
                (Some(alias), _) => Some(alias),
                (None, RelationSource::Named(parts)) => parts.last(),
                _ => None,
            };
            if let Some(w) = written {
                taken.insert(w.text.to_lowercase());
            }
            if let (RelationSource::Named(parts), Some(w)) = (&r.source, written)
                && self.cte_for(parts, None).is_none()
                && let Some(id) = resolve_named(cache, parts)
            {
                anchors.push((id, self.as_written(w)));
            }
        }
        let mut made = 0;
        for (anchor, handle) in anchors {
            let (Some(rel), Some(schema)) = (cache.relation(anchor), cache.schema(anchor.schema))
            else {
                continue;
            };
            // Outgoing: the anchor references the target.
            for fk in &rel.foreign_keys {
                self.tick()?;
                let target_schema = fk.ref_schema.as_deref().unwrap_or(&schema.name);
                let path = [part(target_schema), part(&fk.ref_table)];
                if let Some(target) = cache.resolve_relation(&path).first().copied()
                    && self.join_item(
                        pass,
                        cache,
                        target,
                        (fk.ref_columns.as_slice(), fk.columns.as_slice()),
                        &handle,
                        &taken,
                    )
                {
                    made += 1;
                }
            }
            // Incoming: another relation references the anchor.
            for (s, other_schema) in cache.snapshot().schemas.iter().enumerate() {
                for (r, other) in other_schema.relations.iter().enumerate() {
                    self.tick()?;
                    if made >= MAX_JOINS {
                        return Ok(());
                    }
                    for fk in &other.foreign_keys {
                        let ref_schema = fk.ref_schema.as_deref().unwrap_or(&other_schema.name);
                        if fk.ref_table != rel.name || ref_schema != schema.name {
                            continue;
                        }
                        let target = RelationId {
                            schema: u32::try_from(s).unwrap_or(u32::MAX),
                            index: u32::try_from(r).unwrap_or(u32::MAX),
                        };
                        if self.join_item(
                            pass,
                            cache,
                            target,
                            (fk.columns.as_slice(), fk.ref_columns.as_slice()),
                            &handle,
                            &taken,
                        ) {
                            made += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// One join item: `target alias ON alias.t1 = handle.a1 AND ...` for column pairs
    /// (`target` side, anchor side). `true` when it was added.
    fn join_item(
        &mut self,
        pass: Pass,
        cache: &SchemaCache,
        target: RelationId,
        (target_cols, anchor_cols): (&[String], &[String]),
        handle: &str,
        taken: &HashSet<String>,
    ) -> bool {
        let Some(rel) = cache.relation(target) else {
            return false;
        };
        if target_cols.is_empty() || target_cols.len() != anchor_cols.len() {
            return false;
        }
        let Some((tier, score)) = self.pattern.matches(pass, &rel.name) else {
            return false;
        };
        let alias = self.join_alias(&rel.name, taken);
        let name = self.schema_level_insert(cache, target.schema, &rel.name, self.rules.relations);
        let cols = self.rules.columns;
        let condition: Vec<String> = target_cols
            .iter()
            .zip(anchor_cols)
            .map(|(t, a)| {
                format!(
                    "{alias}.{} = {handle}.{}",
                    self.dialect.quote_name(t, cols),
                    self.dialect.quote_name(a, cols)
                )
            })
            .collect();
        let text = format!("{name} {alias} ON {}", condition.join(" AND "));
        let mut item = self.item(text.clone(), CompletionKind::Join, escape_snippet(&text));
        item.is_snippet = true;
        item.detail = Some(format!("join · {}", relation_word(rel.kind)));
        item.filter_text = Some(rel.name.clone());
        let rank = Rank {
            tier,
            group: self.group(Expected::Relation, sub::JOIN),
            score,
            recent: Some(&rel.name),
        };
        self.push(item, rank);
        true
    }

    /// A short alias for `name` (its word initials: `customer_order_17` → `co`) that is not
    /// in `taken` and needs no quotes.
    fn join_alias(&self, name: &str, taken: &HashSet<String>) -> String {
        let mut base: String = name
            .split(|c: char| !c.is_alphanumeric())
            .filter_map(|w| w.chars().next())
            .filter(|c| c.is_alphabetic())
            .flat_map(char::to_lowercase)
            .collect();
        if base.is_empty() {
            base.push('t');
        }
        let free = |a: &str| {
            !taken.contains(a) && !self.dialect.language.needs_quotes(a, IdentCase::FoldLower)
        };
        if free(&base) {
            return base;
        }
        let mut n = 2_u32;
        loop {
            let alias = format!("{base}{n}");
            if free(&alias) {
                return alias;
            }
            n += 1;
        }
    }

    // --- columns --------------------------------------------------------------------------

    /// Where the columns of `rel` come from, if that is known.
    fn source_of(&self, rel: &ScopeRelation) -> Option<Source> {
        let ctx = self.ctx;
        let handle = match (&rel.alias, &rel.source) {
            (Some(alias), _) => Some(self.as_written(alias)),
            (None, RelationSource::Named(parts)) => parts.last().map(|p| self.as_written(p)),
            (None, RelationSource::Cte { query, index }) => ctx
                .outline
                .queries
                .get(*query)
                .and_then(|q| q.ctes.get(*index))
                .map(|c| self.as_written(&c.name)),
            (None, _) => None,
        };
        if !rel.column_aliases.is_empty() {
            let names = rel
                .column_aliases
                .iter()
                .map(|c| (c.text.clone(), None))
                .collect();
            return Some(Source {
                handle,
                columns: Columns::Listed(names),
            });
        }
        let columns = match &rel.source {
            RelationSource::Named(parts) => {
                // A CTE written as a plain name hides a catalog relation of that name.
                if let Some(cte) = self.cte_for(parts, None) {
                    Columns::Listed(self.block_columns(&cte.columns, cte.query, 0))
                } else {
                    Columns::Catalog(resolve_named(self.cache?, parts)?)
                }
            }
            RelationSource::Cte { query, index } => {
                let cte = ctx.outline.queries.get(*query)?.ctes.get(*index)?;
                Columns::Listed(self.block_columns(&cte.columns, cte.query, 0))
            }
            RelationSource::Derived { query, columns } => {
                Columns::Listed(self.block_columns(columns, Some(*query), 0))
            }
            RelationSource::Values { columns } => {
                Columns::Listed(columns.iter().map(|c| (c.text.clone(), None)).collect())
            }
            RelationSource::Function(_) => return None,
        };
        Some(Source { handle, columns })
    }

    /// The output columns of a CTE body or derived table `block`: `names` (the block's own
    /// output names when empty) with the catalog detail of the column each comes from when
    /// one of the block's relations has it; when the block selects `*` (and `names` are its
    /// outputs, not an explicit column list), also every column of the block's relations,
    /// through CTEs and derived tables up to [`MAX_STAR_DEPTH`] levels.
    fn block_columns(
        &self,
        names: &[Ident],
        block: Option<usize>,
        depth: usize,
    ) -> Vec<(String, Option<String>)> {
        let q = block.and_then(|b| self.ctx.outline.queries.get(b));
        let outputs = q.map_or(&[][..], |q| q.output.as_slice());
        let star = q.is_some_and(|q| q.star) && (names.is_empty() || same_names(names, outputs));
        let names = if names.is_empty() { outputs } else { names };
        let relations: Vec<RelationId> = match (self.cache, q) {
            (Some(cache), Some(q)) => q
                .relations
                .iter()
                .filter_map(|r| match &r.source {
                    RelationSource::Named(parts) if self.cte_for(parts, block).is_none() => {
                        resolve_named(cache, parts)
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut out: Vec<(String, Option<String>)> = names
            .iter()
            .map(|n| {
                let detail = self.cache.and_then(|cache| {
                    relations.iter().find_map(|id| {
                        let c = cache.resolve_column(*id, n)?;
                        let rel = cache.relation(*id)?;
                        Some(column_detail(rel, rel.columns.get(c as usize)?))
                    })
                });
                (n.text.clone(), detail)
            })
            .collect();
        if let Some(q) = q.filter(|_| star && depth < MAX_STAR_DEPTH) {
            let mut seen: HashSet<String> = out.iter().map(|(n, _)| n.to_lowercase()).collect();
            for r in &q.relations {
                for (name, detail) in self.relation_columns(r, block, depth + 1) {
                    if seen.insert(name.to_lowercase()) {
                        out.push((name, detail));
                    }
                }
            }
        }
        out
    }

    /// Every column relation `r` of block `inside` contributes (for `*`).
    fn relation_columns(
        &self,
        r: &ScopeRelation,
        inside: Option<usize>,
        depth: usize,
    ) -> Vec<(String, Option<String>)> {
        if !r.column_aliases.is_empty() {
            return r
                .column_aliases
                .iter()
                .map(|c| (c.text.clone(), None))
                .collect();
        }
        match &r.source {
            RelationSource::Named(parts) => {
                if let Some(cte) = self.cte_for(parts, inside) {
                    return self.block_columns(&cte.columns, cte.query, depth);
                }
                let Some(cache) = self.cache else {
                    return Vec::new();
                };
                resolve_named(cache, parts)
                    .and_then(|id| cache.relation(id))
                    .map_or_else(Vec::new, |rel| {
                        rel.columns
                            .iter()
                            .map(|c| (c.name.clone(), Some(column_detail(rel, c))))
                            .collect()
                    })
            }
            RelationSource::Cte { query, index } => self
                .ctx
                .outline
                .queries
                .get(*query)
                .and_then(|b| b.ctes.get(*index))
                .map_or_else(Vec::new, |cte| {
                    self.block_columns(&cte.columns, cte.query, depth)
                }),
            RelationSource::Derived { query, columns } => {
                self.block_columns(columns, Some(*query), depth)
            }
            RelationSource::Values { columns } => {
                columns.iter().map(|c| (c.text.clone(), None)).collect()
            }
            RelationSource::Function(_) => Vec::new(),
        }
    }

    /// The CTE `q` names (in scope without an alias, else any visible one) as a column source.
    fn cte_named(&self, q: &Ident) -> Option<Source> {
        let ctx = self.ctx;
        let case = self.rules.relations;
        let in_scope = ctx.scope.iter().find(|r| {
            r.alias.is_none()
                && match &r.source {
                    RelationSource::Cte { query, index } => ctx
                        .outline
                        .queries
                        .get(*query)
                        .and_then(|b| b.ctes.get(*index))
                        .is_some_and(|c| case.matches(&q.text, q.quoted, &c.name.text)),
                    _ => false,
                }
        });
        if let Some(r) = in_scope {
            return self.source_of(r);
        }
        let cte = ctx
            .ctes
            .iter()
            .find(|c| case.matches(&q.text, q.quoted, &c.name.text))?;
        Some(Source {
            handle: None,
            columns: Columns::Listed(self.block_columns(&cte.columns, cte.query, 0)),
        })
    }

    pub(super) fn columns(&mut self, pass: Pass) -> Result<()> {
        let ctx = self.ctx;
        let qualified = !ctx.qualifier.is_empty();
        let scoped = ctx.column_sources(self.rules.relations);
        // The qualifier names a relation in scope (even one whose columns are unknown, like a
        // table function): no fallback below may guess other columns for it.
        let in_scope = !scoped.is_empty();
        let mut sources: Vec<Source> = scoped
            .into_iter()
            .filter_map(|r| self.source_of(r))
            .collect();
        if !in_scope
            && let [q] = ctx.qualifier.as_slice()
            && let Some(source) = self.cte_named(q)
        {
            // `x.` where `x` is a CTE without an alias.
            sources.push(source);
        }
        if sources.is_empty()
            && !in_scope
            && qualified
            && let Some(cache) = self.cache
        {
            // `x.` where `x` is a relation that is not in scope (or there is no FROM yet).
            if let Some(id) = resolve_named(cache, &ctx.qualifier) {
                sources.push(Source {
                    handle: None,
                    columns: Columns::Catalog(id),
                });
            }
        }
        if sources.is_empty() && !qualified && pass == Pass::Prefix && !self.pattern.is_empty() {
            self.loose_columns()?;
        }
        // Names that more than one source has are inserted qualified.
        let mut counts: HashMap<String, usize> = HashMap::new();
        if !qualified && sources.len() > 1 {
            for s in &sources {
                let names: Vec<String> = match &s.columns {
                    Columns::Catalog(id) => self
                        .cache
                        .and_then(|c| c.relation(*id))
                        .map_or_else(Vec::new, |r| {
                            r.columns.iter().map(|c| c.name.to_lowercase()).collect()
                        }),
                    Columns::Listed(list) => list.iter().map(|(n, _)| n.to_lowercase()).collect(),
                };
                for n in names {
                    *counts.entry(n).or_default() += 1;
                }
            }
        }
        for source in &sources {
            self.check()?;
            self.source_columns(pass, source, &counts)?;
        }
        // Aliases qualify columns, except where only the target's own columns may be written.
        let own_columns_only = matches!(ctx.clause, Clause::InsertColumns | Clause::UpdateSet);
        if !qualified && !self.pattern.quoted && !own_columns_only {
            self.aliases(pass);
        }
        Ok(())
    }

    /// The columns of one source matching the pattern.
    fn source_columns(
        &mut self,
        pass: Pass,
        source: &Source,
        counts: &HashMap<String, usize>,
    ) -> Result<()> {
        let group = self.group(Expected::Column, sub::MAIN);
        let found: Vec<Found> = match &source.columns {
            Columns::Catalog(id) => {
                let Some(cache) = self.cache else {
                    return Ok(());
                };
                let (Some(rel), Some(schema)) = (cache.relation(*id), cache.schema(id.schema))
                else {
                    return Ok(());
                };
                let doc = format!("Column of `{}.{}`", schema.name, rel.name);
                self.search(pass, &[NameKind::Column], None, Some(*id), 0)
                    .into_iter()
                    .filter_map(|(hit, tier)| match hit.target {
                        Target::Column(_, c) => {
                            let col = rel.columns.get(c as usize)?;
                            Some(Found {
                                name: col.name.clone(),
                                detail: Some(column_detail(rel, col)),
                                doc: Some(doc.clone()),
                                tier,
                                score: hit.score,
                            })
                        }
                        _ => None,
                    })
                    .collect()
            }
            Columns::Listed(list) => list
                .iter()
                .filter_map(|(name, detail)| {
                    let (tier, score) = self.pattern.matches(pass, name)?;
                    Some(Found {
                        name: name.clone(),
                        detail: detail.clone(),
                        doc: None,
                        tier,
                        score,
                    })
                })
                .collect(),
        };
        for Found {
            name,
            detail,
            doc,
            tier,
            score,
        } in found
        {
            self.tick()?;
            let mut insert = self.ins_name(&name, self.rules.columns);
            let mut label = name.clone();
            if let Some(handle) = &source.handle
                && counts.get(&name.to_lowercase()).is_some_and(|n| *n > 1)
            {
                insert = format!("{handle}.{insert}");
                label = format!("{handle}.{name}");
            }
            let filter = if label == name {
                self.name_filter(&name, &insert)
            } else {
                Some(match self.ctx.quote {
                    Some(open) => format!("{}{name}", char::from(open)),
                    None => name.clone(),
                })
            };
            let mut item = self.item(label, CompletionKind::Column, insert);
            item.detail = detail;
            item.documentation = doc;
            item.filter_text = filter;
            self.push(
                item,
                Rank {
                    tier,
                    group,
                    score,
                    recent: None,
                },
            );
        }
        Ok(())
    }

    /// Columns of any relation matching the typed prefix, one item per name (when nothing in
    /// scope says where columns come from).
    fn loose_columns(&mut self) -> Result<()> {
        let Some(cache) = self.cache else {
            return Ok(());
        };
        let group = self.group(Expected::Column, sub::LOOSE);
        let mut seen: HashSet<String> = HashSet::new();
        for (hit, tier) in self.search(Pass::Prefix, &[NameKind::Column], None, None, 0) {
            self.tick()?;
            let Target::Column(id, c) = hit.target else {
                continue;
            };
            let Some(rel) = cache.relation(id) else {
                continue;
            };
            let Some(col) = rel.columns.get(c as usize) else {
                continue;
            };
            if !seen.insert(col.name.to_lowercase()) {
                continue;
            }
            let insert = self.ins_name(&col.name, self.rules.columns);
            let filter = self.name_filter(&col.name, &insert);
            let mut item = self.item(col.name.clone(), CompletionKind::Column, insert);
            item.detail = Some(column_detail(rel, col));
            item.documentation = Some(format!("Column of `{}` and maybe others", rel.name));
            item.filter_text = filter;
            let rank = Rank {
                tier,
                group,
                score: hit.score,
                recent: None,
            };
            self.push(item, rank);
        }
        Ok(())
    }

    /// Aliases and relation names in scope (so `o` then `.` is one step away).
    fn aliases(&mut self, pass: Pass) {
        let ctx = self.ctx;
        let group = self.group(Expected::Column, sub::ALIAS);
        let mut seen: HashSet<String> = HashSet::new();
        for rel in &ctx.scope {
            let (written, detail) = match (&rel.alias, &rel.source) {
                (Some(alias), RelationSource::Named(parts)) => (
                    alias,
                    format!(
                        "alias · {}",
                        parts
                            .iter()
                            .map(|p| p.text.as_str())
                            .collect::<Vec<_>>()
                            .join(".")
                    ),
                ),
                (Some(alias), _) => (alias, "alias".to_owned()),
                (None, RelationSource::Named(parts)) => match parts.last() {
                    Some(last) => (last, "table in scope".to_owned()),
                    None => continue,
                },
                (None, _) => continue,
            };
            if !seen.insert(written.text.to_lowercase()) {
                continue;
            }
            let Some((tier, score)) = self.pattern.matches(pass, &written.text) else {
                continue;
            };
            let insert = self.as_written(written);
            let filter = self.name_filter(&written.text, &insert);
            let mut item = self.item(written.text.clone(), CompletionKind::Alias, insert);
            item.detail = Some(detail);
            item.filter_text = filter;
            self.push(
                item,
                Rank {
                    tier,
                    group,
                    score,
                    recent: None,
                },
            );
        }
    }
}
