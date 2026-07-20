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
//! - [`harness`] — the session: recording-wrapped network, agent binding,
//!   virtual-time advance, `finish()` + the RFC hard gate.
//! - [`run_guards`] — Drop-armed backstops: panic-time trace dump, the
//!   forgot-to-`finish` RFC gate.
//! - [`step`] — [`StepError`], the fallible-core step vocabulary.
//! - [`ua`] + [`tolerant_recv`] — [`Agent`]: the send/receive cores and the
//!   method-filtered receive policies.
//! - [`txn_view`] — the §17.2 once-and-only-once receive view and §17.1.1.3
//!   UAS-side ACK obligations.
//! - [`invite`] / [`client_invite`] / [`out_of_dialog`] / [`dialog`] —
//!   client-side builders and transactions (initial INVITE, any out-of-dialog
//!   method, in-dialog).
//! - [`server_txn`] — the UAS side: [`ServerTxn`] + the [`Respond`] builder.
//! - [`client_txn`] — the shared client-side expect core, §17.1.1.3 auto-ACK,
//!   CANCEL build+send.
//! - [`proxy`] — the minimal scripted loose-routing [`Proxy`].
//! - [`rr_fold`] — the per-UA Record-Route folding coin flip.
//! - [`addressing`] — wire-address resolution (next hop, Via sent-by); SIP
//!   parsing does NOT live here — see `sip_message::message_helpers`.

mod addressing;
mod client_invite;
mod client_txn;
mod dialog;
mod harness;
mod invite;
mod out_of_dialog;
mod proxy;
mod rr_fold;
mod run_guards;
mod server_txn;
mod step;
pub(crate) mod waiver;
#[cfg(test)]
mod tests;
mod tolerant_recv;
mod txn_view;
mod ua;

pub use client_invite::{CancelHandle, ClientInvite};
pub use dialog::{ClientReinvite, Dialog, InDialogRequest, InDialogTxn};
pub use harness::Harness;
pub use invite::Invite;
pub use out_of_dialog::OutOfDialogRequest;
pub use proxy::Proxy;
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
pub(crate) use txn_view::{AckObligations, TxnView};
