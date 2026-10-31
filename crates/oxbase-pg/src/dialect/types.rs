//! Engine-neutral [`DataType`] of a column from its PostgreSQL type text (`format_type`).

use oxbase_types::DataType;

/// Maps the type text `format_type(atttypid, atttypmod)` prints (`integer`,
/// `character varying(20)`, `timestamp(3) with time zone`, `numeric(12,2)`, `text[]`) to a
/// [`DataType`]. Domains, enums, composites, schema-qualified and quoted names and every type
/// without a neutral equivalent become [`DataType::Other`] with the text unchanged.
pub(crate) fn data_type(native: &str) -> DataType {
    let native = native.trim();
    if let Some(element) = native.strip_suffix("[]") {
        return DataType::Array {
            element: Box::new(data_type(element)),
        };
    }
    let (base, args) = split_modifier(native);
    let first = args.first().copied();
    let precision = first.and_then(|n| u8::try_from(n).ok());
    match base.as_str() {
        "boolean" => DataType::Bool,
        "smallint" => DataType::INT16,
        "integer" => DataType::INT32,
        "bigint" => DataType::INT64,
        "real" => DataType::Float { bits: 32 },
        "double precision" => DataType::FLOAT64,
        "numeric" => DataType::Decimal {
            precision: first,
            // `numeric(p)` means scale 0.
            scale: match (first, args.get(1)) {
                (_, Some(scale)) => Some(*scale),
                (Some(_), None) => Some(0),
                (None, None) => None,
            },
        },
        "text" => DataType::Text,
        "character varying" => DataType::VarChar { length: first },
        "character" | "bpchar" => DataType::Char { length: first },
        "bytea" => DataType::VarBinary { length: None },
        "date" => DataType::Date,
        "time without time zone" => DataType::Time { precision },
        "timestamp without time zone" => DataType::Timestamp { precision },
        "timestamp with time zone" => DataType::TimestampTz { precision },
        "uuid" => DataType::Uuid,
        "json" => DataType::Json { binary: false },
        "jsonb" => DataType::Json { binary: true },
        b if b == "interval" || b.starts_with("interval ") => DataType::Interval,
        _ => DataType::Other {
            name: native.to_owned(),
        },
    }
}

/// Splits `timestamp(3) with time zone` into (`timestamp with time zone`, `[3]`) and
/// `numeric(12,2)` into (`numeric`, `[12, 2]`). Text without a modifier, or with one that is not
/// a list of numbers, is returned unchanged with no arguments.
fn split_modifier(native: &str) -> (String, Vec<u32>) {
    let (Some(open), Some(close)) = (native.find('('), native.find(')')) else {
        return (native.to_owned(), Vec::new());
    };
    if close < open || native.starts_with('"') {
        return (native.to_owned(), Vec::new());
    }
    let args: Option<Vec<u32>> = native[open + 1..close]
        .split(',')
        .map(|a| a.trim().parse().ok())
        .collect();
    let Some(args) = args else {
        return (native.to_owned(), Vec::new());
    };
    let base = format!("{}{}", &native[..open], &native[close + 1..]);
    (base.trim().to_owned(), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other(name: &str) -> DataType {
        DataType::Other { name: name.into() }
    }

    #[test]
    fn scalar_types() {
        assert_eq!(data_type("integer"), DataType::INT32);
        assert_eq!(data_type("smallint"), DataType::INT16);
        assert_eq!(data_type("bigint"), DataType::INT64);
        assert_eq!(data_type("boolean"), DataType::Bool);
        assert_eq!(data_type("real"), DataType::Float { bits: 32 });
        assert_eq!(data_type("double precision"), DataType::FLOAT64);
        assert_eq!(data_type("text"), DataType::Text);
        assert_eq!(
            data_type("character varying(20)"),
            DataType::VarChar { length: Some(20) }
        );
        assert_eq!(
            data_type("character varying"),
            DataType::VarChar { length: None }
        );
        assert_eq!(
            data_type("character(1)"),
            DataType::Char { length: Some(1) }
        );
        assert_eq!(data_type("bpchar"), DataType::Char { length: None });
        assert_eq!(data_type("bytea"), DataType::VarBinary { length: None });
        assert_eq!(data_type("uuid"), DataType::Uuid);
        assert_eq!(data_type("jsonb"), DataType::Json { binary: true });
        assert_eq!(data_type("json"), DataType::Json { binary: false });
    }

    #[test]
    fn numeric_and_temporal_modifiers() {
        assert_eq!(
            data_type("numeric(12,2)"),
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            }
        );
        assert_eq!(
            data_type("numeric(7)"),
            DataType::Decimal {
                precision: Some(7),
                scale: Some(0)
            }
        );
        assert_eq!(
            data_type("numeric"),
            DataType::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(data_type("date"), DataType::Date);
        assert_eq!(
            data_type("timestamp without time zone"),
            DataType::Timestamp { precision: None }
        );
        assert_eq!(
            data_type("timestamp(3) with time zone"),
            DataType::TimestampTz { precision: Some(3) }
        );
        assert_eq!(
            data_type("time(0) without time zone"),
            DataType::Time { precision: Some(0) }
        );
        assert_eq!(data_type("interval"), DataType::Interval);
        assert_eq!(data_type("interval year to month"), DataType::Interval);
        assert_eq!(
            data_type("time with time zone"),
            other("time with time zone")
        );
    }

    #[test]
    fn arrays_and_unknown_types() {
        assert_eq!(
            data_type("integer[]"),
            DataType::Array {
                element: Box::new(DataType::INT32)
            }
        );
        assert_eq!(
            data_type("character varying(5)[]"),
            DataType::Array {
                element: Box::new(DataType::VarChar { length: Some(5) })
            }
        );
        assert_eq!(data_type("m2.mood"), other("m2.mood"));
        assert_eq!(data_type("\"Weird(1)\""), other("\"Weird(1)\""));
        assert_eq!(
            data_type("geometry(Point,4326)"),
            other("geometry(Point,4326)")
        );
        assert_eq!(data_type("bit(3)"), other("bit(3)"));
    }
}
