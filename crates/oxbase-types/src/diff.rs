//! Differences between two object definitions.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ObjectRef, PropId, Value};

/// What kind of difference a [`PropertyDiff`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Present only in the target (`b`).
    Added,
    /// Present only in the source (`a`).
    Removed,
    /// Present in both with different values.
    Changed,
}

/// One difference found by a comparator between a source definition (`a`, `old`) and a target
/// definition (`b`, `new`).
///
/// `object` names the object the difference is on, which may be a child of the compared object
/// (a column of a compared table). `prop` is `None` when a whole object was added or removed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyDiff {
    pub object: ObjectRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prop: Option<PropId>,
    pub kind: DiffKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
}

impl PropertyDiff {
    /// A whole object exists only in the target.
    pub fn object_added(object: ObjectRef) -> Self {
        Self {
            object,
            prop: None,
            kind: DiffKind::Added,
            old: None,
            new: None,
        }
    }

    /// A whole object exists only in the source.
    pub fn object_removed(object: ObjectRef) -> Self {
        Self {
            object,
            prop: None,
            kind: DiffKind::Removed,
            old: None,
            new: None,
        }
    }

    /// A property differs. The kind is derived: `Added` when `old` is `None`, `Removed` when
    /// `new` is `None`, else `Changed`.
    pub fn property(
        object: ObjectRef,
        prop: PropId,
        old: Option<Value>,
        new: Option<Value>,
    ) -> Self {
        let kind = match (&old, &new) {
            (None, Some(_)) => DiffKind::Added,
            (Some(_), None) => DiffKind::Removed,
            _ => DiffKind::Changed,
        };
        Self {
            object,
            prop: Some(prop),
            kind,
            old,
            new,
        }
    }
}

impl fmt::Display for PropertyDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = match self.kind {
            DiffKind::Added => '+',
            DiffKind::Removed => '-',
            DiffKind::Changed => '~',
        };
        write!(f, "{sign} {}", self.object)?;
        if let Some(prop) = &self.prop {
            write!(f, " .{prop}")?;
        }
        match (&self.old, &self.new) {
            (Some(o), Some(n)) => write!(f, ": {o} -> {n}"),
            (Some(o), None) => write!(f, ": {o}"),
            (None, Some(n)) => write!(f, ": {n}"),
            (None, None) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectKind, props};
    use pretty_assertions::assert_eq;

    #[test]
    fn kinds_and_display() {
        let col = ObjectRef::table(None, None, "t").child(ObjectKind::Column, "c");
        let d = PropertyDiff::property(
            col.clone(),
            props::NULLABLE,
            Some(Value::Bool(true)),
            Some(Value::Bool(false)),
        );
        assert_eq!(d.kind, DiffKind::Changed);
        assert_eq!(d.to_string(), "~ column:t.c .nullable: true -> false");

        let d = PropertyDiff::property(col.clone(), props::DEFAULT, None, Some(Value::Int(0)));
        assert_eq!(d.kind, DiffKind::Added);

        let d = PropertyDiff::object_removed(col);
        assert_eq!(d.to_string(), "- column:t.c");

        let back: PropertyDiff = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }
}
