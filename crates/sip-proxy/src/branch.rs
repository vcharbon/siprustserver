//! The RFC 3261 §16.11 branch function: the one place a request becomes the
//! `branch` of the Via this stateless proxy pushes on it.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use sip_message::header::{fold_branch, has_magic_cookie, HeaderValue, BRANCH_MAGIC_COOKIE};
use sip_message::SipRequest;

/// Domain tags keeping §16.11's two input sets in disjoint hash spaces.
const COOKIED_VIA: &str = "b";
const BRANCHLESS_VIA: &str = "v";

/// The Via branch for a forward of `req` (RFC 3261 §16.11): a function of the
/// message, so a retransmission of a request with a cookied branch, and the
/// CANCEL and non-2xx ACK for it, map to the branch its first forward carried,
/// at any instance of this proxy, remembering nothing.
///
/// A top Via without the magic cookie takes §16.11's alternate input set: the
/// Via, the To tag, the Request-URI, Call-ID, From tag and CSeq number. Its
/// non-2xx ACK, carrying the final's To tag, maps apart from its INVITE, as
/// §16.11 states the set.
///
/// A cookied branch is hashed with the received sent-by, Call-ID, From tag and
/// CSeq number: one branch token from two senders (two transactions, §17.2.3),
/// or reused by one upstream across a restart, maps to two branches. The
/// received branch is folded to lower case (§7.3.1), so a re-cased one maps to
/// the same forward.
///
/// The branch is `z9hG4bK` and 16 hex digits of a SHA-256 digest, the same in
/// every process, build and instance.
pub(crate) fn stateless_branch(req: &SipRequest) -> String {
    let mut hasher = Sha256::new();
    let top = req.top_via();
    match top.branch().filter(|b| has_magic_cookie(b)) {
        Some(received) => {
            field(&mut hasher, COOKIED_VIA);
            fold_branch(received, |chunk| hasher.update(chunk));
            hasher.update([0u8]);
            let _ = top.sent_by_ref().write_canonical(&mut HashText(&mut hasher));
            hasher.update([0u8]);
        }
        None => {
            field(&mut hasher, BRANCHLESS_VIA);
            field(&mut hasher, &top.to_wire());
            field(&mut hasher, req.to().tag().unwrap_or_default());
            field(&mut hasher, &req.request_uri().to_string());
        }
    }
    field(&mut hasher, req.call_id().as_str());
    field(&mut hasher, req.from().tag().unwrap_or_default());
    field(&mut hasher, &req.cseq().seq().to_string());

    let digest = hasher.finalize();
    let mut branch = String::with_capacity(BRANCH_MAGIC_COOKIE.len() + 16);
    branch.push_str(BRANCH_MAGIC_COOKIE);
    for byte in digest.iter().take(8) {
        let _ = write!(branch, "{byte:02x}");
    }
    branch
}

struct HashText<'a>(&'a mut Sha256);

impl std::fmt::Write for HashText<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.update(s.as_bytes());
        Ok(())
    }
}

/// Absorb one input field, NUL-terminated: no value fed here can contain a
/// NUL, so no two distinct input sets hash the same bytes.
fn field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.as_bytes());
    hasher.update([0u8]);
}
