//! Engine -> driver lookup.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use oxbase_types::{Engine, Error, Result};

use crate::{ConnectParams, Connection, Driver};

/// Maps each [`Engine`] to the driver that serves it.
///
/// Built once at startup by the app or CLI (each engine crate registers itself) and shared
/// read-only afterwards.
#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: HashMap<Engine, Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `driver` for its own [`Driver::engine`], returning the driver it replaced.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> Option<Arc<dyn Driver>> {
        let engine = driver.engine();
        self.register_as(engine, driver)
    }

    /// Registers `driver` for `engine`, which may differ from the driver's own engine (the
    /// MySQL driver also serves MariaDB). Returns the driver it replaced.
    pub fn register_as(
        &mut self,
        engine: Engine,
        driver: Arc<dyn Driver>,
    ) -> Option<Arc<dyn Driver>> {
        self.drivers.insert(engine, driver)
    }

    /// The driver for `engine`, if one is registered.
    pub fn get(&self, engine: Engine) -> Option<Arc<dyn Driver>> {
        self.drivers.get(&engine).cloned()
    }

    /// The driver for `engine`, or an `Unsupported` error naming the engine.
    pub fn require(&self, engine: Engine) -> Result<Arc<dyn Driver>> {
        self.get(engine).ok_or_else(|| {
            Error::unsupported(format!(
                "no driver registered for {}",
                engine.display_name()
            ))
        })
    }

    /// `true` if a driver is registered for `engine`.
    pub fn contains(&self, engine: Engine) -> bool {
        self.drivers.contains_key(&engine)
    }

    /// Registered engines, in [`Engine::ALL`] order.
    pub fn engines(&self) -> Vec<Engine> {
        Engine::ALL
            .into_iter()
            .filter(|e| self.drivers.contains_key(e))
            .collect()
    }

    /// Validates `params` and connects with the driver registered for `params.engine`.
    pub async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        params.validate()?;
        self.require(params.engine)?.connect(params).await
    }
}

impl fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("engines", &self.engines())
            .finish()
    }
}
