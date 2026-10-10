//! What an `expect` **gates on**, and what it does with a datagram that does
//! not match (`PCAP2TEST_PIVOT_V3.md` §14 item 4).
//!
//! Gating is structural and nothing else: op, leg alignment, the discriminator
//! (`method`, or `status` plus `cseq-method`), and the budget. Then `check`
//! decides whether the stored content is MATCHED or merely RECORDED, and the
//! inline `checks` run over what matched.
//!
//! **The non-matching-datagram contract.** A byte-identical retransmission of
//! something already surfaced is absorbed BELOW this API — the harness's
//! RFC 3261 §17.2 two-view seam
//! ([`Absorption`](scenario_harness::absorption), keyed Call-ID / top-Via
//! branch / method for a request, plus CSeq and status for a final)
//! is the one implementation, and putting a second one here would let the two
//! disagree.
//! Everything that does surface is either matched by the armed expect, answered
//! by a background policy, or a FAILURE — it is never quietly tolerated and
//! never silently dropped. There is no absorb-set and no window construct.

use pivot_schema::body::{Body, BodyCompare, BodyShape};
use pivot_schema::bundle::{Arrived, Failure};
use pivot_schema::known_bug::KnownBug;
use pivot_schema::msg::{Header, MsgSpec};
use sip_message::generators::states_send_time;
use sip_message::header::{HeaderValue, MediaType};
use sip_message::{
    header_forms_equivalent, HeaderName, HeaderProjection, Method, SipMessage, SipStr,
};

use crate::early::LearnedForks;
use crate::plan::{CompiledStep, Discriminator};
use crate::resolve::Resolver;
use crate::scope::{Finding, Scope};

/// The facts a gate reads off an inbound datagram. Extraction lives in
/// `sip-message`; this is the projection the gate compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub method: Option<String>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub cseq_method: String,
    pub cseq: u32,
    pub call_id: String,
    pub rseq: Option<u32>,
    pub headers: HeaderProjection,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// A request's Request-URI user part — what a `ruri-pos` claim reads. `None`
    /// on a response and on a user-less URI.
    pub ruri_user: Option<String>,
    /// The From/To dialog identity, off `sip-message`'s own accessors: the two
    /// tags and the two user parts the §9 check vocabulary names.
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    pub from_user: Option<String>,
    pub to_user: Option<String>,
}

impl Inbound {
    /// Project a parsed message. Every field comes off `sip-message`'s own
    /// accessors — no header text is re-parsed here.
    pub fn of(msg: &SipMessage) -> Inbound {
        let headers = HeaderProjection::of(msg);
        let content_type = msg.raw(HeaderName::ContentType).next().map(str::to_string);
        let rseq = msg.raw(HeaderName::RSeq).next().and_then(|v| v.trim().parse().ok());
        match msg {
            SipMessage::Request(r) => Inbound {
                ruri_user: r.request_uri().user_identity(),
                from_tag: r.from().tag().map(str::to_string),
                to_tag: r.to().tag().map(str::to_string),
                from_user: r.from().uri().user_identity(),
                to_user: r.to().uri().user_identity(),
                method: Some(r.method().to_string()),
                status: None,
                reason: None,
                cseq_method: r.cseq().method().to_string(),
                cseq: r.cseq().seq(),
                call_id: r.call_id().to_string(),
                rseq,
                headers,
                body: r.body().to_vec(),
                content_type,
            },
            SipMessage::Response(r) => Inbound {
                ruri_user: None,
                from_tag: r.from().tag().map(str::to_string),
                to_tag: r.to().tag().map(str::to_string),
                from_user: r.from().uri().user_identity(),
                to_user: r.to().uri().user_identity(),
                method: None,
                status: Some(r.status()),
                reason: Some(r.reason().to_string()),
                cseq_method: r.cseq().method().to_string(),
                cseq: r.cseq().seq(),
                call_id: r.call_id().to_string(),
                rseq,
                headers,
                body: r.body().to_vec(),
                content_type,
            },
        }
    }

    /// The structural identity a failure states for this datagram — the
    /// machine-readable form; its `Display` is the human one.
    pub fn arrived(&self) -> Arrived {
        match (&self.method, self.status) {
            (Some(m), _) => Arrived::Request { method: m.clone(), cseq: self.cseq },
            (None, Some(s)) => Arrived::Response {
                status: s,
                reason: self.reason.clone().unwrap_or_default(),
                cseq_method: self.cseq_method.clone(),
                cseq: self.cseq,
            },
            (None, None) => Arrived::Unreadable,
        }
    }

    /// A one-line description, for a failure that has to say what arrived.
    pub fn describe(&self) -> String {
        self.arrived().to_string()
    }

    /// The value of `name` by header IDENTITY — a compact spelling is found —
    /// first occurrence in wire order.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.first(name)
    }
}

/// What the gate decided about one datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// The datagram satisfies the step.
    Matches,
    /// It does not, and this is why. The reason reaches the verdict verbatim.
    Rejects(String),
}

impl GateVerdict {
    pub fn matches(&self) -> bool {
        matches!(self, GateVerdict::Matches)
    }
}

/// Whether the datagram satisfies the step's DISCRIMINATOR — the structural
/// gate, applied under both `check` modes.
pub fn discriminates(step: &CompiledStep, inbound: &Inbound) -> GateVerdict {
    if !step.is_expect() {
        return GateVerdict::Rejects(format!("step {:?} is a send, not an expect", step.id));
    }
    match &step.discriminator {
        Discriminator::Request { method } => match &inbound.method {
            None => GateVerdict::Rejects(format!(
                "gated on request {method}; a {} response arrived",
                inbound.status.unwrap_or(0)
            )),
            Some(got) if Method::from_wire(got) == Method::from_wire(method) => {
                GateVerdict::Matches
            }
            Some(got) => GateVerdict::Rejects(format!("gated on request {method}; {got} arrived")),
        },
        Discriminator::Response { status, cseq_method } => {
            let Some(got) = inbound.status else {
                return GateVerdict::Rejects(format!(
                    "gated on response {status}; a {} request arrived",
                    inbound.method.as_deref().unwrap_or("?")
                ));
            };
            if got != *status {
                return GateVerdict::Rejects(format!("gated on response {status}; {got} arrived"));
            }
            match cseq_method {
                None => GateVerdict::Matches,
                Some(want)
                    if Method::from_wire(want) == Method::from_wire(&inbound.cseq_method) =>
                {
                    GateVerdict::Matches
                }
                Some(want) => GateVerdict::Rejects(format!(
                    "gated on {status} to {want}; a {status} to {} arrived",
                    inbound.cseq_method
                )),
            }
        }
    }
}

/// Whether the datagram rides the ANSWERED fork the step names (§6.1), under
/// both `check` modes as the discriminator is.
///
/// A forking callee's early dialogs differ only by the To-tag it minted
/// (RFC 3261 §12.1.1), so this is what tells two rings of one leg apart — and
/// what keeps a PRACK for one fork from completing the other's step. A step
/// naming no early dialog is scoped to none and takes whatever its
/// discriminator accepts. An OBSERVED fork — the peer minted the tag — gates
/// through [`rides_observed_fork`] instead.
pub fn rides_early_dialog(
    step: &CompiledStep,
    inbound: &Inbound,
    tag: Option<&str>,
) -> GateVerdict {
    let Some(early) = &step.early else { return GateVerdict::Matches };
    let Some(tag) = tag else {
        return GateVerdict::Rejects(format!(
            "gated on early dialog {early:?}, which this run minted no tag for"
        ));
    };
    match inbound.to_tag.as_deref() {
        Some(got) if got == tag => GateVerdict::Matches,
        Some(got) => GateVerdict::Rejects(format!(
            "gated on early dialog {early:?} (To-tag {tag:?}); To-tag {got:?} arrived"
        )),
        None => GateVerdict::Rejects(format!(
            "gated on early dialog {early:?} (To-tag {tag:?}); the message carries no To-tag"
        )),
    }
}

/// Whether the datagram rides the OBSERVED fork the step names (§6.1) — the
/// reading of `early` where the PEER minted the To-tag and the run learns it.
///
/// Pure, exactly as [`rides_early_dialog`] is: it answers against the CURRENT
/// bindings and learns nothing — the runner binds on consumption. An unbound id
/// takes any To-tag no other id on the leg holds, and that arm is the demux:
/// two steps naming two observed forks cannot both be satisfied by one tag, so
/// a system that collapses two early dialogs into one is refused at the second
/// step.
pub fn rides_observed_fork(
    step: &CompiledStep,
    inbound: &Inbound,
    learned: &LearnedForks,
) -> GateVerdict {
    let Some(early) = &step.early else { return GateVerdict::Matches };
    let bound = learned.tag(&step.leg, early);
    let Some(got) = inbound.to_tag.as_deref() else {
        return GateVerdict::Rejects(match bound {
            Some(tag) => format!(
                "gated on early dialog {early:?} (To-tag {tag:?}); the message carries no To-tag"
            ),
            None => format!("gated on early dialog {early:?}; the message carries no To-tag"),
        });
    };
    match bound {
        Some(tag) if got == tag => GateVerdict::Matches,
        Some(tag) => GateVerdict::Rejects(format!(
            "gated on early dialog {early:?} (To-tag {tag:?}); To-tag {got:?} arrived"
        )),
        None => match learned.holder(&step.leg, got) {
            Some(holder) => GateVerdict::Rejects(format!(
                "gated on early dialog {early:?}, which must be a dialog of its own; \
                 To-tag {got:?} already rides {holder:?}"
            )),
            None => GateVerdict::Matches,
        },
    }
}

/// The WHOLE content an asserted expect states, headers included (§6.4). Under
/// `record` the stored content is the recorded value and is matched against
/// nothing — which is a document decision, never a lane's.
///
/// This is the PREFERRED reading rather than the match itself: a datagram that
/// satisfies it is the step's beyond doubt, so `exec` offers every armed step
/// this gate before falling back to [`body_content_holds`]. What every declared
/// header found is [`header_findings`], under either reading.
///
/// `scope` is what makes an `assert` expect scopable at HEADER granularity
/// (§9.1): a frozen header whose class does not gate here is left out of the
/// preference too, because a header this lane does not read is no evidence
/// about which step the datagram belongs to.
pub fn content_holds(
    step: &CompiledStep,
    inbound: &Inbound,
    scope: &Scope<'_>,
    resolver: &Resolver<'_>,
) -> GateVerdict {
    if !step.asserts_content() {
        return GateVerdict::Matches;
    }
    if let Some(reason) = headers_hold(&step.msg, inbound, scope, resolver) {
        return GateVerdict::Rejects(reason);
    }
    body_content_holds(step, inbound, scope)
}

/// The content an asserted expect gates its MATCH on: the body, and nothing
/// about the header set.
///
/// A header set that differs is a divergence the run NAMES, never one it
/// abandons the call over. Refusing the datagram leaves the message unanswered,
/// the SUT retransmitting, and the run dying several steps past the difference
/// with no evidence anywhere naming the header; taking it answers the message,
/// walks the dialog to its captured teardown, and still fails the run through
/// the [`header_findings`] the verdict carries.
pub fn body_content_holds(
    step: &CompiledStep,
    inbound: &Inbound,
    scope: &Scope<'_>,
) -> GateVerdict {
    if !step.asserts_content() {
        return GateVerdict::Matches;
    }
    if let Some((reason, bug)) = body_holds(&step.msg, inbound) {
        if !bug.is_some_and(|bug| scope.waives(bug)) {
            return GateVerdict::Rejects(reason);
        }
    }
    GateVerdict::Matches
}

/// What a body assertion this lane does NOT gate on found — its declared known
/// bug and the miss itself. Empty where the body held, where no known bug names
/// the miss, and under `check: record`, which asserts nothing at all.
pub fn waived_body_findings(
    step: &CompiledStep,
    inbound: &Inbound,
    scope: &Scope<'_>,
) -> Vec<(KnownBug, Failure)> {
    if !step.asserts_content() {
        return Vec::new();
    }
    body_holds(&step.msg, inbound)
        .and_then(|(reason, bug)| bug.filter(|bug| scope.waives(*bug)).map(|bug| (bug, reason)))
        .map(|(bug, reason)| {
            vec![(
                bug,
                Failure::CheckFailed {
                    site: format!("step {:?}", step.id),
                    field: "body".into(),
                    op: "shape".into(),
                    expected: shape_token(&step.msg).unwrap_or_default(),
                    observed: reason,
                },
            )]
        })
        .unwrap_or_default()
}

/// The body shape token an expect declares, for a finding to name.
fn shape_token(spec: &MsgSpec) -> Option<String> {
    match spec.body.as_ref()? {
        Body::Shape(shape) => Some(match shape.mode {
            BodyShape::Absent => "absent".into(),
            BodyShape::SdpPresent => "sdp-present".into(),
            BodyShape::MultipartPresent => "multipart-present".into(),
        }),
        Body::Resource(_) | Body::Multipart(_) => Some("present".into()),
    }
}

/// What every header an asserted expect declares found on the datagram that
/// satisfied it — one finding per header that did not hold, frozen values and
/// existence checks alike. Empty under `check: record`, which asserts nothing
/// at all.
///
/// Purely DESCRIPTIVE: a finding states the header's name, the value the
/// document froze and the values the datagram carried (`absent` where it
/// carried none). Whether a named difference is ACCEPTABLE is the lane's
/// ruling, held in the delta registry against the confrontation's own probes,
/// and no part of it belongs here.
///
/// `Finding::class` is what each one costs: an unclassified header gates and
/// fails the run, one whose vocabulary this lane does not read is recorded
/// informative (§9.1), and an existence check carries no class because
/// existence is not a vocabulary. A clock stamp is reported like any other —
/// it leaves the MATCH, not the verdict.
pub fn header_findings(
    step: &CompiledStep,
    inbound: &Inbound,
    resolver: &Resolver<'_>,
) -> Vec<Finding> {
    if !step.asserts_content() {
        return Vec::new();
    }
    let site = format!("step {:?}", step.id);
    let frozen = step
        .msg
        .headers
        .iter()
        .filter(|want| header_holds(want, &step.msg.headers, inbound, resolver).is_some())
        .map(|want| {
            Finding::new(
                want.class,
                Failure::CheckFailed {
                    site: site.clone(),
                    field: format!("header({})", want.name),
                    op: "eq".into(),
                    expected: frozen_value(want, resolver).unwrap_or_else(|detail| detail),
                    observed: header_occurrences(&want.name, inbound),
                },
            )
        });
    let present = step
        .msg
        .headers_present
        .iter()
        .filter(|name| !states_send_time(name) && inbound.header(name).is_none())
        .map(|name| {
            Finding::gating(Failure::CheckFailed {
                site: site.clone(),
                field: format!("header({name})"),
                op: "exists".into(),
                expected: "present".into(),
                observed: "absent".into(),
            })
        });
    frozen.chain(present).collect()
}

/// What `name` CARRIES on the datagram, as a finding states it: its occurrences
/// in wire order, or `absent` where the message holds none.
fn header_occurrences(name: &str, inbound: &Inbound) -> String {
    let present = inbound.headers.all(name);
    if present.is_empty() {
        return "absent".into();
    }
    present.join(", ")
}

/// The step's declared tier-3 headers, and how many of them the datagram
/// actually CARRIES — by header identity, whatever the value.
///
/// Identity, not value, because this answers "is this the message the step is
/// about?" and not "does it hold?". A relayed request carries the peer-origin
/// headers the step froze; the SUT's own background traffic of the same method
/// carries none of them, and that is the only thing on the wire that tells the
/// two apart when they land in one instant (`exec::arbitration`).
pub fn declared_headers_carried(step: &CompiledStep, inbound: &Inbound) -> (usize, usize) {
    let declared = &step.msg.headers;
    let carried = declared.iter().filter(|want| inbound.header(&want.name).is_some()).count();
    (declared.len(), carried)
}

/// The frozen tier-3 header list this run gates on, and the existence checks.
/// Order is not asserted: `raw-order` is an EMISSION property, and a relayed
/// message is free to carry the same values in its own order.
///
/// `headers_present` carries no class — existence is not a vocabulary — so it
/// gates on every lane.
///
/// A frozen CLOCK STAMP never gates: `Date`/`Timestamp` state when the message
/// that carries them was sent. The relaying stack carries the peer's on this
/// run (or none, from a stored copy), a reading of that peer's clock, so the
/// captured value is no property of the call. What it found is
/// [`scoped_header_findings`].
fn headers_hold(
    spec: &MsgSpec,
    inbound: &Inbound,
    scope: &Scope<'_>,
    resolver: &Resolver<'_>,
) -> Option<String> {
    for want in spec.headers.iter().filter(|want| scope.gates(want.class) && !clock_stamp(want)) {
        if let Some(reason) = header_holds(want, &spec.headers, inbound, resolver) {
            return Some(reason);
        }
    }
    for name in spec.headers_present.iter().filter(|name| !states_send_time(name)) {
        if inbound.header(name).is_none() {
            return Some(format!("header {name:?} is absent"));
        }
    }
    None
}

/// True iff this frozen header states when its own message was sent (RFC 3261
/// §20.17 / §20.38): the sending peer's clock, whatever the relay carries.
fn clock_stamp(want: &Header) -> bool {
    states_send_time(&want.name)
}

/// Why one frozen header does not hold on `inbound`, where it does not.
fn header_holds(
    want: &Header,
    declared: &[Header],
    inbound: &Inbound,
    resolver: &Resolver<'_>,
) -> Option<String> {
    let present = inbound.headers.all(&want.name);
    if present.is_empty() {
        return Some(format!("header {:?} is absent", want.name));
    }
    let value = match frozen_value(want, resolver) {
        Err(detail) => return Some(format!("header {:?} states {detail}", want.name)),
        Ok(value) => value,
    };
    if present.iter().any(|v| header_forms_equivalent(&want.name, &[v], &[&value])) {
        return None;
    }
    if folded_rows_hold(want, declared, &present, resolver) {
        return None;
    }
    Some(format!("header {:?} carries {present:?}, not {value:?}", want.name))
}

/// Whether the datagram's rows of this header, folded, state what the step's
/// rows of it state folded — RFC 3261 §7.3.1, where several rows of one
/// comma-list header combine into one without changing the semantics, so a
/// platform that re-lays out its rows has changed nothing.
///
/// Folded and not per-row, because the fold is the value: a row-wise pass would
/// accept the message's rows as a SUPERSET of the step's, and a capability set
/// our stack added is §6.4's finding, not an equivalence.
fn folded_rows_hold(
    want: &Header,
    declared: &[Header],
    present: &[&str],
    resolver: &Resolver<'_>,
) -> bool {
    let mut frozen = Vec::new();
    for header in declared.iter().filter(|h| h.name.eq_ignore_ascii_case(&want.name)) {
        match frozen_value(header, resolver) {
            Ok(value) => frozen.push(value),
            Err(_) => return false,
        }
    }
    if frozen.len() < 2 && present.len() < 2 {
        return false;
    }
    let frozen: Vec<&str> = frozen.iter().map(String::as_str).collect();
    header_forms_equivalent(&want.name, present, &frozen)
}

/// The value a frozen header states, with its `${…}` substituted (§8.1) — the
/// same resolution a `send` gives the same text, so one token cannot mean two
/// things on the two sides of a relay.
///
/// An accessor this run cannot answer is NOT a value: it comes back as the
/// reason it could not, which no wire value equals and every finding can state.
fn frozen_value(want: &Header, resolver: &Resolver<'_>) -> Result<String, String> {
    resolver.text(&want.value).map_err(|e| format!("unresolved accessor: {e}"))
}

/// A declared body SHAPE on an expect, and the lane-declarable known bug that
/// names the miss where one does. A resource or multipart body on an expect
/// gates on PRESENCE only: the content assertion a text resource carries is the
/// post-run confrontation's, read off the recording (§8.3), and a binary
/// resource is presence-only everywhere.
///
/// The one nameable miss is a body on a PROVISIONAL the document expects
/// stripped: that is a SUT that did not apply the 18x rewrite its routing
/// decision armed, and the call continues around it. A body missing where one
/// is owed, or the wrong media type, names no known bug and always gates.
fn body_holds(spec: &MsgSpec, inbound: &Inbound) -> Option<(String, Option<KnownBug>)> {
    let Some(body) = &spec.body else { return None };
    let media =
        inbound.content_type.as_deref().and_then(|t| MediaType::parse(&SipStr::owned(t)).ok());
    let is_sdp = media.as_ref().is_some_and(MediaType::is_sdp);
    let is_multipart = media.as_ref().is_some_and(MediaType::is_multipart);
    match body {
        Body::Shape(shape) => match shape.mode {
            BodyShape::Absent if !inbound.body.is_empty() => Some((
                format!("body must be absent; {} bytes arrived", inbound.body.len()),
                unstripped_provisional(spec),
            )),
            BodyShape::SdpPresent if !is_sdp => Some((
                format!(
                    "body must be SDP; content type is {:?}",
                    inbound.content_type.as_deref().unwrap_or("absent")
                ),
                None,
            )),
            BodyShape::MultipartPresent if !is_multipart => Some((
                format!(
                    "body must be multipart; content type is {:?}",
                    inbound.content_type.as_deref().unwrap_or("absent")
                ),
                None,
            )),
            _ => None,
        },
        Body::Resource(_) | Body::Multipart(_) if inbound.body.is_empty() => {
            Some(("body must be present; none arrived".into(), None))
        }
        // A resource compared as a session description keeps the media-type
        // gate the `sdp-present` shape carries: what arrived must be SDP for
        // the confrontation to read it as one.
        Body::Resource(resource) if resource.compare == Some(BodyCompare::Sdp) && !is_sdp => {
            Some((
                format!(
                    "body must be SDP; content type is {:?}",
                    inbound.content_type.as_deref().unwrap_or("absent")
                ),
                None,
            ))
        }
        // A multipart expect keeps the media-type gate the `multipart-present`
        // shape carries: what arrived must be multipart for the confrontation
        // to locate its parts. The parts' content stays the confrontation's.
        Body::Multipart(_) if !is_multipart => Some((
            format!(
                "body must be multipart; content type is {:?}",
                inbound.content_type.as_deref().unwrap_or("absent")
            ),
            None,
        )),
        Body::Resource(_) | Body::Multipart(_) => None,
    }
}

/// The known bug a body-on-a-stripped-provisional miss falls under, where the
/// step is one: a 1xx above 100, which is the message a `relay18x` mode
/// rewrites. On any other message an unexpected body is nobody's known defect.
fn unstripped_provisional(spec: &MsgSpec) -> Option<KnownBug> {
    matches!(spec.status, Some(101..=199)).then_some(KnownBug::ProvisionalRewriteNotApplied)
}

#[cfg(test)]
mod tests;
