//! Redis catalog fetchers (redis stream): databases, key namespaces, keys and ACL users, as
//! described in the [`super`] module docs. Every command goes through the connection the tree
//! hands in (a session on the node's database); fetchers reach the concrete
//! [`RedisConnection`](crate::RedisConnection) with `Connection::as_any` for its options
//! (namespace separator, tree scan limit) and, on a cluster, its masters. Never `KEYS`.
//!
//! # As built
//!
//! - **Databases**: `CONFIG GET databases` (16 when an ACL forbids `CONFIG`; a cluster: only
//!   `0`), `KEY_COUNT` from `INFO keyspace` (a cluster: `DBSIZE` summed over the masters), and
//!   `extras::MODE` on every row. `load` answers `DBSIZE` on the database's session.
//! - **Namespaces and keys**: one `SCAN MATCH <prefix>*` pass (every master of a cluster) per
//!   listing, at most `tree_scan_limit` distinct keys; keys group by their next separator.
//!   Only UTF-8 prefixes become namespaces (a ref name must round-trip); other keys show at
//!   their level with escaped names and `props::KEY_HEX`. `TYPE` + `PTTL` only for the (at
//!   most 1000) keys shown. `TRUNCATED` is set on every row of a listing that was cut (scan
//!   limit, or more than 1000 namespaces or keys).
//! - **Users**: `ACL USERS`; `load` reads `ACL GETUSER` and keeps flags, command rules, key
//!   and channel patterns. Password hashes are never read into the definition, whose text is
//!   the rule line without them (`user <name> on ~m7:* &* +@read`).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    BinaryText, Error, Extras, GenericDef, KeyAddress, ObjectDef, ObjectKind, ObjectRef,
    ObjectSummary, Result, SchemaDef, props,
};
use redis::Value;

use super::keys::common::{
    Nodes, check, escape_glob, int_of, items, key_name, map_of, native, scan_done, text_of,
    unescape_name,
};
use crate::RedisConnection;
use crate::extras;
use crate::options::RedisMode;

/// Namespaces and keys a tree listing shows at most (each).
pub(crate) const TREE_PAGE: usize = 1000;
/// `COUNT` of the tree's `SCAN` round trips.
const TREE_SCAN_COUNT: u32 = 1000;
/// Logical databases assumed when `CONFIG GET databases` is not allowed.
const DEFAULT_DATABASES: u32 = 16;

/// The fetcher of one Redis object kind (`Database`, `Schema`, `Key` or `User`).
#[derive(Debug, Clone, Copy)]
pub struct RedisFetcher {
    kind: ObjectKind,
}

impl RedisFetcher {
    pub fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }

    /// The kind this fetcher lists.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
}

#[async_trait]
impl Fetcher for RedisFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        check(&cancel)?;
        let redis = native(c)?;
        let rows = match self.kind {
            ObjectKind::Database => databases(redis, &cancel).await?,
            ObjectKind::User => users(redis, &cancel).await?,
            ObjectKind::Schema | ObjectKind::Key => {
                let Some(prefix) = prefix_of(parent) else {
                    return Ok(Vec::new());
                };
                let level = Level::scan(redis, prefix.as_bytes(), &cancel).await?;
                if self.kind == ObjectKind::Schema {
                    level.namespaces(parent)
                } else {
                    level.keys(redis, parent, &cancel).await?
                }
            }
            _ => Vec::new(),
        };
        check(&cancel)?;
        Ok(rows)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        check(&cancel)?;
        let redis = native(c)?;
        let def = match self.kind {
            ObjectKind::Database => {
                let keys = key_count(redis, &cancel).await?;
                let mut def = SchemaDef::new(obj.clone());
                def.extras = Extras::new()
                    .with(props::KEY_COUNT, to_i64(keys))
                    .with(extras::MODE, mode_name(redis));
                ObjectDef::Schema(def)
            }
            ObjectKind::Schema => {
                let level = Level::scan(redis, obj.name.as_bytes(), &cancel).await?;
                let mut def = SchemaDef::new(obj.clone());
                def.extras.insert(props::KEY_COUNT, to_i64(level.all.len()));
                if level.cut {
                    def.extras.insert(props::TRUNCATED, true);
                }
                ObjectDef::Schema(def)
            }
            ObjectKind::Key => ObjectDef::Generic(load_key(redis, obj, &cancel).await?),
            ObjectKind::User => ObjectDef::Generic(load_user(redis, obj, &cancel).await?),
            other => {
                return Err(Error::unsupported(format!("Redis has no {other} objects")));
            }
        };
        check(&cancel)?;
        Ok(def)
    }
}

fn to_i64<N: TryInto<i64>>(n: N) -> i64 {
    n.try_into().unwrap_or(i64::MAX)
}

fn mode_name(c: &RedisConnection) -> &'static str {
    match c.options().mode {
        RedisMode::Standalone => "standalone",
        RedisMode::Cluster => "cluster",
        RedisMode::Sentinel { .. } => "sentinel",
    }
}

/// The key prefix a listing under `parent` covers: `""` for a database, the namespace's name
/// for a namespace; `None` for other parents.
fn prefix_of(parent: &ObjectRef) -> Option<String> {
    match parent.kind {
        ObjectKind::Database if !parent.is_root() => Some(String::new()),
        ObjectKind::Schema => Some(parent.name.clone()),
        _ => None,
    }
}

// ---- databases --------------------------------------------------------------------------------

async fn databases(c: &RedisConnection, cancel: &CancellationToken) -> Result<Vec<ObjectSummary>> {
    let mode = mode_name(c);
    if c.is_cluster() {
        let keys = key_count(c, cancel).await.ok();
        let mut row = ObjectSummary::new(ObjectRef::database("0"));
        row.extras.insert(extras::MODE, mode);
        if let Some(keys) = keys {
            row.extras.insert(props::KEY_COUNT, to_i64(keys));
        }
        return Ok(vec![row]);
    }
    let count = match c
        .command(&super::keys::common::cmd("CONFIG GET databases"), cancel)
        .await
    {
        Ok(reply) => map_of(reply)
            .get("databases")
            .and_then(int_of)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_DATABASES),
        Err(e) if e.is_cancelled() => return Err(e),
        Err(_) => DEFAULT_DATABASES,
    };
    let keyspace = match c
        .command(&super::keys::common::cmd("INFO keyspace"), cancel)
        .await
    {
        Ok(reply) => Some(parse_keyspace(&text_of(&reply).unwrap_or_default())),
        Err(e) if e.is_cancelled() => return Err(e),
        Err(_) => None,
    };
    Ok((0..count)
        .map(|n| {
            let mut row = ObjectSummary::new(ObjectRef::database(n.to_string()));
            row.extras.insert(extras::MODE, mode);
            if let Some(keyspace) = &keyspace {
                row.extras.insert(
                    props::KEY_COUNT,
                    to_i64(keyspace.get(&n).copied().unwrap_or(0)),
                );
            }
            row
        })
        .collect())
}

/// `INFO keyspace` → keys per database index (`db3:keys=12,expires=1,...`).
pub(crate) fn parse_keyspace(text: &str) -> BTreeMap<u32, u64> {
    text.lines()
        .filter_map(|line| {
            let (db, fields) = line.trim().split_once(':')?;
            let n = db.strip_prefix("db")?.parse().ok()?;
            let keys = fields
                .split(',')
                .find_map(|f| f.strip_prefix("keys="))?
                .parse()
                .ok()?;
            Some((n, keys))
        })
        .collect()
}

/// `DBSIZE` of the session's database (summed over the masters of a cluster).
async fn key_count(c: &RedisConnection, cancel: &CancellationToken) -> Result<u64> {
    let nodes = Nodes::of(c, cancel).await?;
    let mut total = 0_u64;
    for node in &nodes.endpoints {
        let reply = c.command_on(node, &redis::cmd("DBSIZE"), cancel).await?;
        total += int_of(&reply)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0);
    }
    Ok(total)
}

// ---- namespaces and keys ------------------------------------------------------------------------

/// The keys under one prefix (up to the tree scan limit), grouped for one tree level.
struct Level {
    prefix: Vec<u8>,
    separator: Option<Vec<u8>>,
    /// Every distinct key found (at most `tree_scan_limit`).
    all: BTreeSet<Vec<u8>>,
    /// The scan stopped at the limit.
    cut: bool,
}

impl Level {
    async fn scan(c: &RedisConnection, prefix: &[u8], cancel: &CancellationToken) -> Result<Self> {
        let options = c.options();
        let limit = usize::try_from(options.tree_scan_limit).unwrap_or(usize::MAX);
        let nodes = Nodes::of(c, cancel).await?;
        let mut pattern = escape_glob(prefix);
        pattern.push(b'*');
        let mut all = BTreeSet::new();
        let mut cut = false;
        'nodes: for index in 0..nodes.endpoints.len() {
            let mut cursor = b"0".to_vec();
            loop {
                check(cancel)?;
                let (next, keys) = nodes
                    .scan(index, &cursor, &pattern, TREE_SCAN_COUNT, None, cancel)
                    .await?;
                for key in keys {
                    if all.len() >= limit {
                        cut = true;
                        break 'nodes;
                    }
                    all.insert(key);
                }
                if scan_done(&next) {
                    break;
                }
                cursor = next;
            }
        }
        Ok(Self {
            prefix: prefix.to_vec(),
            separator: options
                .namespace_separator
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|s| s.as_bytes().to_vec()),
            all,
            cut,
        })
    }

    /// The namespace directly below the prefix that `key` falls in (UTF-8 only).
    fn namespace_of(&self, key: &[u8]) -> Option<String> {
        let separator = self.separator.as_deref()?;
        let rest = key.strip_prefix(self.prefix.as_slice())?;
        let at = find(rest, separator)?;
        let end = self.prefix.len() + at + separator.len();
        std::str::from_utf8(&key[..end]).ok().map(str::to_owned)
    }

    fn namespaces(&self, parent: &ObjectRef) -> Vec<ObjectSummary> {
        let mut groups: BTreeMap<String, u64> = BTreeMap::new();
        for key in &self.all {
            if let Some(ns) = self.namespace_of(key) {
                *groups.entry(ns).or_default() += 1;
            }
        }
        let truncated = self.cut || groups.len() > TREE_PAGE;
        groups
            .into_iter()
            .take(TREE_PAGE)
            .map(|(name, count)| {
                let mut row = ObjectSummary::new(parent.child(ObjectKind::Schema, name));
                row.extras.insert(props::KEY_COUNT, to_i64(count));
                if truncated {
                    row.extras.insert(props::TRUNCATED, true);
                }
                row
            })
            .collect()
    }

    async fn keys(
        &self,
        c: &RedisConnection,
        parent: &ObjectRef,
        cancel: &CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let mut here: Vec<(String, &Vec<u8>)> = self
            .all
            .iter()
            .filter(|k| self.namespace_of(k).is_none())
            .map(|k| (key_name(k), k))
            .collect();
        here.sort();
        let truncated = self.cut || here.len() > TREE_PAGE;
        here.truncate(TREE_PAGE);
        let keys: Vec<Vec<u8>> = here.iter().map(|(_, k)| (*k).clone()).collect();
        let nodes = Nodes::of(c, cancel).await?;
        let infos = nodes.types_and_ttls(&keys, cancel).await?;
        Ok(here
            .into_iter()
            .zip(infos)
            .filter_map(|((name, bytes), info)| {
                let (key_type, ttl) = info?;
                let mut row = ObjectSummary::new(parent.child(ObjectKind::Key, name));
                row.extras.insert(props::KEY_TYPE, key_type.as_str());
                if let Some(ttl) = ttl {
                    row.extras.insert(props::TTL_MS, to_i64(ttl));
                }
                if std::str::from_utf8(bytes).is_err() {
                    row.extras
                        .insert(props::KEY_HEX, oxbase_types::keyvalue::to_hex(bytes));
                }
                if truncated {
                    row.extras.insert(props::TRUNCATED, true);
                }
                Some(row)
            })
            .collect())
    }
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `load` of a key: its info as extras.
async fn load_key(
    c: &RedisConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<GenericDef> {
    let database = obj.catalog.clone().unwrap_or_else(|| "0".to_owned());
    let mut candidates = vec![obj.name.as_bytes().to_vec()];
    if let Some(raw) = unescape_name(&obj.name) {
        candidates.push(raw);
    }
    for bytes in candidates {
        let address = KeyAddress::new(database.clone(), BinaryText::from_bytes(&bytes));
        let Some(info) = super::keys::read_info(c, &address, &bytes, cancel).await? else {
            continue;
        };
        let mut x = Extras::new().with(props::KEY_TYPE, info.key_type.as_str());
        if let Some(ttl) = info.ttl_ms {
            x.insert(props::TTL_MS, to_i64(ttl));
        }
        if let Some(length) = info.length {
            x.insert(extras::LENGTH, to_i64(length));
        }
        if let Some(encoding) = info.encoding {
            x.insert(extras::ENCODING, encoding);
        }
        if let Some(memory) = info.memory_bytes {
            x.insert(extras::MEMORY_BYTES, to_i64(memory));
        }
        if std::str::from_utf8(&bytes).is_err() {
            x.insert(props::KEY_HEX, oxbase_types::keyvalue::to_hex(&bytes));
        }
        return Ok(GenericDef {
            obj: obj.clone(),
            definition: None,
            comment: None,
            extras: x,
        });
    }
    Err(Error::not_found(format!(
        "the key {} does not exist",
        obj.name
    )))
}

// ---- users --------------------------------------------------------------------------------------

async fn users(c: &RedisConnection, cancel: &CancellationToken) -> Result<Vec<ObjectSummary>> {
    let reply = c
        .command(&super::keys::common::cmd("ACL USERS"), cancel)
        .await?;
    let mut names: Vec<String> = items(reply).iter().filter_map(text_of).collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| ObjectSummary::new(ObjectRef::new(ObjectKind::User, name)))
        .collect())
}

/// A rule field of `ACL GETUSER`: a string (Redis 7+) or a list of patterns (Redis 6).
fn rule_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Array(list) | Value::Set(list)) => list
            .iter()
            .filter_map(text_of)
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => text_of(other).unwrap_or_default(),
        None => String::new(),
    }
}

async fn load_user(
    c: &RedisConnection,
    obj: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<GenericDef> {
    let mut cmd = super::keys::common::cmd("ACL GETUSER");
    cmd.arg(obj.name.as_str());
    let reply = c.command(&cmd, cancel).await?;
    if matches!(reply, Value::Nil) {
        return Err(Error::not_found(format!("no ACL user {}", obj.name)));
    }
    Ok(user_def(obj, reply))
}

/// The definition of a user from its `ACL GETUSER` reply (never its passwords).
pub(crate) fn user_def(obj: &ObjectRef, reply: Value) -> GenericDef {
    let fields = map_of(reply);
    let flags = rule_text(fields.get("flags"));
    let commands = rule_text(fields.get("commands"));
    let keys = rule_text(fields.get("keys"));
    let channels = rule_text(fields.get("channels"));
    let definition = [
        "user",
        obj.name.as_str(),
        &flags,
        &keys,
        &channels,
        &commands,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    GenericDef {
        obj: obj.clone(),
        definition: Some(definition),
        comment: None,
        extras: Extras::new()
            .with(extras::ACL_FLAGS, flags)
            .with(extras::ACL_COMMANDS, commands)
            .with(extras::ACL_KEYS, keys)
            .with(extras::ACL_CHANNELS, channels),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn keyspace_lines_give_key_counts() {
        let text = "# Keyspace\r\ndb0:keys=12,expires=1,avg_ttl=0,subexpiry=0\r\ndb1:keys=1,expires=0,avg_ttl=0\r\n";
        let counts = parse_keyspace(text);
        assert_eq!(counts.get(&0), Some(&12));
        assert_eq!(counts.get(&1), Some(&1));
        assert_eq!(counts.get(&2), None);
        assert!(parse_keyspace("").is_empty());
    }

    #[test]
    fn users_never_show_passwords() {
        // Redis 7+ (RESP2 flat form): rules as strings.
        let reply = Value::Array(vec![
            bulk("flags"),
            Value::Array(vec![bulk("on")]),
            bulk("passwords"),
            Value::Array(vec![bulk(
                "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8",
            )]),
            bulk("commands"),
            bulk("-@all +@read"),
            bulk("keys"),
            bulk("~m7:*"),
            bulk("channels"),
            bulk(""),
            bulk("selectors"),
            Value::Array(vec![]),
        ]);
        let def = user_def(&ObjectRef::new(ObjectKind::User, "m7_reader"), reply);
        let text = def.definition.unwrap();
        assert_eq!(text, "user m7_reader on ~m7:* -@all +@read");
        assert!(!text.contains("5e88"));
        assert_eq!(
            def.extras.get(&extras::ACL_KEYS),
            Some(&oxbase_types::Value::Text("~m7:*".into()))
        );
        // Redis 6 (RESP3 map): key patterns as a list.
        let reply = Value::Map(vec![
            (bulk("flags"), Value::Set(vec![bulk("on"), bulk("allkeys")])),
            (bulk("keys"), Value::Array(vec![bulk("*"), bulk("m7:*")])),
            (bulk("commands"), bulk("+@all")),
        ]);
        let def = user_def(&ObjectRef::new(ObjectKind::User, "admin"), reply);
        assert_eq!(
            def.definition.unwrap(),
            "user admin on allkeys * m7:* +@all"
        );
    }

    #[test]
    fn levels_group_keys_by_separator() {
        let level = Level {
            prefix: b"m7:".to_vec(),
            separator: Some(b":".to_vec()),
            all: [
                b"m7:ns:a:1".to_vec(),
                b"m7:ns:leaf".to_vec(),
                b"m7:list".to_vec(),
                b"m7:\xff:x".to_vec(),
            ]
            .into_iter()
            .collect(),
            cut: false,
        };
        let parent = ObjectRef::schema(Some("0"), "m7:");
        let ns = level.namespaces(&parent);
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].obj.name, "m7:ns:");
        assert_eq!(ns[0].obj.catalog.as_deref(), Some("0"));
        assert_eq!(ns[0].obj.schema.as_deref(), Some("m7:"));
        assert_eq!(
            ns[0].extras.get(&props::KEY_COUNT),
            Some(&oxbase_types::Value::Int(2))
        );
        assert_eq!(level.namespace_of(b"m7:list"), None);
        assert_eq!(level.namespace_of(b"m7:\xff:x"), None);
        assert_eq!(find(b"a::b", b"::"), Some(1));
        assert_eq!(find(b"ab", b""), None);
        assert_eq!(prefix_of(&ObjectRef::database("3")).as_deref(), Some(""));
        assert_eq!(prefix_of(&ObjectRef::root()), None);
        assert_eq!(prefix_of(&parent).as_deref(), Some("m7:"));
    }
}
