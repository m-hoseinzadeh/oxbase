//! Hover information (`docs/m6-contracts.md` §4.4). Owner of [`hover`]: the **diagnose**
//! stream.
//!
//! The statement under the cursor is read by the diagnostics parser
//! (`crate::diagnose::analyze`), so a hover resolves names exactly the way diagnostics do:
//! through CTEs, aliases and derived tables first, then the [`SchemaCache`].

use std::fmt::Write as _;
use std::ops::Range;

use oxbase_schema_cache::{RelationId, SchemaCache};
use oxbase_types::{CatalogColumn, CatalogRelation, RelationKind, RoutineKind};
use serde::{Deserialize, Serialize};

use crate::Dialect;
use crate::diagnose::analyze::{self, Name, Rel, Source, Stmt};
use crate::diagnose::checks::{self, Env, Lookup, Resolved};
use crate::diagnose::features::features;
use crate::diagnose::scan::{Kind, Toks, floor_boundary};

/// What the editor shows over an identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hover {
    /// Byte range of the identifier (or keyword / function name) hovered.
    pub range: Range<usize>,
    /// Markdown: the object's kind and qualified name, then for a relation its columns
    /// (name, type, nullability, primary and foreign keys) and comment; for a column its type,
    /// nullability, keys and relation; for a routine its signature; for a built-in function
    /// its signature and summary; for a keyword nothing (no hover).
    pub markdown: String,
}

/// Columns listed in a relation's hover before "and N more".
const MAX_COLUMNS: usize = 40;
/// Built-in overloads listed in a function's hover.
const MAX_OVERLOADS: usize = 6;

/// The hover for the identifier at byte `offset` of `text`, resolved through the statement's
/// aliases and CTEs and then `cache`. `None` when there is nothing to say (whitespace, a
/// literal, a name the cache does not know and that is not built in). Never panics.
pub fn hover(
    dialect: &Dialect,
    text: &str,
    offset: usize,
    cache: Option<&SchemaCache>,
) -> Option<Hover> {
    let offset = floor_boundary(text, offset);
    let feat = features(dialect.sql);
    let statements = oxbase_sql::split(dialect.sql, text);
    let statement = statements
        .iter()
        .find(|s| s.range.start <= offset && offset <= s.range.end)?;
    let toks = Toks::new(dialect, &feat, text, statement.range.clone());
    let k = token_at(&toks, offset)?;
    let range = toks.range(k);
    let stmts = analyze::analyze(&dialect.language, &feat, &toks);
    let st = stmts
        .iter()
        .find(|s| s.range.start <= range.start && range.start < s.range.end)?;
    let env = Env::new(dialect, cache);
    let markdown = describe(&env, dialect, st, &range, &toks, k)?;
    Some(Hover { range, markdown })
}

/// The name token at `offset` (a cursor right after a name counts).
fn token_at(toks: &Toks<'_>, offset: usize) -> Option<usize> {
    let is_name = |i: usize| matches!(toks.kind(i), Some(Kind::Word | Kind::Quoted));
    let i = toks.all.partition_point(|t| t.end < offset);
    let tok = toks.all.get(i)?;
    if tok.start <= offset && offset <= tok.end && is_name(i) {
        return Some(i);
    }
    // `(|name`: the cursor between a punctuation and the name it touches.
    let next = toks.all.get(i + 1)?;
    (tok.end == offset && next.start == offset && is_name(i + 1)).then_some(i + 1)
}

fn describe(
    env: &Env<'_>,
    dialect: &Dialect,
    st: &Stmt,
    range: &Range<usize>,
    toks: &Toks<'_>,
    k: usize,
) -> Option<String> {
    let at = |n: &Name| n.range == *range;
    // CTE names and relations of FROM lists and DML targets.
    for (b, blk) in st.blocks.iter().enumerate() {
        for (c, cte) in blk.ctes.iter().enumerate() {
            if at(&cte.name) {
                return Some(cte_text(st, b, c));
            }
        }
        for rel in &blk.rels {
            if rel.alias.as_ref().is_some_and(at) {
                return rel_text(env, st, b, rel, true);
            }
            if let Source::Table(path) | Source::Function(path) = &rel.source
                && let Some(p) = path.iter().position(at)
            {
                return path_part(env, dialect, st, Some(b), rel, path, p);
            }
        }
    }
    for path in st.targets.iter().chain(&st.dropped) {
        if let Some(p) = path.iter().position(at) {
            let rel = Rel {
                source: Source::Table(path.clone()),
                alias: None,
                col_aliases: Vec::new(),
                check: true,
                target: false,
                hidden_target: false,
                dup: false,
            };
            return path_part(env, dialect, st, None, &rel, path, p);
        }
    }
    // Column references.
    for c in &st.cols {
        let Some(p) = c.parts.iter().position(at) else {
            continue;
        };
        if p + 1 < c.parts.len() {
            let (owner, rel) = checks::find_qualified(env, st, c.block, &c.parts[..=p])?;
            return rel_text(env, st, owner, rel, true);
        }
        let col = &c.parts[p];
        if c.parts.len() > 1 {
            let (owner, rel) =
                checks::find_qualified(env, st, c.block, &c.parts[..c.parts.len() - 1])?;
            return column_text(env, st, owner, rel, col);
        }
        if c.ctx == analyze::RefCtx::TargetOnly {
            let (owner, rel) = checks::target_rel(st, c.block)?;
            return column_text(env, st, owner, rel, col);
        }
        if let Lookup::Found(owner, rel) = checks::column_owner(env, st, c.block, col, c.hidden) {
            return column_text(env, st, owner, rel, col);
        }
        return None;
    }
    // Function calls.
    for call in &st.calls {
        if call.name.last().is_some_and(at) {
            return function_text(env, dialect, &call.name);
        }
    }
    // A name the parser did not take for a reference (a keyword used as a column name).
    if toks.kind(k) == Some(Kind::Word) {
        let word = toks.text(k);
        let col = Name {
            text: word.to_owned(),
            quoted: false,
            range: range.clone(),
        };
        let block = innermost(st, range.start)?;
        if let Lookup::Found(owner, rel) = checks::column_owner(env, st, block, &col, true) {
            return column_text(env, st, owner, rel, &col);
        }
        if toks.is_punct(k + 1, b'(') {
            return function_text(env, dialect, std::slice::from_ref(&col));
        }
    }
    None
}

/// The innermost block whose range holds `offset`.
fn innermost(st: &Stmt, offset: usize) -> Option<usize> {
    st.blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.range.start <= offset && offset <= b.range.end)
        .min_by_key(|(i, b)| (b.range.end - b.range.start, usize::MAX - i))
        .map(|(i, _)| i)
}

/// Inline code, safe for names holding backticks.
fn code(s: &str) -> String {
    if s.contains('`') {
        format!("`` {s} ``")
    } else {
        format!("`{s}`")
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn cte_text(st: &Stmt, b: usize, c: usize) -> String {
    let name = st.blocks[b].ctes[c].name.text.as_str();
    let mut md = format!("**CTE** {}", code(name));
    if let Some(cols) = checks::cte_columns(st, b, c) {
        let list: Vec<String> = cols.iter().map(|n| code(&n.text)).collect();
        let _ = write!(md, "\n\nColumns: {}", list.join(", "));
    }
    md
}

fn kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MatView => "materialized view",
        RelationKind::Foreign => "foreign table",
        RelationKind::Temporary => "temporary table",
    }
}

fn qualified(cache: &SchemaCache, id: RelationId) -> String {
    let rel = cache.relation(id).map_or("", |r| r.name.as_str());
    match cache.schema(id.schema) {
        Some(s) => format!("{}.{rel}", s.name),
        None => rel.to_owned(),
    }
}

/// The key notes of a column: `PK`, `FK → t(c)`.
fn keys(rel: &CatalogRelation, col: &CatalogColumn) -> Vec<String> {
    let mut out = Vec::new();
    if col.primary_key {
        out.push("PK".to_owned());
    }
    for fk in &rel.foreign_keys {
        if let Some(pos) = fk.columns.iter().position(|c| *c == col.name) {
            let target = match &fk.ref_schema {
                Some(s) => format!("{s}.{}", fk.ref_table),
                None => fk.ref_table.clone(),
            };
            let ref_col = fk.ref_columns.get(pos).map_or("", String::as_str);
            out.push(format!("FK → {target}({ref_col})"));
        }
    }
    out
}

fn catalog_relation_text(cache: &SchemaCache, id: RelationId) -> Option<String> {
    let rel = cache.relation(id)?;
    let mut md = format!(
        "**{}** {}",
        kind_name(rel.kind),
        code(&qualified(cache, id))
    );
    if let Some(comment) = rel.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        let _ = write!(md, "\n\n{}", comment.trim());
    }
    if !rel.columns.is_empty() {
        md.push_str("\n\n| Column | Type | Null | Key |\n|---|---|---|---|");
        for col in rel.columns.iter().take(MAX_COLUMNS) {
            let null = if col.nullable { "null" } else { "not null" };
            let _ = write!(
                md,
                "\n| {} | {} | {null} | {} |",
                cell(&code(&col.name)),
                cell(&col.native_type),
                cell(&keys(rel, col).join(", "))
            );
        }
        if rel.columns.len() > MAX_COLUMNS {
            let _ = write!(
                md,
                "\n\n… and {} more columns",
                rel.columns.len() - MAX_COLUMNS
            );
        }
    }
    Some(md)
}

/// A relation (`alias`: the hovered name is the alias or qualifier standing for it).
fn rel_text(env: &Env<'_>, st: &Stmt, owner: usize, rel: &Rel, alias: bool) -> Option<String> {
    let lead = match (&rel.alias, alias) {
        (Some(a), true) => format!("**alias** {} of ", code(&a.text)),
        _ => String::new(),
    };
    let body = match &rel.source {
        Source::Table(path) => match checks::resolve_table(env, st, Some(owner), path) {
            Resolved::Catalog(ids) => catalog_relation_text(env.cache?, *ids.first()?)?,
            Resolved::Cte(b, c) => cte_text(st, b, c),
            Resolved::Opaque | Resolved::Missing { .. } => return None,
        },
        Source::Derived(child) => {
            let mut md = "**subquery**".to_owned();
            let cols: Option<Vec<String>> = if rel.col_aliases.is_empty() {
                checks::outputs(st, *child).map(|o| o.iter().map(|n| code(&n.text)).collect())
            } else {
                Some(rel.col_aliases.iter().map(|n| code(&n.text)).collect())
            };
            if let Some(cols) = cols {
                let _ = write!(md, "\n\nColumns: {}", cols.join(", "));
            }
            md
        }
        Source::Function(path) => format!(
            "**table function** {}",
            code(
                &path
                    .iter()
                    .map(|n| n.text.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            )
        ),
        Source::Unknown => return None,
    };
    Some(format!("{lead}{body}"))
}

fn column_text(env: &Env<'_>, st: &Stmt, owner: usize, rel: &Rel, col: &Name) -> Option<String> {
    let rel_name = rel.display();
    match &rel.source {
        Source::Table(path) => match checks::resolve_table(env, st, Some(owner), path) {
            Resolved::Catalog(ids) => {
                let cache = env.cache?;
                ids.iter().find_map(|&id| {
                    let index = cache.resolve_column(id, &col.part())?;
                    let catalog = cache.relation(id)?;
                    let c = catalog.columns.get(index as usize)?;
                    let mut md = format!(
                        "**column** {} · {} · {}",
                        code(&format!("{}.{}", catalog.name, c.name)),
                        code(&c.native_type),
                        if c.nullable { "null" } else { "not null" }
                    );
                    for key in keys(catalog, c) {
                        let _ = write!(md, " · {key}");
                    }
                    let _ = write!(
                        md,
                        "\n\nIn {} {}",
                        kind_name(catalog.kind),
                        code(&qualified(cache, id))
                    );
                    Some(md)
                })
            }
            Resolved::Cte(_, _) => Some(format!(
                "**column** {} of CTE {}",
                code(&col.text),
                code(&rel_name)
            )),
            Resolved::Opaque | Resolved::Missing { .. } => None,
        },
        Source::Derived(_) => Some(format!(
            "**column** {} of subquery {}",
            code(&col.text),
            code(&rel_name)
        )),
        Source::Function(_) | Source::Unknown => None,
    }
}

/// A hovered part of a relation path: the relation itself (last part) or its schema.
fn path_part(
    env: &Env<'_>,
    dialect: &Dialect,
    st: &Stmt,
    block: Option<usize>,
    rel: &Rel,
    path: &[Name],
    p: usize,
) -> Option<String> {
    if p + 1 == path.len() {
        if let Source::Function(_) = rel.source {
            return function_text(env, dialect, path);
        }
        return match checks::resolve_table(env, st, block, path) {
            Resolved::Catalog(ids) => catalog_relation_text(env.cache?, *ids.first()?),
            Resolved::Cte(b, c) => Some(cte_text(st, b, c)),
            Resolved::Opaque | Resolved::Missing { .. } => None,
        };
    }
    let cache = env.cache?;
    let part = &path[p];
    if p + 2 == path.len() {
        let s = cache.resolve_schema(&part.part())?;
        let schema = cache.schema(s)?;
        return Some(format!(
            "**schema** {} · {} relations, {} routines",
            code(&schema.name),
            schema.relations.len(),
            schema.routines.len()
        ));
    }
    let snap = cache.snapshot();
    env.rules
        .schemas
        .matches(&part.text, part.quoted, &snap.database)
        .then(|| format!("**database** {}", code(&snap.database)))
}

fn function_text(env: &Env<'_>, dialect: &Dialect, name: &[Name]) -> Option<String> {
    let last = name.last()?;
    let mut sections: Vec<String> = Vec::new();
    if name.len() == 1 {
        for f in dialect
            .language
            .function(&last.text)
            .iter()
            .filter(|f| dialect.version.as_ref().is_none_or(|v| f.available_on(v)))
            .take(MAX_OVERLOADS)
        {
            let mut md = format!("```sql\n{}({}) → {}\n```", f.name, f.args, f.returns);
            if !f.summary.is_empty() {
                let _ = write!(md, "\n{}", f.summary);
            }
            sections.push(md);
        }
    }
    if let Some(cache) = env.cache {
        let parts: Vec<_> = name.iter().map(Name::part).collect();
        for id in cache
            .resolve_routine(&parts)
            .into_iter()
            .take(MAX_OVERLOADS)
        {
            let Some(r) = cache.routine(id) else {
                continue;
            };
            let schema = cache.schema(id.schema).map_or("", |s| s.name.as_str());
            let kind = match r.kind {
                RoutineKind::Function => "function",
                RoutineKind::Procedure => "procedure",
                RoutineKind::Aggregate => "aggregate",
            };
            let mut md = format!(
                "**{kind}** {}",
                code(&format!("{schema}.{}({})", r.name, r.args))
            );
            if let Some(ret) = &r.returns {
                let _ = write!(md, " → {}", code(ret));
            }
            sections.push(md);
        }
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n---\n\n"))
    }
}
