//! Keys of [`ConnectParams::options`](oxbase_driver::ConnectParams::options) the Redis driver
//! reads (profile `options` in the connection dialog), and their parsed form
//! [`RedisOptions`]. Unknown keys are refused with `InvalidInput` naming the key, so a typo
//! never silently changes a connection. Contract: `docs/m7-contracts.md` §7.2.
//!
//! The profile's other fields map as usual: `host` / `port` (the first node), `user` (ACL
//! user; `default` when empty), `password`, `database` (the logical database index, `0` when
//! empty; must be `0` on a cluster), `tls`, `ssh` (standalone only), `connect_timeout`.

use oxbase_driver::ConnectParams;
use oxbase_types::{Endpoint, Error, Result};

/// `standalone` (default), `cluster` or `sentinel`.
pub const MODE: &str = "mode";
/// More seed nodes, `host:port` comma-separated: cluster nodes besides `host:port` (cluster),
/// or the sentinels besides `host:port` (sentinel; `host:port` is then a sentinel too).
pub const NODES: &str = "nodes";
/// The monitored master's name (sentinel mode; required there).
pub const MASTER_NAME: &str = "master_name";
/// Whether the sentinels get credentials: `none` (default) or `same` (the profile's user and
/// password, for sentinels with `requirepass` / ACLs).
pub const SENTINEL_AUTH: &str = "sentinel_auth";
/// `auto` (default: RESP3 with `HELLO 3` when the server has it, else RESP2), `resp2` or
/// `resp3` (fail when the server cannot).
pub const PROTOCOL: &str = "protocol";
/// Separator that splits keys into tree namespaces (default `:`). Empty: no namespaces, every
/// key directly under its database.
pub const NAMESPACE_SEPARATOR: &str = "namespace_separator";
/// Keys a tree listing scans at most per node (default [`DEFAULT_TREE_SCAN_LIMIT`]); past it
/// the node's listing is marked `props::TRUNCATED` and the key browser shows the rest.
pub const TREE_SCAN_LIMIT: &str = "tree_scan_limit";
/// Cluster: `true` sends reads of the console to replicas (default `false`).
pub const READ_FROM_REPLICAS: &str = "read_from_replicas";

/// Every key above, for validation.
pub const ALL: [&str; 8] = [
    MODE,
    NODES,
    MASTER_NAME,
    SENTINEL_AUTH,
    PROTOCOL,
    NAMESPACE_SEPARATOR,
    TREE_SCAN_LIMIT,
    READ_FROM_REPLICAS,
];

/// Default [`TREE_SCAN_LIMIT`].
pub const DEFAULT_TREE_SCAN_LIMIT: u64 = 100_000;
/// Default [`NAMESPACE_SEPARATOR`].
pub const DEFAULT_NAMESPACE_SEPARATOR: &str = ":";
/// The Redis port used when a node has none.
pub const DEFAULT_PORT: u16 = 6379;

/// Who the sentinels authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SentinelAuth {
    None,
    Same,
}

/// How the profile reaches its data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RedisMode {
    /// One server (or a replica) at `host:port`.
    Standalone,
    /// Redis Cluster: the seeds are `host:port` plus [`NODES`]; slots are discovered with
    /// `CLUSTER SLOTS` / `CLUSTER SHARDS` and keys routed by slot.
    Cluster,
    /// Sentinel: the seeds are sentinels; the master of `master_name` is asked for, then
    /// connected to.
    Sentinel {
        master_name: String,
        auth: SentinelAuth,
    },
}

/// The RESP version to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Auto,
    Resp2,
    Resp3,
}

/// [`ConnectParams`] options of a Redis profile, parsed and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisOptions {
    pub mode: RedisMode,
    /// `host:port` first, then [`NODES`], without repeats.
    pub seeds: Vec<Endpoint>,
    pub protocol: Protocol,
    /// `None` when namespaces are off.
    pub namespace_separator: Option<String>,
    pub tree_scan_limit: u64,
    pub read_from_replicas: bool,
    /// The logical database (`ConnectParams::database`, default 0).
    pub database: u16,
}

impl RedisOptions {
    /// Parses and checks `params.options` (and `host`, `port`, `database`, `ssh`): unknown
    /// keys, bad values, a cluster on a database other than 0, sentinel without a master name,
    /// and cluster or sentinel through SSH (one tunnel reaches one node; M7) are
    /// `InvalidInput`.
    pub fn from_params(params: &ConnectParams) -> Result<Self> {
        if let Some(key) = params.options.keys().find(|k| !ALL.contains(&k.as_str())) {
            return Err(Error::invalid_input(format!(
                "unknown Redis option '{key}'"
            )));
        }
        let get = |key: &str| params.options.get(key).map(|v| v.trim());
        let database = match params.database.as_deref().map(str::trim) {
            None | Some("") => 0,
            Some(db) => db.parse::<u16>().map_err(|_| {
                Error::invalid_input(format!("the Redis database must be a number, not '{db}'"))
            })?,
        };
        let mode = match get(MODE) {
            None | Some("" | "standalone") => RedisMode::Standalone,
            Some("cluster") => RedisMode::Cluster,
            Some("sentinel") => {
                let master_name = get(MASTER_NAME)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| Error::invalid_input("sentinel mode needs the master name"))?
                    .to_owned();
                let auth = match get(SENTINEL_AUTH) {
                    None | Some("" | "none") => SentinelAuth::None,
                    Some("same") => SentinelAuth::Same,
                    Some(other) => {
                        return Err(Error::invalid_input(format!(
                            "sentinel_auth must be none or same, not '{other}'"
                        )));
                    }
                };
                RedisMode::Sentinel { master_name, auth }
            }
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "mode must be standalone, cluster or sentinel, not '{other}'"
                )));
            }
        };
        if mode != RedisMode::Standalone && params.ssh.is_some() {
            return Err(Error::invalid_input(
                "SSH tunnels reach one node: use them with standalone Redis only",
            ));
        }
        if mode == RedisMode::Cluster && database != 0 {
            return Err(Error::invalid_input("a Redis cluster has only database 0"));
        }
        let mut seeds = Vec::new();
        if let Some(endpoint) = params.server_endpoint() {
            seeds.push(endpoint);
        }
        if let Some(nodes) = get(NODES) {
            for node in Endpoint::parse_list(nodes, DEFAULT_PORT)? {
                if !seeds.contains(&node) {
                    seeds.push(node);
                }
            }
        }
        if seeds.is_empty() {
            return Err(Error::invalid_input("the Redis profile has no host"));
        }
        let protocol = match get(PROTOCOL) {
            None | Some("" | "auto") => Protocol::Auto,
            Some("resp2") => Protocol::Resp2,
            Some("resp3") => Protocol::Resp3,
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "protocol must be auto, resp2 or resp3, not '{other}'"
                )));
            }
        };
        let namespace_separator = match params.options.get(NAMESPACE_SEPARATOR) {
            None => Some(DEFAULT_NAMESPACE_SEPARATOR.to_owned()),
            Some(sep) if sep.is_empty() => None,
            Some(sep) => Some(sep.clone()),
        };
        let tree_scan_limit = match get(TREE_SCAN_LIMIT) {
            None | Some("") => DEFAULT_TREE_SCAN_LIMIT,
            Some(n) => n.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                Error::invalid_input(format!(
                    "tree_scan_limit must be a positive number, not '{n}'"
                ))
            })?,
        };
        let read_from_replicas = match get(READ_FROM_REPLICAS) {
            None | Some("" | "false") => false,
            Some("true") => true,
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "read_from_replicas must be true or false, not '{other}'"
                )));
            }
        };
        Ok(Self {
            mode,
            seeds,
            protocol,
            namespace_separator,
            tree_scan_limit,
            read_from_replicas,
            database,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Engine, SshAuth, SshOptions};

    fn params() -> ConnectParams {
        let mut p = ConnectParams::network(Engine::Redis, "127.0.0.1");
        p.port = Some(15479);
        p
    }

    #[test]
    fn standalone_defaults() {
        let o = RedisOptions::from_params(&params()).unwrap();
        assert_eq!(o.mode, RedisMode::Standalone);
        assert_eq!(o.seeds, [Endpoint::new("127.0.0.1", 15479)]);
        assert_eq!(o.protocol, Protocol::Auto);
        assert_eq!(o.namespace_separator.as_deref(), Some(":"));
        assert_eq!(o.tree_scan_limit, DEFAULT_TREE_SCAN_LIMIT);
        assert!(!o.read_from_replicas);
        assert_eq!(o.database, 0);
    }

    #[test]
    fn cluster_and_sentinel_options() {
        let mut p = params();
        p.options.insert(MODE.into(), "cluster".into());
        p.options
            .insert(NODES.into(), "127.0.0.1:15502, 127.0.0.1:15479,h3".into());
        p.options.insert(NAMESPACE_SEPARATOR.into(), String::new());
        let o = RedisOptions::from_params(&p).unwrap();
        assert_eq!(o.mode, RedisMode::Cluster);
        assert_eq!(
            o.seeds,
            [
                Endpoint::new("127.0.0.1", 15479),
                Endpoint::new("127.0.0.1", 15502),
                Endpoint::new("h3", DEFAULT_PORT)
            ]
        );
        assert_eq!(o.namespace_separator, None);
        p.database = Some("3".into());
        assert!(RedisOptions::from_params(&p).is_err());

        let mut s = params();
        s.options.insert(MODE.into(), "sentinel".into());
        assert!(RedisOptions::from_params(&s).is_err());
        s.options.insert(MASTER_NAME.into(), "oxmaster".into());
        s.options.insert(SENTINEL_AUTH.into(), "same".into());
        assert_eq!(
            RedisOptions::from_params(&s).unwrap().mode,
            RedisMode::Sentinel {
                master_name: "oxmaster".into(),
                auth: SentinelAuth::Same
            }
        );
        s.ssh = Some(SshOptions::new("127.0.0.1", "oxbase", SshAuth::Agent));
        assert!(RedisOptions::from_params(&s).is_err());
    }

    #[test]
    fn bad_values_are_refused() {
        for (key, value) in [
            ("modee", "cluster"),
            (MODE, "ring"),
            (PROTOCOL, "resp4"),
            (TREE_SCAN_LIMIT, "0"),
            (READ_FROM_REPLICAS, "yes"),
        ] {
            let mut p = params();
            p.options.insert(key.into(), value.into());
            assert!(RedisOptions::from_params(&p).is_err(), "{key}={value}");
        }
        let mut p = params();
        p.database = Some("x".into());
        assert!(RedisOptions::from_params(&p).is_err());
    }
}
