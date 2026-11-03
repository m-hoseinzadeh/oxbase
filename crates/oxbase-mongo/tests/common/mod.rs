//! Shared helpers of the mongo stream's live tests (`fetch_live`, `documents_live`): fixture
//! parameters from `tests/fixtures.toml` (`$OXBASE_FIXTURES`), connections, a per-run token.
#![allow(dead_code, reason = "each test file uses its own subset")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bson::Document;
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, collect};
use oxbase_mongo::{MongoConnection, options};
use oxbase_types::{Engine, TlsMode, TlsOptions};

pub const DATABASE: &str = "oxbase_test";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

pub fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    workspace_root().join(p)
                }
            },
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let mut table = t;
    let mut out = None;
    for (i, key) in path.iter().enumerate() {
        let next = table
            .get(*key)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
        if i + 1 < path.len() {
            table = next
                .as_table()
                .unwrap_or_else(|| panic!("fixtures.toml: {key} is not a table"));
        }
        out = Some(next);
    }
    out.expect("non-empty path")
}

pub fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path)
        .as_str()
        .unwrap_or_else(|| panic!("fixtures.toml: {} is not a string", path.join(".")))
        .to_owned()
}

pub fn n(t: &toml::Table, path: &[&str]) -> i64 {
    get(t, path)
        .as_integer()
        .unwrap_or_else(|| panic!("fixtures.toml: {} is not an integer", path.join(".")))
}

pub fn list(t: &toml::Table, path: &[&str]) -> Vec<String> {
    get(t, path)
        .as_array()
        .unwrap_or_else(|| panic!("fixtures.toml: {} is not a list", path.join(".")))
        .iter()
        .map(|v| v.as_str().expect("a string").to_owned())
        .collect()
}

/// The fixture user on `oxbase_test`, TLS disabled; `sample_size` sets the profile option.
pub fn params(sample_size: Option<usize>) -> ConnectParams {
    let f = fixtures();
    let port = u16::try_from(n(&f, &["engines", "mongodb", "port"])).expect("port");
    let mut p = ConnectParams::network(Engine::MongoDb, s(&f, &["engines", "mongodb", "host"]))
        .with_port(port)
        .with_user(s(&f, &["engines", "mongodb", "user"]))
        .with_password(s(&f, &["engines", "mongodb", "password"]))
        .with_database(s(&f, &["engines", "mongodb", "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    p.options.insert(
        options::AUTH_SOURCE.into(),
        s(&f, &["engines", "mongodb", "auth_source"]),
    );
    if let Some(size) = sample_size {
        p.options
            .insert(options::SAMPLE_SIZE.into(), size.to_string());
    }
    p.connect_timeout = Some(Duration::from_secs(15));
    p
}

pub async fn connect(sample_size: Option<usize>) -> MongoConnection {
    let c = MongoConnection::connect(&params(sample_size))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    require_seed(&c).await;
    c
}

/// Fails unless the M7 seed (`tests/docker/init/mongodb/02-m7.js`) is loaded.
async fn require_seed(c: &MongoConnection) {
    let f = fixtures();
    let expected = u64::try_from(n(&f, &["m7", "mongodb", "docs_count"])).unwrap();
    let docs = c
        .client()
        .database(DATABASE)
        .collection::<Document>("m7_docs")
        .count_documents(bson::doc! {})
        .await
        .expect("count m7_docs");
    assert_eq!(
        docs, expected,
        "the M7 MongoDB seed is missing: recreate the fixture (scripts/fixtures-up.sh mongodb)"
    );
}

/// `<epoch secs>x<pid>`: unique per test process, and old ones can be recognized.
pub fn token() -> String {
    format!("{}x{}", epoch_secs(), std::process::id())
}

pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub async fn run(c: &dyn Connection, text: &str) -> Collected {
    collect(
        c.execute(text, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{text}: {e}"))
}
