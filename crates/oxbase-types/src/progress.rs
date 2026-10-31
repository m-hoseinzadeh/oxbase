//! [`Progress`]: one progress event of a long-running operation (PLAN §1, rule 4).
//!
//! Operations report progress through a channel (`oxbase_driver::ProgressSink`), never through a
//! return value, so the UI and `oxbase-cli` can show it while the operation runs. This type is the
//! payload: plain data, serializable so it can cross the Tauri boundary as JSON.

use serde::{Deserialize, Serialize};

/// One progress event.
///
/// `done` and `total` count whatever unit the operation works in (rows, bytes, objects); `unit`
/// names it for display. Events of one operation are monotonic in `done`, but a consumer must not
/// rely on seeing every event: sinks may drop events when the consumer is slow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// What the operation is doing right now ("listing tables", "importing orders.csv").
    pub stage: String,
    /// Units finished so far.
    pub done: u64,
    /// Total units, when known up front.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Unit of `done` / `total`.
    #[serde(default)]
    pub unit: ProgressUnit,
    /// Optional detail line (current object name, warning text, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What [`Progress::done`] and [`Progress::total`] count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    /// Generic steps (the default).
    #[default]
    Items,
    Rows,
    Bytes,
    Objects,
}

impl Progress {
    /// An event at `done` units of `stage`, with an unknown total.
    pub fn new(stage: impl Into<String>, done: u64) -> Self {
        Self {
            stage: stage.into(),
            done,
            total: None,
            unit: ProgressUnit::Items,
            message: None,
        }
    }

    /// Sets the known total.
    #[must_use]
    pub fn with_total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    /// Sets the unit.
    #[must_use]
    pub fn with_unit(mut self, unit: ProgressUnit) -> Self {
        self.unit = unit;
        self
    }

    /// Sets the detail line.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Completed fraction in `0.0..=1.0`, or `None` when the total is unknown. A zero total
    /// counts as complete.
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total?;
        if total == 0 {
            return Some(1.0);
        }
        Some((self.done.min(total) as f64) / (total as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction() {
        assert_eq!(Progress::new("x", 5).fraction(), None);
        assert_eq!(Progress::new("x", 5).with_total(10).fraction(), Some(0.5));
        assert_eq!(Progress::new("x", 50).with_total(10).fraction(), Some(1.0));
        assert_eq!(Progress::new("x", 0).with_total(0).fraction(), Some(1.0));
    }

    #[test]
    fn serde_round_trip_and_defaults() {
        let p = Progress::new("listing tables", 120)
            .with_total(5000)
            .with_unit(ProgressUnit::Objects)
            .with_message("public.orders");
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            json,
            r#"{"stage":"listing tables","done":120,"total":5000,"unit":"objects","message":"public.orders"}"#
        );
        assert_eq!(serde_json::from_str::<Progress>(&json).unwrap(), p);

        let minimal: Progress = serde_json::from_str(r#"{"stage":"s","done":1}"#).unwrap();
        assert_eq!(minimal, Progress::new("s", 1));
    }
}
