//! Extended JSON v2 ↔ BSON (mongo stream), used by requests, document edits, `document_get`
//! and the shell's `EJSON` helper. Field order is kept both ways (`serde_json` has
//! `preserve_order` in this workspace; documents travel as text, see
//! `oxbase_types::document`).
//!
//! Parsing accepts the relaxed and the canonical form (`bson`'s own Extended JSON reader);
//! writing produces the canonical form for everything the core hands to the UI (so a document
//! survives a round trip without losing a type: `1` stays an `int32`, `{"$numberLong": "1"}` an
//! `int64`) and the relaxed form for display cells.

use std::fmt::Write as _;

use bson::{Bson, Document};
use oxbase_types::{Error, Result};
use serde_json::Value as Json;

/// A document from Extended JSON text (relaxed or canonical). `InvalidInput` when the text is
/// not JSON, not an object, or holds a malformed wrapper (`{"$oid": "xyz"}`).
pub fn parse_document(text: &str) -> Result<Document> {
    let json: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the document is not valid JSON: {e}")))?;
    if !json.is_object() {
        return Err(Error::invalid_input("a document must be a JSON object"));
    }
    match Bson::try_from(json) {
        Ok(Bson::Document(doc)) => Ok(doc),
        Ok(other) => Err(Error::invalid_input(format!(
            "a document must be an object, not a {} value",
            crate::driver::convert::type_name(other.element_type())
        ))),
        Err(e) => Err(Error::invalid_input(format!(
            "the document holds an invalid Extended JSON value: {e}"
        ))),
    }
}

/// Any BSON value from Extended JSON text (an `_id`, a field value).
pub fn parse_value(text: &str) -> Result<Bson> {
    let json: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the value is not valid JSON: {e}")))?;
    Bson::try_from(json)
        .map_err(|e| Error::invalid_input(format!("invalid Extended JSON value: {e}")))
}

/// A filter, projection, sort, hint, collation or pipeline stage from a JSON value: Extended
/// JSON, except that an object whose keys are exactly `$regex` (and `$options`) stays the
/// query operator. `what` names the part in errors.
pub fn query_document(value: &Json, what: &str) -> Result<Document> {
    crate::driver::statement::query_document(value, what)
}

/// `value` as canonical Extended JSON text (what the core sends to the UI).
pub fn to_canonical_text(value: &Bson) -> String {
    value.clone().into_canonical_extjson().to_string()
}

/// `value` as relaxed Extended JSON (cells of JSON text columns, the shell's `printjson`).
pub fn to_relaxed(value: &Bson) -> Json {
    value.clone().into_relaxed_extjson()
}

/// `value` as mongo-shell code that evaluates to the same BSON (find / aggregate history text,
/// [`crate::request::request_text`]). Plain wrapper objects such as `{"$numberLong": "1"}` are
/// not typed values in the shell, so typed values are written with the shell's helpers:
///
/// | BSON | Shell text |
/// |---|---|
/// | int32 | `5` |
/// | int64 | `NumberLong("5")` |
/// | double | `1.5`, `1.0`, `NaN`, `Infinity`, `-Infinity` |
/// | decimal | `NumberDecimal("12.50")` |
/// | string, bool, null | JSON |
/// | ObjectId | `ObjectId("...")` |
/// | date | `ISODate("2026-01-02T00:00:00Z")` (`new Date(<ms>)` outside RFC 3339 years) |
/// | binary | `UUID("...")` for subtype 4 of 16 bytes, else `BinData(<subtype>, "<base64>")` |
/// | timestamp | `Timestamp(<t>, <i>)` |
/// | regex | `/pattern/flags` when JavaScript accepts it, else `{"$regex": ..., "$options": ...}` (the query operator form) |
/// | min / max key | `MinKey()`, `MaxKey()` |
/// | documents, arrays | `{"k":v,...}`, `[v,...]` with keys as JSON strings, order kept |
/// | anything else | `EJSON.parse("<canonical Extended JSON>")` |
///
/// A double holding a whole number is written as `1.0`; the shell (like mongosh) turns a
/// whole JavaScript number that fits an int32 into an int32, so pasted text compares the same
/// (numbers compare by value) but stores an int32 where the request had a double.
pub fn to_shell_text(value: &Bson) -> String {
    let mut out = String::new();
    write_shell(&mut out, value);
    out
}

fn write_shell(out: &mut String, value: &Bson) {
    match value {
        Bson::Document(d) => {
            out.push('{');
            for (i, (key, v)) in d.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&js_string(key));
                out.push(':');
                write_shell(out, v);
            }
            out.push('}');
        }
        Bson::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_shell(out, v);
            }
            out.push(']');
        }
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "NumberLong(\"{n}\")");
        }
        Bson::Double(f) => out.push_str(&shell_double(*f)),
        Bson::Decimal128(d) => {
            let _ = write!(out, "NumberDecimal(\"{d}\")");
        }
        Bson::String(text) => out.push_str(&js_string(text)),
        Bson::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::ObjectId(oid) => {
            let _ = write!(out, "ObjectId(\"{}\")", oid.to_hex());
        }
        Bson::DateTime(dt) => {
            let _ = match dt.try_to_rfc3339_string() {
                Ok(text) => write!(out, "ISODate({})", js_string(&text)),
                Err(_) => write!(out, "new Date({})", dt.timestamp_millis()),
            };
        }
        Bson::Binary(_) => out.push_str(&shell_binary(value)),
        Bson::Timestamp(ts) => {
            let _ = write!(out, "Timestamp({}, {})", ts.time, ts.increment);
        }
        Bson::RegularExpression(re) => {
            let pattern = re.pattern.as_str();
            let options = re.options.as_str();
            if let Some(literal) = regex_literal(pattern, options) {
                out.push_str(&literal);
            } else {
                let _ = write!(
                    out,
                    "{{\"$regex\":{},\"$options\":{}}}",
                    js_string(pattern),
                    js_string(options)
                );
            }
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        other => {
            let _ = write!(out, "EJSON.parse({})", js_string(&to_canonical_text(other)));
        }
    }
}

/// `text` as a JavaScript string literal (JSON string syntax is valid JavaScript).
fn js_string(text: &str) -> String {
    Json::String(text.to_owned()).to_string()
}

fn shell_double(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        // `Debug` keeps a `.0` on whole numbers and uses exponents for very large or small
        // ones (`1e300`, `1e-7`), both valid JavaScript.
        format!("{f:?}")
    }
}

/// `UUID("...")` for a subtype-4 binary of 16 bytes, else `BinData(subtype, "base64")`.
fn shell_binary(value: &Bson) -> String {
    let canonical = value.clone().into_canonical_extjson();
    let binary = canonical.get("$binary");
    let base64 = binary
        .and_then(|b| b.get("base64"))
        .and_then(Json::as_str)
        .unwrap_or_default();
    let subtype = binary
        .and_then(|b| b.get("subType"))
        .and_then(Json::as_str)
        .and_then(|s| u8::from_str_radix(s, 16).ok())
        .unwrap_or_default();
    if let Bson::Binary(b) = value
        && subtype == 4
        && b.bytes.len() == 16
    {
        let hex = b
            .bytes
            .iter()
            .fold(String::with_capacity(32), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        return format!(
            "UUID(\"{}-{}-{}-{}-{}\")",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        );
    }
    format!("BinData({subtype}, {})", js_string(base64))
}

/// `/pattern/flags` when that literal is valid JavaScript with the same meaning: flags only
/// from `imsu`, a non-empty pattern on one line that does not end in a lone backslash;
/// unescaped `/` are escaped. `None` otherwise.
fn regex_literal(pattern: &str, options: &str) -> Option<String> {
    if pattern.is_empty()
        || !options.chars().all(|c| matches!(c, 'i' | 'm' | 's' | 'u'))
        || pattern.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    {
        return None;
    }
    let mut body = String::with_capacity(pattern.len() + 2);
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                body.push(c);
                body.push(chars.next()?);
            }
            '/' => body.push_str("\\/"),
            _ => body.push(c),
        }
    }
    Some(format!("/{body}/{options}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;
    use bson::{Decimal128, doc};
    use serde_json::json;

    #[test]
    fn documents_parse_from_both_forms_and_keep_their_field_order() {
        let doc = parse_document(
            r#"{"z": 1, "a": {"$numberLong": "2"}, "_id": {"$oid": "65a000000000000000000001"},
                "d": {"$numberDecimal": "12.50"}, "when": {"$date": "2026-01-02T00:00:00Z"}}"#,
        )
        .unwrap();
        let keys: Vec<&String> = doc.keys().collect();
        assert_eq!(keys, ["z", "a", "_id", "d", "when"]);
        assert_eq!(doc.get("z"), Some(&Bson::Int32(1)));
        assert_eq!(doc.get("a"), Some(&Bson::Int64(2)));
        assert_eq!(
            doc.get("_id"),
            Some(&Bson::ObjectId(
                ObjectId::parse_str("65a000000000000000000001").unwrap()
            ))
        );
        assert_eq!(
            doc.get("d"),
            Some(&Bson::Decimal128("12.50".parse::<Decimal128>().unwrap()))
        );
        assert!(matches!(doc.get("when"), Some(Bson::DateTime(_))));
    }

    #[test]
    fn canonical_text_round_trips_types() {
        let original = doc! {"b": 1_i32, "a": 1_i64, "f": 1.5, "s": "x", "n": null};
        let text = to_canonical_text(&Bson::Document(original.clone()));
        assert_eq!(
            text,
            r#"{"b":{"$numberInt":"1"},"a":{"$numberLong":"1"},"f":{"$numberDouble":"1.5"},"s":"x","n":null}"#
        );
        assert_eq!(parse_document(&text).unwrap(), original);
        assert_eq!(
            to_relaxed(&Bson::Document(original)),
            json!({"b": 1, "a": 1, "f": 1.5, "s": "x", "n": null})
        );
    }

    #[test]
    fn values_parse_and_bad_input_is_invalid_input() {
        assert_eq!(parse_value("7").unwrap(), Bson::Int32(7));
        assert_eq!(
            parse_value(r#""t1-1""#).unwrap(),
            Bson::String("t1-1".into())
        );
        assert_eq!(parse_value("null").unwrap(), Bson::Null);
        for bad in ["{", r#"{"$oid": "xyz"}"#, ""] {
            let err = parse_value(bad).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{bad}: {err}");
        }
        for bad in ["[1]", "3", r#"{"$oid": "65a000000000000000000001"}"#, "{"] {
            let err = parse_document(bad).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn shell_text_uses_the_shell_helpers() {
        use bson::spec::BinarySubtype;
        use bson::{Binary, DateTime, Regex, Timestamp};
        let regex = |pattern: &str, options: &str| Regex {
            pattern: pattern.try_into().unwrap(),
            options: options.try_into().unwrap(),
        };
        let oid = ObjectId::parse_str("65a000000000000000000001").unwrap();
        let uuid = Binary {
            subtype: BinarySubtype::Uuid,
            bytes: (0u8..16).collect(),
        };
        let bin = Binary {
            subtype: BinarySubtype::Generic,
            bytes: vec![0, 255, 1],
        };
        let ts = Timestamp {
            time: 3,
            increment: 4,
        };
        let price: Decimal128 = "12.50".parse().unwrap();
        let value = doc! {
            "i": 5_i32,
            "neg": -1_i32,
            "l": 7_i64,
            "f": 1.5,
            "whole": 2.0,
            "d": price,
            "s": "a\"b",
            "t": true,
            "n": null,
            "_id": oid,
            "at": DateTime::from_millis(1_767_225_600_000),
            "u": uuid,
            "b": bin,
            "ts": ts,
            "re": regex("^a/b", "i"),
            "x": regex("a b", "x"),
            "min": Bson::MinKey,
            "arr": [1_i32, "two"],
        };
        assert_eq!(
            to_shell_text(&Bson::Document(value)),
            concat!(
                r#"{"i":5,"neg":-1,"l":NumberLong("7"),"f":1.5,"whole":2.0,"#,
                r#""d":NumberDecimal("12.50"),"s":"a\"b","t":true,"n":null,"#,
                r#""_id":ObjectId("65a000000000000000000001"),"#,
                r#""at":ISODate("2026-01-01T00:00:00Z"),"#,
                r#""u":UUID("00010203-0405-0607-0809-0a0b0c0d0e0f"),"#,
                r#""b":BinData(0, "AP8B"),"ts":Timestamp(3, 4),"#,
                r#""re":/^a\/b/i,"x":{"$regex":"a b","$options":"x"},"#,
                r#""min":MinKey(),"arr":[1,"two"]}"#
            )
        );
        assert_eq!(to_shell_text(&Bson::Double(f64::NAN)), "NaN");
        assert_eq!(to_shell_text(&Bson::Double(f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(to_shell_text(&Bson::Double(1e300)), "1e300");
        assert_eq!(regex_literal(r"a\/b", ""), Some(r"/a\/b/".to_owned()));
        assert_eq!(regex_literal("", "i"), None);
        assert_eq!(regex_literal("a\\", ""), None, "a lone trailing backslash");
        assert_eq!(regex_literal("a\nb", ""), None);
        assert_eq!(
            to_shell_text(&Bson::Undefined),
            r#"EJSON.parse("{\"$undefined\":true}")"#
        );
    }

    #[test]
    fn query_documents_keep_the_regex_operator() {
        let filter = query_document(
            &json!({"name": {"$regex": "^a", "$options": "i"}, "n": {"$gt": {"$numberLong": "3"}}}),
            "filter",
        )
        .unwrap();
        assert_eq!(
            filter,
            doc! {"name": {"$regex": "^a", "$options": "i"}, "n": {"$gt": 3_i64}}
        );
        let err = query_document(&json!([1]), "sort").unwrap_err();
        assert!(err.to_string().contains("sort"), "{err}");
    }
}
