//! **TEST-ONLY.** RFC 3261 audit rules over the recorded signaling layer.
//!
//! These run at layer close (when wired into [`ScopedAuditOptions`](crate::ScopedAuditOptions))
//! or directly over a channel snapshot. They flag on-wire protocol invariants
//! that a real UAC/UAS enforces but the test UAs (which answer whatever they are
//! handed, regardless of CSeq) do not — so the recording itself, not the per-step
//! `expect`, becomes the place those invariants are checked. Wiring them into the
//! default options gives every harness the same "post-run all-clean" CSeq check
//! that the live SIPp endpoints apply in endurance.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::{extract_tag, get_header, get_headers, parse_via_params};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};

/// Per-request-stream state for the in-dialog CSeq audit. A *stream* is one
/// `(receiving endpoint, Call-ID, From-tag)` — one originating UA's request flow
/// in one direction. Within a stream each **dialog** is tracked separately by its
/// To-tag (`""` for the dialog-creating request that has no To-tag yet): forking
/// materialises several dialogs that share a Call-ID + From-tag but diverge by
/// To-tag, and each owns an independent CSeq space.
#[derive(Default)]
struct StreamState {
    /// Transactions already observed, keyed by `(top-Via branch, method, CSeq)`,
    /// to fold away on-wire retransmissions. A genuine retransmission repeats all
    /// three; method + CSeq are part of the key (not the branch alone) because the
    /// simulated fabric's per-worker `IdGen` resets on a failover restart, so a
    /// *different* request relayed by the backup can reuse a branch the crashed
    /// primary spent — keying on the branch alone would mis-skip it as a phantom
    /// retransmit (RFC 3261 assumes globally-unique branches, §8.1.1.7).
    seen_txns: HashSet<(String, String, u32)>,
    /// Per dialog (To-tag, `""` = dialog-creating) → its accumulated CSeq set.
    dialogs: HashMap<String, DialogCseqs>,
}

/// One dialog's worth of new-request CSeq accounting, gathered **order-
/// independently**. §12.2.1.1 is a property of the SET of sequence numbers a
/// dialog's UAC emitted, not of the order they were *recorded* in: a request
/// dropped and later recovered by re-emission (Timer A/E/G) legitimately arrives
/// out of order — after a request the UAC sent *later* — yet the UAC still
/// incremented by exactly one and left no hole. So we accumulate the values as
/// they arrive and judge contiguity + reuse once, over the whole set, rather than
/// arrival-to-arrival (which would charge every recovered-after-drop reordering as
/// a bug — the TEST-MODEL false positive this rule used to produce).
#[derive(Default)]
struct DialogCseqs {
    /// Distinct new-transaction CSeq → `(top-Via branch, method)` of the FIRST new
    /// transaction that carried it. A `BTreeMap`, so the keys come out ascending
    /// for the contiguity scan. A *later* new transaction with a **different**
    /// branch at the same CSeq is a genuine reuse — the dialog CSeq never advanced.
    by_cseq: BTreeMap<u32, (String, String)>,
    /// Reuse violations, in arrival order: `(reusing method, CSeq)`.
    reuses: Vec<(String, u32)>,
}

/// **RFC 3261 §12.2.1.1 — in-dialog request sequencing.** Within a dialog the
/// UAC MUST increment the CSeq sequence number by **exactly one** for each new
/// request (ACK and CANCEL excepted — they reuse the CSeq of the request they
/// acknowledge/cancel). So the sequence numbers a dialog's UAC emits form a
/// **contiguous run with no value used twice and none skipped**. That is what
/// this rule checks, and it does so over the whole recorded SET per dialog — it
/// is deliberately **independent of arrival order**. An on-wire retransmission
/// reuses both its CSeq *and* its top-Via branch — folded away as one transaction.
///
/// **Why order is not judged.** §12.2.1.1 constrains what the UAC *generates*,
/// not the order the recording *observes*. Over a lossy fabric a request can be
/// dropped and then recovered by re-emission (Timer A/E/G); its recovered copy
/// legitimately lands *after* a request the UAC sent later (e.g. a REFER-progress
/// NOTIFY at CSeq 3 recovered after the BYE at CSeq 4). The UAC still incremented
/// by exactly one and left no hole — the set is `{…,3,4}`, contiguous. Charging
/// that reordering as "out of order" is a TEST-MODEL false positive (the SUT
/// emitted a contiguous stream, the harness merely saw it recovered late), so it
/// is not flagged. Only two things flag: a **reuse** (a CSeq carried by two
/// distinct new transactions on one dialog) and a **gap** (a sequence value the
/// UAC skipped, leaving the sorted set non-contiguous).
///
/// **Per-dialog, not per-leg.** The check keys each stream by `(receiving
/// endpoint, Call-ID, From-tag)` and then tracks each dialog **by To-tag**. This
/// matters under forking (RFC 3261 §12.1.2 / §13.2.2.4): one INVITE creates
/// several early dialogs that share Call-ID + From-tag but each carry a distinct
/// callee To-tag, and each maintains its OWN CSeq sequence seeded from the
/// INVITE. So two forks' first PRACKs both at `INVITE_CSeq + 1` is **correct**
/// (distinct dialogs), not a reuse — conflating them by ignoring the To-tag would
/// be a false positive. The dialog-creating INVITE (no To-tag, tracked under
/// `""`) is the baseline: a forked/confirmed dialog's first in-dialog request
/// must be `INVITE_CSeq + 1` (folded in as the set's lower anchor), and that
/// To-tag's sequence then advances by one per request.
///
/// Within a stream it distinguishes a *retransmission* from a *new transaction*
/// by the top (first) `Via` header's `branch=` token (a retransmission reuses
/// it). This is the teeth for a keepalive loop that never increments the dialog
/// CSeq (each new OPTIONS, and the eventual BYE, reuses the previous request's
/// CSeq → a reuse) and for a takeover that re-originates a dialog request from a
/// stale (pre-failover) CSeq snapshot — the survivor mints a `local_cseq + 1`
/// the dialog already spent, so the value shows up twice (a reuse). Both are
/// invisible to a test UA that answers whatever it is handed.
pub struct CSeqInDialogOrderRule;

/// The `branch=` token of the TOP (first) `Via` header, if present and
/// non-empty. A retransmission reuses this exact token; a new client
/// transaction mints a fresh one.
fn top_via_branch(req: &sip_message::SipRequest) -> Option<String> {
    let top = get_headers(&req.headers, "via").into_iter().next()?;
    parse_via_params(top).branch.filter(|b| !b.is_empty())
}

impl CrossMessageAuditRule for CSeqInDialogOrderRule {
    fn name(&self) -> &'static str {
        "rfc3261.cseqInDialogOrder"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        // (receiving bind, Call-ID, From-tag) -> stream state, plus first-seen
        // order so findings come out deterministically (HashMap order is not).
        let mut streams: HashMap<(LaneKey, String, String), StreamState> = HashMap::new();
        let mut stream_order: Vec<(LaneKey, String, String)> = Vec::new();
        let parser = CustomParser::new();

        // ── Pass 1: accumulate each dialog's new-transaction CSeq set. ──────────
        for s in events {
            let SignalingNetworkEvent::RecvItem { bind_key, packet, .. } = &s.event else {
                continue;
            };
            let Ok(SipMessage::Request(req)) = parser.parse(&packet.raw) else {
                continue; // responses / unparseable: not our concern here
            };
            let (Some(call_id), Some(from_tag)) = (
                get_header(&req.headers, "call-id").map(str::to_string),
                get_header(&req.headers, "from").and_then(extract_tag),
            ) else {
                continue;
            };
            let key = (bind_key.clone(), call_id, from_tag);
            if !streams.contains_key(&key) {
                stream_order.push(key.clone());
                streams.insert(key.clone(), StreamState::default());
            }
            let st = streams.get_mut(&key).unwrap();
            let seq = req.cseq.seq;
            let method = req.method.to_string();
            let branch = top_via_branch(&req);

            // 1. A repeat of the SAME (branch, method, CSeq) is a retransmission
            //    of a transaction we already accounted for: skip entirely. Method +
            //    CSeq guard against an `IdGen`-reset branch collision masquerading
            //    as a retransmit (see `seen_txns`).
            if let Some(b) = &branch {
                if !st.seen_txns.insert((b.clone(), method.clone(), seq)) {
                    continue;
                }
            }

            // 2. ACK/CANCEL legitimately reuse the related request's CSeq: exempt,
            //    and they do not advance the per-dialog sequence.
            if method.eq_ignore_ascii_case("ACK") || method.eq_ignore_ascii_case("CANCEL") {
                continue;
            }

            // 3. A genuinely new in-dialog request. The dialog it belongs to is
            //    identified by the To-tag (`""` for the dialog-creating request
            //    that has none yet); each dialog owns an independent CSeq space. We
            //    only ACCUMULATE here — contiguity is judged below, over the whole
            //    set, because arrival order is irrelevant to §12.2.1.1.
            let to_tag = req.to.tag.clone().unwrap_or_default();
            let branch = branch.unwrap_or_default();
            let dlg = st.dialogs.entry(to_tag).or_default();
            match dlg.by_cseq.get(&seq).map(|(b, _)| b.clone()) {
                None => {
                    dlg.by_cseq.insert(seq, (branch, method));
                }
                Some(first_branch) if first_branch != branch => {
                    // A *different* transaction carried a CSeq the dialog already
                    // spent → the dialog CSeq failed to increment (a reuse).
                    dlg.reuses.push((method, seq));
                }
                Some(_) => {} // same branch/CSeq shape: nothing new to record.
            }
        }

        // ── Pass 2: judge each dialog's CSeq set — reuse + non-contiguity. ──────
        let mut findings = Vec::new();
        for key in &stream_order {
            let st = &streams[key];
            // Every dialog-CREATING request (empty To-tag) on this stream. For a
            // plain call that is the sole INVITE; but under a deferred-auth §22.2
            // retry it holds BOTH attempts — the challenged one (CSeq n, its 401
            // established no dialog) AND the resent one (CSeq n+1) that actually
            // minted the confirmed dialog. Each forked/confirmed dialog anchors on
            // the attempt that established IT (chosen per To-tag below), not the
            // lowest attempt in the bucket.
            let creating_cseqs: Vec<u32> =
                st.dialogs.get("").map(|d| d.by_cseq.keys().copied().collect()).unwrap_or_default();
            let mut to_tags: Vec<&String> = st.dialogs.keys().collect();
            to_tags.sort();
            for to_tag in to_tags {
                let dlg = &st.dialogs[to_tag];

                // Reuse: a CSeq spent by two distinct transactions on this dialog.
                for (method, seq) in &dlg.reuses {
                    findings.push((
                        key.0.clone(),
                        cseq_reuse_msg(method, *seq, &key.1, &key.2, to_tag),
                    ));
                }

                // Contiguity: the sorted distinct CSeqs must have no interior hole.
                // A forked/confirmed dialog folds in the dialog-creating INVITE that
                // ESTABLISHED IT as the lower anchor — the largest empty-To-tag
                // attempt CSeq ≤ this dialog's first in-dialog request. For a plain
                // single-attempt call that is the sole INVITE CSeq (unchanged); for
                // an auth retry it is the resent, higher CSeq — NOT the challenged
                // first attempt, whose abandoned CSeq would fabricate a gap. A first
                // request that is not `anchor + 1` still surfaces as a real gap.
                let mut seqs: Vec<u32> = dlg.by_cseq.keys().copied().collect();
                if !to_tag.is_empty() {
                    if let Some(&first) = seqs.first() {
                        // The dialog-creating request this dialog advanced FROM: the
                        // largest empty-To-tag attempt ≤ this dialog's first in-dialog
                        // request. If the first request is at or below EVERY recorded
                        // attempt, fall back to the smallest so a regression is still
                        // measured against a real anchor rather than escaping unjudged.
                        let anchor = creating_cseqs
                            .iter()
                            .copied()
                            .filter(|&c| c <= first)
                            .max()
                            .or_else(|| creating_cseqs.iter().copied().min());
                        if let Some(anchor) = anchor {
                            if first <= anchor {
                                // The first in-dialog request did NOT advance past the
                                // dialog-creating CSeq (§12.2.1.1): it reuses the
                                // INVITE's own number — a cross-dialog reuse the
                                // per-To-tag bucketing otherwise hides — or regresses
                                // below it (a CSeq that went backwards; CSeq 0 always).
                                findings.push((
                                    key.0.clone(),
                                    cseq_below_anchor_msg(
                                        dlg.by_cseq[&first].1.as_str(),
                                        first,
                                        anchor,
                                        &key.1,
                                        &key.2,
                                        to_tag,
                                    ),
                                ));
                            } else if let Err(pos) = seqs.binary_search(&anchor) {
                                seqs.insert(pos, anchor);
                            }
                        }
                    }
                }
                for w in seqs.windows(2) {
                    let (lo, hi) = (w[0], w[1]);
                    if hi > lo + 1 {
                        let method =
                            dlg.by_cseq.get(&hi).map(|(_, m)| m.as_str()).unwrap_or("request");
                        findings.push((
                            key.0.clone(),
                            cseq_gap_msg(method, hi, lo, &key.1, &key.2, to_tag),
                        ));
                    }
                }
            }
        }
        findings
    }
}

/// The `Call-ID=… from-tag=… to-tag=…` dialog descriptor shared by the messages.
fn dialog_desc(call_id: &str, from_tag: &str, to_tag: &str) -> String {
    format!(
        "Call-ID={call_id} from-tag={from_tag} to-tag={}",
        if to_tag.is_empty() { "<none>" } else { to_tag },
    )
}

/// Phrase a §12.2.1.1 **reuse**: `{method} CSeq {seq}` was already carried by an
/// earlier transaction on this dialog. A new in-dialog request MUST increment the
/// dialog CSeq by exactly one, so a repeat means the CSeq never advanced — a real
/// UAS drops it as a retransmission.
fn cseq_reuse_msg(method: &str, seq: u32, call_id: &str, from_tag: &str, to_tag: &str) -> String {
    let dialog = dialog_desc(call_id, from_tag, to_tag);
    format!(
        "in-dialog CSeq reused (RFC 3261 §12.2.1.1): {method} CSeq {seq} reuses a prior \
         request's CSeq (a new in-dialog transaction must increment the dialog CSeq by \
         exactly one) on {dialog} — a real UAS treats this as a retransmission and drops the \
         new request (the test UA answers it, hiding the bug)"
    )
}

/// Phrase a §12.2.1.1 **gap**: once the dialog's requests are folded (retransmits)
/// and re-ordered by CSeq, a value between `prev` and `seq` was never emitted — the
/// UAC skipped a sequence number (incremented by more than one). Arrival order is
/// deliberately not judged: a request dropped and recovered by re-emission arrives
/// late but leaves no hole, so only a genuinely MISSING number reaches here.
fn cseq_gap_msg(
    method: &str,
    seq: u32,
    prev: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
) -> String {
    let dialog = dialog_desc(call_id, from_tag, to_tag);
    format!(
        "in-dialog CSeq not contiguous (RFC 3261 §12.2.1.1): {method} CSeq {seq} skips ahead \
         of CSeq {prev}; the UAC MUST increment the dialog CSeq by exactly one (expected \
         {}) on {dialog}",
        prev + 1,
    )
}

/// Phrase a §12.2.1.1 **below-anchor** violation: a dialog's FIRST in-dialog
/// request did not advance past the dialog-creating request's CSeq. Either it
/// reuses the INVITE's own sequence number — a reuse the per-To-tag bucketing
/// hides because the tag-less INVITE and the tagged in-dialog request never
/// share a bucket — or it regressed below it (a CSeq that went backwards; CSeq 0
/// is always below a ≥1 anchor). A confirmed/forked dialog's first request MUST
/// be exactly `anchor + 1`.
fn cseq_below_anchor_msg(
    method: &str,
    seq: u32,
    anchor: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
) -> String {
    let dialog = dialog_desc(call_id, from_tag, to_tag);
    format!(
        "in-dialog CSeq did not advance (RFC 3261 §12.2.1.1): {method} CSeq {seq} does not \
         exceed the dialog-creating CSeq {anchor} on {dialog}; the first in-dialog request MUST \
         increment the dialog CSeq by exactly one (expected {}) — a value at or below the \
         INVITE's own CSeq reuses or regresses it, which a real UAS rejects (the test UA answers \
         it, hiding the bug)",
        anchor + 1,
    )
}

/// Top (first) `Via` branch token from a raw header list — the transaction
/// identifier a response echoes from the request it answers (RFC 3261 §8.1.1.7,
/// §17). Works for both requests and responses.
fn top_via_branch_headers(headers: &[sip_message::SipHeader]) -> Option<String> {
    let top = get_headers(headers, "via").into_iter().next()?;
    parse_via_params(top).branch.filter(|b| !b.is_empty())
}

/// **RFC 3261 §8.1.3.5 / §17 — a response's CSeq MUST equal its request's.** A
/// response is matched to the client transaction by the topmost `Via` branch,
/// and §8.1.3.5 requires the response to copy the request's `CSeq` (sequence
/// number *and* method) verbatim. So for every transaction (keyed by that
/// branch) the responses' `(CSeq number, method)` must be one the requests on
/// that branch actually carried.
///
/// This is the teeth for a B2BUA that, failing to correlate an in-dialog
/// response to its pending request (e.g. a forked early dialog whose PRACK/UPDATE
/// 200 was looked up on the wrong fork), regenerates it on the *INVITE* server
/// transaction — emitting a spurious `200 (INVITE)` carrying the PRACK/UPDATE's
/// CSeq number on the INVITE's branch. A real UAC discards a response whose CSeq
/// does not match the transaction it sent (the test UA accepts whatever 200 it
/// sees, hiding the corruption). Conservative: a branch whose *request* was never
/// observed is skipped (cannot judge), so only a genuine mismatch flags.
pub struct ResponseCseqMatchesTransactionRule;

impl CrossMessageAuditRule for ResponseCseqMatchesTransactionRule {
    fn name(&self) -> &'static str {
        "rfc3261.responseCseqMatchesTransaction"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let parser = CustomParser::new();
        // top-Via branch -> the set of (CSeq number, method) seen on REQUESTS.
        let mut req_cseqs: HashMap<String, HashSet<(u32, String)>> = HashMap::new();
        for s in events {
            let SignalingNetworkEvent::RecvItem { packet, .. } = &s.event else {
                continue;
            };
            if let Ok(SipMessage::Request(req)) = parser.parse(&packet.raw) {
                if let Some(branch) = top_via_branch_headers(&req.headers) {
                    req_cseqs
                        .entry(branch)
                        .or_default()
                        .insert((req.cseq.seq, req.cseq.method.as_str().to_string()));
                }
            }
        }
        let mut findings = Vec::new();
        for s in events {
            let SignalingNetworkEvent::RecvItem { bind_key, packet, .. } = &s.event else {
                continue;
            };
            let Ok(SipMessage::Response(resp)) = parser.parse(&packet.raw) else {
                continue;
            };
            let Some(branch) = top_via_branch_headers(&resp.headers) else {
                continue;
            };
            // Never saw the request for this branch → cannot judge (the other
            // side may not be recorded). Only a positive mismatch is a finding.
            let Some(reqs) = req_cseqs.get(&branch) else {
                continue;
            };
            let pair = (resp.cseq.seq, resp.cseq.method.as_str().to_string());
            if !reqs.contains(&pair) {
                findings.push((
                    bind_key.clone(),
                    format!(
                        "response CSeq does not match its transaction (RFC 3261 §8.1.3.5): \
                         {} {} response carries CSeq {} {} on Via branch {branch}, but no request \
                         on that transaction had that CSeq — a real UAC drops a response whose \
                         CSeq/method does not match the request it sent (the test UA accepts it, \
                         hiding the bug)",
                        resp.status, resp.reason, resp.cseq.seq, resp.cseq.method.as_str(),
                    ),
                ));
            }
        }
        findings
    }
}

/// **RFC 3261 §13.2.2.4 — the ACK for a 2xx reuses the INVITE's CSeq.** The ACK
/// for a 2xx (and the hop-by-hop ACK for a non-2xx final) carries the same CSeq
/// sequence number as the INVITE it acknowledges. So within a request stream
/// (`receiving endpoint, Call-ID, From-tag`) every ACK's CSeq number must be one
/// that an INVITE on that stream actually used.
///
/// The in-dialog CSeq rule ([`CSeqInDialogOrderRule`]) deliberately *exempts*
/// ACK/CANCEL (they legitimately reuse a CSeq), so it cannot catch an ACK that
/// reuses the *wrong* number. This rule does: a B2BUA that builds the 2xx ACK
/// from the dialog's running `local_cseq` — advanced past the INVITE by an early
/// PRACK/UPDATE — sends `ACK CSeq 3` for an `INVITE CSeq 1`, which a real UAS
/// cannot match to the INVITE server transaction (§13.2.2.4 / §17.2.1).
pub struct AckCseqMatchesInviteRule;

impl CrossMessageAuditRule for AckCseqMatchesInviteRule {
    fn name(&self) -> &'static str {
        "rfc3261.ackCseqMatchesInvite"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let parser = CustomParser::new();
        // (receiving bind, Call-ID, From-tag) -> INVITE CSeq numbers seen so far.
        let mut invite_cseqs: HashMap<(LaneKey, String, String), HashSet<u32>> = HashMap::new();
        let mut findings = Vec::new();
        for s in events {
            let SignalingNetworkEvent::RecvItem { bind_key, packet, .. } = &s.event else {
                continue;
            };
            let Ok(SipMessage::Request(req)) = parser.parse(&packet.raw) else {
                continue;
            };
            let (Some(call_id), Some(from_tag)) = (
                get_header(&req.headers, "call-id").map(str::to_string),
                get_header(&req.headers, "from").and_then(extract_tag),
            ) else {
                continue;
            };
            let key = (bind_key.clone(), call_id, from_tag);
            let method = req.method.as_str();
            if method.eq_ignore_ascii_case("INVITE") {
                invite_cseqs.entry(key).or_default().insert(req.cseq.seq);
            } else if method.eq_ignore_ascii_case("ACK") {
                // Only judge when an INVITE was seen on this stream (an ACK always
                // follows its INVITE on the wire). No INVITE → cannot judge.
                if let Some(seen) = invite_cseqs.get(&key) {
                    if !seen.contains(&req.cseq.seq) {
                        findings.push((
                            bind_key.clone(),
                            format!(
                                "ACK CSeq does not match any INVITE (RFC 3261 §13.2.2.4): \
                                 ACK CSeq {} on Call-ID={} from-tag={} acknowledges no INVITE the \
                                 stream sent (an INVITE 2xx ACK reuses the INVITE's CSeq; a running \
                                 dialog CSeq advanced by an intervening PRACK/UPDATE is wrong) — a \
                                 real UAS cannot match it to the INVITE server transaction",
                                req.cseq.seq, key.1, key.2,
                            ),
                        ));
                    }
                }
            }
        }
        findings
    }
}

/// The in-dialog CSeq / response-CSeq / ACK-CSeq cross-message rules (the
/// RFC 3261 §8/§12/§13 wire invariants). Aggregated into the full default set by
/// [`super::rfc_cross_message_rules`].
///
/// **§8.1.1.5 (CSeq < 2^31) is NOT a rule here on purpose:** the parser's
/// registry-driven numeric pass (ADR-0007) rejects a CSeq ≥ 2^31 at ingest
/// unconditionally, so such a message never becomes a recorded, re-parseable
/// event — a cross-message rule over the strict-parsed trace could never fire.
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(CSeqInDialogOrderRule),
        Arc::new(ResponseCseqMatchesTransactionRule),
        Arc::new(AckCseqMatchesInviteRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    /// A minimal, parseable in-dialog request. `method`, `cseq` and the
    /// top-Via `branch` are all caller-controlled so a test can model a new
    /// transaction (fresh branch), a retransmission (reused branch), or an
    /// ACK/CANCEL that reuses a CSeq.
    fn req(method: &str, call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bob@127.0.0.1>;tag=btag\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A new-transaction OPTIONS (distinct branch derived from the CSeq).
    fn options(call_id: &str, from_tag: &str, cseq: u32) -> Vec<u8> {
        req("OPTIONS", call_id, from_tag, cseq, &format!("z9hG4bK-{cseq}"))
    }

    /// Like [`req`] but with an explicit dialog To-tag — `None` models the
    /// dialog-creating request (initial INVITE) that has no To-tag yet, and a
    /// distinct `Some(tag)` per fork models the forked early dialogs that share a
    /// Call-ID + From-tag but diverge by To-tag.
    fn req_to(
        method: &str,
        call_id: &str,
        from_tag: &str,
        cseq: u32,
        branch: &str,
        to_tag: Option<&str>,
    ) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A minimal parseable response carrying a chosen `(CSeq, method)` and a
    /// top-Via `branch` — to model the response a UAS sends back on a given
    /// client transaction.
    fn resp(status: u16, cseq: u32, method: &str, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag=ft\r\n\
             To: <sip:bob@127.0.0.1>;tag=btag\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// Wrap raw bytes as a `RecvItem` at `bind` (the receiving endpoint).
    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket { raw, src: "127.0.0.1:5091".parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn in_order_cseq_is_clean() {
        // INVITE(1) / OPTIONS(2) / BYE(3) on distinct branches: strictly
        // increasing across genuinely new transactions — clean.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-o"), 1),
            recv_at("bob", req("BYE", "cid-1", "ft", 3, "z9hG4bK-b"), 2),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn equal_cseq_retransmit_is_clean() {
        // A retransmit reuses its CSeq *and* its top-Via branch — one
        // transaction, equal is allowed, not a reuse-by-a-new-transaction.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-same"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-same"), 1),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn equal_cseq_reuse_different_method_is_flagged() {
        // The exact production bug: OPTIONS keepalive at CSeq 2, then a BYE that
        // *reuses* CSeq 2 on a NEW branch — a new transaction that failed to
        // increment the dialog CSeq.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-x"), 0),
            recv_at("bob", req("BYE", "cid-1", "ft", 2, "z9hG4bK-y"), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the CSeq reuse must be flagged");
        assert_eq!(findings[0].0, "bob", "attributed to the receiving endpoint");
        assert!(findings[0].1.contains("reuses a prior request's CSeq"), "{}", findings[0].1);
    }

    #[test]
    fn equal_cseq_reuse_same_method_new_transaction_is_flagged() {
        // The repeated-keepalive case: two successive OPTIONS at CSeq 2 on
        // DIFFERENT branches — two transactions, the second failed to increment.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-x"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-y"), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the new-transaction CSeq reuse must be flagged");
        assert!(findings[0].1.contains("reuses a prior request's CSeq"), "{}", findings[0].1);
    }

    #[test]
    fn out_of_order_recovered_drop_is_clean() {
        // The endurance / nk_ct_refer shape. On ONE dialog the UAC emits a
        // contiguous 2,3,4 (re-INVITE, NOTIFY, BYE), but NOTIFY(3) is dropped by
        // the loss model and its re-emission lands AFTER the BYE(4) — so the
        // recording captures the requests out of order (…4 then 3). The UAC still
        // incremented by exactly one and left NO hole: §12.2.1.1 is a property of
        // the CSeq SET, not of arrival order, so this is clean (it was the
        // TEST-MODEL false positive this rule used to charge).
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 2, "z9hG4bK-ri"), 0),
            recv_at("bob", req("BYE", "cid-1", "ft", 4, "z9hG4bK-b"), 1),
            recv_at("bob", req("NOTIFY", "cid-1", "ft", 3, "z9hG4bK-n"), 2),
        ];
        assert!(
            CSeqInDialogOrderRule.check(&evs).is_empty(),
            "a recovered-after-drop request that lands out of order leaves no hole — clean",
        );
    }

    #[test]
    fn stale_takeover_reusing_cseq_is_flagged() {
        // The stale-snapshot takeover as it appears in a FULL recording: the
        // primary keepalives OPTIONS 2 then 3; the survivor takes over with a
        // pre-failover snapshot and re-originates OPTIONS 2 on a NEW transaction
        // (fresh branch) — reusing a CSeq the dialog already spent. The reuse is
        // the violation (a real UAS drops it as a retransmission); the fact that
        // it also arrives "after" CSeq 3 is not, on its own, judged.
        let evs = vec![
            recv_at("bob", options("cid-1", "ft", 2), 0),
            recv_at("bob", options("cid-1", "ft", 3), 1),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-stale"), 2),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the stale-snapshot CSeq reuse must be flagged");
        assert_eq!(findings[0].0, "bob", "attributed to the receiving endpoint");
        assert!(findings[0].1.contains("reuses a prior request's CSeq"), "{}", findings[0].1);
    }

    #[test]
    fn ack_reusing_invite_cseq_is_exempt() {
        // ACK for a 2xx INVITE has a NEW branch but reuses the INVITE CSeq:
        // method exemption keeps it clean. Same for a re-INVITE + its ACK.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1"), 0),
            recv_at("bob", req("ACK", "cid-1", "ft", 1, "z9hG4bK-a1"), 1),
            recv_at("bob", req("INVITE", "cid-1", "ft", 2, "z9hG4bK-i2"), 2),
            recv_at("bob", req("ACK", "cid-1", "ft", 2, "z9hG4bK-a2"), 3),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn cancel_reusing_invite_cseq_is_exempt() {
        // CANCEL reuses the CSeq of the INVITE it cancels — exempt.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1"), 0),
            recv_at("bob", req("CANCEL", "cid-1", "ft", 1, "z9hG4bK-c1"), 1),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn separate_streams_do_not_alias() {
        // Different Call-ID, and different From-tag (the two directions of a
        // dialog), each carry an independent CSeq space — no cross-stream alias.
        let evs = vec![
            recv_at("bob", options("cid-A", "ft1", 5), 0),
            recv_at("bob", options("cid-B", "ft1", 1), 1), // other dialog
            recv_at("bob", options("cid-A", "ft2", 1), 2), // other direction
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn forked_early_dialogs_reusing_cseq_across_dialogs_is_clean() {
        // Forking (RFC 3261 §12.1.2): ONE INVITE (no To-tag) creates TWO early
        // dialogs that share Call-ID + From-tag but carry distinct callee To-tags.
        // Each owns its own CSeq space seeded from the INVITE, so BOTH first
        // PRACKs land at INVITE_CSeq + 1 = 2. That is NOT a reuse — distinct
        // dialogs — and the per-dialog (To-tag-keyed) check must stay clean. The
        // confirmed dialog (fork1) then BYEs at the next number IN THAT dialog (3).
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p1", Some("fork1")), 1),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p2", Some("fork2")), 2),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 3, "z9hG4bK-b", Some("fork1")), 3),
        ];
        assert!(
            CSeqInDialogOrderRule.check(&evs).is_empty(),
            "two forks' PRACKs at the same CSeq are distinct dialogs, not a reuse",
        );
    }

    #[test]
    fn forked_dialog_first_request_not_invite_plus_one_is_flagged() {
        // A forked early dialog's FIRST in-dialog request must be exactly the
        // dialog-creating INVITE's CSeq + 1 (= 2). A PRACK at CSeq 3 skips a
        // number — the increment-by-exactly-one rule (§12.2.1.1) is violated.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 3, "z9hG4bK-p", Some("fork1")), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the +2 jump off the INVITE baseline must be flagged");
        assert!(findings[0].1.contains("not contiguous"), "{}", findings[0].1);
    }

    #[test]
    fn confirmed_dialog_continues_its_own_sequence_by_one() {
        // The early dialog (fork1, PRACK 2) becomes the confirmed dialog and keeps
        // advancing its OWN To-tag's sequence by one: re-INVITE 3 (+ACK 3 exempt),
        // BYE 4. Strictly +1 within the dialog → clean.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p", Some("fork1")), 1),
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 3, "z9hG4bK-ri", Some("fork1")), 2),
            recv_at("bob", req_to("ACK", "cid-1", "ft", 3, "z9hG4bK-a", Some("fork1")), 3),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 4, "z9hG4bK-b", Some("fork1")), 4),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn auth_retry_confirmed_dialog_anchors_on_the_resent_invite_clean() {
        // Deferred-auth §22.2 retry: the establishing INVITE draws a 401 and is
        // resent ONCE with a bumped CSeq (fresh branch, new transaction). BOTH
        // attempts are dialog-CREATING (no To-tag) — the 401 established no dialog
        // — so the empty-To-tag bucket holds {1, 2}. The 200 to the RESENT INVITE
        // (CSeq 2) mints the confirmed dialog's To-tag; its first in-dialog request
        // (the BYE) is CSeq 3 = 2 + 1. The confirmed dialog must anchor on the
        // attempt that ESTABLISHED it (CSeq 2), NOT the lowest/abandoned attempt
        // (CSeq 1) — folding CSeq 1 fabricates a [1, 3] gap. The ACKs (of the 401
        // and the 2xx) reuse their INVITE's CSeq and are exempt. Clean.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1", None), 0),
            recv_at("bob", req_to("ACK", "cid-1", "ft", 1, "z9hG4bK-a1", Some("server6")), 1),
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 2, "z9hG4bK-i2", None), 2),
            recv_at("bob", req_to("ACK", "cid-1", "ft", 2, "z9hG4bK-a2", Some("server6")), 3),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 3, "z9hG4bK-b", Some("server6")), 4),
        ];
        assert!(
            CSeqInDialogOrderRule.check(&evs).is_empty(),
            "the confirmed dialog anchors on the resent INVITE (CSeq 2), not the 401'd attempt \
             (CSeq 1) — no fabricated [1, 3] gap",
        );
    }

    #[test]
    fn auth_retry_with_a_real_gap_after_the_resent_invite_is_flagged() {
        // Same auth-retry shape (empty-To-tag {1, 2}), but the confirmed dialog's
        // first in-dialog request skips a number: BYE at CSeq 4 instead of 3. The
        // anchor is still the RESENT INVITE (CSeq 2, the largest attempt ≤ 4), so
        // the fold is [2, 4] — a genuine +2 gap the caller really left. Correcting
        // the false positive must NOT swallow a real skip past the establishing
        // attempt.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1", None), 0),
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 2, "z9hG4bK-i2", None), 1),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 4, "z9hG4bK-b", Some("server6")), 2),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "a real +2 gap off the establishing INVITE must be flagged");
        assert!(findings[0].1.contains("not contiguous"), "{}", findings[0].1);
    }

    // ── ResponseCseqMatchesTransactionRule (RFC 3261 §8.1.3.5) ──────────────

    #[test]
    fn response_cseq_matching_its_request_is_clean() {
        // PRACK CSeq 2 on branch brP, answered by 200 CSeq 2 PRACK on the same
        // branch — the response copies the request's CSeq verbatim.
        let evs = vec![
            recv_at("b2bua", req("PRACK", "cid-1@127.0.0.1", "ft", 2, "z9hG4bK-brP"), 0),
            recv_at("alice", resp(200, 2, "PRACK", "z9hG4bK-brP"), 1),
        ];
        assert!(ResponseCseqMatchesTransactionRule.check(&evs).is_empty());
    }

    #[test]
    fn response_cseq_mismatch_on_branch_is_flagged() {
        // The forking corruption: alice's INVITE is CSeq 1 on branch brI, but a
        // `200 (INVITE)` carrying CSeq 2 (the PRACK's number, mis-regenerated on
        // the INVITE server txn) comes back on that same branch — no INVITE
        // request had CSeq 2, so the response cannot belong to this transaction.
        let evs = vec![
            recv_at("b2bua", req("INVITE", "cid-1@127.0.0.1", "ft", 1, "z9hG4bK-brI"), 0),
            recv_at("alice", resp(200, 2, "INVITE", "z9hG4bK-brI"), 1),
        ];
        let findings = ResponseCseqMatchesTransactionRule.check(&evs);
        assert_eq!(findings.len(), 1, "the CSeq/transaction mismatch must be flagged");
        assert!(findings[0].1.contains("does not match its transaction"), "{}", findings[0].1);
    }

    #[test]
    fn response_on_unseen_request_branch_is_skipped() {
        // A response whose request branch was never recorded cannot be judged —
        // no false positive.
        let evs = vec![recv_at("alice", resp(200, 7, "OPTIONS", "z9hG4bK-unseen"), 0)];
        assert!(ResponseCseqMatchesTransactionRule.check(&evs).is_empty());
    }

    // ── AckCseqMatchesInviteRule (RFC 3261 §13.2.2.4) ───────────────────────

    #[test]
    fn ack_reusing_the_invite_cseq_is_clean() {
        // INVITE 1 / ACK 1, then re-INVITE 4 / ACK 4: each ACK reuses its
        // INVITE's CSeq — clean.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1@127.0.0.1", "ft", 1, "z9hG4bK-brI1"), 0),
            recv_at("bob", req("ACK", "cid-1@127.0.0.1", "ft", 1, "z9hG4bK-brA1"), 1),
            recv_at("bob", req("INVITE", "cid-1@127.0.0.1", "ft", 4, "z9hG4bK-brI2"), 2),
            recv_at("bob", req("ACK", "cid-1@127.0.0.1", "ft", 4, "z9hG4bK-brA2"), 3),
        ];
        assert!(AckCseqMatchesInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn ack_reusing_a_non_invite_cseq_is_flagged() {
        // The bug: INVITE 1, then an early PRACK/UPDATE advanced the dialog CSeq,
        // so the 2xx ACK lands at CSeq 3 — a number no INVITE used. A real UAS
        // cannot match it to the INVITE server transaction (§13.2.2.4).
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1@127.0.0.1", "ft", 1, "z9hG4bK-brI"), 0),
            recv_at("bob", req("ACK", "cid-1@127.0.0.1", "ft", 3, "z9hG4bK-brA"), 1),
        ];
        let findings = AckCseqMatchesInviteRule.check(&evs);
        assert_eq!(findings.len(), 1, "the ACK reusing a non-INVITE CSeq must be flagged");
        assert!(findings[0].1.contains("acknowledges no INVITE"), "{}", findings[0].1);
    }

    #[test]
    fn ack_without_any_invite_seen_is_skipped() {
        // No INVITE on the stream → cannot judge the ACK; no false positive.
        let evs = vec![recv_at("bob", req("ACK", "cid-1@127.0.0.1", "ft", 9, "z9hG4bK-brA"), 0)];
        assert!(AckCseqMatchesInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn cseq_gap_within_a_dialog_is_flagged() {
        // A +2 jump between two new requests on the SAME dialog violates the
        // increment-by-exactly-one rule even though it is strictly increasing.
        let evs = vec![
            recv_at("bob", options("cid-1", "ft", 2), 0),
            recv_at("bob", options("cid-1", "ft", 4), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "a CSeq gap (+2) must be flagged");
        assert!(findings[0].1.contains("not contiguous"), "{}", findings[0].1);
    }

    #[test]
    fn cross_dialog_reuse_of_the_invite_cseq_is_flagged() {
        // Blind spot (b): the dialog-creating INVITE is CSeq 1 (empty To-tag
        // bucket); a later in-dialog INFO reuses CSeq 1 in the To-tagged bucket.
        // The per-To-tag split means the two never collide as a same-bucket reuse,
        // yet the INFO failed to advance the dialog CSeq past the INVITE — a
        // §12.2.1.1 violation a real UAS drops as a retransmission.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("INFO", "cid-1", "ft", 1, "z9hG4bK-info", Some("btag")), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the cross-dialog INVITE-CSeq reuse must be flagged");
        assert!(findings[0].1.contains("did not advance"), "{}", findings[0].1);
    }

    #[test]
    fn in_dialog_request_below_the_invite_cseq_is_flagged() {
        // Blind spot (a): the dialog-creating INVITE is CSeq 5; the confirmed
        // dialog's first in-dialog request is CSeq 3 — BELOW the anchor. The old
        // `c <= first` filter found no anchor and let it pass; a CSeq that
        // regressed below the INVITE is a §12.2.1.1 violation.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 5, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 3, "z9hG4bK-b", Some("btag")), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "a below-anchor in-dialog CSeq must be flagged");
        assert!(findings[0].1.contains("did not advance"), "{}", findings[0].1);
    }

    #[test]
    fn in_dialog_cseq_zero_is_flagged() {
        // CSeq 0 as an in-dialog request is below any ≥1 dialog-creating anchor —
        // the nonsensical below-anchor value the sharpened check now catches.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("INFO", "cid-1", "ft", 0, "z9hG4bK-info", Some("btag")), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "an in-dialog CSeq 0 must be flagged");
        assert!(findings[0].1.contains("did not advance"), "{}", findings[0].1);
    }

    #[test]
    fn parser_rejects_cseq_at_or_above_2pow31() {
        // §8.1.1.5 (CSeq < 2^31) is enforced at the PARSER (ADR-0007 numeric
        // registry), unconditionally — so a magnitude cross-message rule would be
        // dead. This pins that enforcement: 2^31 is rejected, 2^31 - 1 parses.
        let over = req("INFO", "cid-1", "ft", 2_147_483_648, "z9hG4bK-m");
        assert!(CustomParser::new().parse(&over).is_err(), "CSeq 2^31 must be rejected at parse");
        let max = req("INFO", "cid-1", "ft", 2_147_483_647, "z9hG4bK-m");
        assert!(CustomParser::new().parse(&max).is_ok(), "CSeq 2^31 - 1 is the largest legal value");
    }
}

