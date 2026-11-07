//! The UAS side: [`ServerTxn`], a received request and its response state;
//! its [`Respond`] builder lives in [`respond`].

use std::collections::{HashMap, HashSet};
use std::future::Future;

use sip_message::generators::StackDialog;
use sip_message::header::HeaderValue;
use sip_message::{SipMessage, SipRequest};
use sip_template::Mismatch;

use crate::{EmitOpts, MatchOpts, MessageTemplate};

use super::addressing::{next_hop, top_via_addr, top_via_branch};
use super::client_txn::request_label;
use super::dialog::Dialog;
use super::step::{unwrap_step_at, StepError};
use super::Agent;

mod respond;

pub use respond::Respond;

/// UAS-side transaction for a received request. `respond` echoes Via/From/To/
/// Call-ID/CSeq and mints a stable To-tag on the first non-100 response.
pub struct ServerTxn {
    agent: Agent,
    pub(super) request: SipRequest,
    to_tag: Option<String>,
    route_set: Vec<String>,
    /// Early-dialog id → its minted To-tag. On ONE INVITE server transaction a
    /// UAS drives several early dialogs (a simulated downstream fork), each an
    /// id mapped to a distinct To-tag (RFC 3261 §12.1.2).
    early_tags: HashMap<String, String>,
    /// Early-dialog ids that have emitted at least one provisional. A winner
    /// declared by [`win`](Self::win) is one the caller saw an early dialog for.
    emitted_early: HashSet<String>,
    /// The declared winner, once [`win`](Self::win) or
    /// [`answer_unrung`](Self::answer_unrung) is called. A second, different
    /// winner or a forked 2xx with none declared is a silent misuse the wire
    /// audit cannot catch (both are wire-legal) — the surface rejects it.
    winner: Option<Winner>,
    /// This transaction's final response status, once one is sent. A new 1xx or a
    /// different final after it is rejected here; a same-status retransmission
    /// (RFC 3261 §13.3.1.4) is allowed through.
    final_status: Option<u16>,
    /// The To-tag [`adopt_to_tag`](Self::adopt_to_tag) pinned or the 2xx
    /// carried, whichever came last: the dialog [`dialog`](Self::dialog)
    /// names (RFC 3261 §12.1.1).
    dialog_tag: Option<String>,
    /// The To-tag of the first tagged provisional: the early dialog
    /// [`dialog`](Self::dialog) names when nothing else does.
    provisional_tag: Option<String>,
}

impl ServerTxn {
    /// The ONE constructor from a received request. Captures the UAS route set
    /// (RFC 3261 §12.1.1): the request's Record-Route rows in received order —
    /// used if this UAS later originates in-dialog requests (e.g. bob sends
    /// the BYE).
    pub(super) fn from_request(agent: Agent, request: SipRequest) -> Self {
        // §12.1.1: the UAS route set is the request's Record-Route rows in
        // RECEIVED order. A row the strict reader rejects teaches no route.
        let route_set = request
            .record_route_set()
            .map(|set| set.iter().map(HeaderValue::to_wire).collect())
            .unwrap_or_default();
        ServerTxn {
            agent,
            request,
            to_tag: None,
            route_set,
            early_tags: HashMap::new(),
            emitted_early: HashSet::new(),
            winner: None,
            final_status: None,
            dialog_tag: None,
            provisional_tag: None,
        }
    }

    /// The received request (for inspecting headers / SDP).
    pub fn request(&self) -> &SipRequest {
        &self.request
    }

    /// Assert this received request matches `tmpl` under the tiered match model
    /// ([`MessageTemplate::match_inbound`]): tier-1 fields structural-only,
    /// remote-target headers compared by params modulo `opts.ignore_params`,
    /// frozen headers + body byte-compared. Returns the first [`Mismatch`].
    /// The response-side equivalent is `tmpl.match_inbound(&SipMessage::Response(resp), opts)`
    /// on the response a client transaction's `expect` returned.
    // `Mismatch` is a rich diagnostic value, returned by-value so it reads
    // directly in an assertion / test failure (see `match_inbound`).
    #[allow(clippy::result_large_err)]
    pub fn expect_template(
        &self,
        tmpl: &MessageTemplate,
        opts: &MatchOpts,
    ) -> Result<(), Mismatch> {
        tmpl.match_inbound(&SipMessage::Request(self.request.clone()), opts)
    }

    /// Send a response. Returns a builder for attaching an SDP answer and/or
    /// custom headers (e.g. `Require: 100rel` + `RSeq` on a reliable 18x).
    pub fn respond(&mut self, status: u16, reason: &str) -> Respond<'_> {
        Respond::new(self, status, reason)
    }

    /// Answer this request from a captured [`MessageTemplate`]: the status /
    /// reason come from the template's response start line; the stack regenerates
    /// the echoed tier-1 fields (Via, From, To + minted tag, Call-ID, CSeq,
    /// Contact, Content-Length) while every frozen header rides verbatim (value
    /// bytes, name casing, duplicate-header layout). The captured `Content-Type`
    /// is carried as a frozen header. Panics if the template is not a response.
    /// See [`EmitOpts`] for the v1 header-order limitation.
    pub fn respond_template<'t>(
        &'t mut self,
        tmpl: &MessageTemplate,
        opts: EmitOpts,
    ) -> Respond<'t> {
        let (status, reason) = tmpl.status().unwrap_or_else(|| {
            panic!("respond_template requires a response template, got {:?}", tmpl.start())
        });
        let reason = reason.to_string();
        self.respond(status, &reason).template(tmpl, opts)
    }

    /// Assert the §17.1.1.3 hop ACK for THIS transaction's non-2xx final.
    /// Returns immediately if the transaction layer already claimed it (it may
    /// have landed before or after any of the body's other receives — matching
    /// is by `(Call-ID, INVITE branch)`, never positional); otherwise pulls
    /// until the ACK arrives, the panicking-veneer sibling of
    /// [`try_expect_ack`](Self::try_expect_ack). Purely optional: an unread ACK
    /// is still recorded at delivery and the gating
    /// `unacked-invite-non-2xx-final` wire rule settles the obligation at
    /// `finish()` — reach for this when the test asserts the ACK at a specific
    /// point in the flow.
    #[track_caller]
    pub fn expect_ack(&self) -> impl Future<Output = ()> + '_ {
        let site = self.agent.step("expect_ack");
        async move { unwrap_step_at(self.try_expect_ack().await, site) }
    }

    /// Fallible core of [`expect_ack`](Self::expect_ack).
    pub async fn try_expect_ack(&self) -> Result<(), StepError> {
        let Some(branch) = top_via_branch(&self.request) else {
            return Err(StepError::UnexpectedKind {
                who: self.agent.name.clone(),
                detail: "expect_ack on a request with no top-Via branch".to_string(),
            });
        };
        loop {
            if self.agent.txn.hop_ack_is_fulfilled(self.request.call_id().as_str(), &branch) {
                return Ok(());
            }
            // Pull; the receive core sights (and thereby fulfils) a matching
            // ACK. Anything else arriving while the test explicitly awaits the
            // ACK is a deviation, exactly as `receive("ACK")` would treat it.
            let next = self.agent.try_recv().await.map_err(|e| {
                e.awaiting(|| {
                    let status = self.final_status.map(|s| format!(" {s}")).unwrap_or_default();
                    format!(
                        "the ACK of its{status} final to {}",
                        request_label(None, &self.request)
                    )
                })
            });
            match next? {
                SipMessage::Request(r) if r.method().as_str() == "ACK" => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // ours (loop returns Ok) or another txn's obligation
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: "got an ACK for a different transaction".to_string(),
                    });
                }
                SipMessage::Request(r) => {
                    return Err(StepError::WrongMethod {
                        who: self.agent.name.clone(),
                        expected: "ACK".to_string(),
                        got: r.method().to_string(),
                    })
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected the ACK to this txn's final",
                            r.status(),
                            r.reason()
                        ),
                    })
                }
            }
        }
    }

    /// Pin this transaction's sticky local To-tag (the dialog-forming tag) to an
    /// explicit value — the forking-UAS winner path: after emitting per-fork 18x
    /// with explicit tags (`Respond::with_to_tag`, which deliberately does NOT
    /// disturb the sticky tag), the winning fork's 200 must both carry the
    /// winner's tag AND leave [`dialog`](Self::dialog) keyed under it. Call
    /// before responding the winning 2xx.
    pub fn adopt_to_tag(&mut self, tag: &str) {
        self.to_tag = Some(tag.to_string());
        self.dialog_tag = Some(tag.to_string());
    }

    /// Open (or return) the early dialog `early_id` on THIS INVITE server
    /// transaction: mint a distinct To-tag the first time it is named, stable
    /// thereafter. A UAS drives arbitrary early dialogs by tagging its
    /// provisionals with these — a simulated downstream fork presenting several
    /// early dialogs to the caller on one server transaction (RFC 3261 §12.1.2).
    pub fn early_tag(&mut self, early_id: &str) -> String {
        if let Some(t) = self.early_tags.get(early_id) {
            return t.clone();
        }
        let tag = self.agent.tag();
        self.early_tags.insert(early_id.to_string(), tag.clone());
        tag
    }

    /// Send a provisional (18x) on the early dialog `early_id`: its To-tag is
    /// pinned via [`Respond::with_to_tag`] (which does NOT disturb the sticky
    /// winner tag). Chain `.with_sdp` / `.reliable` / `.with_header`. The order of
    /// early dialogs is arbitrary — the winner need not be the last created.
    pub fn respond_early(&mut self, early_id: &str, status: u16, reason: &str) -> Respond<'_> {
        assert!(
            (101..=199).contains(&status),
            "respond_early is for provisionals (101..=199), got {status}: send the final with win({early_id:?}) then respond({status})"
        );
        self.emitted_early.insert(early_id.to_string());
        let tag = self.early_tag(early_id);
        self.respond(status, reason).with_to_tag(&tag)
    }

    /// Send a TEMPLATE provisional on the early dialog `early_id` — frozen
    /// headers byte-preserved (template-emission semantics), To-tag = the early dialog's.
    pub fn respond_template_early<'t>(
        &'t mut self,
        early_id: &str,
        tmpl: &MessageTemplate,
        opts: EmitOpts,
    ) -> Respond<'t> {
        let status = tmpl.status().map(|(s, _)| s).unwrap_or(0);
        assert!(
            (101..=199).contains(&status),
            "respond_template_early requires a provisional (101..=199) response template, got {status}"
        );
        self.emitted_early.insert(early_id.to_string());
        let tag = self.early_tag(early_id);
        self.respond_template(tmpl, opts).with_to_tag(&tag)
    }

    /// Declare `early_id` the WINNER: adopt its To-tag as the sticky dialog tag
    /// so the final 2xx carries it and [`dialog`](Self::dialog) is keyed under
    /// it. The losing early dialogs settle by the stack's automatics — no final
    /// is sent for them, and the caller's ACK addresses only the winner
    /// (RFC 3261 §13.3.1.4). Call before responding the winning 2xx.
    pub fn win(&mut self, early_id: &str) {
        assert!(
            self.emitted_early.contains(early_id),
            "win({early_id:?}): no provisional was emitted on that early dialog (known early dialogs: {:?}) — {DECLARE_WINNER}",
            self.emitted_early
        );
        self.declare_winner(Winner::Early(early_id.to_string()));
        let tag = self.early_tag(early_id);
        self.adopt_to_tag(&tag);
    }

    /// Declare the WINNER a fork that sent no provisional: mint a fresh To-tag,
    /// distinct from every early dialog's, as the sticky dialog tag, so the 2xx
    /// carries it and [`dialog`](Self::dialog) is keyed under it (RFC 3261
    /// §12.1.2, §13.2.2.4: a 2xx may create a dialog no 1xx announced). The
    /// rung early dialogs settle by the stack's automatics. Returns the tag.
    /// Call before responding the 2xx; a repeat call returns the same tag.
    pub fn answer_unrung(&mut self) -> String {
        if self.winner == Some(Winner::Unrung) {
            return self.to_tag.clone().expect("an unrung winner holds its minted tag");
        }
        self.declare_winner(Winner::Unrung);
        let tag = self.agent.tag();
        self.adopt_to_tag(&tag);
        tag
    }

    fn declare_winner(&mut self, winner: Winner) {
        if let Some(w) = &self.winner {
            assert!(
                *w == winner,
                "{winner:?}: {w:?} already won this transaction — a transaction has one winner"
            );
        }
        self.winner = Some(winner);
    }

    /// The UAS-side [`Dialog`] of this transaction (RFC 3261 §12.1.1): its
    /// local tag the response that created it carried (the request's own
    /// when it names one), the caller's Contact as target, the Record-Route
    /// as route set, its sequence from [`Party::first_cseq`](super::Party::first_cseq).
    /// Panics before any response created a dialog.
    pub fn dialog(&self) -> Dialog {
        let req = &self.request;
        let local_tag = req
            .to()
            .tag()
            .map(str::to_owned)
            .or_else(|| self.dialog_tag.clone())
            .or_else(|| self.to_tag.clone())
            .or_else(|| self.provisional_tag.clone())
            .unwrap_or_else(|| {
                panic!(
                    "{}: dialog() on {} before a response created a dialog (RFC 3261 §12.1.1)",
                    self.agent.name,
                    request_label(None, req)
                )
            });
        let remote_target = req
            .contacts()
            .as_slice()
            .first()
            .map(|contact| contact.uri().to_string())
            .unwrap_or_else(|| req.from().uri().to_string());
        let dialog = StackDialog {
            call_id: req.call_id().to_string(),
            local_tag,
            remote_tag: req.from().tag().map(str::to_owned).unwrap_or_default().to_string(),
            // From the UAS's view, "local" is itself and "remote" is the caller.
            // RFC 3261 §12.1.1: the dialog LOCAL URI is the To field of the
            // request, NOT the agent's own AOR — they coincide in the usual
            // alice→bob case (To:bob == bob's uri) but diverge when a UAS is
            // handed an INVITE addressed to a third party (e.g. the MRF media
            // leg carries To:dest): the callee's in-dialog requests must then
            // carry From:dest, and the recorded-trace mid-dialog-uri audit — which
            // merges both tag orientations into one dialog slice — checks it.
            local_uri: req.to().uri().to_string(),
            remote_uri: req.from().uri().to_string(),
            remote_target,
            local_cseq: self.agent.answers_as.first_cseq() - 1,
            route_set: self.route_set.clone(),
        };
        let fallback = next_hop(&dialog, top_via_addr(req).unwrap_or(self.agent.addr));
        Dialog::new(self.agent.clone(), fallback, dialog)
    }
}

/// The fork a forked INVITE server transaction answers with its 2xx.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Winner {
    /// An early dialog that emitted a provisional, by its id.
    Early(String),
    /// A fork that sent no provisional: its 2xx carries a fresh To-tag
    /// (RFC 3261 §12.1.2, §13.2.2.4).
    Unrung,
}

/// The panic text of a 2xx or `win` that names no declared winner.
const DECLARE_WINNER: &str =
    "declare the winner with win(id) for a rung early dialog or answer_unrung() for a fork that sent no provisional";
