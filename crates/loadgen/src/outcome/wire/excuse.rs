//! Which in-dialog failures a refusal the loss model's drops explain excuses:
//! the system under test relays a UA's refusal to the other side of the call,
//! under that side's own Call-ID and CSeq.

use super::WireFailure;

/// A refusal a UA of the call gave a request the loss model's drops explain:
/// the refusing party's callee leg (`None`: the caller), the request method
/// and the status.
pub type Refusal = (Option<String>, String, u16);

/// Which of `side`'s `failures` stay (`side`: the callee leg's role, `None`:
/// the caller): each refusal another party gave excuses at most one failure
/// of its method and status; a refusal never excuses one on its own party's
/// side.
pub(super) fn kept(
    failures: &[WireFailure],
    side: Option<&str>,
    refusals: &[Refusal],
) -> Vec<bool> {
    let mut kept = vec![true; failures.len()];
    for (_, method, code) in refusals.iter().filter(|(by, _, _)| by.as_deref() != side) {
        let relayed = failures
            .iter()
            .zip(&kept)
            .position(|(f, k)| *k && f.method == *method && f.code == Some(*code));
        if let Some(i) = relayed {
            kept[i] = false;
        }
    }
    kept
}

/// `failures` less those `kept` marks excused.
pub(super) fn retain(failures: &mut Vec<WireFailure>, kept: &[bool]) {
    let mut marks = kept.iter();
    failures.retain(|_| marks.next().copied().unwrap_or(true));
}
