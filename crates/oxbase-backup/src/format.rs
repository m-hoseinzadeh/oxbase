//! The Oxbase backup archive, format version 1 (`docs/m8-contracts.md` §7.1).
//!
//! # Container
//!
//! One file (suggested extension `.oxbackup`), a **ZIP** archive (zip64 when large) so a
//! manifest, one object or one table can be read without unpacking the rest:
//!
//! | Entry | Content | ZIP method |
//! |---|---|---|
//! | `manifest.json` | [`Manifest`] (UTF-8 JSON, camelCase) | deflate |
//! | `objects/NNNN.json` | the object's `ObjectDef` JSON exactly as the source fetcher loaded it | deflate |
//! | `objects/NNNN.sql` | its `CREATE` text from the source engine's `DdlGenerator` (for people; restore regenerates DDL for the target container) | deflate |
//! | `data/NNNN.arrows` | the table's rows: an Arrow IPC **stream** in the *load schema* of its columns (`oxbase_dialect::LoadTarget::schema`; generated columns left out), body buffers compressed with zstd ([`Compression`]) | stored |
//!
//! `NNNN` is the object's [`ManifestObject::index`], zero-padded to 4 digits (more digits past
//! 9999). Entry names never contain object names, so no name can escape the archive or collide.
//!
//! # Integrity
//!
//! Every entry but the manifest has a SHA-256 of its uncompressed bytes in
//! [`Manifest::entries`] ([`sha256_hex`]); `verify` recomputes them all and checks row counts
//! of the data entries against the manifest. ZIP's own CRC-32 covers the manifest.
//!
//! # What is never in an archive
//!
//! Connection details (host, port, user, password, SSH or TLS settings), the profile name, file
//! paths of the machine that wrote it. [`Manifest::source`] names only the database and schema.
//!
//! # Versioning
//!
//! [`FORMAT_VERSION`] is 1. A reader accepts every version up to its own and refuses newer
//! ones with `Unsupported` ([`Manifest::check_version`]); unknown manifest fields are kept
//! (`#[serde(flatten)] unknown`), so a newer writer's extra facts survive a round trip.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use oxbase_types::{Engine, Error, ObjectKind, ObjectRef, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The `format` value of every manifest.
pub const FORMAT_NAME: &str = "oxbase-backup";
/// The archive format version this build writes and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;
/// The manifest's entry name.
pub const MANIFEST_ENTRY: &str = "manifest.json";
/// The suggested file extension (without the dot).
pub const FILE_EXTENSION: &str = "oxbackup";

/// How table data entries are compressed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "codec",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Compression {
    /// Arrow IPC buffers uncompressed.
    None,
    /// Arrow IPC buffer compression with zstd (level 1-22; Arrow uses the codec's default
    /// level, the field is kept for the record).
    #[default]
    Zstd,
}

/// What a backup holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupContent {
    /// Definitions and table rows.
    #[default]
    StructureAndData,
    /// Definitions only.
    StructureOnly,
    /// Table rows only (issue #22): only tables are backed up, and their definitions are
    /// kept just to read the rows back. A restore loads the rows into existing tables and
    /// creates or changes nothing.
    DataOnly,
}

impl BackupContent {
    /// Whether the backup reads and stores table rows.
    #[must_use]
    pub fn has_rows(self) -> bool {
        self != Self::StructureOnly
    }

    /// Whether a restore may create or change definitions from the backup.
    #[must_use]
    pub fn has_structure(self) -> bool {
        self != Self::DataOnly
    }
}

/// The source of a backup: names only (module docs, *What is never in an archive*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSource {
    /// The backed-up container (`Schema` or `Database` ref).
    pub container: ObjectRef,
}

/// The data entry of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableData {
    pub entry: String,
    pub rows: u64,
    /// The load columns in entry order (names; types follow from the object definition).
    pub columns: Vec<String>,
}

/// One backed-up object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestObject {
    /// 1-based, unique, in backup order.
    pub index: u32,
    pub kind: ObjectKind,
    /// The object as it was named on the source.
    #[serde(rename = "ref")]
    pub object: ObjectRef,
    /// `objects/NNNN.json`.
    pub definition: String,
    /// `objects/NNNN.sql`, when the source engine could generate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ddl: Option<String>,
    /// Tables of a `structure_and_data` backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<TableData>,
}

/// Size and checksum of one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryInfo {
    /// Uncompressed bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the uncompressed bytes.
    pub sha256: String,
}

/// The archive's table of contents (module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Always [`FORMAT_NAME`].
    pub format: String,
    pub format_version: u32,
    /// The Oxbase version that wrote it.
    pub oxbase_version: String,
    pub created_at: DateTime<Utc>,
    pub engine: Engine,
    /// The source server's version text (`ServerVersion::raw`).
    pub server_version: String,
    pub source: BackupSource,
    pub content: BackupContent,
    pub compression: Compression,
    pub objects: Vec<ManifestObject>,
    /// Creation order on restore (object indexes, dependencies first; computed at backup time
    /// from the definitions, `oxbase_dialect::DependencyGraph`).
    pub order: Vec<u32>,
    /// Every entry but the manifest, by name.
    pub entries: BTreeMap<String, EntryInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Fields of newer format versions, kept as they are.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Manifest {
    /// `InvalidInput` when this is not an Oxbase backup manifest; `Unsupported` when it was
    /// written by a newer format version than [`FORMAT_VERSION`].
    pub fn check_version(&self) -> Result<()> {
        if self.format != FORMAT_NAME {
            return Err(Error::invalid_input("the file is not an Oxbase backup"));
        }
        if self.format_version == 0 {
            return Err(Error::invalid_input(
                "the backup manifest has no valid format version",
            ));
        }
        if self.format_version > FORMAT_VERSION {
            return Err(Error::unsupported(format!(
                "the backup was written in format version {} by Oxbase {}; this version reads up \
                 to format version {FORMAT_VERSION}",
                self.format_version, self.oxbase_version
            )));
        }
        Ok(())
    }

    /// The object with `index`.
    pub fn object(&self, index: u32) -> Option<&ManifestObject> {
        self.objects.iter().find(|o| o.index == index)
    }

    /// Total rows of every table data entry.
    pub fn total_rows(&self) -> u64 {
        self.objects
            .iter()
            .filter_map(|o| o.data.as_ref())
            .map(|d| d.rows)
            .sum()
    }

    /// Structural checks a reader runs before trusting the manifest: indexes unique and
    /// non-zero, `order` a permutation of them, every referenced entry listed in `entries`,
    /// entry names of the documented shapes only. `InvalidInput` naming the first problem.
    pub fn validate(&self) -> Result<()> {
        self.check_version()?;
        let mut seen = std::collections::BTreeSet::new();
        for o in &self.objects {
            if o.index == 0 || !seen.insert(o.index) {
                return Err(Error::invalid_input(format!(
                    "the backup manifest repeats or misnumbers object {}",
                    o.index
                )));
            }
            let mut names = vec![o.definition.as_str()];
            names.extend(o.ddl.as_deref());
            names.extend(o.data.as_ref().map(|d| d.entry.as_str()));
            for name in names {
                if !is_entry_name(name) || !self.entries.contains_key(name) {
                    return Err(Error::invalid_input(format!(
                        "the backup manifest names an unknown entry '{name}'"
                    )));
                }
            }
        }
        let mut order = self.order.clone();
        order.sort_unstable();
        if order != seen.into_iter().collect::<Vec<_>>() {
            return Err(Error::invalid_input(
                "the backup manifest's object order does not list every object once",
            ));
        }
        Ok(())
    }
}

/// The entry name of object `index`'s definition (`objects/0007.json`).
pub fn definition_entry(index: u32) -> String {
    format!("objects/{index:04}.json")
}

/// The entry name of object `index`'s DDL text (`objects/0007.sql`).
pub fn ddl_entry(index: u32) -> String {
    format!("objects/{index:04}.sql")
}

/// The entry name of table `index`'s rows (`data/0007.arrows`).
pub fn data_entry(index: u32) -> String {
    format!("data/{index:04}.arrows")
}

/// `true` for the entry names this format writes (module docs): `objects/<digits>.json`,
/// `objects/<digits>.sql`, `data/<digits>.arrows`.
pub fn is_entry_name(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name.strip_prefix("objects/") {
        rest.strip_suffix(".json")
            .or_else(|| rest.strip_suffix(".sql"))
            .is_some_and(digits)
    } else if let Some(rest) = name.strip_prefix("data/") {
        rest.strip_suffix(".arrows").is_some_and(digits)
    } else {
        false
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// An incremental SHA-256 for entries written or read in pieces.
#[derive(Debug, Clone, Default)]
pub struct EntryHasher {
    hasher: Sha256,
    size: u64,
}

impl EntryHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.size += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    }

    /// The entry's size and checksum.
    pub fn finish(self) -> EntryInfo {
        EntryInfo {
            size: self.size,
            sha256: hex(&self.hasher.finalize()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn manifest() -> Manifest {
        let table = ObjectRef::table(Some("oxbase_test"), Some("m8_src"), "orders");
        let mut entries = BTreeMap::new();
        for name in [definition_entry(1), ddl_entry(1), data_entry(1)] {
            entries.insert(
                name,
                EntryInfo {
                    size: 3,
                    sha256: sha256_hex(b"abc"),
                },
            );
        }
        Manifest {
            format: FORMAT_NAME.into(),
            format_version: FORMAT_VERSION,
            oxbase_version: "0.8.0".into(),
            created_at: DateTime::parse_from_rfc3339("2026-11-03T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            engine: Engine::Postgres,
            server_version: "18.6".into(),
            source: BackupSource {
                container: ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            },
            content: BackupContent::StructureAndData,
            compression: Compression::Zstd,
            objects: vec![ManifestObject {
                index: 1,
                kind: ObjectKind::Table,
                object: table,
                definition: definition_entry(1),
                ddl: Some(ddl_entry(1)),
                data: Some(TableData {
                    entry: data_entry(1),
                    rows: 2,
                    columns: vec!["id".into()],
                }),
            }],
            order: vec![1],
            entries,
            warnings: Vec::new(),
            unknown: serde_json::Map::new(),
        }
    }

    #[test]
    fn checksums_are_lowercase_hex_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = EntryHasher::new();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(
            h.finish(),
            EntryInfo {
                size: 3,
                sha256: sha256_hex(b"abc")
            }
        );
    }

    #[test]
    fn entry_names_have_fixed_shapes() {
        assert_eq!(definition_entry(7), "objects/0007.json");
        assert_eq!(ddl_entry(12_345), "objects/12345.sql");
        assert_eq!(data_entry(1), "data/0001.arrows");
        assert!(is_entry_name("objects/0001.json") && is_entry_name("data/99.arrows"));
        for bad in [
            "manifest.json",
            "objects/../x.json",
            "objects/.json",
            "data/1.json",
            "/etc/passwd",
            "objects/1a.sql",
        ] {
            assert!(!is_entry_name(bad), "{bad}");
        }
    }

    #[test]
    fn manifests_round_trip_and_validate() {
        let m = manifest();
        m.validate().unwrap();
        assert_eq!(m.total_rows(), 2);
        assert!(m.object(1).is_some() && m.object(2).is_none());
        let mut json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["formatVersion"], 1);
        assert_eq!(json["objects"][0]["ref"]["name"], "orders");
        assert_eq!(json["compression"]["codec"], "zstd");
        // Fields of a newer writer survive.
        json["futureField"] = serde_json::json!({"x": 1});
        let back: Manifest = serde_json::from_value(json).unwrap();
        assert_eq!(back.unknown["futureField"]["x"], 1);
        let mut again = back.clone();
        again.unknown.clear();
        assert_eq!(again, m);
        // No connection details anywhere in the manifest.
        let text = serde_json::to_string(&m).unwrap();
        for word in ["host", "port", "user", "password", "ssh", "tls"] {
            assert!(!text.contains(&format!("\"{word}\"")), "{word}");
        }
    }

    #[test]
    fn readers_refuse_newer_or_broken_manifests() {
        let mut newer = manifest();
        newer.format_version = FORMAT_VERSION + 1;
        assert_eq!(
            newer.check_version().unwrap_err().category(),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut other = manifest();
        other.format = "zip".into();
        assert!(other.check_version().is_err());
        let mut order = manifest();
        order.order = vec![1, 1];
        assert!(order.validate().is_err());
        let mut missing = manifest();
        missing.entries.remove(&data_entry(1));
        assert!(missing.validate().is_err());
        let mut escape = manifest();
        escape.objects[0].definition = "../manifest.json".into();
        assert!(escape.validate().is_err());
    }
}
