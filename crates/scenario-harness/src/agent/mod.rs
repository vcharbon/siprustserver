//! The fluent, dialog-aware harness — the auto-generating DSL. Agents are
//! stateful UAs; each high-level call generates a correct-by-default B2B
//! message via `sip_message::generators` and tracks the dialog state needed
//! for the next one, so a scenario does **not** hand-author headers:
//!
//! ```ignore
//! let h = Harness::new("alice-calls-bob");
//! let alice = h.agent("alice", "127.0.0.1:5060").await;
//! let bob   = h.agent("bob",   "127.0.0.1:5070").await;
//!
//! let mut call = alice.invite(&bob).with_sdp(OFFER).send().await; // INVITE auto-built
//! let mut uas  = bob.receive("INVITE").await;
//! uas.respond(180, "Ringing").await;                              // To-tag minted here
//! call.expect(180).await;                                         // learns remote tag/target
//! uas.respond(200, "OK").with_sdp(ANSWER).await;
//! call.expect(200).await;
//! let mut dialog = call.ack().await;                              // ACK reuses INVITE CSeq
//! bob.receive("ACK").await;
//! let mut bye = dialog.bye().await;                               // BYE auto-increments CSeq
//! bob.receive("BYE").await.respond(200, "OK").await;
//! bye.expect(200).await;
//! let report = h.finish().await;                                  // render from the recording
//! ```
//!
//! What the harness fills in automatically, per RFC 3261: Via (fresh branch per
//! transaction, magic cookie), From/To with tags, Call-ID continuity, CSeq
//! numbering (1 INVITE → 1 ACK → n BYE; responses echo), Contact, Max-Forwards,
//! Content-Type/Length, remote-target routing (in-dialog requests go to the
//! peer's Contact). Everything flows through the recording-wrapped
//! `SignalingNetwork`, so the reports are projected from the record — the
//! auto-generation only changes *who writes the bytes*.
//!
//! Module map (one concern per file):
//! - `harness` — the session: recording-wrapped network, agent binding,
//!   virtual-time advance, `finish()` + the RFC hard gate.
//! - `dialog_gate` — `finish()`'s gate on the UAs' dialog verdicts
//!   ([`crate::dialogs`]) and the `dialog-…` waivers that opt one out.
//! - `run_guards` — the RFC audit hard gate: its findings, its panic text,
//!   the forgot-to-`finish` backstop.
//! - `failure_dump` — the panic-time dump of a failing run: the digest line,
//!   then `ladder` (the wire), `call_trace` (each sampled call's SUT trace),
//!   the [`FailureView`] sections a SUT adds, the UAs' dialog tables, and
//!   `log_dump` (the capture buffer and the SUT log tail).
//! - `rule_coverage` — the per-test record of each RFC rule's occasions,
//!   written at `finish()` under the cargo target dir.
//! - `artifact_dump` — the Drop-time report-artifact writer
//!   (`SCENARIO_ARTIFACT_DIR`-gated), so a run that never reaches `finish()`
//!   still leaves its SVG/HTML/text ladders.
//! - `panic_note` — the chained process-wide panic hook that captures the
//!   panic message the failure dump digests and the artifact writer stamps
//!   into its FAIL banner.
//! - `step` — [`StepError`], the fallible-core step vocabulary, and the
//!   ordinal + test line of a scripted wait.
//! - `ua` + `tolerant_recv` — [`Agent`]: the send/receive cores and the
//!   method-filtered receive policies. The §17.2 receive view itself is NOT
//!   here: it lives once for the whole tree in [`crate::absorption`], which
//!   every agent mode reads.
//! - `invite` / `client_invite` / `out_of_dialog` / `dialog` —
//!   client-side builders and transactions (initial INVITE, any out-of-dialog
//!   method, in-dialog).
//! - `party` — [`Party`]: the side of the SUT a UA answers on, and the first
//!   CSeq each side numbers from.
//! - `server_txn` — the UAS side: [`ServerTxn`] + the [`Respond`] builder.
//! - `client_txn` — the shared client-side expect core, §17.1.1.3 auto-ACK,
//!   CANCEL build+send.
//! - `proxy` — the minimal scripted loose-routing [`Proxy`].
//! - `rr_fold` — the per-UA Record-Route folding coin flip.
//! - `addr_alloc` — the loopback addresses [`Harness::next_addr`] hands out.
//! - `addressing` — wire-address resolution (next hop, Via sent-by); SIP
//!   parsing does NOT live here — the values come from `sip_message::header`.

mod addr_alloc;
mod addressing;
mod artifact_dump;
mod call_trace;
mod client_invite;
mod client_txn;
mod dialog;
pub(crate) mod dialog_gate;
mod failure_dump;
mod harness;
mod invite;
mod ladder;
mod log_dump;
mod out_of_dialog;
mod panic_note;
mod party;
mod proxy;
mod rr_fold;
mod rule_coverage;
mod run_guards;
mod server_txn;
mod step;
#[cfg(test)]
mod tests;
mod tolerant_recv;
mod ua;
pub(crate) mod waiver;

pub use client_invite::{CancelHandle, ClientInvite};
pub use dialog::{ClientReinvite, Dialog, InDialogRequest, InDialogTxn};
pub use failure_dump::{expect_failure, DumpSection, FailedRun, FailureView};
pub use harness::Harness;
pub use invite::Invite;
pub use out_of_dialog::OutOfDialogRequest;
pub use party::Party;
pub use proxy::{
    forwarded_request, forwarded_response, request_next_hop, response_next_hop, Proxy,
};
pub use rr_fold::RecordRouteFold;
pub use server_txn::{Respond, ServerTxn};
pub use step::StepError;
pub use ua::{Agent, Inbound};
pub use waiver::WaiverScope;

// Crate-internal seams: the Send agent factory (`loadbind`) and the shared-
// socket callee group construct `Agent`s directly; the reactive actor
// dispatches on the INVITE fate and the top-Via branch.
pub(crate) use addressing::top_via_branch;
pub(crate) use client_invite::InviteResponseFate;
pub(crate) use harness::Ids;
pub(crate) use rr_fold::decide_rr_fold;
