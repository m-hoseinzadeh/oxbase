//! Findings that need the schema cache: unknown relations, schemas and columns, ambiguous
//! columns, argument counts and unknown functions. Also the name resolution hover uses.

use std::collections::HashSet;
use std::ops::Range;

use oxbase_dialect::FunctionInfo;
use oxbase_schema_cache::{RelationId, SchemaCache};
use oxbase_sql::{DdlObject, DdlVerb, NamePart, SchemaChange};
use oxbase_types::{CaseRules, IdentCase};

use super::analyze::{Name, RefCtx, Rel, Source, Stmt};
use super::words::{self, has};
use super::{Diagnostic, DiagnosticCode, Severity};
use crate::Dialect;

/// What the script has done so far that the cache cannot know.
#[derive(Debug)]
pub(crate) struct Env<'c> {
    pub cache: Option<&'c SchemaCache>,
    pub rules: CaseRules,
    /// Lower-case names of relations created or renamed earlier in the script.
    created: HashSet<String>,
    /// Lower-case names of schemas or databases created or attached in the script.
    schemas: HashSet<String>,
    /// A rename whose target is unknown (`sp_rename`): any name may be new.
    renamed: bool,
    /// `USE` of another database or a changed search path: names resolve elsewhere.
    elsewhere: bool,
}

impl<'c> Env<'c> {
    pub fn new(dialect: &Dialect, cache: Option<&'c SchemaCache>) -> Self {
        Self {
            cache,
            rules: cache.map_or(dialect.case_rules, SchemaCache::case_rules),
            created: HashSet::new(),
            schemas: HashSet::new(),
            renamed: false,
            elsewhere: false,
        }
    }

    /// Findings may be warnings: the cache is complete and the script has not moved away from
    /// it.
    fn trusted(&self) -> bool {
        self.cache.is_some_and(SchemaCache::is_complete) && !self.renamed && !self.elsewhere
    }

    /// Notes the DDL of a statement (before its findings: a statement may create what it
    /// uses).
    pub fn apply_changes(&mut self, changes: &[SchemaChange]) {
        for c in changes {
            if !matches!(c.verb, DdlVerb::Create | DdlVerb::Rename) {
                continue;
            }
            let Some(last) = c.name.last() else {
                self.renamed = true;
                continue;
            };
            let key = last.text.to_lowercase();
            match c.object {
                DdlObject::Schema | DdlObject::Database => {
                    self.schemas.insert(key);
                }
                _ => {
                    self.created.insert(key);
                }
            }
        }
    }

    /// Notes what a statement did after its findings (`USE`, `SELECT INTO`, `ATTACH`).
    pub fn after(&mut self, st: &Stmt) {
        for parts in &st.created {
            if let Some(last) = parts.last() {
                self.created.insert(last.text.to_lowercase());
            }
        }
        if let Some(db) = &st.use_db {
            self.elsewhere = !self.cache.is_some_and(|c| {
                self.rules
                    .schemas
                    .matches(&db.text, db.quoted, &c.snapshot().database)
            });
        }
        if st.sets_path {
            self.elsewhere = true;
        }
        if let Some(a) = &st.attached {
            self.schemas.insert(a.text.to_lowercase());
        }
    }
}

/// `true` when the written names `a` and `b` stand for the same object under `rule`.
pub(crate) fn same(rule: IdentCase, a: &Name, b: &Name) -> bool {
    let canon = if b.quoted {
        b.text.clone()
    } else {
        rule.fold_unquoted(&b.text)
    };
    rule.matches(&a.text, a.quoted, &canon)
}

fn parts(path: &[Name]) -> Vec<NamePart> {
    path.iter().map(Name::part).collect()
}

/// What a relation name stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    Catalog(Vec<RelationId>),
    /// A CTE: (owning block, index).
    Cte(usize, usize),
    /// Exists, but its columns are unknown.
    Opaque,
    /// Not found.
    Missing {
        code: DiagnosticCode,
        range: Range<usize>,
        message: String,
        /// `false` when the name may well exist where the cache does not look.
        certain: bool,
    },
}

/// The CTE `name` visible from `block`.
pub(crate) fn find_cte(
    env: &Env<'_>,
    st: &Stmt,
    block: Option<usize>,
    name: &Name,
) -> Option<(usize, usize)> {
    let mut at = block;
    let mut guard = 0;
    while let Some(b) = at {
        let blk = st.blocks.get(b)?;
        if let Some(k) = blk
            .ctes
            .iter()
            .position(|c| same(env.rules.relations, name, &c.name))
        {
            return Some((b, k));
        }
        at = blk.parent;
        guard += 1;
        if guard > st.blocks.len() {
            break;
        }
    }
    None
}

fn display_path(path: &[Name]) -> String {
    path.iter()
        .map(|n| n.text.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Resolves a relation name read in `block` (`None`: outside any query, DDL).
pub(crate) fn resolve_table(
    env: &Env<'_>,
    st: &Stmt,
    block: Option<usize>,
    path: &[Name],
) -> Resolved {
    let Some(last) = path.last() else {
        return Resolved::Opaque;
    };
    if path.len() == 1
        && let Some((b, k)) = find_cte(env, st, block, last)
    {
        return Resolved::Cte(b, k);
    }
    if path.iter().any(|p| p.text.is_empty())
        || last.text.starts_with('#')
        || (path.len() == 1 && has(words::PSEUDO_TABLES, &last.text.to_ascii_uppercase()))
    {
        return Resolved::Opaque;
    }
    let Some(cache) = env.cache else {
        return Resolved::Opaque;
    };
    let ids = cache.resolve_relation(&parts(path));
    if !ids.is_empty() {
        return Resolved::Catalog(ids);
    }
    if env.created.contains(&last.text.to_lowercase()) || path.len() > 3 {
        return Resolved::Opaque;
    }
    let missing = |certain: bool| Resolved::Missing {
        code: DiagnosticCode::UnknownRelation,
        range: last.range.clone(),
        message: format!("Unknown table or view `{}`.", display_path(path)),
        certain,
    };
    let snap = cache.snapshot();
    let Some(schema) = path.len().checked_sub(2).and_then(|i| path.get(i)) else {
        return missing(!words::system_like(&last.text));
    };
    if path.len() == 3 {
        let db = &path[0];
        if !env
            .rules
            .schemas
            .matches(&db.text, db.quoted, &snap.database)
        {
            return missing(false);
        }
    }
    if cache.resolve_schema(&schema.part()).is_some() {
        return missing(true);
    }
    let lower = schema.text.to_lowercase();
    let elsewhere = snap.databases.iter().any(|d| d.to_lowercase() == lower)
        || env.schemas.contains(&lower)
        || words::system_like(&schema.text);
    if elsewhere {
        return missing(false);
    }
    Resolved::Missing {
        code: DiagnosticCode::UnknownSchema,
        range: schema.range.clone(),
        message: format!("Unknown schema `{}`.", schema.text),
        certain: true,
    }
}

/// The output names of block `b`, when all are known.
pub(crate) fn outputs(st: &Stmt, b: usize) -> Option<Vec<&Name>> {
    let blk = st.blocks.get(b)?;
    if blk.star || blk.outputs.is_empty() {
        return None;
    }
    blk.outputs.iter().map(Option::as_ref).collect()
}

/// The columns of CTE `k` of block `b`, when known.
pub(crate) fn cte_columns(st: &Stmt, b: usize, k: usize) -> Option<Vec<&Name>> {
    let cte = st.blocks.get(b)?.ctes.get(k)?;
    if !cte.columns.is_empty() {
        return Some(cte.columns.iter().collect());
    }
    outputs(st, cte.block?)
}

/// Whether `rel` (of block `owner`) has column `col`; `None` when its columns are unknown.
pub(crate) fn rel_has(
    env: &Env<'_>,
    st: &Stmt,
    owner: usize,
    rel: &Rel,
    col: &Name,
) -> Option<bool> {
    let rule = env.rules.columns;
    if rel.col_aliases.iter().any(|a| same(rule, col, a)) {
        return Some(true);
    }
    if !col.quoted && has(words::HIDDEN_COLUMNS, &col.text.to_ascii_uppercase()) {
        return Some(true);
    }
    let names_have = |names: Vec<&Name>| names.iter().any(|n| same(rule, col, n));
    match &rel.source {
        Source::Table(path) => match resolve_table(env, st, Some(owner), path) {
            Resolved::Catalog(ids) => {
                let cache = env.cache?;
                Some(
                    ids.iter()
                        .any(|&id| cache.resolve_column(id, &col.part()).is_some()),
                )
            }
            Resolved::Cte(b, k) => cte_columns(st, b, k).map(names_have),
            Resolved::Opaque | Resolved::Missing { .. } => None,
        },
        Source::Derived(child) => {
            if rel.col_aliases.is_empty() {
                outputs(st, *child).map(names_have)
            } else {
                Some(false)
            }
        }
        Source::Function(_) | Source::Unknown => None,
    }
}

/// The relation a qualifier (`o` of `o.id`, `s.t` of `s.t.c`) names, seen from `block`.
pub(crate) fn find_qualified<'s>(
    env: &Env<'_>,
    st: &'s Stmt,
    block: usize,
    qualifier: &[Name],
) -> Option<(usize, &'s Rel)> {
    let rule = env.rules.relations;
    let mut at = Some(block);
    let mut guard = 0;
    while let Some(b) = at {
        let blk = st.blocks.get(b)?;
        for rel in &blk.rels {
            // `UPDATE x ... FROM t x`: the target `x` only stands for the alias.
            if rel.dup && !rel.check {
                continue;
            }
            let hit = match (&rel.alias, &rel.source) {
                (Some(a), _) => qualifier.len() == 1 && same(rule, &qualifier[0], a),
                (None, Source::Table(path) | Source::Function(path)) => {
                    path.len() >= qualifier.len()
                        && path[path.len() - qualifier.len()..]
                            .iter()
                            .zip(qualifier)
                            .all(|(p, q)| same(rule, q, p))
                }
                (None, _) => false,
            };
            if hit {
                return Some((b, rel));
            }
        }
        at = blk.parent;
        guard += 1;
        if guard > st.blocks.len() {
            break;
        }
    }
    None
}

/// The DML target relation visible from `block`.
pub(crate) fn target_rel(st: &Stmt, block: usize) -> Option<(usize, &Rel)> {
    let mut at = Some(block);
    let mut guard = 0;
    while let Some(b) = at {
        let blk = st.blocks.get(b)?;
        if let Some(rel) = blk.rels.iter().find(|r| r.target) {
            return Some((b, rel));
        }
        at = blk.parent;
        guard += 1;
        if guard > st.blocks.len() {
            break;
        }
    }
    None
}

/// The relation an unqualified column comes from, searching block `block` and then its
/// parents. `Err(())` when it cannot be said (several, or relations with unknown columns).
pub(crate) fn column_owner<'s>(
    env: &Env<'_>,
    st: &'s Stmt,
    block: usize,
    col: &Name,
    hidden: bool,
) -> Lookup<'s> {
    let mut at = Some(block);
    let mut any = false;
    let mut guard = 0;
    while let Some(b) = at {
        let Some(blk) = st.blocks.get(b) else {
            break;
        };
        let mut hits: Vec<&Rel> = Vec::new();
        let mut unknown = false;
        for rel in &blk.rels {
            if rel.dup || (rel.hidden_target && !hidden) {
                continue;
            }
            any = true;
            match rel_has(env, st, b, rel, col) {
                Some(true) => hits.push(rel),
                Some(false) => {}
                None => unknown = true,
            }
        }
        match hits.len() {
            1 => return Lookup::Found(b, hits[0]),
            0 if unknown => return Lookup::Unknown,
            0 => {}
            _ => {
                return if blk.merged {
                    Lookup::Unknown
                } else {
                    Lookup::Ambiguous(hits)
                };
            }
        }
        at = blk.parent;
        guard += 1;
        if guard > st.blocks.len() {
            break;
        }
    }
    if any {
        Lookup::Missing
    } else {
        Lookup::Unknown
    }
}

/// Result of [`column_owner`].
#[derive(Debug)]
pub(crate) enum Lookup<'s> {
    Found(usize, &'s Rel),
    Ambiguous(Vec<&'s Rel>),
    /// Not in any relation (all of whose columns are known).
    Missing,
    /// Cannot be said.
    Unknown,
}

/// `true` when `col` names an output of block `b` (or of the first core of its set
/// operation).
fn is_output(env: &Env<'_>, st: &Stmt, b: usize, col: &Name) -> bool {
    let Some(blk) = st.blocks.get(b) else {
        return false;
    };
    [Some(b), blk.set_root].into_iter().flatten().any(|x| {
        st.blocks.get(x).is_some_and(|o| {
            o.outputs
                .iter()
                .flatten()
                .any(|n| n.range != col.range && same(env.rules.columns, col, n))
        })
    })
}

/// `true` when `col` is the name or alias of a relation in scope (PostgreSQL whole-row
/// references such as `SELECT t FROM t`).
fn names_relation(env: &Env<'_>, st: &Stmt, b: usize, col: &Name) -> bool {
    find_qualified(env, st, b, std::slice::from_ref(col)).is_some()
}

/// Every cache-dependent finding of `st`.
pub(crate) fn check(env: &Env<'_>, dialect: &Dialect, st: &Stmt, out: &mut Vec<Diagnostic>) {
    let sure = env.trusted() && !st.uncertain && !st.in_routine;
    let sev = |certain: bool| {
        if certain && sure {
            Severity::Warning
        } else {
            Severity::Hint
        }
    };
    let mut reported: HashSet<Range<usize>> = HashSet::new();
    let mut relation = |block: Option<usize>, path: &[Name], out: &mut Vec<Diagnostic>| {
        if let Resolved::Missing {
            code,
            range,
            message,
            certain,
        } = resolve_table(env, st, block, path)
            && reported.insert(range.clone())
        {
            out.push(Diagnostic::new(code, range, sev(certain), message));
        }
    };
    for (b, blk) in st.blocks.iter().enumerate() {
        for rel in &blk.rels {
            if let (true, Source::Table(path)) = (rel.check, &rel.source) {
                relation(Some(b), path.as_slice(), out);
            }
        }
    }
    for path in &st.targets {
        relation(None, path.as_slice(), out);
    }
    for path in &st.dropped {
        if let Resolved::Missing {
            code,
            range,
            message,
            ..
        } = resolve_table(env, st, None, path)
        {
            out.push(Diagnostic::new(code, range, Severity::Hint, message));
        }
    }
    for c in &st.cols {
        let Some(col) = c.parts.last() else {
            continue;
        };
        if c.parts.len() > 1 {
            let qualifier = &c.parts[..c.parts.len() - 1];
            if let Some((owner, rel)) = find_qualified(env, st, c.block, qualifier)
                && rel_has(env, st, owner, rel, col) == Some(false)
            {
                out.push(Diagnostic::new(
                    DiagnosticCode::UnknownColumn,
                    col.range.clone(),
                    sev(true),
                    format!("Column `{}` is not in `{}`.", col.text, rel.display()),
                ));
            }
            continue;
        }
        if c.ctx == RefCtx::TargetOnly {
            if let Some((owner, rel)) = target_rel(st, c.block)
                && rel_has(env, st, owner, rel, col) == Some(false)
            {
                out.push(Diagnostic::new(
                    DiagnosticCode::UnknownColumn,
                    col.range.clone(),
                    sev(true),
                    format!("Column `{}` is not in `{}`.", col.text, rel.display()),
                ));
            }
            continue;
        }
        match column_owner(env, st, c.block, col, c.hidden) {
            Lookup::Found(..) | Lookup::Unknown => {}
            Lookup::Ambiguous(rels) => {
                if c.ctx == RefCtx::AliasOk && is_output(env, st, c.block, col) {
                    continue;
                }
                let names: Vec<String> =
                    rels.iter().map(|r| format!("`{}`", r.display())).collect();
                out.push(Diagnostic::new(
                    DiagnosticCode::AmbiguousColumn,
                    col.range.clone(),
                    sev(true),
                    format!(
                        "Column `{}` is in more than one table here ({}); qualify it.",
                        col.text,
                        names.join(", ")
                    ),
                ));
            }
            Lookup::Missing => {
                let alias = c.ctx == RefCtx::AliasOk && is_output(env, st, c.block, col);
                if alias || names_relation(env, st, c.block, col) {
                    continue;
                }
                out.push(Diagnostic::new(
                    DiagnosticCode::UnknownColumn,
                    col.range.clone(),
                    sev(c.ctx == RefCtx::Normal),
                    format!("Column `{}` is not in any table of this query.", col.text),
                ));
            }
        }
    }
    for call in &st.calls {
        function(
            env,
            dialect,
            st,
            call.name.as_slice(),
            call.args,
            &call.range,
            out,
        );
    }
}

/// `1 argument`, `1 to 3 arguments`, `at least 2 arguments`.
fn arity(f: &FunctionInfo) -> String {
    let plural = |n: u8| if n == 1 { "" } else { "s" };
    match (f.min_args, f.max_args) {
        (0, Some(0)) => "no arguments".to_owned(),
        (a, Some(b)) if a == b => format!("{a} argument{}", plural(a)),
        (a, Some(b)) => format!("{a} to {b} arguments"),
        (a, None) => format!("at least {a} argument{}", plural(a)),
    }
}

fn function(
    env: &Env<'_>,
    dialect: &Dialect,
    st: &Stmt,
    name: &[Name],
    args: Option<usize>,
    range: &Range<usize>,
    out: &mut Vec<Diagnostic>,
) {
    let (Some(last), Some(cache)) = (name.last(), env.cache) else {
        return;
    };
    let lang = &dialect.language;
    let routines = cache.resolve_routine(&parts(name));
    if name.len() == 1 {
        let all = lang.function(&last.text);
        let infos: Vec<&FunctionInfo> = all
            .iter()
            .filter(|f| dialect.version.as_ref().is_none_or(|v| f.available_on(v)))
            .collect();
        if !all.is_empty() {
            if let Some(n) = args
                && !infos.is_empty()
                && !infos.iter().any(|f| f.accepts(n))
                && routines.is_empty()
            {
                let mut forms: Vec<String> = infos.iter().copied().map(arity).collect();
                forms.dedup();
                out.push(Diagnostic::new(
                    DiagnosticCode::WrongArgumentCount,
                    range.clone(),
                    if st.uncertain {
                        Severity::Hint
                    } else {
                        Severity::Warning
                    },
                    format!("`{}` takes {}, not {n}.", last.text, forms.join(" or ")),
                ));
            }
            return;
        }
    }
    if lang.engine.is_none()
        || !routines.is_empty()
        || lang.type_info(&last.text).is_some()
        || lang.is_keyword(&last.text)
        || env.created.contains(&last.text.to_lowercase())
    {
        return;
    }
    if name.len() >= 2 {
        let schema = &name[name.len() - 2];
        if cache.resolve_schema(&schema.part()).is_none() {
            return;
        }
    }
    out.push(Diagnostic::new(
        DiagnosticCode::UnknownFunction,
        last.range.clone(),
        Severity::Hint,
        format!(
            "Function `{}` is neither built in nor in the schema cache.",
            display_path(name)
        ),
    ));
}
