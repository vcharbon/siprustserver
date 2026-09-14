//! The RFC 3261 §16.11 branch function: the one place a request becomes the
//! `branch` of the Via this stateless proxy pushes on it.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use sip_message::header::{HeaderValue, BRANCH_MAGIC_COOKIE};
use sip_message::SipRequest;

/// Domain tags keeping §16.11's two input sets in disjoint hash spaces.
const COOKIED_VIA: &str = "b";
const BRANCHLESS_VIA: &str = "v";

/// The Via branch for a forward of `req` (RFC 3261 §16.11): a function of the
/// message, so a retransmission of a request and the CANCEL and non-2xx ACK
/// for it map to the branch its first forward carried — at any instance of
/// this proxy, remembering nothing.
pub(crate) fn stateless_branch(req: &SipRequest) -> String {
    let mut hasher = Sha256::new();
    let top = req.top_via();
    match top.branch().filter(|b| b.starts_with(BRANCH_MAGIC_COOKIE)) {
        // Call-ID, From tag and CSeq number ride beside the received branch
        // (fed below): identical across a request, its retransmissions, its
        // CANCEL and its non-2xx ACK, so §16.11's mapping holds — while an
        // upstream that spends one branch token twice (§8.1.1.7 violated
        // across a restart) still gets two transactions, not one merged.
        Some(received) => {
            field(&mut hasher, COOKIED_VIA);
            field(&mut hasher, received);
        }
        // §16.11's alternate input set for a pre-RFC-3261 upstream, which
        // carries no branch to derive from. Its non-2xx ACK hashes APART from
        // its INVITE, carrying the final's To tag — the RFC keeps that tag to
        // separate a 2xx ACK whose Request-URI equals the INVITE's, and the
        // recipe stands as stated.
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

    // `z9hG4bK` + 16 hex chars — the §8.1.1.7 shape every branch this stack
    // emits has. SHA-256 rather than a `std` hasher: the value must be the
    // same at another process, another build and another proxy instance.
    let digest = hasher.finalize();
    let mut branch = String::with_capacity(BRANCH_MAGIC_COOKIE.len() + 16);
    branch.push_str(BRANCH_MAGIC_COOKIE);
    for byte in &digest[..8] {
        let _ = write!(branch, "{byte:02x}");
    }
    branch
}

/// Absorb one input field, NUL-terminated: no value fed here can contain a
/// NUL, so no two distinct input sets hash the same bytes.
fn field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.as_bytes());
    hasher.update([0u8]);
}
