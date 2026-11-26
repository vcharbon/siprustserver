//! The RFC 3261 §16.11 branch function: the one place a request becomes the
//! `branch` of the Via this stateless proxy pushes on it.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use sip_message::header::{fold_branch, BRANCH_MAGIC_COOKIE};
use sip_message::SipRequest;

/// The Via branch for a forward of `req`, whose top Via carries the RFC 3261
/// branch `received` (§8.1.1.7; the wire parser refuses a top Via without the
/// magic cookie, so §16.11's first input set is the only one): a function of
/// the message, so a retransmission, and the CANCEL and non-2xx ACK for it,
/// map to the branch its first forward carried, at any instance of this proxy,
/// remembering nothing.
///
/// `received` is hashed with the received sent-by, Call-ID, From tag and CSeq
/// number: one branch token from two senders (two transactions, §17.2.3), or
/// reused by one upstream across a restart, maps to two branches. `received`
/// is folded to lower case (§7.3.1), so a re-cased one maps to the same
/// forward.
///
/// The branch is `z9hG4bK` and 16 hex digits of a SHA-256 digest, the same in
/// every process, build and instance.
pub(crate) fn stateless_branch(req: &SipRequest, received: &str) -> String {
    let mut hasher = Sha256::new();
    fold_branch(received, |chunk| hasher.update(chunk));
    hasher.update([0u8]);
    let _ = req.top_via().sent_by_ref().write_canonical(&mut HashText(&mut hasher));
    hasher.update([0u8]);
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
/// NUL, so no two distinct inputs hash the same bytes.
fn field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.as_bytes());
    hasher.update([0u8]);
}
