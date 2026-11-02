//! BSON documents → Arrow, as the driver streams them (the layout contract of
//! `oxbase_dialect::document`): columns inferred from a sample, `_id` first, one typed column
//! per single BSON type, relaxed Extended JSON text (`arrow.json`) for nested, other and mixed
//! values, NULL plus a count for values that do not fit their column, and the `$document`
//! column (canonical Extended JSON of the whole document) last.
//!
//! A missing field is NULL. An explicit BSON `null` is NULL in typed columns and the JSON text
//! `null` in JSON text columns (so the grid tells it apart from a missing field there). A
//! column that held only nulls in the sample is a JSON text column tagged `mixed`
//! (`oxbase.bson_types = null`) that takes any later value. A document field named like the
//! document column (`$document`, allowed since MongoDB 5.0) gets no column of its own; the
//! document column holds it.
//!
//! The driver's request statements and command replies use it; `crate::arrow::DocumentLayout`
//! is the crate-wide entry point with the same rules (mongo stream).

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Float64Builder, Int32Builder, Int64Builder, StringBuilder,
    TimestampMillisecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bson::spec::ElementType;
use bson::{Bson, Document};
use oxbase_dialect::document::{
    DOCUMENT_COLUMN, META_BSON_TYPE, META_BSON_TYPES, META_ROLE, ROLE_DOCUMENT,
};
use oxbase_driver::from_arrow;
use oxbase_types::Result;

/// Arrow field metadata: the column's native type (same value as [`META_BSON_TYPE`]).
const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Arrow field metadata: the binary subtype (two lowercase hex digits) when uniform.
const META_BSON_SUBTYPE: &str = "oxbase.bson_subtype";
const META_EXTENSION_NAME: &str = "ARROW:extension:name";
const JSON_EXTENSION: &str = "arrow.json";

/// The MongoDB `$type` name of a BSON element type.
pub(crate) fn type_name(t: ElementType) -> &'static str {
    match t {
        ElementType::Double => "double",
        ElementType::String => "string",
        ElementType::EmbeddedDocument => "object",
        ElementType::Array => "array",
        ElementType::Binary => "binData",
        ElementType::Undefined => "undefined",
        ElementType::ObjectId => "objectId",
        ElementType::Boolean => "bool",
        ElementType::DateTime => "date",
        ElementType::Null => "null",
        ElementType::RegularExpression => "regex",
        ElementType::DbPointer => "dbPointer",
        ElementType::JavaScriptCode => "javascript",
        ElementType::Symbol => "symbol",
        ElementType::JavaScriptCodeWithScope => "javascriptWithScope",
        ElementType::Int32 => "int",
        ElementType::Timestamp => "timestamp",
        ElementType::Int64 => "long",
        ElementType::Decimal128 => "decimal",
        ElementType::MaxKey => "maxKey",
        ElementType::MinKey => "minKey",
    }
}

/// How a column stores its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ObjectId,
    Str,
    Int32,
    /// `long`, or `int` and `long` (ints widen).
    Int64,
    Double,
    /// Exact decimal text.
    Decimal,
    Bool,
    Date,
    Binary,
    /// Relaxed Extended JSON text: objects, arrays, other types, and mixed columns.
    Json,
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    kind: Kind,
    /// `None`: any type fits (a mixed column); else the single type that fits.
    accepts: Option<ElementType>,
}

impl Column {
    fn fits(&self, t: ElementType) -> bool {
        match self.accepts {
            None => true,
            Some(want) => want == t || (self.kind == Kind::Int64 && t == ElementType::Int32),
        }
    }
}

/// The columns of one document result set.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    schema: SchemaRef,
    columns: Vec<Column>,
}

/// What the sample showed for one field.
#[derive(Default)]
struct Seen {
    /// Non-null types in order of first appearance.
    types: Vec<ElementType>,
    /// Binary subtypes seen.
    subtypes: Vec<u8>,
    /// A null or undefined value appeared.
    nulls: bool,
}

/// Top-level fields a projection includes, in its order (`{"a.b": 1}` includes `a`).
fn projected_fields(projection: &Document) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (key, value) in projection {
        let included = match value {
            Bson::Int32(0) | Bson::Boolean(false) => false,
            Bson::Int64(n) => *n != 0,
            Bson::Double(n) => *n != 0.0,
            _ => true,
        };
        let top = key.split('.').next().unwrap_or(key).to_owned();
        if included && !top.is_empty() && !out.contains(&top) {
            out.push(top);
        }
    }
    out
}

impl Layout {
    /// Infers the columns from `sample`; `projection` fixes the order of the fields it
    /// includes (those seen in the sample).
    pub(crate) fn infer(sample: &[Document], projection: Option<&Document>) -> Self {
        let mut order: Vec<String> = Vec::new();
        let mut seen: HashMap<String, Seen> = HashMap::new();
        for doc in sample {
            for (key, value) in doc {
                if key == DOCUMENT_COLUMN {
                    continue;
                }
                let entry = seen.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Seen::default()
                });
                match value {
                    Bson::Null | Bson::Undefined => entry.nulls = true,
                    other => {
                        let t = other.element_type();
                        if !entry.types.contains(&t) {
                            entry.types.push(t);
                        }
                        if let Bson::Binary(b) = other {
                            let subtype = u8::from(b.subtype);
                            if !entry.subtypes.contains(&subtype) {
                                entry.subtypes.push(subtype);
                            }
                        }
                    }
                }
            }
        }
        // `_id` first, then the projection's fields, then the rest by first appearance.
        let mut names: Vec<String> = Vec::with_capacity(order.len());
        if seen.contains_key("_id") {
            names.push("_id".to_owned());
        }
        if let Some(projection) = projection {
            for name in projected_fields(projection) {
                if seen.contains_key(&name) && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        for name in order {
            if !names.contains(&name) {
                names.push(name);
            }
        }

        let mut fields = Vec::with_capacity(names.len() + 1);
        let mut columns = Vec::with_capacity(names.len());
        for name in names {
            let s = &seen[&name];
            let (column, field) = column_for(&name, s);
            columns.push(column);
            fields.push(field);
        }
        fields.push(
            Field::new(DOCUMENT_COLUMN, DataType::Utf8, true).with_metadata(HashMap::from([
                (META_ROLE.to_owned(), ROLE_DOCUMENT.to_owned()),
                (META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned()),
                (META_BSON_TYPE.to_owned(), "object".to_owned()),
                (META_NATIVE_TYPE.to_owned(), "object".to_owned()),
            ])),
        );
        Self {
            schema: Arc::new(Schema::new(fields)),
            columns,
        }
    }

    /// The Arrow schema (ends with the `$document` column).
    pub(crate) fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// One batch of `documents`, and how many values did not fit their column.
    pub(crate) fn batch(&self, documents: &[Document]) -> Result<(RecordBatch, u64)> {
        let rows = documents.len();
        let mut misfits = 0u64;
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.columns.len() + 1);
        for column in &self.columns {
            let mut values: Vec<Option<&Bson>> = Vec::with_capacity(rows);
            for doc in documents {
                values.push(match doc.get(&column.name) {
                    // JSON text columns write the JSON `null` (or `{"$undefined": true}`).
                    Some(v @ (Bson::Null | Bson::Undefined)) if column.kind == Kind::Json => {
                        Some(v)
                    }
                    None | Some(Bson::Null | Bson::Undefined) => None,
                    Some(v) if column.fits(v.element_type()) => Some(v),
                    Some(_) => {
                        misfits += 1;
                        None
                    }
                });
            }
            arrays.push(build(column.kind, &values));
        }
        let mut docs = StringBuilder::with_capacity(rows, rows * 64);
        for doc in documents {
            let json = Bson::Document(doc.clone()).into_canonical_extjson();
            docs.append_value(json.to_string());
        }
        arrays.push(Arc::new(docs.finish()));
        let batch = RecordBatch::try_new(self.schema(), arrays).map_err(from_arrow)?;
        Ok((batch, misfits))
    }
}

/// The column and Arrow field of `name`, from what the sample showed.
fn column_for(name: &str, seen: &Seen) -> (Column, Field) {
    let types = &seen.types;
    let single = |t: ElementType| types.len() == 1 && types[0] == t;
    let int_long = types.len() == 2
        && types.contains(&ElementType::Int32)
        && types.contains(&ElementType::Int64);
    let (kind, accepts, data_type, bson_type) = if int_long || single(ElementType::Int64) {
        (
            Kind::Int64,
            Some(ElementType::Int64),
            DataType::Int64,
            "long",
        )
    } else if types.len() == 1 {
        let t = types[0];
        let (kind, data_type) = match t {
            ElementType::ObjectId => (Kind::ObjectId, DataType::Utf8),
            ElementType::String => (Kind::Str, DataType::Utf8),
            ElementType::Int32 => (Kind::Int32, DataType::Int32),
            ElementType::Double => (Kind::Double, DataType::Float64),
            ElementType::Decimal128 => (Kind::Decimal, DataType::Utf8),
            ElementType::Boolean => (Kind::Bool, DataType::Boolean),
            ElementType::DateTime => (
                Kind::Date,
                DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            ),
            ElementType::Binary => (Kind::Binary, DataType::Binary),
            _ => (Kind::Json, DataType::Utf8),
        };
        (kind, Some(t), data_type, type_name(t))
    } else {
        // Several types, or only nulls: a JSON column that any later value fits.
        (Kind::Json, None, DataType::Utf8, "mixed")
    };
    let mut metadata = HashMap::from([
        (META_BSON_TYPE.to_owned(), bson_type.to_owned()),
        (META_NATIVE_TYPE.to_owned(), bson_type.to_owned()),
    ]);
    if kind == Kind::Json {
        metadata.insert(META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned());
    }
    if kind == Kind::Json && accepts.is_none() {
        let mut listed: Vec<&str> = types.iter().map(|t| type_name(*t)).collect();
        if seen.nulls {
            listed.push("null");
        }
        metadata.insert(META_BSON_TYPES.to_owned(), listed.join(","));
    }
    if kind == Kind::Binary
        && let [subtype] = seen.subtypes.as_slice()
    {
        metadata.insert(META_BSON_SUBTYPE.to_owned(), format!("{subtype:02x}"));
    }
    let field = Field::new(name, data_type, true).with_metadata(metadata);
    (
        Column {
            name: name.to_owned(),
            kind,
            accepts,
        },
        field,
    )
}

/// The Arrow array of one column from its (fitting) values.
fn build(kind: Kind, values: &[Option<&Bson>]) -> ArrayRef {
    let rows = values.len();
    match kind {
        Kind::ObjectId | Kind::Str | Kind::Decimal | Kind::Json => {
            let mut b = StringBuilder::with_capacity(rows, rows * 16);
            for v in values.iter().copied() {
                match v {
                    None => b.append_null(),
                    Some(Bson::ObjectId(oid)) if kind == Kind::ObjectId => {
                        b.append_value(oid.to_hex());
                    }
                    Some(Bson::String(s)) if kind != Kind::Json => b.append_value(s),
                    Some(Bson::Decimal128(d)) if kind == Kind::Decimal => {
                        b.append_value(d.to_string());
                    }
                    Some(other) => b.append_value(other.clone().into_relaxed_extjson().to_string()),
                }
            }
            Arc::new(b.finish())
        }
        Kind::Int32 => {
            let mut b = Int32Builder::with_capacity(rows);
            for v in values.iter().copied() {
                b.append_option(match v {
                    Some(Bson::Int32(n)) => Some(*n),
                    _ => None,
                });
            }
            Arc::new(b.finish())
        }
        Kind::Int64 => {
            let mut b = Int64Builder::with_capacity(rows);
            for v in values.iter().copied() {
                b.append_option(match v {
                    Some(Bson::Int64(n)) => Some(*n),
                    Some(Bson::Int32(n)) => Some(i64::from(*n)),
                    _ => None,
                });
            }
            Arc::new(b.finish())
        }
        Kind::Double => {
            let mut b = Float64Builder::with_capacity(rows);
            for v in values.iter().copied() {
                b.append_option(match v {
                    Some(Bson::Double(n)) => Some(*n),
                    _ => None,
                });
            }
            Arc::new(b.finish())
        }
        Kind::Bool => {
            let mut b = BooleanBuilder::with_capacity(rows);
            for v in values.iter().copied() {
                b.append_option(match v {
                    Some(Bson::Boolean(x)) => Some(*x),
                    _ => None,
                });
            }
            Arc::new(b.finish())
        }
        Kind::Date => {
            let mut b = TimestampMillisecondBuilder::with_capacity(rows).with_timezone("UTC");
            for v in values.iter().copied() {
                b.append_option(match v {
                    Some(Bson::DateTime(d)) => Some(d.timestamp_millis()),
                    _ => None,
                });
            }
            Arc::new(b.finish())
        }
        Kind::Binary => {
            let mut b = BinaryBuilder::with_capacity(rows, rows * 16);
            for v in values.iter().copied() {
                match v {
                    Some(Bson::Binary(bin)) => b.append_value(&bin.bytes),
                    _ => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Float64Type, Int32Type, Int64Type, TimestampMillisecondType};
    use bson::oid::ObjectId;
    use bson::spec::BinarySubtype;
    use bson::{Binary, DateTime, Decimal128, doc};

    fn meta<'a>(layout: &'a Layout, column: &str, key: &str) -> Option<&'a str> {
        let schema = &layout.schema;
        let field = schema.field_with_name(column).unwrap();
        field.metadata().get(key).map(String::as_str)
    }

    fn names(layout: &Layout) -> Vec<String> {
        layout
            .schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    #[test]
    fn every_mapped_type_gets_its_column() {
        let oid = ObjectId::parse_str("65a000000000000000000001").unwrap();
        let price: Decimal128 = "12.50".parse().unwrap();
        let at = DateTime::from_millis(1_767_225_600_000);
        let docs = vec![
            doc! {
                "name": "a",
                "_id": oid,
                "i": 7_i32,
                "l": 9_000_000_000_i64,
                "d": 1.5,
                "price": price,
                "flag": true,
                "at": at,
                "bin": Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 0xff, 1] },
                "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: vec![7; 16] },
                "meta": { "city": "Oslo", "geo": { "lat": 1.0 } },
                "tags": ["x", "y"],
                "code": Bson::JavaScriptCode("return 1".into()),
            },
            doc! { "_id": ObjectId::parse_str("65a000000000000000000002").unwrap(), "name": "b" },
        ];
        let layout = Layout::infer(&docs, None);
        assert_eq!(
            names(&layout),
            [
                "_id",
                "name",
                "i",
                "l",
                "d",
                "price",
                "flag",
                "at",
                "bin",
                "uuid",
                "meta",
                "tags",
                "code",
                "$document"
            ]
        );
        let s = layout.schema();
        let ty = |n: &str| s.field_with_name(n).unwrap().data_type().clone();
        assert_eq!(ty("_id"), DataType::Utf8);
        assert_eq!(ty("i"), DataType::Int32);
        assert_eq!(ty("l"), DataType::Int64);
        assert_eq!(ty("d"), DataType::Float64);
        assert_eq!(ty("price"), DataType::Utf8);
        assert_eq!(ty("flag"), DataType::Boolean);
        assert_eq!(
            ty("at"),
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
        );
        assert_eq!(ty("bin"), DataType::Binary);
        assert_eq!(ty("meta"), DataType::Utf8);
        assert_eq!(meta(&layout, "_id", META_BSON_TYPE), Some("objectId"));
        assert_eq!(meta(&layout, "price", META_BSON_TYPE), Some("decimal"));
        assert_eq!(meta(&layout, "price", META_NATIVE_TYPE), Some("decimal"));
        assert_eq!(meta(&layout, "bin", META_BSON_SUBTYPE), Some("00"));
        assert_eq!(meta(&layout, "uuid", META_BSON_SUBTYPE), Some("04"));
        assert_eq!(meta(&layout, "meta", META_BSON_TYPE), Some("object"));
        assert_eq!(
            meta(&layout, "meta", META_EXTENSION_NAME),
            Some(JSON_EXTENSION)
        );
        assert_eq!(meta(&layout, "tags", META_BSON_TYPE), Some("array"));
        assert_eq!(meta(&layout, "code", META_BSON_TYPE), Some("javascript"));
        assert_eq!(meta(&layout, "i", META_EXTENSION_NAME), None);
        assert_eq!(meta(&layout, "$document", META_ROLE), Some(ROLE_DOCUMENT));

        let (batch, misfits) = layout.batch(&docs).unwrap();
        assert_eq!(misfits, 0);
        assert_eq!(batch.num_rows(), 2);
        let col = |n: &str| batch.column(s.index_of(n).unwrap()).clone();
        assert_eq!(
            col("_id").as_string::<i32>().value(0),
            "65a000000000000000000001"
        );
        assert_eq!(col("i").as_primitive::<Int32Type>().value(0), 7);
        assert!(col("i").is_null(1), "a missing field is NULL");
        assert_eq!(col("l").as_primitive::<Int64Type>().value(0), 9_000_000_000);
        assert!((col("d").as_primitive::<Float64Type>().value(0) - 1.5).abs() < f64::EPSILON);
        assert_eq!(
            col("price").as_string::<i32>().value(0),
            "12.50",
            "exact decimal text"
        );
        assert!(col("flag").as_boolean().value(0));
        assert_eq!(
            col("at")
                .as_primitive::<TimestampMillisecondType>()
                .value(0),
            1_767_225_600_000
        );
        assert_eq!(col("bin").as_binary::<i32>().value(0), [0, 0xff, 1]);
        assert_eq!(
            col("meta").as_string::<i32>().value(0),
            r#"{"city":"Oslo","geo":{"lat":1.0}}"#
        );
        assert_eq!(col("tags").as_string::<i32>().value(0), r#"["x","y"]"#);
        let document = col("$document");
        let first: serde_json::Value =
            serde_json::from_str(document.as_string::<i32>().value(0)).unwrap();
        // Canonical Extended JSON, field order kept (`name` was first in the document).
        let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "name");
        assert_eq!(
            first["_id"],
            serde_json::json!({"$oid": "65a000000000000000000001"})
        );
        assert_eq!(first["i"], serde_json::json!({"$numberInt": "7"}));
        assert_eq!(
            first["price"],
            serde_json::json!({"$numberDecimal": "12.50"})
        );
        assert_eq!(
            first["at"],
            serde_json::json!({"$date": {"$numberLong": "1767225600000"}})
        );
    }

    #[test]
    fn int_and_long_share_one_int64_column() {
        let docs = vec![doc! {"n": 1_i32}, doc! {"n": 2_i64}, doc! {"n": Bson::Null}];
        let layout = Layout::infer(&docs, None);
        let field = layout.schema.field_with_name("n").unwrap().clone();
        assert_eq!(field.data_type(), &DataType::Int64);
        assert_eq!(meta(&layout, "n", META_BSON_TYPE), Some("long"));
        let (batch, misfits) = layout.batch(&docs).unwrap();
        assert_eq!(misfits, 0);
        let n = batch.column(0).as_primitive::<Int64Type>();
        assert_eq!((n.value(0), n.value(1)), (1, 2));
        assert!(n.is_null(2));
    }

    #[test]
    fn mixed_values_become_json_text_with_the_types_listed() {
        let docs = vec![
            doc! {"_id": 1, "mixed": "text"},
            doc! {"_id": 2, "mixed": 5_i32},
            doc! {"_id": 3, "mixed": 2.5},
            doc! {"_id": 4, "mixed": Bson::Null},
            doc! {"_id": 5, "mixed": {"a": 1}},
            doc! {"_id": 6, "mixed": [1, 2]},
            doc! {"_id": 7},
        ];
        let layout = Layout::infer(&docs, None);
        assert_eq!(meta(&layout, "mixed", META_BSON_TYPE), Some("mixed"));
        assert_eq!(
            meta(&layout, "mixed", META_BSON_TYPES),
            Some("string,int,double,object,array,null")
        );
        assert_eq!(
            meta(&layout, "mixed", META_EXTENSION_NAME),
            Some(JSON_EXTENSION)
        );
        let (batch, misfits) = layout.batch(&docs).unwrap();
        assert_eq!(misfits, 0, "everything fits a mixed column");
        let mixed = batch.column(1).as_string::<i32>();
        assert_eq!(mixed.value(0), r#""text""#);
        assert_eq!(mixed.value(1), "5");
        assert_eq!(mixed.value(2), "2.5");
        assert_eq!(mixed.value(3), "null", "an explicit null is JSON text");
        assert_eq!(mixed.value(4), r#"{"a":1}"#);
        assert_eq!(mixed.value(5), "[1,2]");
        assert!(mixed.is_null(6), "missing");
    }

    #[test]
    fn late_misfits_are_null_and_counted_and_late_fields_stay_in_the_document() {
        let sample = vec![doc! {"_id": 1, "n": 1_i32}];
        let layout = Layout::infer(&sample, None);
        let later = vec![
            doc! {"_id": 2, "n": "two"},
            doc! {"_id": 3, "n": 3_i32, "late": true},
        ];
        let (batch, misfits) = layout.batch(&later).unwrap();
        assert_eq!(misfits, 1);
        assert_eq!(names(&layout), ["_id", "n", "$document"]);
        let n = batch.column(1).as_primitive::<Int32Type>();
        assert!(n.is_null(0));
        assert_eq!(n.value(1), 3);
        let doc = batch.column(2).as_string::<i32>().value(0);
        assert!(doc.contains(r#""n":"two""#), "{doc}");
        assert!(
            batch.column(2).as_string::<i32>().value(1).contains("late"),
            "a field first seen after the sample is in the document column"
        );
    }

    #[test]
    fn projection_orders_included_fields_and_id_stays_first() {
        let docs = vec![doc! {"a": 1, "b": 2, "c": 3, "_id": 9}];
        let projection = doc! {"c": 1, "x.y": 1, "a": true, "b": 0};
        let layout = Layout::infer(&docs, Some(&projection));
        assert_eq!(names(&layout), ["_id", "c", "a", "b", "$document"]);
        assert_eq!(projected_fields(&projection), ["c", "x", "a"]);
    }

    #[test]
    fn empty_results_have_only_the_document_column() {
        let layout = Layout::infer(&[], None);
        assert_eq!(names(&layout), ["$document"]);
        let (batch, misfits) = layout.batch(&[]).unwrap();
        assert_eq!((batch.num_rows(), misfits), (0, 0));
    }

    #[test]
    fn decimal128_text_is_exact_and_other_types_are_named() {
        for text in ["0.10", "12.50", "1E+3", "12345678901234567890.5"] {
            let d: Decimal128 = text.parse().unwrap();
            let docs = vec![doc! {"p": d}];
            let layout = Layout::infer(&docs, None);
            let (batch, _) = layout.batch(&docs).unwrap();
            assert_eq!(batch.column(0).as_string::<i32>().value(0), text);
        }
        assert_eq!(type_name(ElementType::Timestamp), "timestamp");
        assert_eq!(type_name(ElementType::MinKey), "minKey");
        let docs = vec![doc! {"only_null": Bson::Null}];
        let layout = Layout::infer(&docs, None);
        assert_eq!(meta(&layout, "only_null", META_BSON_TYPE), Some("mixed"));
        assert_eq!(meta(&layout, "only_null", META_BSON_TYPES), Some("null"));
    }

    #[test]
    fn object_ids_in_json_columns_stay_extended_json() {
        let oid = ObjectId::parse_str("65a000000000000000000001").unwrap();
        let digits = ObjectId::parse_str("650000000000000000000001").unwrap();
        // `_id` mixes ObjectId and int; `owner` is null-only in the sample.
        let sample = vec![
            doc! {"_id": oid, "owner": Bson::Null},
            doc! {"_id": 2_i32, "owner": Bson::Null},
        ];
        let layout = Layout::infer(&sample, None);
        assert_eq!(meta(&layout, "_id", META_BSON_TYPE), Some("mixed"));
        assert_eq!(meta(&layout, "_id", META_BSON_TYPES), Some("objectId,int"));
        assert_eq!(meta(&layout, "owner", META_BSON_TYPE), Some("mixed"));
        let later = vec![doc! {"_id": digits, "owner": digits}, doc! {"_id": 3_i32}];
        let (batch, misfits) = layout.batch(&sample).unwrap();
        assert_eq!(misfits, 0);
        let id = batch.column(0).as_string::<i32>();
        assert_eq!(id.value(0), r#"{"$oid":"65a000000000000000000001"}"#);
        assert_eq!(id.value(1), "2");
        let owner = batch.column(1).as_string::<i32>();
        assert_eq!(owner.value(0), "null");
        let (batch, misfits) = layout.batch(&later).unwrap();
        assert_eq!(misfits, 0, "a null-only column takes any later value");
        let id = batch.column(0).as_string::<i32>();
        assert_eq!(id.value(0), r#"{"$oid":"650000000000000000000001"}"#);
        let owner = batch.column(1).as_string::<i32>();
        assert_eq!(owner.value(0), r#"{"$oid":"650000000000000000000001"}"#);
        assert!(owner.is_null(1), "a missing field stays NULL");
        for row in 0..2 {
            let text = batch.column(0).as_string::<i32>().value(row);
            serde_json::from_str::<serde_json::Value>(text).unwrap();
        }
    }

    #[test]
    fn a_field_named_like_the_document_column_has_no_column() {
        let docs = vec![doc! {"_id": 1_i32, "$document": "user data", "n": Bson::Null}];
        let layout = Layout::infer(&docs, None);
        assert_eq!(names(&layout), ["_id", "n", "$document"]);
        let (batch, _) = layout.batch(&docs).unwrap();
        let document = batch.column(2).as_string::<i32>().value(0);
        assert!(document.contains("user data"), "{document}");
        assert_eq!(batch.column(1).as_string::<i32>().value(0), "null");
    }
}
