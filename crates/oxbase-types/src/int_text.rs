//! Serde adapter: `i128` written as a decimal string, read from a string or a JSON integer.
//!
//! Used for numbers that may not fit a JavaScript `number` (sequence bounds, identity options),
//! so the webview never rounds them. Use `#[serde(with = "crate::int_text")]` on an `i128` and
//! `#[serde(default, skip_serializing_if = "Option::is_none", with = "crate::int_text::opt")]`
//! on an `Option<i128>`.

use serde::{Deserialize, Deserializer, Serializer, de};

/// What may appear in JSON for one of these numbers.
#[derive(Deserialize)]
#[serde(untagged)]
enum Repr {
    Text(String),
    Int(i64),
    UInt(u64),
}

impl Repr {
    fn into_i128<E: de::Error>(self) -> Result<i128, E> {
        match self {
            Repr::Text(s) => s
                .trim()
                .parse::<i128>()
                .map_err(|_| E::custom(format!("'{s}' is not an integer"))),
            Repr::Int(n) => Ok(i128::from(n)),
            Repr::UInt(n) => Ok(i128::from(n)),
        }
    }
}

// serde's `with` protocol passes the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn serialize<S: Serializer>(value: &i128, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(value)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i128, D::Error> {
    Repr::deserialize(d)?.into_i128()
}

/// The same for `Option<i128>` (`null` reads as `None`).
pub mod opt {
    use super::Repr;
    use serde::{Deserialize, Deserializer, Serializer};

    // serde's `with` protocol passes the field by reference.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(value: &Option<i128>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.collect_str(v),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i128>, D::Error> {
        Option::<Repr>::deserialize(d)?
            .map(Repr::into_i128)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct T {
        #[serde(with = "crate::int_text")]
        a: i128,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::int_text::opt"
        )]
        b: Option<i128>,
    }

    #[test]
    fn writes_strings_and_reads_both_forms() {
        let t = T {
            a: i128::from(u64::MAX) + 1,
            b: Some(-3),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"a":"18446744073709551616","b":"-3"}"#);
        assert_eq!(serde_json::from_str::<T>(&json).unwrap(), t);
        let t: T = serde_json::from_str(r#"{"a":5,"b":null}"#).unwrap();
        assert_eq!(t, T { a: 5, b: None });
        let t: T = serde_json::from_str(r#"{"a":18446744073709551615}"#).unwrap();
        assert_eq!(t.a, i128::from(u64::MAX));
        assert!(serde_json::from_str::<T>(r#"{"a":"x"}"#).is_err());
        assert!(serde_json::from_str::<T>(r#"{"a":1.5}"#).is_err());
    }
}
