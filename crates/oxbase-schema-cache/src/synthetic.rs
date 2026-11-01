//! Synthetic catalogs of any size, for tests and the M6 latency check
//! (`docs/m6-contracts.md` §5): [`SyntheticSpec::exit_check`] is 5 000 tables x 20 columns.
//!
//! Deterministic: the same spec always gives the same snapshot. Names are built from small
//! word lists (`customer_order_17`, `created_at`, `warehouse_id`), so prefixes are shared by
//! many names the way real schemas share them; every table has an `id` primary key, and
//! `<word>_id` columns get foreign keys to a table of that word when there is one.

use oxbase_types::{
    CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, Engine, RelationKind, RoutineKind, ServerVersion,
};

/// The shape of a synthetic catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticSpec {
    pub engine: Engine,
    /// Total relations (every tenth one is a view).
    pub tables: usize,
    /// Columns per relation (at least 1: the `id`).
    pub columns_per_table: usize,
    /// Schemas the relations are spread over (1 on MySQL, MariaDB and SQLite).
    pub schemas: usize,
    /// Total routines.
    pub routines: usize,
    pub seed: u64,
}

impl SyntheticSpec {
    /// The M6 exit check: PostgreSQL, 5 000 relations x 20 columns in 4 schemas, 500
    /// routines.
    pub const fn exit_check() -> Self {
        Self {
            engine: Engine::Postgres,
            tables: 5_000,
            columns_per_table: 20,
            schemas: 4,
            routines: 500,
            seed: 6,
        }
    }
}

const NOUNS: [&str; 32] = [
    "account",
    "address",
    "audit",
    "batch",
    "billing",
    "campaign",
    "category",
    "customer",
    "delivery",
    "device",
    "discount",
    "employee",
    "event",
    "invoice",
    "item",
    "ledger",
    "location",
    "message",
    "order",
    "payment",
    "price",
    "product",
    "project",
    "region",
    "report",
    "sale",
    "session",
    "shipment",
    "stock",
    "supplier",
    "ticket",
    "warehouse",
];

const COLUMN_WORDS: [&str; 24] = [
    "amount",
    "code",
    "comment",
    "created_at",
    "created_by",
    "currency",
    "deleted_at",
    "description",
    "due_date",
    "email",
    "external_ref",
    "flags",
    "is_active",
    "label",
    "name",
    "notes",
    "phone",
    "quantity",
    "rank",
    "status",
    "total",
    "updated_at",
    "updated_by",
    "version",
];

const TYPES: [&str; 8] = [
    "integer",
    "bigint",
    "text",
    "character varying(80)",
    "numeric(12,2)",
    "timestamp with time zone",
    "boolean",
    "date",
];

const SCHEMAS: [&str; 8] = [
    "public",
    "sales",
    "inventory",
    "audit",
    "billing",
    "crm",
    "staging",
    "reporting",
];

/// xorshift64*: small, deterministic, good enough for names.
struct Rng(u64);

impl Rng {
    fn step(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.step() % n).unwrap_or(0)
    }
}

/// The snapshot for `spec`.
pub fn snapshot(spec: &SyntheticSpec) -> CatalogSnapshot {
    let mut rng = Rng(spec.seed | 1);
    let single = matches!(
        spec.engine,
        Engine::MySql | Engine::MariaDb | Engine::Sqlite
    );
    let database = if spec.engine == Engine::Sqlite {
        "main"
    } else {
        "app"
    };
    let schema_names: Vec<String> = if single {
        vec![database.to_owned()]
    } else {
        (0..spec.schemas.max(1))
            .map(|i| match SCHEMAS.get(i) {
                Some(name) => (*name).to_owned(),
                None => format!("schema_{i}"),
            })
            .collect()
    };
    let mut snap = CatalogSnapshot::new(spec.engine, ServerVersion::new(18, 0, 0), database);
    snap.databases = vec![
        database.to_owned(),
        "postgres".to_owned(),
        "analytics".to_owned(),
    ];
    snap.default_schema = Some(schema_names[0].clone());
    snap.search_path = vec![schema_names[0].clone()];
    snap.schemas = schema_names.iter().map(CatalogSchema::new).collect();
    snap.read_at_ms = 1_700_000_000_000;

    for t in 0..spec.tables {
        let a = NOUNS[rng.below(NOUNS.len())];
        let b = NOUNS[rng.below(NOUNS.len())];
        let name = format!("{a}_{b}_{t}");
        let mut columns = vec![CatalogColumn {
            name: "id".to_owned(),
            native_type: "bigint".to_owned(),
            nullable: false,
            primary_key: true,
        }];
        let mut foreign_keys = Vec::new();
        for c in 1..spec.columns_per_table {
            let (base, fk) = if c % 5 == 1 {
                let noun = NOUNS[rng.below(NOUNS.len())];
                (format!("{noun}_id"), Some(noun))
            } else {
                (COLUMN_WORDS[rng.below(COLUMN_WORDS.len())].to_owned(), None)
            };
            let col_name = if columns.iter().any(|x| x.name == base) {
                format!("{base}_{c}")
            } else {
                base
            };
            if let Some(noun) = fk {
                foreign_keys.push(CatalogForeignKey {
                    columns: vec![col_name.clone()],
                    ref_schema: None,
                    ref_table: format!("{noun}_{noun}_{}", t % 7),
                    ref_columns: vec!["id".to_owned()],
                });
            }
            columns.push(CatalogColumn {
                name: col_name,
                native_type: TYPES[rng.below(TYPES.len())].to_owned(),
                nullable: c % 3 != 0,
                primary_key: false,
            });
        }
        let kind = if t % 10 == 9 {
            RelationKind::View
        } else {
            RelationKind::Table
        };
        if kind == RelationKind::View {
            foreign_keys.clear();
        }
        let schema = t % snap.schemas.len();
        snap.schemas[schema].relations.push(CatalogRelation {
            name,
            kind,
            columns,
            foreign_keys,
            comment: (t % 50 == 0).then(|| format!("Synthetic table number {t}.")),
        });
    }
    for r in 0..spec.routines {
        let noun = NOUNS[rng.below(NOUNS.len())];
        let schema = r % snap.schemas.len();
        let procedure = r % 4 == 3;
        snap.schemas[schema].routines.push(CatalogRoutine {
            name: format!("{}_{noun}_{r}", if procedure { "refresh" } else { "get" }),
            kind: if procedure {
                RoutineKind::Procedure
            } else {
                RoutineKind::Function
            },
            args: format!("p_{noun}_id bigint"),
            returns: (!procedure).then(|| "numeric".to_owned()),
        });
    }
    for (i, schema) in snap.schemas.iter_mut().enumerate() {
        schema.sequences.push(format!("{}_seq_{i}", schema.name));
        schema.types.push(format!("{}_status_{i}", schema.name));
    }
    snap.normalize();
    snap
}
