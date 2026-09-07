//! The actor substrate's pinned tests, grouped by concern: substrate/runner
//! basics, forking (C1/E3), CANCEL×200 racing (C2/E5), renegotiation (C4/C5),
//! deferred auth (§22.2), the ACK-body offer/answer decision, the Scripted
//! disposition (replay, CANCEL machinery, flow ordering), accepted deltas
//! (ADR-0024 §6), the reception-observation hook, the reception goals' body
//! expectation, the parked-request pick, and message templates.
//! Shared plan/template builders live in [`testkit`].

mod auth;
mod body_expect;
mod cancel_race;
mod delta_request;
mod delta_response;
mod forking;
mod reception_observer;
mod reinvite_ack_body;
mod reneg;
mod request_pick;
mod script_flow;
mod scripted_cancel;
mod scripted_replay;
mod substrate;
mod template_request;
mod template_respond;
mod testkit;
