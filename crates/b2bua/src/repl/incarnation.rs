//! Call incarnations on the replication path. A `call_ref` can carry
//! successive calls (a retried INVITE reuses its Call-ID and From tag); the
//! `(p,b)` version vector orders versions of one of them only, so a write of
//! another call is compared by its incarnation first (`call::Call::incarnation`).

/// Whether a write of `incoming` is a call other than the `held` one on the
/// same `call_ref`: both are named and differ. An unnamed side is taken as
/// the held call, so writes that name no incarnation keep comparing by the
/// `(p,b)` vector alone.
pub(super) fn is_another_call(held: Option<&str>, incoming: Option<&str>) -> bool {
    matches!((held, incoming), (Some(held), Some(incoming)) if held != incoming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_two_named_and_different_incarnations_are_two_calls() {
        assert!(is_another_call(Some("r#1"), Some("r#2")));
        assert!(!is_another_call(Some("r#1"), Some("r#1")));
        assert!(!is_another_call(None, Some("r#1")));
        assert!(!is_another_call(Some("r#1"), None));
        assert!(!is_another_call(None, None));
    }
}
