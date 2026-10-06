//! The failure image slot: the `Call.ext` key that holds the relayable
//! header image of the `/call/failure` consult in flight, and the merge that
//! clears it.

/// The `Call.ext` slot holding the relayable header image of the
/// `/call/failure` consult in flight. The core folds it into the a-facing
/// final that answers that consult; every consult restates it.
pub const RELAYED_FAILURE_HEADERS_EXT: &str = "relayed-failure-headers";

/// The `Call.ext` merge a consult with no peer final states with its
/// `FailureAsyncHttp`: it clears the image, so an earlier failure's headers
/// never answer this consult.
pub fn no_failure_image() -> call::ExtMap {
    let mut ext = call::ExtMap::new();
    ext.insert(RELAYED_FAILURE_HEADERS_EXT.to_string(), serde_json::Value::Null);
    ext
}
