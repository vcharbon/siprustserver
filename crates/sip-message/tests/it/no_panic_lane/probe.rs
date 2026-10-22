//! Every entry point that takes bytes or decoded peer data, run on one input:
//! message parse under each limit set, the typed header reads and what each
//! value answers, the read helpers over a parsed message, the generators that
//! echo a received request, raw-datagram sniffing, sipfrag, MSCML, multipart,
//! and every SDP read, answer and restatement. Each outcome is folded into a
//! [`Digest`], so two runs that agree on the digest agree on every outcome.

use std::fmt::Debug;

use super::echo;
use sip_message::generators::cancel::generate_cancel;
use sip_message::generators::generate_invalid_request;
use sip_message::generators::response::{generate_response, retag_response, GenerateResponseOpts};
use sip_message::generators::spec::InviteClientTransactionHandle;
use sip_message::header::{self, HeaderName, HeaderValue, MediaType, Uri};
use sip_message::parser::custom::CustomParser;
use sip_message::payload::{BodyLayout, Payload};
use sip_message::sdp_session::StatedSession;
use sip_message::{
    answer_from_own, answer_from_own_agreeing, answer_reoffer_both_ways,
    build_held_sdp_from_profile, canonical_form, canonical_header_items, decompose_multipart,
    entity_parts, extract_codec_profile, header_forms_equivalent, in_author_order, mscml,
    parse_origin, parse_sdp_body, reject_offer, remote_target, restate_session,
    restate_session_again, rewrite_connection_and_ports, sdp_media_equivalent, sdp_range, sipfrag,
    sniff, stamp_session, validate_sdp_body, BuildHeldSdpOptions, Bytes, FormatPreference,
    HeaderProjection, InDialogRequest, InviteRequest, Method, SipMessage, SipParser,
    SipParserLimits, SipRequest, SipResponse, SipResponseTagged,
};

/// FNV-1a over every outcome's label and `Debug` form: stable across runs and
/// toolchains, unlike the std hasher.
pub struct Digest {
    hash: u64,
    pub outcomes: usize,
}

impl Digest {
    pub fn new() -> Self {
        Self { hash: 0xcbf2_9ce4_8422_2325, outcomes: 0 }
    }

    pub fn value(&self) -> u64 {
        self.hash
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// Folds one outcome in.
    pub fn note(&mut self, label: &str, value: &dyn Debug) {
        self.outcomes += 1;
        self.bytes(label.as_bytes());
        self.bytes(b"=");
        self.bytes(format!("{value:?}").as_bytes());
        self.bytes(b"\n");
    }

    /// Folds one outcome in that is bytes already, such as a built message's
    /// image.
    pub fn raw(&mut self, label: &str, value: &[u8]) {
        self.outcomes += 1;
        self.bytes(label.as_bytes());
        self.bytes(b"=");
        self.bytes(value);
        self.bytes(b"\n");
    }
}

/// The limit sets a datagram is parsed under.
fn parsers() -> Vec<(&'static str, CustomParser)> {
    let lenient = SipParserLimits { wire_grammar: false, ..SipParserLimits::default() };
    let stream =
        SipParserLimits { framing: sip_message::Framing::Stream, ..SipParserLimits::default() };
    let tight =
        SipParserLimits { max_header_length: 16, max_uri_length: 8, ..SipParserLimits::default() };
    vec![
        ("default", CustomParser::new()),
        ("lenient", CustomParser::with_limits(lenient)),
        ("stream", CustomParser::with_limits(stream)),
        ("tight", CustomParser::with_limits(tight)),
    ]
}

/// One datagram through every message entry point.
pub fn message(raw: &[u8], d: &mut Digest) {
    sniffs(raw, d);
    for (name, parser) in parsers() {
        let parsed = parser.parse(raw);
        d.note(name, &parsed.as_ref().map(|_| ()));
        if let Err(error) = &parsed {
            invalid_request(error, d);
        }
        if name == "default" {
            d.note("shared", &parser.parse_shared(Bytes::copy_from_slice(raw)));
            d.note("header-fields", &parser.parse_header_fields(raw));
        }
        // The stream and tight sets accept a subset of what these two accept.
        if let (Ok(msg), "default" | "lenient") = (parsed, name) {
            parsed_message(&msg, name == "default", d);
        }
    }
}

/// The 400 answering a refused request, built from its partial.
fn invalid_request(error: &sip_message::SipParseError, d: &mut Digest) {
    let Some(partial) = error.partial() else { return };
    let source = ("198.51.100.7".to_string(), 40000);
    let built = generate_invalid_request(partial, &error.reason, "192.0.2.1:5060", "t400", &source);
    d.note("invalid-request", &built.map(|r| r.image().to_vec()));
}

/// What a parsed message answers; `strict` where the wire grammar read it.
fn parsed_message(msg: &SipMessage, strict: bool, d: &mut Digest) {
    d.note("message", msg);
    d.note("serialize", &sip_message::serialize(msg));
    d.note("summary", &sip_message::message_summary(msg));
    d.note("layout", &BodyLayout::of(msg));
    d.note("payload", &Payload::of_datagram(msg.image()).bytes());
    let projection = HeaderProjection::of(msg);
    d.note("projection", &projection.all("Via"));
    d.note("from", msg.from());
    uri(msg.from().uri(), d);
    d.note("to", msg.to());
    uri(msg.to().uri(), d);
    d.note("call-id", &(msg.call_id(), msg.call_id().host()));
    d.note("cseq", msg.cseq());
    for via in msg.via().iter() {
        d.note(
            "via",
            &(
                via.host_port(),
                via.branch(),
                via.received(),
                via.rport(),
                via.maddr(),
                via.response_target(),
            ),
        );
        d.note("via-wire", &via.to_wire());
    }
    for contact in msg.contacts().as_slice() {
        d.note("contact", &contact.to_wire());
        uri(contact.uri(), d);
    }
    d.note("route-set", &msg.route_set());
    d.note("record-route-set", &msg.record_route_set());
    typed_reads(msg, d);
    helpers(msg, d);
    body(msg, d);
    match msg {
        SipMessage::Request(req) => request(req, strict, d),
        SipMessage::Response(resp) => response(resp, strict, d),
    }
}

/// What a URI answers when asked.
fn uri(u: &Uri, d: &mut Digest) {
    d.note(
        "uri",
        &(
            u.scheme(),
            u.user(),
            u.host_port(),
            u.is_loose_route(),
            u.user_identity(),
            u.user_digits(),
        ),
    );
    d.note("uri-headers", &u.escaped_headers().collect::<Vec<_>>());
    d.note("uri-replaces", &u.escaped_header("Replaces"));
    d.note("uri-text", &u.text());
    d.note("uri-normalized", &u.clone().normalized());
    d.note("uri-same", &u.same_user_identity(u));
}

fn read<H: HeaderValue>(msg: &SipMessage, d: &mut Digest) {
    let one = msg.header::<H>();
    let label = H::header_name().to_string();
    d.note(&label, &one);
    if let Some(Ok(v)) = &one {
        d.note(&label, &v.to_wire());
    }
    if let Ok(all) = msg.list::<H>() {
        for v in &all {
            d.note(&label, &v.to_wire());
        }
    } else {
        d.note(&label, &"list refused");
    }
}

macro_rules! typed {
    ($msg:expr, $d:expr, $($t:ty),* $(,)?) => { $( read::<$t>($msg, $d); )* };
}

fn typed_reads(msg: &SipMessage, d: &mut Digest) {
    typed!(
        msg,
        d,
        header::From,
        header::To,
        header::Contact,
        header::RouteEntry,
        header::RecordRouteEntry,
        header::PathEntry,
        header::ServiceRouteEntry,
        header::ReferTo,
        header::ReferredBy,
        header::ReplyTo,
        header::Diversion,
        header::HistoryInfo,
        header::RemotePartyId,
        header::PAssertedIdentity,
        header::PPreferredIdentity,
        header::Require,
        header::ProxyRequire,
        header::Supported,
        header::Unsupported,
        header::Allow,
        header::AllowEvents,
        header::Privacy,
        header::Event,
        header::SubscriptionState,
        header::ReferSub,
        header::MediaType,
        header::AcceptRange,
        header::ContentDisposition,
        header::SessionExpires,
        header::RetryAfter,
        header::Reason,
        header::Replaces,
        header::MaxForwards,
        header::ContentLength,
        header::Expires,
        header::MinExpires,
        header::MinSe,
        header::RSeq,
        header::Authorization,
        header::ProxyAuthorization,
        header::WwwAuthenticate,
        header::ProxyAuthenticate,
        header::CallId,
        header::CSeq,
        header::RAck,
        header::Via,
        header::ChargingVector,
    );
    if let Ok(refer_to) = msg.list::<header::ReferTo>() {
        for r in &refer_to {
            uri(r.uri(), d);
        }
    }
    if let Ok(pai) = msg.list::<header::PAssertedIdentity>() {
        for p in &pai {
            uri(p.uri(), d);
        }
    }
    if let Some(Ok(cv)) = msg.header::<header::ChargingVector>() {
        d.note("icid", &(cv.icid_value(), cv.icid_generated_at()));
    }
    if let Some(Ok(mf)) = msg.header::<header::MaxForwards>() {
        d.note("max-forwards", &(mf.decremented(), mf.incremented()));
    }
    if let Some(Ok(cseq)) = msg.header::<header::CSeq>() {
        match cseq.stepped(Method::Bye) {
            Some(next) => d.note("cseq-stepped", &next),
            None => d.note("cseq-stepped", &"no next sequence number"),
        }
    }
}

fn raw_text(msg: &SipMessage, name: HeaderName) -> Vec<sip_message::SipStr> {
    match msg {
        SipMessage::Request(r) => r.raw_text(name).collect(),
        SipMessage::Response(r) => r.raw_text(name).collect(),
    }
}

fn helpers(msg: &SipMessage, d: &mut Digest) {
    let reasons = header::readable_reasons(raw_text(msg, HeaderName::Reason));
    d.note("reasons", &reasons);
    d.note("reason-q850", &header::reason_for(&reasons, "Q.850"));
    d.note("q850-alone", &header::q850_cause_alone(&reasons));
    d.note("cause-key", &header::cause_key(raw_text(msg, HeaderName::Reason)));
    d.note("retry-after", &header::retry_after_seconds(raw_text(msg, HeaderName::RetryAfter)));
    for w in raw_text(msg, HeaderName::Warning) {
        d.note("warn-agents", &header::warn_agents(w.as_str()));
    }
    for c in raw_text(msg, HeaderName::Contact) {
        let text = c.as_str();
        d.note("rt-elements", &remote_target::parse_elements(text));
        d.note("rt-first", &remote_target::first_hostport(text));
        d.note("rt-rewrite", &remote_target::rewrite_hostport(text, "203.0.113.7", 65535));
        d.note("network-host", &header::network_host(text));
        d.note("host-of", &header::host_of(text));
    }
    for c in raw_text(msg, HeaderName::CallId) {
        d.note("message-id-domain", &header::message_id_domain(c.as_str()));
    }
    let names: Vec<String> = msg.headers().iter().map(|h| h.name.to_string()).collect();
    for name in &names {
        let rows: Vec<&str> = msg
            .headers()
            .iter()
            .filter(|h| h.name.as_str().eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
            .collect();
        d.note("canonical", &canonical_header_items(name, &rows));
        d.note("equivalent", &header_forms_equivalent(name, &rows, &rows));
        d.note("name", &HeaderName::from(name.as_str()));
    }
    let wanted = [HeaderName::Via, HeaderName::Contact, HeaderName::from("X-Trace-Sample")];
    d.note("captured", &sip_message::capture::captured_headers(msg.headers(), &wanted));
}

fn body(msg: &SipMessage, d: &mut Digest) {
    let body = msg.body().as_ref();
    let content_type = raw_text(msg, HeaderName::ContentType).into_iter().next();
    let ct = content_type.as_ref().map(|c| c.as_str());
    d.note("mscml", &mscml::requests_in(ct, body));
    d.note("sipfrag-status", &sipfrag::status(body));
    d.note("sipfrag-fields", &sipfrag::header_fields(body));
    d.note("entity-parts", &entity_parts(ct, &[], body));
    if let Some(Ok(media_type)) = msg.header::<MediaType>() {
        d.note("sdp-range", &sdp_range(&media_type, body));
        if let Some(boundary) = media_type.params().value("boundary") {
            d.note("decompose", &decompose_multipart(body, boundary));
        }
    }
    let sdp = match msg {
        SipMessage::Request(r) => r.sdp().map(<[u8]>::to_vec),
        SipMessage::Response(r) => r.sdp().map(<[u8]>::to_vec),
    };
    if let Some(sdp) = sdp {
        description(&sdp, super::seeds::SDP_ANSWER.as_bytes(), d);
        description(super::seeds::SDP_OFFER.as_bytes(), &sdp, d);
    }
}

fn request(req: &SipRequest, strict: bool, d: &mut Digest) {
    d.note("method", &(req.method(), req.version(), req.offers_100rel()));
    uri(req.request_uri(), d);
    d.note("trace-sample", &sip_message::trace_sample::trace_sample(req));
    d.note("emergency", &sip_message::emergency::is_emergency_request(req));
    d.note(
        "hops",
        &(sip_message::hops::forwarded_max_forwards(req), sip_message::hops::hops_exhausted(req)),
    );
    d.note(
        "in-dialog",
        &InDialogRequest::new(req).map(|r| (r.from_tag().to_owned(), r.to_tag().to_owned())),
    );
    if let Some(invite) = InviteRequest::new(req) {
        d.note("invite-contact", &invite.contact());
    }
    match req.thaw().freeze() {
        Ok(thawed) => d.raw("thaw", thawed.image()),
        Err(e) => d.note("thaw", &e),
    }
    let opts = GenerateResponseOpts {
        to_tag: Some("lane".into()),
        incoming_source: Some(("192.0.2.200".into(), 5060)),
        ..GenerateResponseOpts::default()
    };
    for (status, reason) in
        [(100, "Trying"), (180, "Ringing"), (200, "OK"), (487, "Request Terminated")]
    {
        let built = generate_response(req, status, reason, &opts).map(|r| r.image().to_vec());
        echo::outcome_image("generated", built, strict, d);
    }
    if *req.method() == Method::Invite {
        let handle = InviteClientTransactionHandle { original_invite: req.clone() };
        echo::outcome("cancel", generate_cancel(&handle, &[]), strict, d);
    }
    echo::from_request(req, strict, d);
}

fn response(resp: &SipResponse, strict: bool, d: &mut Digest) {
    echo::from_response(resp, strict, d);
    d.note("status", &(resp.status(), resp.reason(), resp.version()));
    d.note("readiness", &sip_message::worker_report::readiness(resp));
    d.note("incarnation", &sip_message::worker_report::incarnation(resp));
    match retag_response(resp, "lane-tag") {
        Some(retagged) => d.raw("retag", retagged.image()),
        None => d.note("retag", &"none"),
    }
    d.note("tagged", &SipResponseTagged::new(resp).map(|r| r.to_tag().to_owned()));
    match resp.thaw().freeze() {
        Ok(thawed) => d.raw("thaw", thawed.image()),
        Err(e) => d.note("thaw", &e),
    }
}

/// The lenient raw scanners, which read any bytes.
fn sniffs(raw: &[u8], d: &mut Digest) {
    d.note("first-line", &sniff::first_line(raw));
    for name in ["Via", "Contact", "Route", "To", "é"] {
        d.note("header-value", &sniff::header_value(raw, name));
        d.note("header-values", &sniff::header_values(raw, name));
        d.note("has-header", &sniff::has_header(raw, name));
        d.note("option-tags", &sniff::option_tags(raw, name));
        d.note("name-addr-uri", &sniff::name_addr_uri(raw, name));
        d.note("route-uris", &sniff::route_uris(raw, name));
        d.note("route-rows", &sniff::route_rows(raw, name));
    }
    d.note("call-id", &sniff::call_id(raw));
    d.note("tags", &(sniff::to_tag(raw), sniff::from_tag(raw)));
    d.note("kind", &(sniff::is_response(raw), sniff::resp_status(raw), sniff::req_method(raw)));
    d.note("request-uri", &sniff::request_uri(raw));
    d.note(
        "cseq",
        &(
            sniff::cseq_number(raw),
            sniff::cseq_value(raw),
            sniff::cseq_method_token(raw),
            sniff::cseq_method_label(raw),
        ),
    );
    d.note(
        "100rel",
        &(
            sniff::require_has_100rel(raw),
            sniff::rseq_of(raw),
            sniff::rack_rseq(raw),
            sniff::rack_cseq(raw),
        ),
    );
    d.note("sheddable", &sniff::is_sheddable_new_invite(raw));
    d.note("trace-rate", &sniff::trace_sample_rate(raw));
    d.note("branches", &(sniff::via_branch(raw), sniff::via_branches(raw)));
    for at in
        [0, raw.len() / 3, raw.len() / 2, raw.len().saturating_sub(1), raw.len(), raw.len() + 1]
    {
        d.note("line-at", &sniff::line_at(raw, at));
        d.note("site-at", &sip_message::site::site_at(raw, at));
    }
    d.note("content-length", &(sniff::content_length(raw), sniff::has_body(raw)));
    d.note(
        "content-type",
        &(sniff::content_type_is(raw, "application/sdp"), sniff::body(raw), sniff::head_end(raw)),
    );
    d.note("users", &(sniff::from_user(raw), sniff::address_user(&String::from_utf8_lossy(raw))));
    d.note("ruri-facts", &sniff::request_uri_facts(raw));
    d.note("via-rport", &sniff::via_rport(raw));
    d.note("bottom-via", &sniff::bottom_via(raw));
    d.note("stamps", &sniff::relay_stamp_spans(raw));
    d.note("preparse", &sip_message::preparse::is_invite_request_buffer(raw));
    d.note("payload", &Payload::of_datagram(raw).bytes());
    d.note("sip-summary", &sip_message::sip_summary(raw));
    d.note("sipfrag-status", &sipfrag::status(raw));
    d.note("sipfrag-fields", &sipfrag::header_fields(raw));
}

/// Two descriptions through every SDP entry point, `a` as the one read and
/// offered, `b` as the other side.
pub fn description(a: &[u8], b: &[u8], d: &mut Digest) {
    d.note("validate", &validate_sdp_body(a));
    d.note("validate-oa", &sip_message::sdp::validate_offer_answer_body(a));
    let options = BuildHeldSdpOptions { local_ip: "0.0.0.0".into(), now_ms: i64::MAX };
    if let Some(profile) = extract_codec_profile(a) {
        d.note("profile", &profile);
        d.note("held", &build_held_sdp_from_profile(&profile, &options));
    }
    let text = String::from_utf8_lossy(a);
    d.note(
        "rewrite",
        &rewrite_connection_and_ports(&text, Some("203.0.113.1"), |p| p.checked_add(2)),
    );
    d.note("rewrite-ports", &rewrite_connection_and_ports(&text, None, |_| Some(u16::MAX)));
    if let Some(doc) = parse_sdp_body(a) {
        d.note("doc", &doc);
        for m in &doc.media {
            d.note("direction", &sip_message::extract_direction(m));
            d.note("rtpmaps", &sip_message::extract_rtpmaps(m));
            d.note("fmtps", &sip_message::extract_fmtps(m));
            d.note("cryptos", &sip_message::extract_cryptos(m));
            for attr in &m.attributes {
                d.note("direction-of", &sip_message::direction_of(attr));
            }
            if let Some(c) = &m.c_line {
                d.note("unspecified", &sip_message::sdp::c_line_is_unspecified(c));
            }
        }
    }
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("m=") {
            d.note("media-line", &sip_message::media_line(value));
            d.note("format-list", &sip_message::extract_format_list(line));
        }
    }
    if let Some(origin) = parse_origin(a) {
        d.note("origin", &(origin.value(), origin.next_version_line()));
        if let Some(other) = parse_origin(b) {
            d.note("same-session", &origin.identifies_same_session(&other));
        }
    }
    d.note("text-fields", &sip_message::sdp_text::text_fields(a));
    d.note("attributes", &sip_message::sdp_text::attributes(a));
    d.note("canonical", &canonical_form(a));
    d.note("equivalent", &sdp_media_equivalent(a, b));
    for preference in [FormatPreference::Offerer, FormatPreference::Answerer] {
        d.note("answer", &answer_from_own(a, b, preference, None));
        d.note(
            "answer-origin",
            &answer_from_own(a, b, preference, Some("- 1 18446744073709551615 IN IP4 192.0.2.9")),
        );
        d.note("answer-agreeing", &answer_from_own_agreeing(a, b, preference, None, Some(a)));
    }
    d.note("both-ways", &answer_reoffer_both_ways(a, b, b, "- 1 1 IN IP4 192.0.2.9"));
    d.note("live", &sip_message::has_live_stream(a));
    d.note("reject", &reject_offer(a, &options));
    if let Some(stated) = StatedSession::of(a) {
        d.note("stated", &stated);
        let restated = restate_session(b, &stated);
        d.note("restated", &restated);
        d.note("restated-again", &restate_session_again(b, &stated));
        if let Some(r) = restated {
            d.note("author-order", &in_author_order(&r.sdp, &r.slots));
        }
    }
    d.note("author-order-gaps", &in_author_order(a, &[None, Some(u32::MAX), Some(0)]));
    d.note("stamp", &(stamp_session(a, 0), stamp_session(a, u64::MAX)));
}
