//! The JSON payload an internal fold carries: a typed payload serialized, and
//! the keys the stack sets on it.

use serde::Serialize;

/// Serialize a typed payload to the internal-event JSON. Payload structs are
/// program-constructed (no non-string keys, no non-finite floats), so failure
/// is unreachable; degrade to an empty object rather than kill the callout task.
pub(super) fn to_payload<T: Serialize>(p: T) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_else(|_| serde_json::Value::Object(Default::default()))
}

/// Set `key` of the object `payload` to `value`. Every fold payload is an
/// object, built here or by [`to_payload`].
pub(super) fn put(payload: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(object) = payload.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}
