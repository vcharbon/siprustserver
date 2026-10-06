//! The call incarnation grammar: which of the successive calls one `call_ref`
//! carries a record, a timer or a message belongs to. An incarnation is
//! `{call_ref}#{mark}`, the mark minted once when the call is born; the mark
//! alone tells two calls of one `call_ref` apart, so it is what a message on
//! the wire carries beside the `call_ref` (ADR-0014).

/// The incarnation of the call born on `call_ref` under `mark`.
pub fn derive_incarnation(call_ref: &str, mark: &str) -> String {
    format!("{call_ref}#{mark}")
}

/// The mark of `incarnation`: the part after its last `#`, which no mark
/// contains. An incarnation with no `#` is all mark.
pub fn incarnation_mark(incarnation: &str) -> &str {
    incarnation.rsplit_once('#').map_or(incarnation, |(_, mark)| mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_round_trips_through_its_incarnation() {
        let call_ref = crate::derive_call_ref("w0", "cid@host", "ftag");
        let incarnation = derive_incarnation(&call_ref, "k3x9a2bq");
        assert_eq!(incarnation, "w0|cid@host|ftag#k3x9a2bq");
        assert_eq!(incarnation_mark(&incarnation), "k3x9a2bq");
        assert_eq!(derive_incarnation(&call_ref, incarnation_mark(&incarnation)), incarnation);
    }

    #[test]
    fn an_incarnation_with_no_separator_is_all_mark() {
        assert_eq!(incarnation_mark("k"), "k");
    }
}
