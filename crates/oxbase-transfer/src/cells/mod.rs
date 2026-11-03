//! The per-engine [`TypeMapper`](oxbase_dialect::TypeMapper) cells (`docs/m8-contracts.md`
//! §4.1), one module per SQL engine. Owner of the bodies marked `unimplemented!("m8:
//! transfer")`: the **transfer** stream.
//!
//! Each module documents its table: per logical type, the target spelling and the fidelity
//! with its reason. The facts come from the engines' public documentation (type ranges,
//! precision limits, time-zone handling); the golden tests per engine pair pin
//! [`crate::mapping::pair_report`] for [`crate::mapping::representative_types`].

pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;
