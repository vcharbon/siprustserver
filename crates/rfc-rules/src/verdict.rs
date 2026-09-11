//! What a rule states about an occasion: the closed rule vocabulary, the
//! three-valued decision, its evidence, and the population contract.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::wire::Endpoint;

/// A rule an adapter can run. CLOSED vocabulary — a rule with no body here is
/// a claim nothing can verify, so it does not exist. The spelling is the
/// kebab-case wire token of the pivot vocabulary (§11.1); the dotted
/// `rfc_audit` ids die as each live module's rung ports it.
///
/// The WIRE subset — what a pivot document may name in `rfc_violations[].rule`
/// — is [`RuleId::WIRE`], and it grows one census-verified member at a time;
/// membership here alone does not put a rule on the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RuleId {
    /// RFC 3261 §9.2: a UAS that has taken a CANCEL for an INVITE answers that
    /// INVITE 487, never 2xx.
    ///
    /// Spelled out rather than derived: kebab-casing the variant would mint
    /// `no200-after-cancel`, and the token has to match the replay
    /// vocabulary's exactly.
    #[serde(rename = "no-200-after-cancel")]
    No200AfterCancel,
    /// RFC 3262 §4: a UAC that took a reliable provisional answers it with a
    /// PRACK whose RAck names that provisional.
    #[serde(rename = "unacked-reliable-provisional")]
    UnackedReliableProvisional,
    /// RFC 3261 §13.2.2.4: a UAC that took a dialog-creating 2xx to its own
    /// INVITE answers it with an ACK on that dialog. Charges the UAC; a BYE is
    /// corroboration that the dialog died unconfirmed, never a discharge.
    #[serde(rename = "no-ack-to-dialog-creating-2xx")]
    NoAckToDialogCreating2xx,
    /// RFC 3261 §13.3.1.4: a UAS whose 2xx to an INVITE is never ACKed clears
    /// the dialog with a BYE. Charges the UAS; the ACK or any BYE on the
    /// dialog discharges it — the other half of the obligation the rule above
    /// charges to the UAC, split so waivers and verdicts name which party.
    #[serde(rename = "unacked-2xx-not-cleared")]
    Unacked2xxNotCleared,
    /// RFC 3262 §7.2: a PRACK's `RAck` CSeq-num names the INVITE whose RSeq
    /// space the acknowledged provisional belongs to. Charges the PRACK's
    /// sender — the header is copied from the 1xx, never from the PRACK's own
    /// CSeq, and a PRACK naming an unknown INVITE settles nothing.
    #[serde(rename = "rack-without-known-invite")]
    RackWithoutKnownInvite,
    /// RFC 3262 §3: a UAS holds one reliable provisional outstanding at a time
    /// on a dialog — the next waits for the PRACK of the last. Charges the UAS;
    /// a retransmission of an RSeq already sent is not a second provisional.
    #[serde(rename = "no-overlapping-reliable-provisionals")]
    NoOverlappingReliableProvisionals,
    /// RFC 3262 §3: each subsequent reliable provisional on a dialog carries
    /// `RSeq = prior + 1`. Charges the UAS.
    #[serde(rename = "non-contiguous-rseq")]
    NonContiguousRseq,
    /// RFC 3262 §4: a UAC PRACKs reliable provisionals in RSeq order — one that
    /// arrived out of order waits for the gap to fill. Charges the UAC.
    #[serde(rename = "no-prack-of-out-of-order-rseq")]
    NoPrackOfOutOfOrderRseq,
    /// RFC 3261 §17.2.1: a server transaction emits exactly one final response
    /// — after the first, only a same-status retransmission is legal
    /// (§13.3.1.4 for a 2xx). Charges the transaction's UAS side.
    #[serde(rename = "single-final-per-server-txn")]
    SingleFinalPerServerTxn,
    /// RFC 3261 §9.1: a CANCEL carries the same Route values as the INVITE it
    /// cancels — the two take the same path. Charges the CANCEL's sender.
    #[serde(rename = "cancel-route-echoes-invite")]
    CancelRouteEchoesInvite,
    /// RFC 3261 §9.1, bounded by ADR-0028: a UAC CANCELs only once it has taken
    /// a provisional, or once its own grace window has run out. Charges the
    /// UAC; advisory by consumer policy, since the bounded hold is sanctioned.
    #[serde(rename = "cancel-after-1xx")]
    CancelAfter1xx,
    /// RFC 3261 §12.2.1.1: within a dialog a UAC increments the CSeq by exactly
    /// one per new request (ACK and CANCEL excepted). Charges the UAC that
    /// generated the stream; read at the endpoint that TOOK it.
    #[serde(rename = "cseq-in-dialog-order")]
    CseqInDialogOrder,
    /// RFC 3261 §8.1.3.5: a response copies its request's CSeq verbatim, so a
    /// response's `(number, method)` is one the requests on its transaction
    /// carried. Charges the UAS that sent the response.
    #[serde(rename = "response-cseq-matches-transaction")]
    ResponseCseqMatchesTransaction,
    /// RFC 3261 §13.2.2.4: an ACK reuses the CSeq number of the INVITE it
    /// acknowledges. Charges the UAC that sent the ACK.
    #[serde(rename = "ack-cseq-matches-invite")]
    AckCseqMatchesInvite,
    /// RFC 3261 §12.2.1.1: an in-dialog request repeats the dialog's peer URIs
    /// — the From/To URIs learned when the dialog was created. Charges the UAC
    /// that sent it.
    #[serde(rename = "mid-dialog-uri")]
    MidDialogUri,
    /// RFC 3261 §12.2.1.1 / §16.12: an in-dialog request reproduces the dialog
    /// route set — as Route rows behind a loose first route, or in the
    /// Request-URI behind a strict one. Charges the UAC that sent it.
    #[serde(rename = "mid-dialog-route")]
    MidDialogRoute,
    /// RFC 3261 §8.1.2 + RFC 3263 §4: an in-dialog request's bytes go to the
    /// destination its topmost Route (or, with an empty route set, its
    /// Request-URI) resolves to. Charges the UAC that sent it.
    #[serde(rename = "mid-dialog-wire-destination")]
    MidDialogWireDestination,
    /// RFC 3261 §12.1.1 / §12.2.2: Record-Route rides dialog-CREATING
    /// responses only — never a 100, never a response to an in-dialog request.
    /// Charges the endpoint that sent the response.
    #[serde(rename = "record-route-placement")]
    RecordRoutePlacement,
    /// RFC 3581 §4: a server that took a request advertising a bare `;rport`
    /// echoes `rport=<source-port>` on the response's top Via. Charges the
    /// server; advisory by consumer policy, since a loopback fabric has no NAT
    /// to observe.
    #[serde(rename = "rport-echo")]
    RportEcho,
    /// RFC 3261 §13.2.1 / §20.37: a re-INVITE and a 2xx to an INVITE advertise
    /// Allow and Supported. Charges the endpoint that sent the message.
    #[serde(rename = "allow-supported-on-invite")]
    AllowSupportedOnInvite,
    /// RFC 3261 §16.7 step 5: a stateful proxy absorbs the downstream 100
    /// Trying, so a UAC takes at most one 100 per INVITE it sent on the
    /// transaction. Charges the endpoint that forwarded the extra 100.
    #[serde(rename = "proxy-100-trying-not-forwarded")]
    Proxy100TryingNotForwarded,
    /// RFC 3261 §12.2.2: a UAS that takes an in-dialog request naming a dialog
    /// it never confirmed answers 481. Charges that UAS.
    #[serde(rename = "unknown-dialog-481")]
    UnknownDialog481,
    /// RFC 3261 §8.2.1: a UAS that does not recognise a request's method
    /// answers 405 carrying an `Allow` header. Charges that UAS.
    #[serde(rename = "unsupported-method-405-allow")]
    UnsupportedMethod405Allow,
    /// RFC 3261 §8.2.2: a UAS handed a `Require` option tag it does not support
    /// answers 420 carrying `Unsupported`. Charges that UAS.
    #[serde(rename = "unsupported-extension-420")]
    UnsupportedExtension420,
    /// RFC 3261 §8.2.3: a 415 names the formats its sender does accept, via
    /// `Accept` / `Accept-Encoding` / `Accept-Language`. Charges the sender.
    #[serde(rename = "unsupported-415-accepts")]
    Unsupported415Accepts,
    /// RFC 3261 §21.4.15: a 421 lists the extensions it demands of the UAC in a
    /// `Require` header. Charges the endpoint that sent it.
    #[serde(rename = "unsupported-extension-421")]
    UnsupportedExtension421,
    /// RFC 3261 §16.3: a proxy that resolves no target for the Request-URI
    /// answers 404. Charges the proxy; advisory by consumer policy, since a
    /// B2BUA worker legitimately rejects without forwarding.
    #[serde(rename = "no-target-404")]
    NoTarget404,
    /// RFC 3261 §11.2: a 2xx to OPTIONS describes the sender's capabilities
    /// (`Allow` / `Supported` / `Accept`). Charges the sender; advisory by
    /// consumer policy, since a transport-health OPTIONS probe answers without
    /// them.
    #[serde(rename = "options-response-echoes")]
    OptionsResponseEchoes,
    /// RFC 3261 §13.2.2.4: an ACK requires only option tags the INVITE it
    /// acknowledges required. Charges the ACK's sender.
    #[serde(rename = "ack-require-subset-of-invite")]
    AckRequireSubsetOfInvite,
    /// RFC 3261 §17.1.1.3: the ACK of a non-2xx final carries its INVITE's
    /// Route values — the two ride one transaction. Charges the ACK's sender.
    #[serde(rename = "ack-preserves-invite-route")]
    AckPreservesInviteRoute,
    /// RFC 3261 §16.4: a proxy forwarding a request whose topmost Route is
    /// strict swaps that URI into the Request-URI. Charges the proxy that took
    /// the request; scoped to declared proxies by consumer policy.
    #[serde(rename = "strict-route-rewrite-handled")]
    StrictRouteRewriteHandled,
    /// RFC 3261 §10.2: a UA changes an address-of-record's Contact only once
    /// its previous REGISTER for that record has been answered. Charges the
    /// REGISTER's sender.
    #[serde(rename = "serial-register")]
    SerialRegister,
    /// RFC 3261 §10.2: a REGISTER carries no Route header — registration forms
    /// no route set. Charges the REGISTER's sender.
    #[serde(rename = "register-no-route-set")]
    RegisterNoRouteSet,
    /// RFC 3261 §14.2: a UAS that takes a re-INVITE while another INVITE
    /// transaction of the same dialog is still in progress answers 491, or 500
    /// with `Retry-After`. Charges that UAS.
    #[serde(rename = "concurrent-re-invite-500-or-491")]
    ConcurrentReInvite500Or491,
    /// RFC 3261 §15: a BYE names a dialog that exists, and a callee does not
    /// BYE an early one — it CANCELs or answers 4xx/5xx/6xx. Charges the BYE's
    /// sender.
    #[serde(rename = "no-bye-outside-or-early-dialog")]
    NoByeOutsideOrEarlyDialog,
    /// RFC 3261 §14.1: a UAC holds one INVITE transaction outstanding per
    /// dialog DIRECTION — the next waits for the prior to reach Confirmed (RFC
    /// 6026 for a 2xx). Charges the UAC that sent the overtaking INVITE.
    #[serde(rename = "no-re-invite-while-invite-in-progress")]
    NoReInviteWhileInviteInProgress,
    /// RFC 3261 §16.7: a proxy that cannot answer an INVITE promptly emits 100
    /// Trying within its grace window. Charges the endpoint that took the
    /// INVITE; advisory by consumer policy, since a paused test clock advances
    /// virtual time no real latency corresponds to.
    #[serde(rename = "proxy-100-within-grace")]
    Proxy100WithinGrace,
    /// RFC 3261 §17.1.1.3: a non-2xx INVITE final is ACKed, and the ACK reaches
    /// the UAS that sent it. Charges that UAS — an un-ACKed reject retransmits
    /// to Timer H and its transaction never completes.
    #[serde(rename = "unacked-invite-non-2xx-final")]
    UnackedInviteNon2xxFinal,
    /// RFC 3261 §14.1 / §17.1.1.2: a re-INVITE that drew a provisional draws a
    /// final too — a failed one leaves the dialog in its prior state rather
    /// than silently ending it. Charges the re-INVITE's sender.
    #[serde(rename = "failed-reinvite-tears-down-dialog")]
    FailedReinviteTearsDownDialog,
    /// RFC 3261 §13.3.1.1 / §17.2.1: a server transaction that has answered
    /// emits no NEW provisional afterwards. Charges the transaction's UAS side;
    /// the general sibling of [`RuleId::SingleFinalPerServerTxn`].
    #[serde(rename = "no-1xx-after-final")]
    No1xxAfterFinal,
    /// RFC 3262 §3: a UAS handed an INVITE requiring `100rel` answers every
    /// non-100 provisional reliably, or rejects the INVITE 420 naming `100rel`
    /// Unsupported. Charges that UAS.
    #[serde(rename = "require-reliable-1xx-on-require")]
    RequireReliable1xxOnRequire,
    /// RFC 3262 §3: a UAS sends a reliable provisional only where the INVITE
    /// opted into `100rel`, in `Require` or in `Supported`. Charges that UAS;
    /// advisory by consumer policy, since a B2BUA may terminate PRACK on one
    /// leg under a negotiation the other leg carried.
    #[serde(rename = "reliable-needs-client-opt-in")]
    ReliableNeedsClientOptIn,
    /// RFC 3262 §3: the PRACK machinery is scoped to the INVITE METHOD — no
    /// reliable provisional answers a non-INVITE request; a re-INVITE is an
    /// INVITE. Charges the UAS that sent it. The token is a persisted identity
    /// (census, replay verdict registry) and does not track the rule's reading.
    #[serde(rename = "no-reliable-1xx-on-in-dialog")]
    NoReliable1xxOnInDialog,
    /// RFC 3262 §3: a proxy forwards a PRACK matching no reliable provisional
    /// of its own rather than absorbing it. Charges the endpoint that took the
    /// PRACK; advisory by consumer policy, since a per-leg PRACK terminator
    /// answers one it never provisioned.
    #[serde(rename = "unmatched-prack-proxied")]
    UnmatchedPrackProxied,
    /// RFC 3262 §3: a PRACK naming an `RSeq` its taker sent reliably draws a
    /// 2xx; one naming no such `RSeq` draws a 481. Charges the endpoint that
    /// answered the PRACK.
    #[serde(rename = "prack-2xx-or-481")]
    Prack2xxOr481,
    /// RFC 3262 §3: a UAS holds the INVITE 2xx while a reliable provisional of
    /// its own that carried a body is still unPRACKed. Charges that UAS.
    #[serde(rename = "delay-2xx-on-unacked-reliable-1xx-with-sdp")]
    Delay2xxOnUnackedReliable1xxWithSdp,
    /// RFC 3262 §3: a PRACK arriving after the INVITE final still draws a 2xx —
    /// the PRACK server transaction outlives the INVITE's. Charges the endpoint
    /// that answered it.
    #[serde(rename = "prack-accepted-after-final")]
    PrackAcceptedAfterFinal,
    /// RFC 3262 §3: a UAS emits no NEW reliable provisional on an INVITE
    /// transaction it has already answered. Charges that UAS; the reliable-only
    /// sibling of [`RuleId::No1xxAfterFinal`], keyed on `RSeq`.
    #[serde(rename = "no-new-reliable-1xx-after-final")]
    NoNewReliable1xxAfterFinal,
    /// RFC 3262 §4: a 100 Trying is never reliable, so a UAC never PRACKs one
    /// whatever `Require: 100rel` it carries. Charges the PRACK's sender.
    #[serde(rename = "no-prack-of-100-trying")]
    NoPrackOf100Trying,
    /// RFC 3262 §5: the PRACK of a reliable provisional that carried an OFFER
    /// carries the answer. Charges the PRACK's sender; advisory by consumer
    /// policy, since an offer and its answer can straddle two legs of a B2BUA.
    #[serde(rename = "prack-answers-1xx-offer")]
    PrackAnswers1xxOffer,
    /// RFC 3261 §13.2.2.4: an ACK closing a completed offer/answer exchange
    /// carries no body — the round has both halves and the ACK has no answer
    /// left to deliver. Charges the ACK's sender.
    #[serde(rename = "ack-body-after-complete-offer-answer")]
    AckBodyAfterCompleteOfferAnswer,
    /// RFC 3261 §13.2.1 / RFC 3264 §6: the non-failure final to a request that
    /// carried an offer carries the answer, unless a reliable provisional
    /// already stated one on that dialog. Charges the answerer.
    #[serde(rename = "final-2xx-answers-the-offer")]
    Final2xxAnswersTheOffer,
    /// RFC 3261 §13.2.1: one dialog carries ONE answer — a later description on
    /// that dialog re-states the transport plan the first stated, never another
    /// one, since the peer takes the first and ignores the rest. Charges the
    /// answerer.
    #[serde(rename = "second-answer-repeats-the-first")]
    SecondAnswerRepeatsTheFirst,
    /// RFC 3264 §6: the `m=` line an answer sends back for an offered stream
    /// re-states that stream's media type and transport — rejecting it means
    /// port 0, never re-typing it. Charges the answerer.
    #[serde(rename = "answer-stream-matches-offer")]
    AnswerStreamMatchesOffer,
    /// RFC 4566 §5.2 / RFC 3264 §8: every session description an agent sends on
    /// one call keeps the `o=` identity and moves `sess-version` by exactly
    /// what changed. Charges the description's sender; advisory by consumer
    /// policy, since a B2BUA re-offers under an origin of its own.
    #[serde(rename = "sdp-origin-continuity")]
    SdpOriginContinuity,
    /// RFC 3264 §5: an agent holds one offer of its own outstanding at a time
    /// on a call — the next waits for the answer to the last. Charges the
    /// offer's sender; advisory by consumer policy, since a B2BUA's answer
    /// arrives on the other leg.
    #[serde(rename = "no-new-offer-while-offer-pending")]
    NoNewOfferWhileOfferPending,
    /// RFC 3264 §6: an answer holds exactly one `m=` line per offered stream —
    /// a rejected stream keeps its slot at port 0, it never disappears.
    /// Charges the answerer.
    #[serde(rename = "answer-m-line-count-matches-offer")]
    AnswerMLineCountMatchesOffer,
    /// RFC 3264 §6: an answer repeats the offer's `t=` line — the two ends
    /// state one session time. Charges the answerer.
    #[serde(rename = "answer-t-line-equals-offer")]
    AnswerTLineEqualsOffer,
    /// RFC 3264 §6.1: the answer's `m=` line at each offered position states
    /// that position's media type — the per-stream pairing both ends index on.
    /// Charges the answerer.
    #[serde(rename = "answer-media-type-matches-offer")]
    AnswerMediaTypeMatchesOffer,
    /// RFC 3264 §6.1: an answer's direction attribute is one the offer's admits
    /// — `sendonly` draws `recvonly`/`inactive`, `inactive` draws `inactive`.
    /// Charges the answerer; advisory by consumer policy, since a B2BUA
    /// translates direction across legs as policy.
    #[serde(rename = "direction-pair-valid")]
    DirectionPairValid,
    /// RFC 3264 §6: an answer rejecting a stream still lists a media format on
    /// its `m=` line — a bare `m=audio 0 RTP/AVP` is no description at all.
    /// Charges the answerer.
    #[serde(rename = "rejected-stream-minimal-answer")]
    RejectedStreamMinimalAnswer,
    /// RFC 3264 §8: an agent's later description on a call carries at least as
    /// many `m=` lines as the one before — a removed stream keeps its slot at
    /// port 0. Charges the description's sender.
    #[serde(rename = "re-offer-m-line-count-monotonic")]
    ReOfferMLineCountMonotonic,
    /// RFC 3264 §8: a stream offered at port 0 is answered at port 0 — a
    /// disabled stream stays disabled. Charges the answerer; advisory by
    /// consumer policy, since a B2BUA anchors media and assigns its own ports.
    #[serde(rename = "zero-port-propagation")]
    ZeroPortPropagation,
    /// RFC 3264 §8.3.2: once an agent's description binds a payload type to an
    /// encoding, its later descriptions on that call keep the binding — the
    /// peer caches it. Charges the description's sender.
    #[serde(rename = "payload-type-mapping-stable")]
    PayloadTypeMappingStable,
    /// RFC 3261 §8.1.1.7: a request's top Via carries a `branch` beginning with
    /// the magic cookie `z9hG4bK`. Charges the sender that minted it.
    #[serde(rename = "branch-prefix")]
    BranchPrefix,
    /// RFC 3261 §8.1.1.6: a request carries a Max-Forwards in `0..=255`, and no
    /// higher than the recommended initial 70 — above it the count was minted,
    /// not decremented. Charges the sender.
    #[serde(rename = "max-forwards")]
    MaxForwards,
    /// RFC 3261 §20.14: a message's Content-Length equals the byte count of the
    /// body it carried. Charges the sender that wrote both.
    #[serde(rename = "content-length")]
    ContentLength,
    /// RFC 3261 §7.4.1: a message carrying a body names its format in
    /// Content-Type. Charges the sender.
    #[serde(rename = "content-type")]
    ContentType,
    /// RFC 3261 §8.1.1.8: a dialog-establishing request (INVITE, SUBSCRIBE)
    /// carries a Contact — the dialog's remote target. Charges the sender.
    #[serde(rename = "contact-presence")]
    ContactPresence,
    /// RFC 3261 §15.1: a BYE carries no Contact — it ends the dialog, so
    /// target-refresh means nothing on it. Charges the sender.
    #[serde(rename = "no-contact-on-bye")]
    NoContactOnBye,
    /// RFC 3261 §8.2.6.2: a UAS adds a To-tag to every response above 100, so
    /// the peer can dialog-match it. Charges the responding UAS.
    #[serde(rename = "to-tag-presence")]
    ToTagPresence,
    /// RFC 3261 §16.6: Record-Route is a PROXY mechanism, so a UA (a B2BUA
    /// included) inserts none. Charges the endpoint that inserted it; scoped to
    /// declared proxies by consumer policy.
    #[serde(rename = "no-record-route-from-ua")]
    NoRecordRouteFromUa,
    /// RFC 3261 §8.1.3 / §17.1.3: a response reproduces its request's Via stack
    /// unchanged, top branch included, so the UAC can match it to the client
    /// transaction. Charges the endpoint that sent the response; read at the
    /// endpoint whose transaction it claims.
    #[serde(rename = "response-echoes-request-via")]
    ResponseEchoesRequestVia,
    /// RFC 3261 §8.1.3.3: a response's `CSeq` echoes one of a request its taker
    /// actually sent — a `(number, method)` no sent request produced answers a
    /// phantom. Charges the endpoint that sent the response.
    #[serde(rename = "response-correlation")]
    ResponseCorrelation,
    /// RFC 3261 §12.2.1.1: an in-dialog message names the taker's own dialog —
    /// a request's To-tag and a response's From-tag are tags the taker minted.
    /// Charges the endpoint that sent it.
    #[serde(rename = "mid-dialog-tags")]
    MidDialogTags,
    /// RFC 3261 §12.2.1.1: a peer's From URI on its in-dialog requests stays the
    /// URI its dialog-creating INVITE stated. Charges the endpoint that
    /// rewrote it.
    #[serde(rename = "peer-uri-stable")]
    PeerUriStable,
    /// RFC 3261 §12.1: the Call-ID of a dialog is immutable, so every later
    /// in-dialog message carries the one its dialog-creating INVITE established.
    /// Charges the endpoint that changed it.
    #[serde(rename = "dialog-call-id-stable")]
    DialogCallIdStable,
    /// RFC 3261 §9.1: a CANCEL's Request-URI equals that of the INVITE it
    /// cancels. Charges the CANCEL's sender.
    #[serde(rename = "cancel-request-uri")]
    CancelRequestUri,
    /// RFC 3261 §9.1: a CANCEL's top Via branch is the branch of an INVITE its
    /// taker has open, so it reaches that server transaction. Charges the
    /// CANCEL's sender.
    #[serde(rename = "cancel-via-branch")]
    CancelViaBranch,
    /// RFC 3261 §17.2.1 / §12.1.1: a UAS that committed a To-tag on a
    /// provisional carries that same tag on the final of the transaction.
    /// Charges that UAS; advisory by consumer policy, since a forking B2BUA
    /// legitimately answers 2xx off a later early dialog.
    #[serde(rename = "tag-consistency")]
    TagConsistency,
    /// RFC 3262 §4: `Require: 100rel` rides an INVITE and nothing else — the
    /// reliable-provisional contract is the INVITE transaction's. Charges the
    /// sender that stamped it.
    #[serde(rename = "no-100rel-require-on-non-invite")]
    No100relRequireOnNonInvite,
    /// RFC 3262 §3: a 100 (Trying) carries no reliability marker, and a
    /// reliable 1xx carries an `RSeq` in `1..=2^31-1`. Charges the responding
    /// UAS, which writes both rows.
    #[serde(rename = "reliable-1xx-headers")]
    Reliable1xxHeaders,
    /// RFC 3261 §8.1.1.2: a request outside any dialog carries no To-tag —
    /// there is no peer yet to name. Charges the sender.
    #[serde(rename = "no-to-tag-on-initial-request")]
    NoToTagOnInitialRequest,
    /// RFC 3261 §12.2.1.1: a request sent inside a confirmed dialog carries
    /// that dialog's remote tag in To. Charges the sender that omitted it.
    #[serde(rename = "in-dialog-to-tag")]
    InDialogToTag,
    /// RFC 3261 §8.2.2.3: a CANCEL, and the ACK of a non-2xx final, state no
    /// `Require` / `Proxy-Require` — a transaction-management request imposes
    /// no extension. Charges the sender.
    #[serde(rename = "no-require-on-cancel-or-ack")]
    NoRequireOnCancelOrAck,
    /// RFC 3261 §9.1: a CANCEL's CSeq method token is `CANCEL`, whatever
    /// request it cancels. Charges the sender.
    #[serde(rename = "cancel-cseq-method")]
    CancelCseqMethod,
    /// RFC 3261 §16.6 step 6: a request forwarded through a strict-route next
    /// hop has had the Request-URI / Route swap applied, so its topmost Route
    /// is no longer a strict one. Charges the forwarding hop; scoped to
    /// declared proxies by consumer policy.
    #[serde(rename = "strict-route-shuffle-on-send")]
    StrictRouteShuffleOnSend,
    /// RFC 3264 §5-6 / RFC 4566 §5: a body sent as `application/sdp` satisfies
    /// the offer/answer grammar. Charges the sender that minted it.
    #[serde(rename = "sdp-body-parseable")]
    SdpBodyParseable,
    /// RFC 3264 §6 / §8.4: a stream held at `c=0.0.0.0` is not ALSO rejected at
    /// port 0 — the two together state no disposition a peer can act on.
    /// Charges the sender; scoped to the offering UAC by consumer policy.
    #[serde(rename = "c0-port-non-zero")]
    C0PortNonZero,
    /// RFC 3261 §9.1: a UAC CANCELs a client transaction that is still in
    /// flight — once a final has landed the transaction is completed
    /// (§17.1.1.2) and the CANCEL reaches no transaction at all (481, §9.2).
    /// Charges the CANCEL's sender.
    #[serde(rename = "no-cancel-after-final")]
    NoCancelAfterFinal,
    /// RFC 3261 §17 / §13.3.1.4, RFC 3262 §3: a retransmission is the SAME
    /// message, byte for byte — the request the client transaction re-sends,
    /// the final the server transaction re-passes, the 2xx and the reliable
    /// provisional their UAS re-sends. Charges the emitter of a copy whose
    /// bytes differ from the first under one transaction identity (ADR-0029 X3).
    #[serde(rename = "rung-byte-identical")]
    RungByteIdentical,
}

impl RuleId {
    /// Every rule with a body, in report order.
    pub const ALL: &'static [RuleId] = &[
        RuleId::No200AfterCancel,
        RuleId::UnackedReliableProvisional,
        RuleId::NoAckToDialogCreating2xx,
        RuleId::Unacked2xxNotCleared,
        RuleId::RackWithoutKnownInvite,
        RuleId::NoOverlappingReliableProvisionals,
        RuleId::NonContiguousRseq,
        RuleId::NoPrackOfOutOfOrderRseq,
        RuleId::SingleFinalPerServerTxn,
        RuleId::CancelRouteEchoesInvite,
        RuleId::CancelAfter1xx,
        RuleId::CseqInDialogOrder,
        RuleId::ResponseCseqMatchesTransaction,
        RuleId::AckCseqMatchesInvite,
        RuleId::MidDialogUri,
        RuleId::MidDialogRoute,
        RuleId::MidDialogWireDestination,
        RuleId::RecordRoutePlacement,
        RuleId::RportEcho,
        RuleId::AllowSupportedOnInvite,
        RuleId::Proxy100TryingNotForwarded,
        RuleId::UnknownDialog481,
        RuleId::UnsupportedMethod405Allow,
        RuleId::UnsupportedExtension420,
        RuleId::Unsupported415Accepts,
        RuleId::UnsupportedExtension421,
        RuleId::NoTarget404,
        RuleId::OptionsResponseEchoes,
        RuleId::AckRequireSubsetOfInvite,
        RuleId::AckPreservesInviteRoute,
        RuleId::StrictRouteRewriteHandled,
        RuleId::SerialRegister,
        RuleId::RegisterNoRouteSet,
        RuleId::ConcurrentReInvite500Or491,
        RuleId::NoByeOutsideOrEarlyDialog,
        RuleId::NoReInviteWhileInviteInProgress,
        RuleId::Proxy100WithinGrace,
        RuleId::UnackedInviteNon2xxFinal,
        RuleId::FailedReinviteTearsDownDialog,
        RuleId::No1xxAfterFinal,
        RuleId::RequireReliable1xxOnRequire,
        RuleId::ReliableNeedsClientOptIn,
        RuleId::NoReliable1xxOnInDialog,
        RuleId::UnmatchedPrackProxied,
        RuleId::Prack2xxOr481,
        RuleId::Delay2xxOnUnackedReliable1xxWithSdp,
        RuleId::PrackAcceptedAfterFinal,
        RuleId::NoNewReliable1xxAfterFinal,
        RuleId::NoPrackOf100Trying,
        RuleId::PrackAnswers1xxOffer,
        RuleId::AckBodyAfterCompleteOfferAnswer,
        RuleId::Final2xxAnswersTheOffer,
        RuleId::SecondAnswerRepeatsTheFirst,
        RuleId::AnswerStreamMatchesOffer,
        RuleId::SdpOriginContinuity,
        RuleId::NoNewOfferWhileOfferPending,
        RuleId::AnswerMLineCountMatchesOffer,
        RuleId::AnswerTLineEqualsOffer,
        RuleId::AnswerMediaTypeMatchesOffer,
        RuleId::DirectionPairValid,
        RuleId::RejectedStreamMinimalAnswer,
        RuleId::ReOfferMLineCountMonotonic,
        RuleId::ZeroPortPropagation,
        RuleId::PayloadTypeMappingStable,
        RuleId::BranchPrefix,
        RuleId::MaxForwards,
        RuleId::ContentLength,
        RuleId::ContentType,
        RuleId::ContactPresence,
        RuleId::NoContactOnBye,
        RuleId::ToTagPresence,
        RuleId::NoRecordRouteFromUa,
        RuleId::ResponseEchoesRequestVia,
        RuleId::ResponseCorrelation,
        RuleId::MidDialogTags,
        RuleId::PeerUriStable,
        RuleId::DialogCallIdStable,
        RuleId::CancelRequestUri,
        RuleId::CancelViaBranch,
        RuleId::TagConsistency,
        RuleId::No100relRequireOnNonInvite,
        RuleId::Reliable1xxHeaders,
        RuleId::CancelCseqMethod,
        RuleId::NoToTagOnInitialRequest,
        RuleId::InDialogToTag,
        RuleId::NoRequireOnCancelOrAck,
        RuleId::StrictRouteShuffleOnSend,
        RuleId::SdpBodyParseable,
        RuleId::C0PortNonZero,
        RuleId::NoCancelAfterFinal,
        RuleId::RungByteIdentical,
    ];

    /// The pivot §11.1 wire contract: the rules a pivot document may claim.
    /// Each member arrived with its detector, its conservatism and its corpus
    /// numbers; growing this list takes a census run, not a code move.
    pub const WIRE: &'static [RuleId] = &[
        RuleId::No200AfterCancel,
        RuleId::UnackedReliableProvisional,
        RuleId::NoAckToDialogCreating2xx,
        RuleId::NoCancelAfterFinal,
        RuleId::SecondAnswerRepeatsTheFirst,
    ];

    /// The rule's wire token — the same spelling serde uses.
    pub fn token(self) -> &'static str {
        match self {
            RuleId::No200AfterCancel => "no-200-after-cancel",
            RuleId::UnackedReliableProvisional => "unacked-reliable-provisional",
            RuleId::NoAckToDialogCreating2xx => "no-ack-to-dialog-creating-2xx",
            RuleId::Unacked2xxNotCleared => "unacked-2xx-not-cleared",
            RuleId::RackWithoutKnownInvite => "rack-without-known-invite",
            RuleId::NoOverlappingReliableProvisionals => "no-overlapping-reliable-provisionals",
            RuleId::NonContiguousRseq => "non-contiguous-rseq",
            RuleId::NoPrackOfOutOfOrderRseq => "no-prack-of-out-of-order-rseq",
            RuleId::SingleFinalPerServerTxn => "single-final-per-server-txn",
            RuleId::CancelRouteEchoesInvite => "cancel-route-echoes-invite",
            RuleId::CancelAfter1xx => "cancel-after-1xx",
            RuleId::CseqInDialogOrder => "cseq-in-dialog-order",
            RuleId::ResponseCseqMatchesTransaction => "response-cseq-matches-transaction",
            RuleId::AckCseqMatchesInvite => "ack-cseq-matches-invite",
            RuleId::MidDialogUri => "mid-dialog-uri",
            RuleId::MidDialogRoute => "mid-dialog-route",
            RuleId::MidDialogWireDestination => "mid-dialog-wire-destination",
            RuleId::RecordRoutePlacement => "record-route-placement",
            RuleId::RportEcho => "rport-echo",
            RuleId::AllowSupportedOnInvite => "allow-supported-on-invite",
            RuleId::Proxy100TryingNotForwarded => "proxy-100-trying-not-forwarded",
            RuleId::UnknownDialog481 => "unknown-dialog-481",
            RuleId::UnsupportedMethod405Allow => "unsupported-method-405-allow",
            RuleId::UnsupportedExtension420 => "unsupported-extension-420",
            RuleId::Unsupported415Accepts => "unsupported-415-accepts",
            RuleId::UnsupportedExtension421 => "unsupported-extension-421",
            RuleId::NoTarget404 => "no-target-404",
            RuleId::OptionsResponseEchoes => "options-response-echoes",
            RuleId::AckRequireSubsetOfInvite => "ack-require-subset-of-invite",
            RuleId::AckPreservesInviteRoute => "ack-preserves-invite-route",
            RuleId::StrictRouteRewriteHandled => "strict-route-rewrite-handled",
            RuleId::SerialRegister => "serial-register",
            RuleId::RegisterNoRouteSet => "register-no-route-set",
            RuleId::ConcurrentReInvite500Or491 => "concurrent-re-invite-500-or-491",
            RuleId::NoByeOutsideOrEarlyDialog => "no-bye-outside-or-early-dialog",
            RuleId::NoReInviteWhileInviteInProgress => "no-re-invite-while-invite-in-progress",
            RuleId::Proxy100WithinGrace => "proxy-100-within-grace",
            RuleId::UnackedInviteNon2xxFinal => "unacked-invite-non-2xx-final",
            RuleId::FailedReinviteTearsDownDialog => "failed-reinvite-tears-down-dialog",
            RuleId::No1xxAfterFinal => "no-1xx-after-final",
            RuleId::RequireReliable1xxOnRequire => "require-reliable-1xx-on-require",
            RuleId::ReliableNeedsClientOptIn => "reliable-needs-client-opt-in",
            RuleId::NoReliable1xxOnInDialog => "no-reliable-1xx-on-in-dialog",
            RuleId::UnmatchedPrackProxied => "unmatched-prack-proxied",
            RuleId::Prack2xxOr481 => "prack-2xx-or-481",
            RuleId::Delay2xxOnUnackedReliable1xxWithSdp => {
                "delay-2xx-on-unacked-reliable-1xx-with-sdp"
            }
            RuleId::PrackAcceptedAfterFinal => "prack-accepted-after-final",
            RuleId::NoNewReliable1xxAfterFinal => "no-new-reliable-1xx-after-final",
            RuleId::NoPrackOf100Trying => "no-prack-of-100-trying",
            RuleId::PrackAnswers1xxOffer => "prack-answers-1xx-offer",
            RuleId::AckBodyAfterCompleteOfferAnswer => "ack-body-after-complete-offer-answer",
            RuleId::Final2xxAnswersTheOffer => "final-2xx-answers-the-offer",
            RuleId::SecondAnswerRepeatsTheFirst => "second-answer-repeats-the-first",
            RuleId::AnswerStreamMatchesOffer => "answer-stream-matches-offer",
            RuleId::SdpOriginContinuity => "sdp-origin-continuity",
            RuleId::NoNewOfferWhileOfferPending => "no-new-offer-while-offer-pending",
            RuleId::AnswerMLineCountMatchesOffer => "answer-m-line-count-matches-offer",
            RuleId::AnswerTLineEqualsOffer => "answer-t-line-equals-offer",
            RuleId::AnswerMediaTypeMatchesOffer => "answer-media-type-matches-offer",
            RuleId::DirectionPairValid => "direction-pair-valid",
            RuleId::RejectedStreamMinimalAnswer => "rejected-stream-minimal-answer",
            RuleId::ReOfferMLineCountMonotonic => "re-offer-m-line-count-monotonic",
            RuleId::ZeroPortPropagation => "zero-port-propagation",
            RuleId::PayloadTypeMappingStable => "payload-type-mapping-stable",
            RuleId::BranchPrefix => "branch-prefix",
            RuleId::MaxForwards => "max-forwards",
            RuleId::ContentLength => "content-length",
            RuleId::ContentType => "content-type",
            RuleId::ContactPresence => "contact-presence",
            RuleId::NoContactOnBye => "no-contact-on-bye",
            RuleId::ToTagPresence => "to-tag-presence",
            RuleId::NoRecordRouteFromUa => "no-record-route-from-ua",
            RuleId::ResponseEchoesRequestVia => "response-echoes-request-via",
            RuleId::ResponseCorrelation => "response-correlation",
            RuleId::MidDialogTags => "mid-dialog-tags",
            RuleId::PeerUriStable => "peer-uri-stable",
            RuleId::DialogCallIdStable => "dialog-call-id-stable",
            RuleId::CancelRequestUri => "cancel-request-uri",
            RuleId::CancelViaBranch => "cancel-via-branch",
            RuleId::TagConsistency => "tag-consistency",
            RuleId::No100relRequireOnNonInvite => "no-100rel-require-on-non-invite",
            RuleId::Reliable1xxHeaders => "reliable-1xx-headers",
            RuleId::NoToTagOnInitialRequest => "no-to-tag-on-initial-request",
            RuleId::InDialogToTag => "in-dialog-to-tag",
            RuleId::NoRequireOnCancelOrAck => "no-require-on-cancel-or-ack",
            RuleId::CancelCseqMethod => "cancel-cseq-method",
            RuleId::StrictRouteShuffleOnSend => "strict-route-shuffle-on-send",
            RuleId::SdpBodyParseable => "sdp-body-parseable",
            RuleId::C0PortNonZero => "c0-port-non-zero",
            RuleId::NoCancelAfterFinal => "no-cancel-after-final",
            RuleId::RungByteIdentical => "rung-byte-identical",
        }
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

impl FromStr for RuleId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RuleId::ALL
            .iter()
            .copied()
            .find(|r| r.token() == s)
            .ok_or_else(|| format!("unknown RFC rule token: {s}"))
    }
}

/// What one occasion's wire proves. `Violated` carries the proof; `Compliant`
/// is an obligation observably met; `Undecidable` names why the observation
/// could not settle it. What to DO with each is consumer policy — the census
/// under-reports (no hit), a live gate may still refuse to pass — and the
/// population contract keeps that policy's cost visible.
#[derive(Debug, Clone)]
pub enum Decision {
    Violated(Evidence),
    Compliant,
    Undecidable(&'static str),
}

/// One occasion of one rule: WHO is charged, on which dialog and transaction,
/// and what the wire settled. `hits ⊆ decided ⊆ occasions` falls out of the
/// [`Decision`]: every finding is an occasion, non-`Undecidable` ones are
/// decided, `Violated` ones are hits.
#[derive(Debug, Clone)]
pub struct Finding {
    pub rule: RuleId,
    /// The endpoint charged: the one that emitted the offending message, or
    /// owed the one never emitted.
    pub emitter: Endpoint,
    /// The other side of the obligation: where the offending message went, or
    /// who was owed the one that never came.
    pub taker: Endpoint,
    /// CSeq NUMBER of the INVITE transaction the obligation rides.
    pub cseq: u32,
    /// The emitter forwarded the behaviour rather than originating it — a
    /// weaker finding, kept or dropped by consumer policy.
    pub relayed: bool,
    /// Index into the view's `msgs` of the message the occasion rests on, so
    /// reports read in observation order whichever rule decided them.
    pub anchor: usize,
    pub decision: Decision,
}

impl Finding {
    pub fn decided(&self) -> bool {
        !matches!(self.decision, Decision::Undecidable(_))
    }
    pub fn violated(&self) -> bool {
        matches!(self.decision, Decision::Violated(_))
    }
}

/// One rule's denominators over a set of findings.
#[derive(Debug, Clone, Copy, Default)]
pub struct Population {
    pub occasions: u64,
    pub decided: u64,
}

impl Population {
    /// Fold one finding in.
    pub fn count(&mut self, f: &Finding) {
        self.occasions += 1;
        if f.decided() {
            self.decided += 1;
        }
    }
}

/// The messages one rule's verdict rests on. Untagged: `rule` already
/// discriminates, and flattening keeps a serialized hit one flat JSON object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Evidence {
    /// The CANCEL the UAS took and the 2xx it sent afterwards.
    Cancelled {
        /// Index into the view's `msgs` of the CANCEL the emitter took.
        cancel_msg: usize,
        cancel_hop: usize,
        cancel_ts_us: u64,
        /// Index into the view's `msgs` of the 2xx the emitter then sent.
        response_msg: usize,
        response_hop: usize,
        response_ts_us: u64,
        status: u16,
        /// Capture-time distance between the two, microseconds. Small values
        /// are where a capture point away from the emitter could have ordered
        /// the two wrongly — see the census README.
        gap_us: u64,
    },
    /// The reliable provisional the UAC took and never PRACKed.
    Unacked {
        /// Index into the view's `msgs` of the provisional the emitter took.
        provisional_msg: usize,
        provisional_hop: usize,
        provisional_ts_us: u64,
        /// The provisional's `RSeq` — what the missing PRACK's RAck would name.
        rseq: u64,
        status: u16,
        /// How long the dialog stayed observably alive after the provisional
        /// arrived, microseconds: the time the UAC demonstrably had to PRACK
        /// in. At least the rule's window, or (in an open observation) there
        /// is no hit.
        window_us: u64,
    },
    /// The dialog-creating 2xx the UAC took and never ACKed.
    NoAck {
        /// Index into the view's `msgs` of the 2xx the emitter took.
        final_msg: usize,
        final_hop: usize,
        final_ts_us: u64,
        /// The dialog the 2xx confirms: the To tag half of the key the missing
        /// ACK would have carried (RFC 3261 §13.2.2.4).
        to_tag: String,
        status: u16,
        /// Deliveries of that same 2xx to the emitter AFTER the first — the
        /// UAS running its §13.3.1.4 ladder into an ACK that never comes.
        retransmits: u32,
        /// How long the OBSERVATION kept running after the 2xx arrived,
        /// microseconds. At least the rule's window, or (in an open
        /// observation) there is no hit: a shorter capture proves truncation.
        window_us: u64,
        /// How long the observation kept carrying the CHARGED endpoint's own
        /// traffic after the 2xx. Zero means it never showed that endpoint
        /// again — evidence about the vantage, and never a gate.
        emitter_window_us: u64,
        /// Capture-time distance from the 2xx to the first BYE on that dialog.
        /// §13.3.1.4 has an un-ACKed UAS give up after 64*T1 and BYE, so a BYE
        /// here is the dialog going on to die un-confirmed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bye_after_us: Option<u64>,
        /// Who sent that BYE, `ip:port`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bye_by: Option<String>,
    },
    /// The PRACK whose `RAck` CSeq-num names an INVITE its sender never opened.
    UnknownRack {
        /// Index into the view's `msgs` of the PRACK the emitter sent.
        prack_msg: usize,
        prack_hop: usize,
        prack_ts_us: u64,
        /// The `RAck` response-num — the RSeq it claims to acknowledge.
        rack_rseq: u64,
        /// The `RAck` CSeq-num — the INVITE it claims to ride.
        rack_cseq: u32,
        /// The INVITE CSeq numbers the emitter did open on this view, so the
        /// report names the value the sender should have copied.
        known_cseqs: Vec<u32>,
    },
    /// The reliable provisional a UAS sent while an earlier one of its own was
    /// still unPRACKed.
    Overlapping {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        provisional_msg: usize,
        provisional_hop: usize,
        provisional_ts_us: u64,
        rseq: u64,
        status: u16,
        /// The RSeq still awaiting its PRACK when this provisional went out.
        unacked_rseq: u64,
    },
    /// The reliable provisional whose `RSeq` does not continue the prior one.
    RseqGap {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        provisional_msg: usize,
        provisional_hop: usize,
        provisional_ts_us: u64,
        rseq: u64,
        status: u16,
        /// The RSeq of the emitter's previous reliable provisional on this
        /// dialog: the offending one owed `prior_rseq + 1`.
        prior_rseq: u64,
    },
    /// The PRACK a UAC sent for a reliable provisional that arrived out of
    /// RSeq order.
    OutOfOrderRack {
        /// Index into the view's `msgs` of the PRACK the emitter sent.
        prack_msg: usize,
        prack_hop: usize,
        prack_ts_us: u64,
        /// The out-of-order RSeq the PRACK acknowledges.
        rack_rseq: u64,
        /// The RSeq the emitter owed next when that provisional arrived — the
        /// gap it had to see filled first.
        expected_rseq: u64,
    },
    /// The 2xx the UAS emitted and then neither saw ACKed nor cleared.
    Uncleared {
        /// Index into the view's `msgs` of the 2xx the emitter sent.
        final_msg: usize,
        final_hop: usize,
        final_ts_us: u64,
        /// The dialog that 2xx confirmed — the To tag the emitter minted.
        to_tag: String,
        status: u16,
        /// How long the observation kept running after the 2xx, microseconds.
        window_us: u64,
    },
    /// The finals one server transaction emitted when they did not all agree.
    ///
    /// Last in the untagged enum and unambiguous either way: no earlier variant
    /// carries `first_status`/`divergent`, and this one carries none of their
    /// required keys (`cancel_msg`, `provisional_msg`, `prack_msg`,
    /// `final_msg`), so neither can absorb the other's payload.
    MultipleFinals {
        /// Index into the view's `msgs` of the first final the emitter sent —
        /// the status every legal retransmission repeats.
        first_msg: usize,
        first_hop: usize,
        first_ts_us: u64,
        first_status: u16,
        /// Index into the view's `msgs` of the first final that disagreed with
        /// it: where the transaction stopped having one answer.
        second_msg: usize,
        second_hop: usize,
        second_ts_us: u64,
        second_status: u16,
        /// Every status that disagreed with the first, in emission order and
        /// each recorded once — a retransmitted offending final never repeats
        /// here.
        divergent: Vec<u16>,
        /// Capture-time distance from the first final to the first divergent
        /// one, microseconds.
        gap_us: u64,
        /// The transaction the finals answer (RFC 3261 §17): its CSeq method,
        /// ASCII-uppercased, and its top-Via branch. Both are identity, and a
        /// report names them so the reader finds the transaction on the wire.
        method: String,
        branch: String,
    },
    /// The CANCEL whose Route values did not echo its INVITE's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `cancel_routes`/`invite_routes`, and it carries none of the required
    /// keys of any other variant (`response_msg`, `provisional_msg`,
    /// `prack_msg`, `final_msg`, `first_msg`, `since_invite_us`).
    CancelRouteDiverged {
        /// Index into the view's `msgs` of the CANCEL the emitter sent.
        cancel_msg: usize,
        cancel_hop: usize,
        cancel_ts_us: u64,
        /// Index into the view's `msgs` of the INVITE it should have echoed.
        invite_msg: usize,
        invite_hop: usize,
        invite_ts_us: u64,
        /// The Route rows each message carried, in wire order — what §9.1 has
        /// the CANCEL copy.
        cancel_routes: Vec<String>,
        invite_routes: Vec<String>,
        /// The top-Via branch the CANCEL shares with its INVITE (§9.1).
        branch: String,
    },
    /// The CANCEL sent before any provisional arrived and before the sender's
    /// own grace window had run out.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `since_invite_us`, and it carries none of the required keys of any
    /// other variant (`cancel_routes` included).
    EagerCancel {
        /// Index into the view's `msgs` of the CANCEL the emitter sent.
        cancel_msg: usize,
        cancel_hop: usize,
        cancel_ts_us: u64,
        /// Index into the view's `msgs` of the emitter's first INVITE on the
        /// branch — what the grace window is measured from.
        invite_msg: usize,
        invite_hop: usize,
        invite_ts_us: u64,
        /// Observation-time distance from that INVITE to the CANCEL,
        /// microseconds: under the rule's grace floor, or there is no hit.
        since_invite_us: u64,
        /// The top-Via branch the CANCEL shares with its INVITE (§9.1).
        branch: String,
    },
    /// The CANCEL sent for a transaction the emitter had already taken — and
    /// ACKed — a final response on.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `since_final_us`, and it carries none of the required keys of any other
    /// variant (`since_invite_us`, `invite_cseqs`, `to_tag` and `status`
    /// included).
    LateCancel {
        /// Index into the view's `msgs` of the CANCEL the emitter sent.
        cancel_msg: usize,
        cancel_hop: usize,
        cancel_ts_us: u64,
        /// Index into the view's `msgs` of the INVITE the CANCEL names — the
        /// emitter's LAST one ahead of it, since a serial hunt reuses the CSeq
        /// number for each fresh attempt.
        invite_msg: usize,
        invite_hop: usize,
        invite_ts_us: u64,
        /// Index into the view's `msgs` of the first final the emitter TOOK for
        /// that INVITE — where the transaction stopped being cancellable.
        final_msg: usize,
        final_hop: usize,
        final_ts_us: u64,
        final_status: u16,
        /// Index into the view's `msgs` of the emitter's own ACK for that final
        /// (§17.1.1.3) — the proof it had processed the final rather than
        /// crossed it in flight.
        ack_msg: usize,
        ack_hop: usize,
        ack_ts_us: u64,
        /// Observation-time distance from that final to the CANCEL,
        /// microseconds.
        since_final_us: u64,
    },
    /// The in-dialog request carrying a CSeq an earlier transaction on the same
    /// dialog had already spent.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `reuse_msg`/`spent_msg`, and it carries none of the required keys of any
    /// other variant (`cancel_msg`, `provisional_msg`, `prack_msg`,
    /// `final_msg`, `first_msg`, `skip_msg`, `mismatch_msg`, `ack_msg`).
    CseqReused {
        /// Index into the view's `msgs` of the reusing request the taker took.
        reuse_msg: usize,
        reuse_hop: usize,
        reuse_ts_us: u64,
        /// Index into the view's `msgs` of the request that spent the number
        /// first — the transaction whose CSeq this one failed to advance past.
        spent_msg: usize,
        /// The dialog CSeq number two distinct transactions carried.
        cseq: u32,
        /// The reusing request's method, as the wire spelled it.
        method: String,
        /// The two top-Via branches, which is what proves these are two
        /// transactions rather than one retransmission (§17.2.3 folds a repeat
        /// only when the branch repeats too). Empty where the vantage carried
        /// no branch.
        reuse_branch: String,
        spent_branch: String,
        /// The dialog the reuse rides: `from_tag` names the request stream,
        /// `to_tag` the dialog inside it (empty = the dialog-creating request,
        /// which has no To tag yet).
        from_tag: String,
        to_tag: String,
    },
    /// The in-dialog request whose CSeq does not continue the dialog's run —
    /// it skipped a number, or (where `cseq <= prior_cseq`) never advanced past
    /// the dialog-creating request at all.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `skip_msg`/`prior_cseq`, and it carries none of the required keys of any
    /// other variant (`reuse_msg` included).
    CseqNotContiguous {
        /// Index into the view's `msgs` of the offending request.
        skip_msg: usize,
        skip_hop: usize,
        skip_ts_us: u64,
        /// The CSeq that request carried; the dialog owed `prior_cseq + 1`.
        cseq: u32,
        /// The number the dialog had reached before it — an in-dialog
        /// predecessor, or the dialog-creating request's CSeq where this is the
        /// dialog's FIRST in-dialog request. `cseq <= prior_cseq` reads as "did
        /// not advance", anything higher as a skip.
        prior_cseq: u32,
        method: String,
        from_tag: String,
        to_tag: String,
    },
    /// The response whose CSeq names no request its transaction carried.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `mismatch_msg`/`txn_cseqs`, and it carries none of the required keys of
    /// any other variant.
    ResponseCseqUnmatched {
        /// Index into the view's `msgs` of the response the taker took.
        mismatch_msg: usize,
        mismatch_hop: usize,
        mismatch_ts_us: u64,
        status: u16,
        /// The `CSeq` the response carried, number and method.
        response_cseq: u32,
        response_method: String,
        /// The `"<number> <METHOD>"` pairs the requests on this transaction did
        /// carry, ascending — one of which §8.1.3.5 has the response copy.
        txn_cseqs: Vec<String>,
        /// The top-Via branch that names the transaction (RFC 3261 §17).
        branch: String,
    },
    /// The ACK whose CSeq number matches no INVITE its stream carried.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `ack_msg`/`invite_cseqs`, and it carries none of the required keys of
    /// any other variant.
    AckCseqUnmatched {
        /// Index into the view's `msgs` of the ACK the taker took.
        ack_msg: usize,
        ack_hop: usize,
        ack_ts_us: u64,
        /// The CSeq number the ACK carried.
        ack_cseq: u32,
        /// The INVITE CSeq numbers the stream had carried when the ACK arrived
        /// — one of which §13.2.2.4 has the ACK reuse.
        invite_cseqs: Vec<u32>,
        /// The From tag naming the request stream the ACK rides.
        from_tag: String,
    },
    /// The in-dialog request whose From or To URI is not the dialog's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `uri_msg`/`dialog_local_uri`, and it carries none of the required keys
    /// of any other variant.
    MidDialogUriChanged {
        /// Index into the view's `msgs` of the in-dialog request.
        uri_msg: usize,
        uri_hop: usize,
        uri_ts_us: u64,
        /// The request-line method, as the wire spelled it.
        method: String,
        /// The From URI the request carried, and the dialog's local URI it
        /// owed. Equal where only the To URI diverged.
        sent_from_uri: String,
        dialog_local_uri: String,
        /// The To URI the request carried, and the dialog's remote URI it
        /// owed. Equal where only the From URI diverged.
        sent_to_uri: String,
        dialog_remote_uri: String,
    },
    /// The in-dialog request that did not reproduce the dialog route set.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `route_msg`/`dialog_route_set`, and it carries none of the required keys
    /// of any other variant (`cancel_routes`/`invite_routes` included).
    MidDialogRouteDiverged {
        /// Index into the view's `msgs` of the in-dialog request.
        route_msg: usize,
        route_hop: usize,
        route_ts_us: u64,
        method: String,
        /// The dialog route set, in the order §12.2.1.1 has the request
        /// reproduce it (the establishment Record-Route stack, reversed for the
        /// UAC).
        dialog_route_set: Vec<String>,
        /// The Route hops the request carried, in wire order and comma folds
        /// split.
        sent_routes: Vec<String>,
        /// The dialog's first route is a LOOSE route (§12.2.1.1): the whole set
        /// rides the Route rows. False is the strict form (§16.12), where the
        /// first route URI becomes the Request-URI.
        loose_first_route: bool,
        /// The Request-URI the request carried — what §16.12 has carry the
        /// first strict route.
        request_uri: String,
        /// Position in `sent_routes` of the first hop that named a different
        /// `host:port` than the route set did, when the two sets are the same
        /// length and diverge hop-wise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_bad_hop: Option<usize>,
    },
    /// The in-dialog request whose bytes went somewhere its own routing did not
    /// name.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `wire_msg`/`sent_to`, and it carries none of the required keys of any
    /// other variant.
    MidDialogWireTargetDiverged {
        /// Index into the view's `msgs` of the in-dialog request.
        wire_msg: usize,
        wire_hop: usize,
        wire_ts_us: u64,
        method: String,
        /// Where the bytes actually went, `ip:port`.
        sent_to: String,
        /// The URI that named the destination, and the `host:port` RFC 3263 §4
        /// resolves it to.
        target_uri: String,
        target_host: String,
        target_port: u16,
        /// The target came from the topmost Route (§12.2.1.1); false means the
        /// route set was empty and the Request-URI named it (§8.1.2).
        from_route: bool,
    },
    /// The response carrying Record-Route where the route set is already fixed.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `record_route_msg`/`record_route`, and it carries none of the required
    /// keys of any other variant.
    RecordRouteMisplaced {
        /// Index into the view's `msgs` of the response.
        record_route_msg: usize,
        record_route_hop: usize,
        record_route_ts_us: u64,
        status: u16,
        /// The first Record-Route row the response carried — what a strict UAC
        /// would have to reconcile against a route set it may no longer change.
        record_route: String,
        /// The method of the request this response answers, empty where the
        /// response is a 100 (which is vestigial whatever it answers).
        request_method: String,
    },
    /// The response that did not echo the `rport` its request advertised.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `rport_msg`/`echoed_empty`, and it carries none of the required keys of
    /// any other variant.
    RportNotEchoed {
        /// Index into the view's `msgs` of the response.
        rport_msg: usize,
        rport_hop: usize,
        rport_ts_us: u64,
        status: u16,
        /// The method of the request that advertised the bare `;rport`.
        request_method: String,
        /// The top-Via branch the request and its response share (RFC 3261 §17).
        branch: String,
        /// The response kept an `rport` carrying no readable port, rather than
        /// dropping the parameter outright.
        echoed_empty: bool,
    },
    /// The re-INVITE or INVITE 2xx that advertised no capability set.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `capability_msg`/`missing`, and it carries none of the required keys of
    /// any other variant.
    CapabilitiesNotAdvertised {
        /// Index into the view's `msgs` of the message that advertised none.
        capability_msg: usize,
        capability_hop: usize,
        capability_ts_us: u64,
        /// The header names that were absent, in report order: `Allow`
        /// (§13.2.1), `Supported` (§20.37), or both.
        missing: Vec<String>,
        /// The response status, or 0 where the message is the re-INVITE.
        status: u16,
    },
    /// The 100 Trying in excess of the INVITEs the transaction was sent.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `trying_msg`/`trying_taken`, and it carries none of the required keys of
    /// any other variant.
    ExtraTryingForwarded {
        /// Index into the view's `msgs` of the FIRST 100 in excess — where the
        /// count stopped being owed.
        trying_msg: usize,
        trying_hop: usize,
        trying_ts_us: u64,
        /// How many 100s the taker took on this transaction in the whole view.
        trying_taken: u32,
        /// How many INVITEs it sent — each owed one replayed 100 (§17.2.1).
        invites_sent: u32,
    },
    /// The in-dialog request that named a dialog its taker never confirmed.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `unknown_dialog_msg`/`answered_status`, and it carries none of the
    /// required keys of any other variant (`uri_msg`, `reuse_msg`, `skip_msg`,
    /// `ack_msg` included).
    UnknownDialogRequest {
        /// Index into the view's `msgs` of the request the emitter took.
        unknown_dialog_msg: usize,
        unknown_dialog_hop: usize,
        unknown_dialog_ts_us: u64,
        /// The request-line method, as the wire spelled it.
        method: String,
        /// The dialog the request named: `from_tag` is the PEER's half — the
        /// one the taker never minted and cannot match — and `to_tag` the
        /// taker's own.
        from_tag: String,
        to_tag: String,
        /// The final the taker answered with, or 0 where it answered none —
        /// §12.2.2 owes 481, and anything else (silence included) is the miss.
        answered_status: u16,
    },
    /// The request whose method or `Require` tag the taker did not recognise
    /// and did not reject the way §8.2 states.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `rejection_msg`/`listed_rows`, and it carries none of the required keys
    /// of any other variant (`unknown_dialog_msg` included).
    RejectionNotIssued {
        /// Index into the view's `msgs` of the request the emitter took.
        rejection_msg: usize,
        rejection_hop: usize,
        rejection_ts_us: u64,
        /// The request-line method, as the wire spelled it.
        method: String,
        /// The top-Via branch that names the transaction (RFC 3261 §17).
        branch: String,
        /// The `Require` option tags the taker does not support, empty where
        /// the unrecognised thing is the METHOD rather than an extension.
        unsupported_tags: Vec<String>,
        /// The final the taker answered with, or 0 where it answered none.
        answered_status: u16,
        /// Rows of the header the correct rejection owed (`Allow` on a 405,
        /// `Unsupported` on a 420) that the answer actually carried.
        listed_rows: u32,
    },
    /// The response that carried none of the headers its status owes.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `response_headers_msg`, and it carries none of the required keys of any
    /// other variant — `missing` alone cannot absorb it into
    /// `CapabilitiesNotAdvertised`, which requires `capability_msg`.
    ResponseHeadersMissing {
        /// Index into the view's `msgs` of the response the emitter sent.
        response_headers_msg: usize,
        response_headers_hop: usize,
        response_headers_ts_us: u64,
        status: u16,
        /// The header names the status owes, in report order. The response
        /// carried NONE of them — the obligation is met by any one.
        missing: Vec<String>,
        /// The top-Via branch that names the transaction (RFC 3261 §17).
        branch: String,
    },
    /// The error final a proxy authored for a request it never forwarded.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `no_target_msg`, and it carries none of the required keys of any other
    /// variant (`rejection_msg`, `extensions_msg`, `response_headers_msg`
    /// included).
    NoTargetFinal {
        /// Index into the view's `msgs` of the request the proxy took — the
        /// one it resolved no target for.
        no_target_msg: usize,
        no_target_hop: usize,
        no_target_ts_us: u64,
        /// The request-line method, as the wire spelled it.
        method: String,
        /// The top-Via branch that names the server transaction (RFC 3261 §17).
        branch: String,
        /// The 4xx/5xx/6xx the proxy answered instead of the 404 §16.3 owes.
        status: u16,
    },
    /// The ACK requiring an option tag its own INVITE never did.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `ack_require_msg`/`extra_tags`, and it carries none of the required keys
    /// of any other variant — every other variant naming an `invite_msg` also
    /// requires a `cancel_msg` or an `ack_route_msg` this one lacks.
    AckRequireNotSubset {
        /// Index into the view's `msgs` of the ACK the emitter sent.
        ack_require_msg: usize,
        ack_require_hop: usize,
        ack_require_ts_us: u64,
        /// Index into the view's `msgs` of the INVITE it acknowledges — the
        /// offer whose option tags bound it.
        invite_msg: usize,
        /// The option tags each message required, in wire order.
        ack_tags: Vec<String>,
        invite_tags: Vec<String>,
        /// Those the ACK required that its INVITE did not — never empty on a
        /// hit.
        extra_tags: Vec<String>,
        /// The top-Via branch the ACK shares with its INVITE (§17.1.1.3).
        branch: String,
    },
    /// The non-2xx ACK whose Route values did not state its INVITE's path.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `ack_route_msg`/`ack_routes`, and it carries none of the required keys
    /// of any other variant — `CancelRouteDiverged` requires
    /// `cancel_msg`/`cancel_routes`, which this one has not.
    AckRouteDiverged {
        /// Index into the view's `msgs` of the ACK the emitter sent.
        ack_route_msg: usize,
        ack_route_hop: usize,
        ack_route_ts_us: u64,
        /// Index into the view's `msgs` of the INVITE it should have echoed.
        invite_msg: usize,
        invite_hop: usize,
        invite_ts_us: u64,
        /// The Route rows each message carried, in wire order.
        ack_routes: Vec<String>,
        invite_routes: Vec<String>,
        /// The top-Via branch the ACK shares with its INVITE (§17.1.1.3).
        branch: String,
    },
    /// The strict-routed request a proxy took and did not rewrite on its way
    /// out.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `strict_route_msg`/`first_route`, and it carries none of the required
    /// keys of any other variant (`no_target_msg`, `rejection_msg`,
    /// `ack_route_msg` included).
    StrictRouteNotRewritten {
        /// Index into the view's `msgs` of the request the proxy took.
        strict_route_msg: usize,
        strict_route_hop: usize,
        strict_route_ts_us: u64,
        /// The request-line method, as the wire spelled it.
        method: String,
        /// The top-Via branch the incoming request named (RFC 3261 §17), which
        /// is what pairs it with the copy the proxy forwarded.
        branch: String,
        /// The topmost Route URI — what §16.4 has the forward carry as its
        /// Request-URI.
        first_route: String,
        /// The Request-URI the forward carried, empty where the proxy forwarded
        /// nothing on that transaction.
        forwarded_request_uri: String,
    },
    /// The REGISTER stating a route set registration never forms.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `register_route_msg`/`routes`, and it carries none of the required keys
    /// of any other variant.
    RegisterCarriesRoute {
        /// Index into the view's `msgs` of the REGISTER the emitter sent.
        register_route_msg: usize,
        register_route_hop: usize,
        register_route_ts_us: u64,
        /// The Route rows it carried, in wire order — §10.2 owes none.
        routes: Vec<String>,
    },
    /// The REGISTER that changed an address-of-record's Contact while the
    /// sender's previous REGISTER for it was still unanswered.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `concurrent_register_msg`/`aor`, and it carries none of the required
    /// keys of any other variant (`register_route_msg` included).
    ConcurrentRegister {
        /// Index into the view's `msgs` of the REGISTER the emitter sent.
        concurrent_register_msg: usize,
        concurrent_register_hop: usize,
        concurrent_register_ts_us: u64,
        /// The address-of-record both REGISTERs bind — the To URI (§10.2).
        aor: String,
        /// The Contact rows this REGISTER asked for, and those the outstanding
        /// one asked for. They differ, or there is no hit.
        contact: String,
        pending_contact: String,
        /// The two transactions, by top-Via branch: this REGISTER's, and the
        /// one still in flight.
        branch: String,
        pending_branch: String,
        /// Index into the view's `msgs` of that outstanding REGISTER.
        pending_msg: usize,
    },
    /// The re-INVITE that raced a pending INVITE transaction and was not met
    /// with the §14.2 answer.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `concurrent_invite_msg`/`retry_after`, and it carries none of the
    /// required keys of any other variant — `ConcurrentRegister` requires
    /// `concurrent_register_msg`/`aor`/`contact`, which this one has not.
    ConcurrentReInvite {
        /// Index into the view's `msgs` of the re-INVITE the taker took.
        concurrent_invite_msg: usize,
        concurrent_invite_hop: usize,
        concurrent_invite_ts_us: u64,
        /// The top-Via branch of the racing re-INVITE, and of the INVITE
        /// transaction still in progress when it arrived (RFC 3261 §17).
        branch: String,
        pending_invite_branch: String,
        /// Index into the view's `msgs` of that pending INVITE.
        pending_invite_msg: usize,
        /// The FINAL the taker answered the racing re-INVITE with, or 0 where
        /// it answered none — §14.2 owes 491, or 500 with `Retry-After`.
        answered_status: u16,
        /// That answer carried a `Retry-After`, which §14.2 requires of a 500.
        retry_after: bool,
    },
    /// The BYE that named no dialog, or an early one its own sender was the
    /// callee of.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `bye_msg`/`early_dialog`, and it carries none of the required keys of
    /// any other variant.
    ByeOffDialog {
        /// Index into the view's `msgs` of the BYE the emitter sent.
        bye_msg: usize,
        bye_hop: usize,
        bye_ts_us: u64,
        /// The dialog the BYE claimed: both halves as it spelled them. An
        /// empty `to_tag` is the "no dialog at all" shape.
        from_tag: String,
        to_tag: String,
        /// The sender is the CALLEE of a dialog it never accepted — it answered
        /// the establishing INVITE but never with a 2xx, so §15 has it CANCEL
        /// or reject rather than BYE. False is the no-dialog shape.
        early_dialog: bool,
    },
    /// The in-dialog INVITE sent while the sender's own prior INVITE on that
    /// dialog direction was still outstanding.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `overtaking_invite_msg`/`prior_accepted`, and it carries none of the
    /// required keys of any other variant (`concurrent_invite_msg` included).
    OverlappingReInvite {
        /// Index into the view's `msgs` of the overtaking INVITE.
        overtaking_invite_msg: usize,
        overtaking_invite_hop: usize,
        overtaking_invite_ts_us: u64,
        /// The two transactions, by top-Via branch: the overtaking one and the
        /// one still outstanding.
        branch: String,
        prior_branch: String,
        /// Index into the view's `msgs` of that outstanding INVITE.
        prior_msg: usize,
        /// The prior transaction was *Accepted* — its 2xx had arrived and the
        /// sender's ACK had not gone out (RFC 6026). False means it had drawn
        /// no final at all.
        prior_accepted: bool,
    },
    /// The INVITE a hop took and neither answered promptly nor acknowledged
    /// with a 100 Trying.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `trying_owed_msg`/`grace_us`, and it carries none of the required keys
    /// of any other variant (`trying_msg` of `ExtraTryingForwarded` included).
    TryingNotSentInGrace {
        /// Index into the view's `msgs` of the INVITE the emitter took.
        trying_owed_msg: usize,
        trying_owed_hop: usize,
        trying_owed_ts_us: u64,
        /// The top-Via branch that names the server transaction (RFC 3261 §17).
        branch: String,
        /// Observation-time distance from that INVITE to the emitter's first
        /// final on the transaction, microseconds — absent where it sent none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_final_after_us: Option<u64>,
        /// The grace the 100 was owed within, microseconds.
        grace_us: u64,
    },
    /// The non-2xx INVITE final whose ACK never reached the UAS that sent it.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `reject_msg`, and it carries none of the required keys of any other
    /// variant — every other variant naming an `invite_msg` also requires a
    /// `cancel_msg`, an `ack_require_msg` or an `ack_route_msg` this one lacks.
    UnackedReject {
        /// Index into the view's `msgs` of the non-2xx final the emitter sent.
        reject_msg: usize,
        reject_hop: usize,
        reject_ts_us: u64,
        status: u16,
        /// Index into the view's `msgs` of the INVITE it answers — the
        /// transaction whose ACK never came.
        invite_msg: usize,
        /// The top-Via branch the final shares with that INVITE and with the
        /// ACK §17.1.1.3 owes (all three ride one transaction).
        branch: String,
        /// How long the observation kept running after the final,
        /// microseconds. At least the rule's window, or there is no hit.
        window_us: u64,
    },
    /// The re-INVITE that drew a provisional and then nothing, on a dialog
    /// nothing else ended.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `abandoned_invite_msg`/`provisional_status`, and it carries none of the
    /// required keys of any other variant (`overtaking_invite_msg` and
    /// `concurrent_invite_msg` included).
    AbandonedReInvite {
        /// Index into the view's `msgs` of the re-INVITE the emitter sent.
        abandoned_invite_msg: usize,
        abandoned_invite_hop: usize,
        abandoned_invite_ts_us: u64,
        /// The top-Via branch that names the transaction (RFC 3261 §17).
        branch: String,
        /// Index into the view's `msgs` of the last provisional it drew, and
        /// that provisional's status — the proof the transaction was live.
        provisional_msg: usize,
        provisional_status: u16,
        /// How long the observation kept running after that provisional,
        /// microseconds. At least the rule's window, or there is no hit.
        window_us: u64,
    },
    /// The provisional a server transaction emitted after it had answered.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `late_provisional_msg`/`completed_by_msg`, and it carries none of the
    /// required keys of any other variant (`provisional_msg` of `Unacked` /
    /// `Overlapping` / `RseqGap` / `AbandonedReInvite` included, each of which
    /// requires an `rseq` or an `abandoned_invite_msg` this one lacks).
    LateProvisional {
        /// Index into the view's `msgs` of the provisional the emitter sent
        /// after the transaction was complete.
        late_provisional_msg: usize,
        late_provisional_hop: usize,
        late_provisional_ts_us: u64,
        status: u16,
        /// The early dialog it presents — the To tag that, with the status,
        /// tells a NEW provisional from a retransmitted one.
        to_tag: String,
        /// Index into the view's `msgs` of the final that completed the
        /// transaction, and that final's status.
        completed_by_msg: usize,
        completed_by_status: u16,
        /// Observation-time distance from that final to this provisional,
        /// microseconds.
        gap_us: u64,
        /// The top-Via branch that names the server transaction (RFC 3261 §17).
        branch: String,
    },
    /// The plain provisional a UAS sent to an INVITE that required `100rel`,
    /// having rejected it with no 420 either.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `unreliable_1xx_msg`, and it carries none of the required keys of any
    /// other variant — every other variant naming an `invite_msg` also requires
    /// a `cancel_msg`, an `ack_require_msg`, an `ack_route_msg` or a
    /// `reject_msg` this one lacks.
    Unreliable1xx {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        unreliable_1xx_msg: usize,
        unreliable_1xx_hop: usize,
        unreliable_1xx_ts_us: u64,
        status: u16,
        /// Index into the view's `msgs` of the INVITE that required `100rel` —
        /// the demand this response answered without honouring.
        invite_msg: usize,
        /// The top-Via branch the INVITE shares with this response (§17).
        branch: String,
    },
    /// The reliable provisional a UAS sent to an INVITE that opted into
    /// `100rel` in neither `Require` nor `Supported`.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `unsolicited_1xx_msg`, and it carries none of the required keys of any
    /// other variant (`unreliable_1xx_msg` included).
    UnsolicitedReliable1xx {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        unsolicited_1xx_msg: usize,
        unsolicited_1xx_hop: usize,
        unsolicited_1xx_ts_us: u64,
        status: u16,
        /// Index into the view's `msgs` of the INVITE it answers — the one that
        /// licensed no PRACK machinery.
        invite_msg: usize,
        /// The top-Via branch the INVITE shares with this response (§17).
        branch: String,
    },
    /// The reliable provisional a UAS sent to a request that already named a
    /// dialog.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `in_dialog_1xx_msg`/`request_msg`, and it carries none of the required
    /// keys of any other variant.
    InDialogReliable1xx {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        in_dialog_1xx_msg: usize,
        in_dialog_1xx_hop: usize,
        in_dialog_1xx_ts_us: u64,
        status: u16,
        /// The request-line method of the in-dialog request it answers, as the
        /// wire spelled it.
        method: String,
        /// Index into the view's `msgs` of that request.
        request_msg: usize,
        /// The top-Via branch the request shares with this response (§17).
        branch: String,
    },
    /// The PRACK an endpoint took, matched to no reliable provisional of its
    /// own and forwarded nowhere.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `absorbed_prack_msg`/`rack_method`, and it carries none of the required
    /// keys of any other variant (`prack_msg` of `UnknownRack` /
    /// `OutOfOrderRack` included).
    PrackAbsorbed {
        /// Index into the view's `msgs` of the PRACK the emitter took.
        absorbed_prack_msg: usize,
        absorbed_prack_hop: usize,
        absorbed_prack_ts_us: u64,
        /// The `RAck` triple the PRACK named: response-num, CSeq-num and
        /// method (RFC 3262 §7.2), the method ASCII-uppercased.
        rack_rseq: u64,
        rack_cseq: u32,
        rack_method: String,
        /// The `RSeq`s this endpoint took on provisionals of the call, ascending
        /// — none of which the PRACK names.
        known_rseqs: Vec<u64>,
    },
    /// The response an endpoint gave a PRACK that its own `RSeq` state does not
    /// justify.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `prack_answer_msg`/`rack_matched`, and it carries none of the required
    /// keys of any other variant.
    PrackAnsweredWrongly {
        /// Index into the view's `msgs` of the response the emitter sent.
        prack_answer_msg: usize,
        prack_answer_hop: usize,
        prack_answer_ts_us: u64,
        /// The status it answered with — §3 owes a 2xx on a match and a 481
        /// without one.
        status: u16,
        /// Index into the view's `msgs` of the PRACK it answers.
        answered_prack_msg: usize,
        /// The `RAck` response-num that PRACK named, and whether it matched an
        /// `RSeq` this endpoint had already sent reliably on the call.
        rack_rseq: u64,
        rack_matched: bool,
        /// The top-Via branch the PRACK shares with this response (§17).
        branch: String,
    },
    /// The INVITE 2xx a UAS sent while a reliable provisional of its own that
    /// carried a body was still unPRACKed.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `early_2xx_msg`/`unacked_rseqs`, and it carries none of the required keys
    /// of any other variant.
    AnsweredOverUnackedOffer {
        /// Index into the view's `msgs` of the 2xx the emitter sent.
        early_2xx_msg: usize,
        early_2xx_hop: usize,
        early_2xx_ts_us: u64,
        status: u16,
        /// Every `RSeq` still awaiting its PRACK when the 2xx went out, in
        /// emission order and never empty on a hit.
        unacked_rseqs: Vec<u64>,
        /// Index into the view's `msgs` of the first of those provisionals.
        offer_1xx_msg: usize,
        /// The top-Via branch the provisional shares with the 2xx (§17).
        branch: String,
    },
    /// The PRACK that arrived after the INVITE final and drew something other
    /// than a 2xx.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `late_prack_answer_msg`/`late_prack_msg`, and it carries none of the
    /// required keys of any other variant (`prack_answer_msg` included).
    LatePrackRejected {
        /// Index into the view's `msgs` of the response the emitter sent.
        late_prack_answer_msg: usize,
        late_prack_answer_hop: usize,
        late_prack_answer_ts_us: u64,
        status: u16,
        /// Index into the view's `msgs` of the PRACK it answers.
        late_prack_msg: usize,
        /// Index into the view's `msgs` of the INVITE final already sent, and
        /// its status — what made this PRACK late.
        prior_final_msg: usize,
        prior_final_status: u16,
        /// The top-Via branch the PRACK shares with this response (§17).
        branch: String,
    },
    /// The NEW reliable provisional a UAS sent on an INVITE transaction it had
    /// already answered.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `stray_1xx_msg`, and it carries none of the required keys of any other
    /// variant — `LatePrackRejected` requires `late_prack_answer_msg` and
    /// `late_prack_msg`, which this one has not.
    Reliable1xxAfterFinal {
        /// Index into the view's `msgs` of the provisional the emitter sent.
        stray_1xx_msg: usize,
        stray_1xx_hop: usize,
        stray_1xx_ts_us: u64,
        status: u16,
        /// The `RSeq` it carried — the one this transaction had never used.
        rseq: u64,
        /// Index into the view's `msgs` of the final that had already answered
        /// the transaction, and its status.
        prior_final_msg: usize,
        prior_final_status: u16,
        /// The top-Via branch that names the server transaction (§17).
        branch: String,
    },
    /// The PRACK a UAC sent for an `RSeq` a 100 Trying carried.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `trying_prack_msg`, and it carries none of the required keys of any
    /// other variant — `ExtraTryingForwarded` requires `trying_hop`,
    /// `trying_taken` and `invites_sent`, which this one has not.
    PrackedTrying {
        /// Index into the view's `msgs` of the PRACK the emitter sent.
        trying_prack_msg: usize,
        trying_prack_hop: usize,
        trying_prack_ts_us: u64,
        /// The `RSeq` the 100 Trying carried and the PRACK's `RAck` named.
        rseq: u64,
        /// Index into the view's `msgs` of that 100 Trying.
        trying_msg: usize,
    },
    /// The bodiless PRACK answering a reliable provisional that carried the
    /// offer.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `bodiless_prack_msg`, and it carries none of the required keys of any
    /// other variant — `AnsweredOverUnackedOffer` requires `early_2xx_msg` and
    /// `unacked_rseqs`, which this one has not.
    PrackWithoutAnswer {
        /// Index into the view's `msgs` of the PRACK the emitter sent.
        bodiless_prack_msg: usize,
        bodiless_prack_hop: usize,
        bodiless_prack_ts_us: u64,
        /// The `RAck` response-num it named — the offer it left unanswered.
        rack_rseq: u64,
        /// Index into the view's `msgs` of the provisional that carried that
        /// offer.
        offer_1xx_msg: usize,
    },
    /// The ACK that carried a session description onto an offer/answer round
    /// already holding both halves.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `ack_body_msg`/`streams`, so no other payload deserializes into it; and
    /// every other variant requires a key it has not (`ack_msg`,
    /// `ack_require_msg`, `ack_route_msg`, `bodiless_prack_msg`, …), so none
    /// absorbs this one.
    AckBodyOnClosedRound {
        /// Index into the view's `msgs` of the ACK the emitter sent.
        ack_body_msg: usize,
        ack_body_hop: usize,
        ack_body_ts_us: u64,
        /// Index into the view's `msgs` of the description that opened the
        /// round, and of the one that closed it — the two halves that left the
        /// ACK nothing to deliver.
        offer_msg: usize,
        answer_msg: usize,
        /// The ACK body's stream table as `<media>/<transport>` rows, in
        /// document order. Empty where the description holds no `m=` line.
        streams: Vec<String>,
    },
    /// The non-failure final an answerer sent on a round it took an offer for,
    /// carrying no session description while none bound the dialog yet.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `unanswered_final_msg`/`offered_streams`, so no other payload
    /// deserializes into it; and every other variant requires a key it has not
    /// (`ack_body_msg`, `second_answer_msg`, `answer_stream_msg` included).
    OfferLeftUnanswered {
        /// Index into the view's `msgs` of the description-less final the
        /// emitter sent.
        unanswered_final_msg: usize,
        unanswered_final_hop: usize,
        unanswered_final_ts_us: u64,
        status: u16,
        /// Index into the view's `msgs` of the description that opened the
        /// round this final closes.
        offer_msg: usize,
        /// The offer's stream table as `<media>/<transport>` rows, in document
        /// order — what the final left unanswered. Empty where the offer holds
        /// no `m=` line.
        offered_streams: Vec<String>,
    },
    /// The second description an answerer put on one dialog, stating a
    /// transport plan its first answer did not.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `second_answer_msg`/`first_plan`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not (`ack_body_msg`,
    /// `answer_stream_msg`, `m_count_msg` included).
    SecondAnswerDiverged {
        /// Index into the view's `msgs` of the second answer the emitter sent.
        second_answer_msg: usize,
        second_answer_hop: usize,
        second_answer_ts_us: u64,
        /// Index into the view's `msgs` of the description that opened the
        /// round, and of the answer that closed it on this dialog — the plan
        /// the peer is acting on.
        offer_msg: usize,
        first_answer_msg: usize,
        /// Each answer's transport plan: the session `c=` line, then one row
        /// per stream as `<media> <port> <transport> <formats>` with the
        /// stream's own `c=` where it states one. They differ, or there is no
        /// hit.
        first_plan: Vec<String>,
        second_plan: Vec<String>,
    },
    /// The answer that gave an offered stream a different media type or
    /// transport.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `answer_stream_msg`/`stream_indexes`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not
    /// (`ack_body_msg` included — the only other variant naming an `offer_msg`
    /// also requires that one).
    AnswerStreamRetyped {
        /// Index into the view's `msgs` of the answer the emitter sent.
        answer_stream_msg: usize,
        answer_stream_hop: usize,
        answer_stream_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// The offered stream positions the answer re-typed or re-transported,
        /// ascending and never empty on a hit.
        stream_indexes: Vec<usize>,
        /// Per entry of `stream_indexes`, that stream's `"<media> <port>
        /// <proto>"` as the offer and as the answer spelled it (the port `?`
        /// where the `m=` line carried no readable one). The three vectors are
        /// the same length and read together.
        offered: Vec<String>,
        answered: Vec<String>,
    },
    /// The session description whose `o=` line does not continue the sender's
    /// own on that call.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `origin_msg`/`prior_origin_line`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not (`ack_body_msg`,
    /// `answer_stream_msg`, `offer_msg` included).
    SdpOriginDiverged {
        /// Index into the view's `msgs` of the description the emitter sent.
        origin_msg: usize,
        origin_hop: usize,
        origin_ts_us: u64,
        /// Index into the view's `msgs` of the emitter's previous description
        /// with a readable `o=` line on this call.
        prior_origin_msg: usize,
        /// The two `o=` lines as the wire spelled them, leading `o=` included.
        origin_line: String,
        prior_origin_line: String,
        /// The five identity fields agree — the offence is the version alone.
        /// False means the description names a different session outright.
        same_session: bool,
        /// Everything BUT the `o=` line changed, which is what §8 has the
        /// version count: a changed description owes `prior + 1`, an unchanged
        /// one owes `prior`.
        body_changed: bool,
        session_version: u64,
        prior_session_version: u64,
    },
    /// The offer an agent sent while an offer of its own was still unanswered.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `new_offer_msg`/`pending_offer_msg`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not
    /// (`ack_body_msg`, `answer_stream_msg`, `origin_msg`, `offer_msg`
    /// included — the variants naming an `offer_msg` all require one of those).
    OfferWhilePending {
        /// Index into the view's `msgs` of the offer the emitter sent.
        new_offer_msg: usize,
        new_offer_hop: usize,
        new_offer_ts_us: u64,
        /// Index into the view's `msgs` of the emitter's own earlier offer that
        /// had drawn no answer when this one went out, and the CSeq number of
        /// the transaction that one rode.
        pending_offer_msg: usize,
        pending_offer_cseq: u32,
    },
    /// The answer whose `m=` line count is not the offer's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `m_count_msg`/`offer_m_lines`, so no other payload deserializes into it;
    /// and every other variant requires a key it has not (`answer_stream_msg`,
    /// `ack_body_msg`, `new_offer_msg` included).
    AnswerMLineCountDiffers {
        /// Index into the view's `msgs` of the answer the emitter sent.
        m_count_msg: usize,
        m_count_hop: usize,
        m_count_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// How many `m=` lines each description carried. They differ, or there
        /// is no hit.
        offer_m_lines: usize,
        answer_m_lines: usize,
        /// Each description's stream table as `<media>/<transport>` rows, in
        /// document order — what the two ends now disagree about.
        offered_streams: Vec<String>,
        answered_streams: Vec<String>,
    },
    /// The answer whose `t=` line is not the offer's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `t_line_msg`/`offer_t_line`, so no other payload deserializes into it;
    /// and every other variant requires a key it has not (`m_count_msg`,
    /// `answer_stream_msg`, `ack_body_msg` included).
    AnswerTLineDiffers {
        /// Index into the view's `msgs` of the answer the emitter sent.
        t_line_msg: usize,
        t_line_hop: usize,
        t_line_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// The `t=` line each description carried, `""` where it carried none.
        /// They differ, or there is no hit.
        offer_t_line: String,
        answer_t_line: String,
    },
    /// The answer that gave an offered stream position a different media type.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `media_type_msg`/`offered_types`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not
    /// (`answer_stream_msg`, `m_count_msg`, `t_line_msg` included).
    AnswerMediaTypeMismatched {
        /// Index into the view's `msgs` of the answer the emitter sent.
        media_type_msg: usize,
        media_type_hop: usize,
        media_type_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// The stream positions the two descriptions typed differently,
        /// ascending and never empty on a hit.
        stream_indexes: Vec<usize>,
        /// Per entry of `stream_indexes`, the `<media>` token the offer and the
        /// answer put there. The three vectors are the same length and read
        /// together.
        offered_types: Vec<String>,
        answered_types: Vec<String>,
    },
    /// The answer whose direction attribute the offer's does not admit.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `direction_msg`/`offered_directions`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not
    /// (`media_type_msg`, `answer_stream_msg`, `m_count_msg` included).
    DirectionPairInvalid {
        /// Index into the view's `msgs` of the answer the emitter sent.
        direction_msg: usize,
        direction_hop: usize,
        direction_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// The stream positions whose direction pair §6.1 does not admit,
        /// ascending and never empty on a hit.
        stream_indexes: Vec<usize>,
        /// Per entry of `stream_indexes`, the direction token the offer and the
        /// answer stated (an absent attribute reads `sendrecv`, the default
        /// §6.1 states). The three vectors are the same length.
        offered_directions: Vec<String>,
        answered_directions: Vec<String>,
    },
    /// The answer that rejected a stream with an `m=` line listing no format.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `rejected_stream_msg`/`rejected_rows`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not
    /// (`direction_msg`, `media_type_msg`, `answer_stream_msg` included).
    RejectedStreamWithoutFormat {
        /// Index into the view's `msgs` of the answer the emitter sent.
        rejected_stream_msg: usize,
        rejected_stream_hop: usize,
        rejected_stream_ts_us: u64,
        /// The answer's stream positions rejected at port 0 with no format
        /// token, ascending and never empty on a hit.
        stream_indexes: Vec<usize>,
        /// Per entry of `stream_indexes`, that stream's `"<media> <port>
        /// <proto>"` as the answer spelled it. The two vectors are the same
        /// length.
        rejected_rows: Vec<String>,
    },
    /// The description that carried fewer `m=` lines than its sender's previous
    /// one on the call.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `re_offer_msg`/`prior_m_lines`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not (`m_count_msg`,
    /// `origin_msg`, `new_offer_msg` included).
    ReOfferStreamsDropped {
        /// Index into the view's `msgs` of the description the emitter sent.
        re_offer_msg: usize,
        re_offer_hop: usize,
        re_offer_ts_us: u64,
        /// Index into the view's `msgs` of the emitter's previous description
        /// on this call — the stream table this one had to keep.
        prior_offer_msg: usize,
        /// How many `m=` lines each carried: `m_lines < prior_m_lines`, or
        /// there is no hit.
        m_lines: usize,
        prior_m_lines: usize,
    },
    /// The answer that gave a stream offered at port 0 a live port.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `zero_port_msg`/`answered_ports`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not
    /// (`rejected_stream_msg`, `media_type_msg`, `direction_msg` included).
    ZeroPortResurrected {
        /// Index into the view's `msgs` of the answer the emitter sent.
        zero_port_msg: usize,
        zero_port_hop: usize,
        zero_port_ts_us: u64,
        /// Index into the view's `msgs` of the offer it answers.
        offer_msg: usize,
        /// The stream positions the offer disabled and the answer re-enabled,
        /// ascending and never empty on a hit.
        stream_indexes: Vec<usize>,
        /// Per entry of `stream_indexes`, the live port the answer put there.
        /// The two vectors are the same length.
        answered_ports: Vec<i64>,
    },
    /// The description that rebound a payload type its sender had already bound
    /// to another encoding on the call.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `payload_type_msg`/`payload_types`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not
    /// (`re_offer_msg`, `origin_msg`, `zero_port_msg` included).
    PayloadTypeRemapped {
        /// Index into the view's `msgs` of the description the emitter sent.
        payload_type_msg: usize,
        payload_type_hop: usize,
        payload_type_ts_us: u64,
        /// The payload types it rebound, in the order the description states
        /// them and never empty on a hit.
        payload_types: Vec<String>,
        /// Per entry of `payload_types`, the encoding the sender's earlier
        /// description bound it to and the one this description states. The
        /// three vectors are the same length and read together.
        prior_encodings: Vec<String>,
        encodings: Vec<String>,
    },
    /// A header the message owed and did not state — the shape shared by every
    /// per-message rule whose offence is an ABSENCE (Contact on a
    /// dialog-establishing request, Content-Type over a body, Max-Forwards, the
    /// Via `branch` parameter, the To `tag` on a response above 100).
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `absent_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    RequiredHeaderAbsent {
        /// Index into the view's `msgs` of the message the emitter sent.
        absent_msg: usize,
        absent_hop: usize,
        absent_ts_us: u64,
        /// The start-line half the occasion keys on: the request method, or the
        /// response status as decimal.
        on: String,
        /// The header (or `Header;parameter`) the message owed.
        header: String,
    },
    /// A header the message was not entitled to state — the shape shared by the
    /// per-message rules whose offence is a PRESENCE (Contact on a BYE,
    /// Record-Route from a UA).
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `forbidden_msg`, so no other payload deserializes into it; and every
    /// other variant requires a key it has not.
    ForbiddenHeaderPresent {
        /// Index into the view's `msgs` of the message the emitter sent.
        forbidden_msg: usize,
        forbidden_hop: usize,
        forbidden_ts_us: u64,
        /// The start-line half the occasion keys on.
        on: String,
        header: String,
        /// The offending row, as the sender wrote it.
        value: String,
    },
    /// A header the message stated and no reader accepts — the shape shared by
    /// the per-message rules whose offence is a VALUE (a branch without the
    /// magic cookie, a Max-Forwards outside its range, a Content-Length that
    /// disagrees with the bytes).
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `rejected_msg`, so no other payload deserializes into it; and every
    /// other variant requires a key it has not.
    HeaderValueRejected {
        /// Index into the view's `msgs` of the message the emitter sent.
        rejected_msg: usize,
        rejected_hop: usize,
        rejected_ts_us: u64,
        /// The start-line half the occasion keys on.
        on: String,
        header: String,
        /// The value as the sender wrote it.
        value: String,
        /// What the sender owed instead, in the same spelling.
        expected: String,
    },
    /// The response whose Via stack does not reproduce the request's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `via_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    ResponseViaDiverged {
        /// Index into the view's `msgs` of the response the taker took.
        via_msg: usize,
        via_hop: usize,
        via_ts_us: u64,
        /// The response's top-Via branch, `""` where it carried none.
        response_branch: String,
        /// The branch the taker minted on the request the response answers.
        request_branch: String,
        /// Via row counts, response then request: equal where only the branch
        /// diverged.
        response_vias: usize,
        request_vias: usize,
    },
    /// The response whose `CSeq` names a request its taker never sent.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `phantom_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    ResponseCseqPhantom {
        /// Index into the view's `msgs` of the response the taker took.
        phantom_msg: usize,
        phantom_hop: usize,
        phantom_ts_us: u64,
        response_cseq: u32,
        response_method: String,
        /// The CSeq numbers the taker DID send that method at, never empty on a
        /// hit — with none, the mismatch is the method's own concern.
        sent_cseqs: Vec<u32>,
    },
    /// The in-dialog message carrying a tag its taker never minted.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `foreign_tag_msg`, so no other payload deserializes into it; and every
    /// other variant requires a key it has not.
    DialogTagForeign {
        /// Index into the view's `msgs` of the message the taker took.
        foreign_tag_msg: usize,
        foreign_tag_hop: usize,
        foreign_tag_ts_us: u64,
        /// Which header carried it: `To` on a request, `From` on a response.
        tag_header: String,
        tag: String,
        /// The tags the taker had minted on this dialog by then.
        local_tags: Vec<String>,
    },
    /// The in-dialog request whose From URI is not the one the dialog was
    /// created with.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `peer_uri_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    PeerUriRewritten {
        /// Index into the view's `msgs` of the request the taker took.
        peer_uri_msg: usize,
        peer_uri_hop: usize,
        peer_uri_ts_us: u64,
        method: String,
        /// The From URI this request stated.
        sent_uri: String,
        /// The From URI the dialog-creating INVITE stated.
        dialog_uri: String,
    },
    /// The in-dialog message that re-identified its dialog with a fresh
    /// Call-ID.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `call_id_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    DialogCallIdChanged {
        /// Index into the view's `msgs` of the message the taker took.
        call_id_msg: usize,
        call_id_hop: usize,
        call_id_ts_us: u64,
        /// The Call-ID this message carried.
        call_id: String,
        /// The Call-ID the dialog-creating INVITE established for its From-tag.
        dialog_call_id: String,
    },
    /// The CANCEL whose Request-URI is not its INVITE's.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `cancel_uri_msg`, so no other payload deserializes into it; and every
    /// other variant requires a key it has not.
    CancelUriDiverged {
        /// Index into the view's `msgs` of the CANCEL the taker took.
        cancel_uri_msg: usize,
        cancel_uri_hop: usize,
        cancel_uri_ts_us: u64,
        cancel_uri: String,
        /// The Request-URI of the branch-matched INVITE.
        invite_uri: String,
    },
    /// The CANCEL whose top Via branch matches no INVITE its taker has open.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `cancel_branch_msg`, so no other payload deserializes into it; and every
    /// other variant requires a key it has not.
    CancelBranchUnmatched {
        /// Index into the view's `msgs` of the CANCEL the taker took.
        cancel_branch_msg: usize,
        cancel_branch_hop: usize,
        cancel_branch_ts_us: u64,
        cancel_branch: String,
        /// The branches of the INVITEs the taker had open, never empty on a hit
        /// — with none, nothing can say the CANCEL is orphaned.
        invite_branches: Vec<String>,
    },
    /// The final response minting a To-tag the transaction's provisionals did
    /// not establish.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `tag_flip_msg`, so no other payload deserializes into it; and every other
    /// variant requires a key it has not.
    UasTagFlipped {
        /// Index into the view's `msgs` of the final the emitter sent.
        tag_flip_msg: usize,
        tag_flip_hop: usize,
        tag_flip_ts_us: u64,
        status: u16,
        /// The top-Via branch naming the server transaction.
        branch: String,
        /// The To-tag the final carried.
        final_tag: String,
        /// The tags the emitter's prior provisionals on that branch established,
        /// de-duplicated in wire order and never empty on a hit.
        provisional_tags: Vec<String>,
    },
    /// The 1xx whose reliability markers RFC 3262 §3 does not admit — a 100
    /// made reliable, a reliable 1xx with no `RSeq`, an `RSeq` out of range.
    /// ONE finding per response however many rows are at fault: the offence is
    /// one act of writing the reliability contract wrongly.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `reliable_1xx_msg`/`carried`/`admits`, so no other payload deserializes
    /// into it; and every other variant requires a key it has not.
    Reliable1xx {
        /// Index into the view's `msgs` of the 1xx the emitter sent.
        reliable_1xx_msg: usize,
        reliable_1xx_hop: usize,
        reliable_1xx_ts_us: u64,
        status: u16,
        /// The reliability markers the response DID carry, as `Header: value`
        /// rows in the order §3 names them (`RSeq` then `Require`). Empty where
        /// the defect is the marker that is missing.
        carried: Vec<String>,
        /// What §3 admits on this status instead.
        admits: String,
    },
    /// The `application/sdp` body whose bytes the RFC 3264 / RFC 4566 grammar
    /// walk refuses.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `sdp_body_msg`/`sdp_failure`, so no other payload deserializes into it;
    /// and every other variant requires a key it has not.
    SdpBodyRejected {
        /// Index into the view's `msgs` of the message the emitter sent.
        sdp_body_msg: usize,
        sdp_body_hop: usize,
        sdp_body_ts_us: u64,
        /// The FIRST concrete failure, as `sip_message::sdp` names it.
        sdp_failure: String,
    },
    /// The description whose streams are held (`c=` unspecified) AND rejected
    /// (port 0) at once. ONE finding per description, naming every stream at
    /// fault.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `held_stream_msg`/`held_streams`, so no other payload deserializes into
    /// it; and every other variant requires a key it has not.
    HeldAndRejectedStreams {
        /// Index into the view's `msgs` of the description the emitter sent.
        held_stream_msg: usize,
        held_stream_hop: usize,
        held_stream_ts_us: u64,
        /// The offending `m=` positions, 0-based, ascending and never empty on
        /// a hit.
        held_stream_indexes: Vec<usize>,
        /// Per entry of `held_stream_indexes`, the stream's media type and the
        /// `c=` value that applied to it. The three vectors are the same length
        /// and read together.
        held_streams: Vec<String>,
        held_c_lines: Vec<String>,
    },
    /// The copy of a message whose bytes differ from the first copy the same
    /// emitter put on the wire under one transaction identity, and where the
    /// two first disagree.
    ///
    /// Untagged and unambiguous both ways: it is the only variant carrying
    /// `rung_msg`, and it carries none of the required keys of any other
    /// variant.
    RungDiverged {
        /// Index into the view's `msgs` of the divergent copy.
        rung_msg: usize,
        rung_hop: usize,
        rung_ts_us: u64,
        /// Index into the view's `msgs` of the first copy, and when it went out.
        first_msg: usize,
        first_ts_us: u64,
        /// Which emission the divergent copy is: 1 is the first repeat.
        rung: u32,
        /// Copies of the message the vantage carried inside the transaction
        /// envelope, the first included, and how many of them diverged.
        copies: u32,
        divergent: u32,
        /// The retransmitting class the message belongs to — the vocabulary of
        /// `rules::retransmit::Class`, which names the RFC sentence that makes
        /// the repeat the same message.
        class: String,
        /// The transaction identity: CSeq method (the request method for a
        /// request), the response status, the `RSeq` of a reliable
        /// provisional, and the top-Via branch.
        method: String,
        status: Option<u16>,
        rseq: Option<u64>,
        branch: String,
        /// Where the bytes first disagree outside the emitter's own
        /// `Record-Route` rows: `"head"` or `"body"`, the byte offset into that
        /// part of the FIRST copy, and the line spanning the disagreement in
        /// each copy — `None` where that copy ends before it.
        region: String,
        offset: usize,
        first_line: Option<String>,
        rung_line: Option<String>,
        first_len: usize,
        rung_len: usize,
        /// Observation-time distance from the first copy to this one,
        /// microseconds.
        gap_us: u64,
    },
}

impl Evidence {
    /// The evidence's own message index in its view, so a report reads in
    /// observation order whichever rule decided it.
    pub fn anchor(&self) -> usize {
        match self {
            Evidence::Cancelled { response_msg, .. } => *response_msg,
            Evidence::Unacked { provisional_msg, .. }
            | Evidence::Overlapping { provisional_msg, .. }
            | Evidence::RseqGap { provisional_msg, .. } => *provisional_msg,
            Evidence::UnknownRack { prack_msg, .. }
            | Evidence::OutOfOrderRack { prack_msg, .. } => *prack_msg,
            Evidence::NoAck { final_msg, .. } => *final_msg,
            Evidence::Uncleared { final_msg, .. } => *final_msg,
            Evidence::MultipleFinals { second_msg, .. } => *second_msg,
            Evidence::CancelRouteDiverged { cancel_msg, .. }
            | Evidence::EagerCancel { cancel_msg, .. }
            | Evidence::LateCancel { cancel_msg, .. } => *cancel_msg,
            Evidence::CseqReused { reuse_msg, .. } => *reuse_msg,
            Evidence::CseqNotContiguous { skip_msg, .. } => *skip_msg,
            Evidence::ResponseCseqUnmatched { mismatch_msg, .. } => *mismatch_msg,
            Evidence::AckCseqUnmatched { ack_msg, .. } => *ack_msg,
            Evidence::MidDialogUriChanged { uri_msg, .. } => *uri_msg,
            Evidence::MidDialogRouteDiverged { route_msg, .. } => *route_msg,
            Evidence::MidDialogWireTargetDiverged { wire_msg, .. } => *wire_msg,
            Evidence::RecordRouteMisplaced { record_route_msg, .. } => *record_route_msg,
            Evidence::RportNotEchoed { rport_msg, .. } => *rport_msg,
            Evidence::CapabilitiesNotAdvertised { capability_msg, .. } => *capability_msg,
            Evidence::ExtraTryingForwarded { trying_msg, .. } => *trying_msg,
            Evidence::UnknownDialogRequest { unknown_dialog_msg, .. } => *unknown_dialog_msg,
            Evidence::RejectionNotIssued { rejection_msg, .. } => *rejection_msg,
            Evidence::ResponseHeadersMissing { response_headers_msg, .. } => *response_headers_msg,
            Evidence::NoTargetFinal { no_target_msg, .. } => *no_target_msg,
            Evidence::AckRequireNotSubset { ack_require_msg, .. } => *ack_require_msg,
            Evidence::AckRouteDiverged { ack_route_msg, .. } => *ack_route_msg,
            Evidence::StrictRouteNotRewritten { strict_route_msg, .. } => *strict_route_msg,
            Evidence::RegisterCarriesRoute { register_route_msg, .. } => *register_route_msg,
            Evidence::ConcurrentRegister { concurrent_register_msg, .. } => {
                *concurrent_register_msg
            }
            Evidence::ConcurrentReInvite { concurrent_invite_msg, .. } => *concurrent_invite_msg,
            Evidence::ByeOffDialog { bye_msg, .. } => *bye_msg,
            Evidence::OverlappingReInvite { overtaking_invite_msg, .. } => *overtaking_invite_msg,
            Evidence::TryingNotSentInGrace { trying_owed_msg, .. } => *trying_owed_msg,
            Evidence::UnackedReject { reject_msg, .. } => *reject_msg,
            Evidence::AbandonedReInvite { abandoned_invite_msg, .. } => *abandoned_invite_msg,
            Evidence::LateProvisional { late_provisional_msg, .. } => *late_provisional_msg,
            Evidence::Unreliable1xx { unreliable_1xx_msg, .. } => *unreliable_1xx_msg,
            Evidence::UnsolicitedReliable1xx { unsolicited_1xx_msg, .. } => *unsolicited_1xx_msg,
            Evidence::InDialogReliable1xx { in_dialog_1xx_msg, .. } => *in_dialog_1xx_msg,
            Evidence::PrackAbsorbed { absorbed_prack_msg, .. } => *absorbed_prack_msg,
            Evidence::PrackAnsweredWrongly { prack_answer_msg, .. } => *prack_answer_msg,
            Evidence::AnsweredOverUnackedOffer { early_2xx_msg, .. } => *early_2xx_msg,
            Evidence::LatePrackRejected { late_prack_answer_msg, .. } => *late_prack_answer_msg,
            Evidence::Reliable1xxAfterFinal { stray_1xx_msg, .. } => *stray_1xx_msg,
            Evidence::PrackedTrying { trying_prack_msg, .. } => *trying_prack_msg,
            Evidence::PrackWithoutAnswer { bodiless_prack_msg, .. } => *bodiless_prack_msg,
            Evidence::AckBodyOnClosedRound { ack_body_msg, .. } => *ack_body_msg,
            Evidence::OfferLeftUnanswered { unanswered_final_msg, .. } => *unanswered_final_msg,
            Evidence::SecondAnswerDiverged { second_answer_msg, .. } => *second_answer_msg,
            Evidence::AnswerStreamRetyped { answer_stream_msg, .. } => *answer_stream_msg,
            Evidence::SdpOriginDiverged { origin_msg, .. } => *origin_msg,
            Evidence::OfferWhilePending { new_offer_msg, .. } => *new_offer_msg,
            Evidence::AnswerMLineCountDiffers { m_count_msg, .. } => *m_count_msg,
            Evidence::AnswerTLineDiffers { t_line_msg, .. } => *t_line_msg,
            Evidence::AnswerMediaTypeMismatched { media_type_msg, .. } => *media_type_msg,
            Evidence::DirectionPairInvalid { direction_msg, .. } => *direction_msg,
            Evidence::RejectedStreamWithoutFormat { rejected_stream_msg, .. } => {
                *rejected_stream_msg
            }
            Evidence::ReOfferStreamsDropped { re_offer_msg, .. } => *re_offer_msg,
            Evidence::ZeroPortResurrected { zero_port_msg, .. } => *zero_port_msg,
            Evidence::PayloadTypeRemapped { payload_type_msg, .. } => *payload_type_msg,
            Evidence::RequiredHeaderAbsent { absent_msg, .. } => *absent_msg,
            Evidence::ForbiddenHeaderPresent { forbidden_msg, .. } => *forbidden_msg,
            Evidence::HeaderValueRejected { rejected_msg, .. } => *rejected_msg,
            Evidence::ResponseViaDiverged { via_msg, .. } => *via_msg,
            Evidence::ResponseCseqPhantom { phantom_msg, .. } => *phantom_msg,
            Evidence::DialogTagForeign { foreign_tag_msg, .. } => *foreign_tag_msg,
            Evidence::PeerUriRewritten { peer_uri_msg, .. } => *peer_uri_msg,
            Evidence::DialogCallIdChanged { call_id_msg, .. } => *call_id_msg,
            Evidence::CancelUriDiverged { cancel_uri_msg, .. } => *cancel_uri_msg,
            Evidence::CancelBranchUnmatched { cancel_branch_msg, .. } => *cancel_branch_msg,
            Evidence::UasTagFlipped { tag_flip_msg, .. } => *tag_flip_msg,
            Evidence::Reliable1xx { reliable_1xx_msg, .. } => *reliable_1xx_msg,
            Evidence::SdpBodyRejected { sdp_body_msg, .. } => *sdp_body_msg,
            Evidence::HeldAndRejectedStreams { held_stream_msg, .. } => *held_stream_msg,
            Evidence::RungDiverged { rung_msg, .. } => *rung_msg,
        }
    }
}
