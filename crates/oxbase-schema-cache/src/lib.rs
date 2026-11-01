//! Per-connection identifier cache for code insight (M6, `docs/m6-contracts.md` §3).
//!
//! A [`SchemaCache`] is an immutable, cheaply cloned index over one
//! [`CatalogSnapshot`](oxbase_types::CatalogSnapshot): every database, schema, relation,
//! column, routine, sequence and type name of one (connection profile, database), with
//!
//! - **search** ([`SchemaCache::search`]): case-insensitive prefix match (binary search over
//!   lower-cased names) and fuzzy subsequence match ([`fuzzy_score`]), filtered by kind, schema
//!   or relation, best first;
//! - **resolution** ([`SchemaCache::resolve_relation`], [`SchemaCache::resolve_schema`],
//!   [`SchemaCache::resolve_routine`]): a name as written in SQL ([`NamePart`]s, quoted or not)
//!   to catalog objects under the engine's [`CaseRules`](oxbase_types::CaseRules) and search
//!   path;
//! - **incremental refresh** ([`SchemaCache::merge`]): a new cache with some schemas or
//!   relations replaced by a partial read;
//! - **persistence** ([`codec`]): the snapshot as bytes for the app database;
//! - a **synthetic** catalog of any size for tests and the latency check ([`synthetic`]).
//!
//! Refreshing never mutates a cache: it builds a new one (with a new
//! [`SchemaCache::generation`]) and the owner swaps it in, so readers never wait for a refresh.
//! Building the live cache from a connection (catalog readers, invalidation, the app-database
//! snapshot at startup) is `oxbase_services::schema_cache`.
//!
//! # Memory
//!
//! A 5 000-table x 20-column catalog (100 000 columns) must stay under
//! [`MEMORY_BUDGET_BYTES`] as estimated by [`SchemaCache::heap_bytes`] (snapshot plus index).
//!
//! Owner: Design wrote this crate complete with a straightforward index; the **cache** stream
//! may replace the internals (interning, better fuzzy ranking) without changing the API.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxbase_dialect::{CatalogScope, RelationName};
pub use oxbase_sql::NamePart;
use oxbase_types::{
    CaseRules, CatalogRelation, CatalogRoutine, CatalogSchema, CatalogSnapshot, Engine,
};

pub mod codec;
pub mod synthetic;

/// Upper bound of [`SchemaCache::heap_bytes`] for a 5 000-table x 20-column catalog.
pub const MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Identifies a relation: its schema's index in [`CatalogSnapshot::schemas`] and its index in
/// that schema's `relations`. Valid only for the cache (generation) that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelationId {
    pub schema: u32,
    pub index: u32,
}

/// Identifies a routine, like [`RelationId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoutineId {
    pub schema: u32,
    pub index: u32,
}

/// The kind of a cached name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NameKind {
    Database,
    Schema,
    Relation,
    Column,
    Routine,
    Sequence,
    Type,
}

/// What a cached name points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// Index into [`CatalogSnapshot::databases`].
    Database(u32),
    /// Index into [`CatalogSnapshot::schemas`].
    Schema(u32),
    Relation(RelationId),
    /// A column: its relation and its ordinal index (0-based).
    Column(RelationId, u32),
    Routine(RoutineId),
    /// Schema index and index into its `sequences`.
    Sequence(u32, u32),
    /// Schema index and index into its `types`.
    Type(u32, u32),
}

impl Target {
    pub fn kind(self) -> NameKind {
        match self {
            Self::Database(_) => NameKind::Database,
            Self::Schema(_) => NameKind::Schema,
            Self::Relation(_) => NameKind::Relation,
            Self::Column(..) => NameKind::Column,
            Self::Routine(_) => NameKind::Routine,
            Self::Sequence(..) => NameKind::Sequence,
            Self::Type(..) => NameKind::Type,
        }
    }

    /// The schema index of a schema-level target (`None` for databases).
    pub fn schema(self) -> Option<u32> {
        match self {
            Self::Database(_) => None,
            Self::Schema(s) | Self::Sequence(s, _) | Self::Type(s, _) => Some(s),
            Self::Relation(r) | Self::Column(r, _) => Some(r.schema),
            Self::Routine(r) => Some(r.schema),
        }
    }
}

/// How [`SchemaCache::search`] matches the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchMode {
    /// The name starts with the pattern, ignoring case.
    Prefix,
    /// The pattern's characters appear in the name in order, ignoring case ([`fuzzy_score`]).
    Fuzzy,
}

/// A search over the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameQuery<'a> {
    /// What the user typed (unquoted). Empty matches everything.
    pub pattern: &'a str,
    pub mode: MatchMode,
    /// Kinds to return; empty = every kind.
    pub kinds: &'a [NameKind],
    /// Only names inside this schema (schema-level kinds and columns).
    pub schema: Option<u32>,
    /// Only the columns of this relation (other kinds are skipped).
    pub relation: Option<RelationId>,
    /// At most this many hits.
    pub limit: usize,
}

impl<'a> NameQuery<'a> {
    /// A prefix search for `pattern` over `kinds`, at most 100 hits.
    pub fn prefix(pattern: &'a str, kinds: &'a [NameKind]) -> Self {
        Self {
            pattern,
            mode: MatchMode::Prefix,
            kinds,
            schema: None,
            relation: None,
            limit: 100,
        }
    }
}

/// One search result. Higher scores are better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameHit {
    pub target: Target,
    pub score: u32,
}

/// A name in the search index.
#[derive(Debug)]
struct NameEntry {
    /// The name in lower case.
    key: Box<str>,
    target: Target,
}

#[derive(Debug)]
struct Inner {
    snapshot: CatalogSnapshot,
    generation: u64,
    /// Sorted by `(key, target)`.
    names: Vec<NameEntry>,
}

/// The identifier cache of one (profile, database). See the crate docs.
#[derive(Debug, Clone)]
pub struct SchemaCache {
    inner: Arc<Inner>,
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn idx(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl SchemaCache {
    /// Builds the index over `snapshot` (normalized first). Linear in the number of names plus
    /// one sort.
    #[allow(clippy::many_single_char_names)] // short loop and closure names
    pub fn new(mut snapshot: CatalogSnapshot) -> Self {
        snapshot.normalize();
        let mut names = Vec::with_capacity(snapshot.column_count() + snapshot.relation_count());
        let mut push = |name: &str, target: Target| {
            names.push(NameEntry {
                key: name.to_lowercase().into_boxed_str(),
                target,
            });
        };
        for (d, db) in snapshot.databases.iter().enumerate() {
            push(db, Target::Database(idx(d)));
        }
        for (s, schema) in snapshot.schemas.iter().enumerate() {
            let s = idx(s);
            push(&schema.name, Target::Schema(s));
            for (r, rel) in schema.relations.iter().enumerate() {
                let id = RelationId {
                    schema: s,
                    index: idx(r),
                };
                push(&rel.name, Target::Relation(id));
                for (c, col) in rel.columns.iter().enumerate() {
                    push(&col.name, Target::Column(id, idx(c)));
                }
            }
            for (r, routine) in schema.routines.iter().enumerate() {
                push(
                    &routine.name,
                    Target::Routine(RoutineId {
                        schema: s,
                        index: idx(r),
                    }),
                );
            }
            for (q, seq) in schema.sequences.iter().enumerate() {
                push(seq, Target::Sequence(s, idx(q)));
            }
            for (t, ty) in schema.types.iter().enumerate() {
                push(ty, Target::Type(s, idx(t)));
            }
        }
        names.sort_unstable_by(|a, b| a.key.cmp(&b.key).then(a.target.cmp(&b.target)));
        Self {
            inner: Arc::new(Inner {
                snapshot,
                generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
                names,
            }),
        }
    }

    /// The snapshot behind the index.
    pub fn snapshot(&self) -> &CatalogSnapshot {
        &self.inner.snapshot
    }

    /// Unique per built cache (process-wide, increasing). Changes with every refresh.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn engine(&self) -> Engine {
        self.inner.snapshot.engine
    }

    pub fn case_rules(&self) -> CaseRules {
        self.inner.snapshot.case_rules
    }

    /// `false` when the reader stopped at a size limit (names are missing).
    pub fn is_complete(&self) -> bool {
        !self.inner.snapshot.truncated
    }

    pub fn schema(&self, index: u32) -> Option<&CatalogSchema> {
        self.inner.snapshot.schemas.get(index as usize)
    }

    pub fn relation(&self, id: RelationId) -> Option<&CatalogRelation> {
        self.schema(id.schema)?.relations.get(id.index as usize)
    }

    pub fn routine(&self, id: RoutineId) -> Option<&CatalogRoutine> {
        self.schema(id.schema)?.routines.get(id.index as usize)
    }

    /// The name a target points at (empty for a stale target).
    #[allow(clippy::many_single_char_names)] // short loop and closure names
    pub fn name_of(&self, target: Target) -> &str {
        let snap = &self.inner.snapshot;
        let schema = |s: u32| snap.schemas.get(s as usize);
        let name = match target {
            Target::Database(d) => snap.databases.get(d as usize).map(String::as_str),
            Target::Schema(s) => schema(s).map(|s| s.name.as_str()),
            Target::Relation(r) => self.relation(r).map(|r| r.name.as_str()),
            Target::Column(r, c) => self
                .relation(r)
                .and_then(|r| r.columns.get(c as usize))
                .map(|c| c.name.as_str()),
            Target::Routine(r) => self.routine(r).map(|r| r.name.as_str()),
            Target::Sequence(s, i) => schema(s)
                .and_then(|s| s.sequences.get(i as usize))
                .map(String::as_str),
            Target::Type(s, i) => schema(s)
                .and_then(|s| s.types.get(i as usize))
                .map(String::as_str),
        };
        name.unwrap_or_default()
    }

    /// Index range of the names whose key equals `key`.
    fn exact(&self, key: &str) -> &[NameEntry] {
        let names = &self.inner.names;
        let lo = names.partition_point(|n| &*n.key < key);
        let hi = lo + names[lo..].partition_point(|n| &*n.key == key);
        &names[lo..hi]
    }

    fn wanted(query: &NameQuery<'_>, target: Target) -> bool {
        (query.kinds.is_empty() || query.kinds.contains(&target.kind()))
            && query.schema.is_none_or(|s| target.schema() == Some(s))
    }

    /// Names matching `query`, best first (higher score, then shorter, then by name), at most
    /// `query.limit`.
    ///
    /// Prefix scores: 1 000 for an exact (case-insensitive) match, else 900 minus the number of
    /// extra characters (at least 1). Fuzzy scores come from [`fuzzy_score`].
    #[allow(clippy::many_single_char_names)] // short loop and closure names
    pub fn search(&self, query: &NameQuery<'_>) -> Vec<NameHit> {
        let pattern = query.pattern.to_lowercase();
        let mut hits: Vec<(NameHit, &str)> = Vec::new();
        let mut consider = |key: &str, target: Target| {
            let score = match query.mode {
                MatchMode::Prefix => key.strip_prefix(pattern.as_str()).map(|rest| {
                    if rest.is_empty() {
                        1000
                    } else {
                        900_u32.saturating_sub(idx(rest.chars().count())).max(1)
                    }
                }),
                MatchMode::Fuzzy => fuzzy_score(&pattern, key),
            };
            if let Some(score) = score {
                hits.push((NameHit { target, score }, self.name_of(target)));
            }
        };
        if let Some(rel_id) = query.relation {
            let columns_wanted = query.kinds.is_empty() || query.kinds.contains(&NameKind::Column);
            let rel = self.relation(rel_id).filter(|_| columns_wanted);
            for (c, col) in rel.into_iter().flat_map(|r| r.columns.iter().enumerate()) {
                consider(&col.name.to_lowercase(), Target::Column(rel_id, idx(c)));
            }
        } else {
            let names = &self.inner.names;
            let range = if query.mode == MatchMode::Prefix {
                let lo = names.partition_point(|n| &*n.key < pattern.as_str());
                let len = names[lo..].partition_point(|n| n.key.starts_with(pattern.as_str()));
                &names[lo..lo + len]
            } else {
                &names[..]
            };
            for n in range {
                if Self::wanted(query, n.target) {
                    consider(&n.key, n.target);
                }
            }
        }
        hits.sort_by(|(a, an), (b, bn)| {
            b.score
                .cmp(&a.score)
                .then(an.len().cmp(&bn.len()))
                .then(an.cmp(bn))
                .then(a.target.cmp(&b.target))
        });
        hits.truncate(query.limit);
        hits.into_iter().map(|(h, _)| h).collect()
    }

    /// The schema `part` names under the schema case rule, if any.
    pub fn resolve_schema(&self, part: &NamePart) -> Option<u32> {
        let rule = self.case_rules().schemas;
        self.exact(&part.text.to_lowercase())
            .iter()
            .find_map(|n| match n.target {
                Target::Schema(s)
                    if rule.matches(&part.text, part.quoted, self.name_of(n.target)) =>
                {
                    Some(s)
                }
                _ => None,
            })
    }

    /// Schemas searched for an unqualified name, in order: the snapshot's `search_path`, else
    /// its `default_schema`, else every schema.
    pub fn search_schemas(&self) -> Vec<u32> {
        let snap = self.snapshot();
        let find = |name: &str| snap.schemas.iter().position(|s| s.name == name).map(idx);
        let path: Vec<u32> = snap
            .search_path
            .iter()
            .filter_map(|s| find(s.as_str()))
            .collect();
        if !path.is_empty() {
            return path;
        }
        if let Some(s) = snap.default_schema.as_deref().and_then(find) {
            return vec![s];
        }
        (0..snap.schemas.len()).map(idx).collect()
    }

    /// Splits `path` into (schema candidates, last part), checking a leading database part.
    fn scope_of<'p>(&self, path: &'p [NamePart]) -> Option<(Vec<u32>, &'p NamePart)> {
        let (last, qualifier) = path.split_last()?;
        let schemas = match qualifier {
            [] => self.search_schemas(),
            [schema] => vec![self.resolve_schema(schema)?],
            [database, schema] => {
                let rule = self.case_rules().schemas;
                if !rule.matches(&database.text, database.quoted, &self.snapshot().database) {
                    return None;
                }
                vec![self.resolve_schema(schema)?]
            }
            _ => return None,
        };
        Some((schemas, last))
    }

    /// The relations `path` (`name`, `schema.name` or `database.schema.name`) refers to, under
    /// the engine's case rules. An unqualified name is looked up in [`SchemaCache::search_schemas`]
    /// order and the first schema with a match wins (PostgreSQL search-path semantics). Empty
    /// when nothing matches; several entries only when a case-insensitive rule matches names
    /// that differ in case.
    pub fn resolve_relation(&self, path: &[NamePart]) -> Vec<RelationId> {
        let Some((schemas, last)) = self.scope_of(path) else {
            return Vec::new();
        };
        let rule = self.case_rules().relations;
        let entries = self.exact(&last.text.to_lowercase());
        for s in schemas {
            let found: Vec<RelationId> = entries
                .iter()
                .filter_map(|n| match n.target {
                    Target::Relation(r)
                        if r.schema == s
                            && rule.matches(&last.text, last.quoted, self.name_of(n.target)) =>
                    {
                        Some(r)
                    }
                    _ => None,
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// Every relation named `part` in any schema (lenient lookup for completion when the
    /// search path finds nothing).
    pub fn relations_named(&self, part: &NamePart) -> Vec<RelationId> {
        let rule = self.case_rules().relations;
        self.exact(&part.text.to_lowercase())
            .iter()
            .filter_map(|n| match n.target {
                Target::Relation(r)
                    if rule.matches(&part.text, part.quoted, self.name_of(n.target)) =>
                {
                    Some(r)
                }
                _ => None,
            })
            .collect()
    }

    /// The routines (every overload) `path` refers to, like [`SchemaCache::resolve_relation`].
    pub fn resolve_routine(&self, path: &[NamePart]) -> Vec<RoutineId> {
        let Some((schemas, last)) = self.scope_of(path) else {
            return Vec::new();
        };
        let rule = self.case_rules().routines;
        let entries = self.exact(&last.text.to_lowercase());
        for s in schemas {
            let found: Vec<RoutineId> = entries
                .iter()
                .filter_map(|n| match n.target {
                    Target::Routine(r)
                        if r.schema == s
                            && rule.matches(&last.text, last.quoted, self.name_of(n.target)) =>
                    {
                        Some(r)
                    }
                    _ => None,
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// The column of `relation` that `part` names, under the column case rule.
    pub fn resolve_column(&self, relation: RelationId, part: &NamePart) -> Option<u32> {
        let rule = self.case_rules().columns;
        self.relation(relation)?
            .columns
            .iter()
            .position(|c| rule.matches(&part.text, part.quoted, &c.name))
            .map(idx)
    }

    /// An estimate of the heap memory of the snapshot and the index, in bytes.
    #[allow(clippy::many_single_char_names)] // short loop and closure names
    pub fn heap_bytes(&self) -> usize {
        let snap = &self.inner.snapshot;
        let s = |x: &String| x.capacity();
        let mut total = size_of::<Inner>()
            + snap.databases.iter().map(s).sum::<usize>()
            + snap.databases.capacity() * size_of::<String>()
            + snap.search_path.iter().map(s).sum::<usize>()
            + snap.schemas.capacity() * size_of::<CatalogSchema>();
        for schema in &snap.schemas {
            total += schema.name.capacity()
                + schema.relations.capacity() * size_of::<CatalogRelation>()
                + schema.routines.capacity() * size_of::<CatalogRoutine>()
                + (schema.sequences.capacity() + schema.types.capacity()) * size_of::<String>()
                + schema
                    .sequences
                    .iter()
                    .chain(&schema.types)
                    .map(s)
                    .sum::<usize>();
            for rel in &schema.relations {
                total += rel.name.capacity()
                    + rel.comment.as_ref().map_or(0, String::capacity)
                    + rel.columns.capacity() * size_of::<oxbase_types::CatalogColumn>()
                    + rel
                        .columns
                        .iter()
                        .map(|c| c.name.capacity() + c.native_type.capacity())
                        .sum::<usize>()
                    + rel
                        .foreign_keys
                        .iter()
                        .map(|f| {
                            size_of::<oxbase_types::CatalogForeignKey>()
                                + f.ref_table.len()
                                + f.columns
                                    .iter()
                                    .chain(&f.ref_columns)
                                    .map(String::len)
                                    .sum::<usize>()
                        })
                        .sum::<usize>();
            }
            for r in &schema.routines {
                total += r.name.capacity()
                    + r.args.capacity()
                    + r.returns.as_ref().map_or(0, String::capacity);
            }
        }
        total
            + self.inner.names.capacity() * size_of::<NameEntry>()
            + self.inner.names.iter().map(|n| n.key.len()).sum::<usize>()
    }

    /// A new cache: this one with the parts `scope` names replaced by `partial` (a read of
    /// exactly that scope). A full scope replaces everything. Schemas in `scope.schemas` are
    /// replaced whole (or removed when `partial` lacks them); relations in `scope.relations`
    /// are replaced (or removed); a relation whose schema is `None` lives in the default
    /// schema (else the first one). Databases, search path, case rules and `read_at_ms` come
    /// from `partial`.
    #[must_use]
    pub fn merge(&self, mut partial: CatalogSnapshot, scope: &CatalogScope) -> SchemaCache {
        if scope.is_full() {
            return Self::new(partial);
        }
        let mut snap = self.inner.snapshot.clone();
        for name in scope.schemas.iter().flatten() {
            snap.schemas.retain(|s| &s.name != name);
            if let Some(pos) = partial.schemas.iter().position(|s| &s.name == name) {
                snap.schemas.push(partial.schemas[pos].clone());
            }
        }
        for RelationName { schema, name } in scope.relations.iter().flatten() {
            let schema_name = schema
                .clone()
                .or_else(|| snap.default_schema.clone())
                .or_else(|| snap.schemas.first().map(|s| s.name.clone()));
            let Some(schema_name) = schema_name else {
                continue;
            };
            let fresh = partial
                .schemas
                .iter_mut()
                .find(|s| s.name == schema_name)
                .and_then(|s| {
                    let pos = s.relations.iter().position(|r| &r.name == name)?;
                    Some(s.relations.swap_remove(pos))
                });
            let target = match snap.schemas.iter().position(|s| s.name == schema_name) {
                Some(i) => &mut snap.schemas[i],
                None if fresh.is_some() => {
                    snap.schemas.push(CatalogSchema::new(schema_name.as_str()));
                    snap.schemas.last_mut().expect("just pushed")
                }
                None => continue,
            };
            target.relations.retain(|r| &r.name != name);
            target.relations.extend(fresh);
        }
        snap.databases = partial.databases;
        snap.search_path = partial.search_path;
        snap.default_schema = partial.default_schema.or(snap.default_schema);
        snap.case_rules = partial.case_rules;
        snap.server_version = partial.server_version;
        snap.read_at_ms = snap.read_at_ms.max(partial.read_at_ms);
        snap.truncated = snap.truncated || partial.truncated;
        Self::new(snap)
    }
}

/// Fuzzy subsequence score of `pattern` in `candidate` (both already lower case), or `None`
/// when the pattern's characters do not all appear in order. Empty patterns score 1.
///
/// Scoring: 10 per matched character, +15 when it follows the previous match directly, +20
/// when it starts the candidate or follows `_`, `.`, `$` or a digit/letter boundary, +50 when
/// the whole pattern is a prefix; minus 1 per skipped character (at most 100 in total). At
/// least 1.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<u32> {
    if pattern.is_empty() {
        return Some(1);
    }
    if candidate.starts_with(pattern) {
        let extra = idx(candidate.chars().count() - pattern.chars().count());
        return Some(
            10 * idx(pattern.chars().count()) + 15 * idx(pattern.chars().count()) + 70
                - extra.min(50),
        );
    }
    let mut score: i64 = 0;
    let mut gaps: i64 = 0;
    let mut wanted = pattern.chars().peekable();
    let mut prev: Option<char> = None;
    let mut prev_matched = false;
    for c in candidate.chars() {
        let Some(&want) = wanted.peek() else {
            break;
        };
        if c == want {
            wanted.next();
            score += 10;
            if prev_matched {
                score += 15;
            }
            let boundary = prev.is_none_or(|p| {
                matches!(p, '_' | '.' | '$' | ' ' | '-')
                    || (p.is_ascii_digit() != c.is_ascii_digit())
            });
            if boundary {
                score += 20;
            }
            prev_matched = true;
        } else {
            gaps += 1;
            prev_matched = false;
        }
        prev = Some(c);
    }
    if wanted.peek().is_some() {
        return None;
    }
    Some(u32::try_from((score - gaps.min(100)).max(1)).unwrap_or(1))
}

#[cfg(test)]
mod tests;
