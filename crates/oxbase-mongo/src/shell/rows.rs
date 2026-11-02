//! Shell values → Arrow result sets (module docs, *Results*).
//!
//! Documents a statement computed (`findOne`, `toArray()`, `getIndexes()`, a document literal,
//! `show users`) use the document layout of `oxbase_dialect::document`: `_id` first, then the
//! fields by first appearance, one typed column per single BSON type, relaxed Extended JSON
//! text (`arrow.json`) for nested, other and mixed values (an explicit `null` is the text
//! `null` there, a missing field NULL), and the `$document` column last. All values are known
//! here, so every column fits every row. Cursors returned by a statement do not come through
//! here: they stream through the driver's request path.

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

const META_NATIVE_TYPE: &str = "oxbase.native_type";
const META_BSON_SUBTYPE: &str = "oxbase.bson_subtype";
const META_EXTENSION_NAME: &str = "ARROW:extension:name";
const JSON_EXTENSION: &str = "arrow.json";
/// The column of a scalar result.
pub(super) const VALUE_COLUMN: &str = "value";

/// The MongoDB `$type` name of a BSON element type.
fn type_name(t: ElementType) -> &'static str {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ObjectId,
    Str,
    Int32,
    Int64,
    Double,
    Decimal,
    Bool,
    Date,
    Binary,
    Json,
}

#[derive(Default)]
struct Seen {
    types: Vec<ElementType>,
    subtypes: Vec<u8>,
    nulls: bool,
}

/// Relaxed Extended JSON text of one value.
pub(super) fn relaxed_text(value: &Bson) -> String {
    value.clone().into_relaxed_extjson().to_string()
}

/// Canonical Extended JSON text of a document.
fn canonical_text(doc: &Document) -> String {
    Bson::Document(doc.clone())
        .into_canonical_extjson()
        .to_string()
}

fn json_field(name: &str, bson_type: &str) -> Field {
    Field::new(name, DataType::Utf8, true).with_metadata(HashMap::from([
        (META_BSON_TYPE.to_owned(), bson_type.to_owned()),
        (META_NATIVE_TYPE.to_owned(), bson_type.to_owned()),
        (META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned()),
    ]))
}

/// One result set of `docs` (see the module docs).
pub(super) fn documents(docs: &[Document]) -> Result<(SchemaRef, RecordBatch)> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, Seen> = HashMap::new();
    for doc in docs {
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
    if let Some(at) = order.iter().position(|k| k == "_id") {
        let id = order.remove(at);
        order.insert(0, id);
    }
    let mut fields = Vec::with_capacity(order.len() + 1);
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(order.len() + 1);
    for name in &order {
        let (kind, field) = column(name, &seen[name]);
        let values: Vec<Option<&Bson>> = docs.iter().map(|d| d.get(name)).collect();
        arrays.push(build(kind, &values));
        fields.push(field);
    }
    let mut column = StringBuilder::with_capacity(docs.len(), docs.len() * 64);
    for doc in docs {
        column.append_value(canonical_text(doc));
    }
    arrays.push(Arc::new(column.finish()));
    fields.push(
        Field::new(DOCUMENT_COLUMN, DataType::Utf8, true).with_metadata(HashMap::from([
            (META_ROLE.to_owned(), ROLE_DOCUMENT.to_owned()),
            (META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned()),
            (META_BSON_TYPE.to_owned(), "object".to_owned()),
            (META_NATIVE_TYPE.to_owned(), "object".to_owned()),
        ])),
    );
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), arrays).map_err(from_arrow)?;
    Ok((schema, batch))
}

/// One row, column [`VALUE_COLUMN`]: `value` as relaxed Extended JSON text.
pub(super) fn value(value: &Bson) -> Result<(SchemaRef, RecordBatch)> {
    let bson_type = match value {
        Bson::Null => "null",
        other => type_name(other.element_type()),
    };
    let schema = Arc::new(Schema::new(vec![json_field(VALUE_COLUMN, bson_type)]));
    let mut column = StringBuilder::new();
    column.append_value(relaxed_text(value));
    let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(column.finish())])
        .map_err(from_arrow)?;
    Ok((schema, batch))
}

fn column(name: &str, seen: &Seen) -> (Kind, Field) {
    let types = seen.types.as_slice();
    let (kind, data_type, bson_type) = match types {
        [ElementType::Int64]
        | [ElementType::Int32, ElementType::Int64]
        | [ElementType::Int64, ElementType::Int32] => (Kind::Int64, DataType::Int64, "long"),
        [t] => {
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
            (kind, data_type, type_name(*t))
        }
        _ => (Kind::Json, DataType::Utf8, "mixed"),
    };
    let mut metadata = HashMap::from([
        (META_BSON_TYPE.to_owned(), bson_type.to_owned()),
        (META_NATIVE_TYPE.to_owned(), bson_type.to_owned()),
    ]);
    if kind == Kind::Json {
        metadata.insert(META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned());
    }
    if bson_type == "mixed" {
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
    (
        kind,
        Field::new(name, data_type, true).with_metadata(metadata),
    )
}

fn build(kind: Kind, values: &[Option<&Bson>]) -> ArrayRef {
    let rows = values.len();
    match kind {
        Kind::ObjectId | Kind::Str | Kind::Decimal | Kind::Json => {
            let mut b = StringBuilder::with_capacity(rows, rows * 16);
            for v in values.iter().copied() {
                match v {
                    None => b.append_null(),
                    Some(Bson::Null | Bson::Undefined) if kind != Kind::Json => b.append_null(),
                    Some(Bson::ObjectId(oid)) if kind == Kind::ObjectId => {
                        b.append_value(oid.to_hex());
                    }
                    Some(Bson::String(s)) if kind == Kind::Str => b.append_value(s),
                    Some(Bson::Decimal128(d)) if kind == Kind::Decimal => {
                        b.append_value(d.to_string());
                    }
                    Some(other) => b.append_value(relaxed_text(other)),
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

/// A result set of named text columns and nullable 64-bit integer / boolean columns, for
/// `show` listings.
pub(super) enum ShowColumn {
    Text(&'static str, Vec<String>),
    Int(&'static str, Vec<Option<i64>>),
    Bool(&'static str, Vec<Option<bool>>),
}

pub(super) fn listing(columns: Vec<ShowColumn>) -> Result<(SchemaRef, RecordBatch)> {
    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for column in columns {
        match column {
            ShowColumn::Text(name, values) => {
                fields.push(Field::new(name, DataType::Utf8, false));
                let mut b = StringBuilder::new();
                for v in values {
                    b.append_value(v);
                }
                arrays.push(Arc::new(b.finish()));
            }
            ShowColumn::Int(name, values) => {
                fields.push(Field::new(name, DataType::Int64, true));
                let mut b = Int64Builder::new();
                for v in values {
                    b.append_option(v);
                }
                arrays.push(Arc::new(b.finish()));
            }
            ShowColumn::Bool(name, values) => {
                fields.push(Field::new(name, DataType::Boolean, true));
                let mut b = BooleanBuilder::new();
                for v in values {
                    b.append_option(v);
                }
                arrays.push(Arc::new(b.finish()));
            }
        }
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), arrays).map_err(from_arrow)?;
    Ok((schema, batch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int64Type;
    use bson::doc;
    use bson::oid::ObjectId;

    fn meta<'a>(schema: &'a Schema, column: &str, key: &str) -> Option<&'a str> {
        schema
            .field_with_name(column)
            .unwrap()
            .metadata()
            .get(key)
            .map(String::as_str)
    }

    #[test]
    fn documents_follow_the_layout_rules() {
        let oid = ObjectId::parse_str("65a000000000000000000001").unwrap();
        let docs = vec![
            doc! {"name": "a", "_id": oid, "n": 1_i32, "mixed": 1_i32, "nested": {"x": 1_i32}, "nulls": null},
            doc! {"_id": oid, "n": 5_i64, "mixed": "two", "late": true, "nulls": null},
        ];
        let (schema, batch) = documents(&docs).unwrap();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            [
                "_id",
                "name",
                "n",
                "mixed",
                "nested",
                "nulls",
                "late",
                DOCUMENT_COLUMN
            ]
        );
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.column(0).as_string::<i32>().value(0), oid.to_hex());
        assert_eq!(meta(&schema, "_id", META_BSON_TYPE), Some("objectId"));
        let n = batch.column(2).as_primitive::<Int64Type>();
        assert_eq!((n.value(0), n.value(1)), (1, 5));
        assert_eq!(meta(&schema, "n", META_BSON_TYPE), Some("long"));
        assert_eq!(meta(&schema, "mixed", META_BSON_TYPE), Some("mixed"));
        assert_eq!(meta(&schema, "mixed", META_BSON_TYPES), Some("int,string"));
        let mixed = batch.column(3).as_string::<i32>();
        assert_eq!((mixed.value(0), mixed.value(1)), ("1", "\"two\""));
        assert_eq!(batch.column(4).as_string::<i32>().value(0), r#"{"x":1}"#);
        assert!(batch.column(4).is_null(1), "missing field");
        assert_eq!(meta(&schema, "nulls", META_BSON_TYPES), Some("null"));
        assert_eq!(batch.column(5).as_string::<i32>().value(0), "null");
        assert!(batch.column(6).is_null(0));
        assert_eq!(
            meta(&schema, DOCUMENT_COLUMN, META_ROLE),
            Some(ROLE_DOCUMENT)
        );
        let document = batch.column(7).as_string::<i32>().value(1);
        assert!(document.starts_with(r#"{"_id":{"$oid":"#), "{document}");
        assert!(
            document.contains(r#""n":{"$numberLong":"5"}"#),
            "{document}"
        );
    }

    #[test]
    fn values_are_one_json_row() {
        let (schema, batch) = value(&Bson::Int32(12)).unwrap();
        assert_eq!(schema.field(0).name(), VALUE_COLUMN);
        assert_eq!(meta(&schema, VALUE_COLUMN, META_BSON_TYPE), Some("int"));
        assert_eq!(batch.column(0).as_string::<i32>().value(0), "12");
        let (_, batch) = value(&Bson::String("x".into())).unwrap();
        assert_eq!(batch.column(0).as_string::<i32>().value(0), "\"x\"");
        let (schema, _) = value(&Bson::Null).unwrap();
        assert_eq!(meta(&schema, VALUE_COLUMN, META_BSON_TYPE), Some("null"));
    }

    #[test]
    fn listings() {
        let (schema, batch) = listing(vec![
            ShowColumn::Text("name", vec!["a".into(), "b".into()]),
            ShowColumn::Int("sizeOnDisk", vec![Some(1), None]),
            ShowColumn::Bool("empty", vec![Some(false), Some(true)]),
        ])
        .unwrap();
        assert_eq!(schema.fields().len(), 3);
        assert_eq!(batch.num_rows(), 2);
        assert!(batch.column(1).is_null(1));
        assert_eq!(batch.column(1).as_primitive::<Int64Type>().value(0), 1);
    }
}
