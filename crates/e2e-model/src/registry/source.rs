//! Shape sources: where the descriptors of a [`ShapeRegistry`] come from.
//!
//! A source is anything that yields [`ShapeDescriptor`]s at startup: a crate's
//! compiled shape table, a cross-product generator, or a directory of authored
//! documents interpreted into actor bodies. Every source registers through
//! [`ShapeRegistry::register_source`] into the one id space; a duplicate id is
//! an error naming the source that brought it.
//!
//! Descriptors hold `'static` ids and legs. A source that builds them from
//! runtime data (a document's shape name, its leg prefixes) interns them with
//! [`intern`] / [`intern_legs`]: one allocation per distinct value for the
//! process, however often a registry is rebuilt.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use super::{LegSpec, ShapeDescriptor, ShapeRegistry};

/// A producer of shape descriptors registered into a [`ShapeRegistry`].
pub trait ShapeSource {
    /// Where the shapes come from (a crate, a generator, a document directory),
    /// named in registration errors.
    fn origin(&self) -> String;

    /// Every descriptor this source contributes, or why it cannot produce them
    /// (an unreadable or invalid document).
    fn shapes(&self) -> Result<Vec<ShapeDescriptor>, String>;
}

impl ShapeRegistry {
    /// Register `shape`, refusing an id already registered.
    pub fn try_register(&mut self, shape: ShapeDescriptor) -> Result<&mut Self, String> {
        if self.shapes.contains_key(shape.id) {
            return Err(format!(
                "duplicate shape id {:?} in the registry (one id space)",
                shape.id
            ));
        }
        self.shapes.insert(shape.id.to_string(), shape);
        Ok(self)
    }

    /// Register every shape of `source`. Nothing is registered when the source
    /// fails or brings an id the registry already holds (or holds twice itself).
    pub fn register_source(&mut self, source: &dyn ShapeSource) -> Result<&mut Self, String> {
        let origin = source.origin();
        let shapes = source.shapes().map_err(|e| format!("shape source {origin}: {e}"))?;
        let mut seen = HashSet::new();
        for shape in &shapes {
            if self.shapes.contains_key(shape.id) || !seen.insert(shape.id) {
                return Err(format!(
                    "shape source {origin}: duplicate shape id {:?} (one id space)",
                    shape.id
                ));
            }
        }
        for shape in shapes {
            self.shapes.insert(shape.id.to_string(), shape);
        }
        Ok(self)
    }

    /// A registry holding exactly the shapes of `sources`, in order.
    pub fn from_sources(sources: &[&dyn ShapeSource]) -> Result<Self, String> {
        let mut reg = Self::empty();
        for source in sources {
            reg.register_source(*source)?;
        }
        Ok(reg)
    }
}

/// The process-wide `'static` copy of `s`: equal strings intern to the same
/// pointer, allocated once.
pub fn intern(s: &str) -> &'static str {
    static TABLE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut table = TABLE.get_or_init(Default::default).lock().unwrap();
    if let Some(&held) = table.get(s) {
        return held;
    }
    let held: &'static str = Box::leak(s.to_owned().into_boxed_str());
    table.insert(held);
    held
}

/// The process-wide `'static` copy of a leg list: equal lists intern to the
/// same slice, allocated once.
pub fn intern_legs(legs: &[LegSpec]) -> &'static [LegSpec] {
    static TABLE: OnceLock<Mutex<HashSet<&'static [LegSpec]>>> = OnceLock::new();
    let mut table = TABLE.get_or_init(Default::default).lock().unwrap();
    if let Some(&held) = table.get(legs) {
        return held;
    }
    let held: &'static [LegSpec] = Box::leak(legs.to_vec().into_boxed_slice());
    table.insert(held);
    held
}

/// The `'static` prefix list of a leg built from runtime data.
pub fn intern_prefixes(prefixes: &[&str]) -> &'static [&'static str] {
    static TABLE: OnceLock<Mutex<HashSet<&'static [&'static str]>>> = OnceLock::new();
    let interned: Vec<&'static str> = prefixes.iter().map(|p| intern(p)).collect();
    let mut table = TABLE.get_or_init(Default::default).lock().unwrap();
    if let Some(&held) = table.get(interned.as_slice()) {
        return held;
    }
    let held: &'static [&'static str] = Box::leak(interned.into_boxed_slice());
    table.insert(held);
    held
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
