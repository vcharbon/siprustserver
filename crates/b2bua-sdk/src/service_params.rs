//! [`ServiceParams`]: the parameters a callflow service brings to the worker's
//! configuration, one value per type.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// The parameters of the services a worker composes, keyed by their type. A
/// service declares its own parameter type, the composition inserts one value
/// of it at startup, and the service's rules read it back through
/// `ctx.config.service_params`. The map holds what it is given: a value is
/// checked by whoever builds it, before it is inserted.
#[derive(Clone, Default)]
pub struct ServiceParams {
    values: BTreeMap<TypeId, Param>,
}

#[derive(Clone)]
struct Param {
    type_name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
}

impl ServiceParams {
    /// No parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds `value` as the parameters of type `T`, replacing any held before.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        let param = Param { type_name: std::any::type_name::<T>(), value: Arc::new(value) };
        self.values.insert(TypeId::of::<T>(), param);
    }

    /// The parameters of type `T`; `None` when none were inserted.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.values.get(&TypeId::of::<T>())?.value.downcast_ref::<T>()
    }
}

impl fmt::Debug for ServiceParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&str> = self.values.values().map(|p| p.type_name).collect();
        names.sort_unstable();
        f.debug_struct("ServiceParams").field("types", &names).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Pacing {
        connect_ms: i64,
    }

    #[derive(Debug, PartialEq)]
    struct Other(u8);

    #[test]
    fn a_value_reads_back_by_its_type_alone() {
        let mut params = ServiceParams::new();
        assert_eq!(params.get::<Pacing>(), None);
        params.insert(Pacing { connect_ms: 1500 });
        params.insert(Other(7));
        assert_eq!(params.get::<Pacing>(), Some(&Pacing { connect_ms: 1500 }));
        assert_eq!(params.get::<Other>(), Some(&Other(7)));
        assert_eq!(params.get::<u32>(), None);
    }

    #[test]
    fn a_second_insert_of_a_type_replaces_the_first() {
        let mut params = ServiceParams::new();
        params.insert(Pacing { connect_ms: 1 });
        params.insert(Pacing { connect_ms: 2 });
        assert_eq!(params.get::<Pacing>(), Some(&Pacing { connect_ms: 2 }));
    }

    #[test]
    fn a_clone_reads_the_same_values_and_debug_names_the_types() {
        let mut params = ServiceParams::new();
        params.insert(Other(3));
        let copy = params.clone();
        assert_eq!(copy.get::<Other>(), Some(&Other(3)));
        assert!(format!("{copy:?}").contains("Other"), "{copy:?}");
    }
}
