//! The measured operations, each built over the input one axis point names.
//! Building the input happens outside the measured region; the returned
//! closure is the operation alone.

use std::hint::black_box;

use sip_message::draft::{HeaderList, RequestDraft};
use sip_message::generators::{
    generate_in_dialog_request, generate_out_of_dialog_request, generate_response,
    relayable_request_headers, GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts,
    GenerateResponseOpts, InDialogMethod, OutOfDialogMethod, RelayScope, StackDialog,
};
use sip_message::header::{
    self, CSeq, CallId, Contact, MaxForwards, MediaType, ParamValue, RecordRouteEntry, RouteEntry,
    To, Uri, Via,
};
use sip_message::{
    answer_from_own, decompose_multipart, extract_codec_profile, parse_sdp_body, Bytes,
    FormatPreference, HeaderName, Method, SipParser, SipRequest,
};

use super::wire::{self, Shape, PROXY_HOST};

/// One operation over a prepared input.
pub type Op = Box<dyn FnMut()>;

fn parse(shape: Shape) -> Op {
    let parser = wire::parser();
    let datagram = wire::invite(&shape);
    Box::new(move || {
        black_box(parser.parse_shared(datagram.clone()).expect("the fixture parses"));
    })
}

pub fn parse_lines(n: usize) -> Op {
    parse(Shape::lines(n))
}

pub fn parse_vias(n: usize) -> Op {
    parse(Shape::vias(n, 1))
}

pub fn parse_vias_folded(n: usize) -> Op {
    parse(Shape::vias(n, 0))
}

pub fn parse_routes(n: usize) -> Op {
    parse(Shape::own_routes(n, 1))
}

pub fn parse_routes_folded(n: usize) -> Op {
    parse(Shape::own_routes(n, 0))
}

pub fn parse_sdp_body_kib(kib: usize) -> Op {
    parse(Shape::body("application/sdp", wire::sdp_sections(kib * 1024)))
}

pub fn parse_multipart_kib(kib: usize) -> Op {
    parse(Shape::body(&wire::multipart_type(), wire::multipart(kib * 1024)))
}

/// One read of a header the request does not carry.
pub fn read_absent(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    Box::new(move || {
        black_box(request.header::<header::Event>());
    })
}

/// One read of a single-valued header the request carries once, near the top.
pub fn read_present_single(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    Box::new(move || {
        black_box(request.header::<MaxForwards>());
    })
}

/// The reads one B2BUA turn makes of a request it handles.
fn turn_reads(request: &SipRequest) {
    black_box(request.sdp());
    black_box(request.header::<MediaType>());
    black_box(request.raw(HeaderName::ContentType).count());
    black_box(request.header::<header::RAck>());
    black_box(request.header::<header::Reason>());
    black_box(request.raw(HeaderName::To).count());
    black_box(request.raw(HeaderName::RetryAfter).count());
    black_box(request.raw(HeaderName::PEarlyMedia).count());
    black_box(request.raw(HeaderName::Via).count());
    black_box(request.raw(HeaderName::UserToUser).count());
    black_box(request.header::<Contact>());
    black_box(request.header::<header::Require>());
    black_box(request.has(&HeaderName::RSeq));
    black_box(request.header::<MaxForwards>());
    black_box(request.header::<header::Event>());
    black_box(request.offers_100rel());
    let _ = black_box(request.record_route_set());
    let _ = black_box(request.route_set());
    black_box(request.header::<header::ReferTo>());
    black_box(request.header::<header::Supported>());
    black_box(request.has(&HeaderName::PAssertedIdentity));
    black_box(request.raw_text(HeaderName::Allow).count());
    black_box(request.header::<header::SessionExpires>());
    black_box(request.header::<header::MinSe>());
}

pub fn read_turn(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    Box::new(move || turn_reads(&request))
}

pub fn read_route_set(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::own_routes(n, 1)));
    Box::new(move || {
        black_box(request.route_set().expect("the routes read"));
    })
}

pub fn read_sdp_kib(kib: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::body(
        "application/sdp",
        wire::sdp_sections(kib * 1024),
    )));
    Box::new(move || {
        black_box(request.sdp());
    })
}

pub fn read_multipart_sdp_kib(kib: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::body(
        &wire::multipart_type(),
        wire::multipart(kib * 1024),
    )));
    Box::new(move || {
        black_box(request.sdp());
    })
}

/// The rewrite set a forwarding hop applies (RFC 3261 §16.6): stamp the top
/// Via, restate Max-Forwards, record two routes, push its own Via, render.
fn proxy_hop(request: &SipRequest) -> Bytes {
    request
        .thaw()
        .update_top::<Via>(|via| via.stamped_from("192.0.2.10", 33000))
        .expect("the top Via reads")
        .set(MaxForwards::new(69))
        .push_front(RecordRouteEntry::from_uri(Uri::sip(PROXY_HOST).with_flag("lr")))
        .push_front(RecordRouteEntry::from_uri(
            Uri::sip(PROXY_HOST).with_param("w", ParamValue::text("b2b-1")).with_flag("lr"),
        ))
        .push_front(Via::udp(PROXY_HOST, 5060).with_branch("z9hG4bK-hop").requesting_rport())
        .freeze_bytes()
        .expect("a thawed draft is complete")
}

fn hop(shape: Shape) -> Op {
    let request = wire::request(&wire::invite(&shape));
    Box::new(move || {
        black_box(proxy_hop(&request));
    })
}

pub fn hop_lines(n: usize) -> Op {
    hop(Shape::lines(n))
}

pub fn hop_vias(n: usize) -> Op {
    hop(Shape::vias(n, 1))
}

pub fn hop_vias_folded(n: usize) -> Op {
    hop(Shape::vias(n, 0))
}

/// A hop that pops every Route entry naming itself before it forwards.
fn hop_popping(shape: Shape) -> Op {
    let request = wire::request(&wire::invite(&shape));
    Box::new(move || {
        let own = request.route_set().expect("the routes read").len();
        let mut draft = request.thaw();
        for _ in 0..own {
            draft = draft.pop_top::<RouteEntry>().expect("an own Route reads");
        }
        black_box(draft.freeze_bytes().expect("a thawed draft is complete"));
    })
}

pub fn hop_pop_routes(n: usize) -> Op {
    hop_popping(Shape::own_routes(n, 1))
}

pub fn hop_pop_routes_folded(n: usize) -> Op {
    hop_popping(Shape::own_routes(n, 0))
}

/// The most own Route entries one datagram can carry: a hundred to a line,
/// each line within the production length bound.
pub fn hop_pop_routes_datagram(n: usize) -> Op {
    hop_popping(Shape::own_routes(n, 100))
}

/// The Via list edited as one list and written back unchanged.
pub fn draft_list_vias(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::vias(n, 1)));
    Box::new(move || {
        let draft = request.thaw().vias(|list| list).expect("the Vias read");
        black_box(draft.freeze_bytes().expect("a thawed draft is complete"));
    })
}

/// `n` values pushed at the top of one header list.
pub fn header_list_push_front(n: usize) -> Op {
    let hop = Via::udp(PROXY_HOST, 5060).with_branch("z9hG4bK-hop");
    Box::new(move || {
        let mut list = HeaderList::empty();
        for _ in 0..n {
            list = list.push_front(hop.clone());
        }
        black_box(list);
    })
}

/// `n` values taken off the top of one header list.
pub fn header_list_pop_front(n: usize) -> Op {
    let hop = Via::udp(PROXY_HOST, 5060).with_branch("z9hG4bK-hop");
    let full: HeaderList<Via> = std::iter::repeat_n(hop, n).collect();
    Box::new(move || {
        let mut list = full.clone();
        while list.pop_front().is_some() {}
        black_box(list);
    })
}

/// A request thawed and frozen into a message again.
fn thaw_freeze(shape: Shape) -> Op {
    let request = wire::request(&wire::invite(&shape));
    Box::new(move || {
        black_box(request.thaw().set(MaxForwards::new(69)).freeze().expect("it freezes"));
    })
}

pub fn freeze_lines(n: usize) -> Op {
    thaw_freeze(Shape::lines(n))
}

pub fn freeze_vias(n: usize) -> Op {
    thaw_freeze(Shape::vias(n, 1))
}

/// A blank draft stating the mandatory set and `n` typed headers. The values
/// are built once: cloning one copies no text.
pub fn build_typed(n: usize) -> Op {
    let uri = Uri::sip_user("bob", "example.com");
    let via = Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-abc123");
    let from = header::From::from_uri(Uri::sip_user("alice", "example.com")).with_tag("1928");
    let to = To::from_uri(Uri::sip_user("bob", "example.com"));
    let call_id = CallId::new("a84b4c76e66710@pc33.example.com");
    let record_route = RecordRouteEntry::from_uri(Uri::sip(PROXY_HOST).with_flag("lr"));
    Box::new(move || {
        let mut draft = RequestDraft::new(Method::Invite, uri.clone())
            .push(via.clone())
            .push(MaxForwards::new(70))
            .push(from.clone())
            .push(to.clone())
            .push(call_id.clone())
            .push(CSeq::new(314159, Method::Invite));
        for _ in 0..n {
            draft = draft.push(record_route.clone());
        }
        black_box(draft.freeze().expect("it freezes"));
    })
}

/// The same request thawed and rendered to bytes, no message assembled: the
/// render alone, beside [`freeze_lines`].
pub fn freeze_bytes_lines(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    Box::new(move || {
        black_box(request.thaw().set(MaxForwards::new(69)).freeze_bytes().expect("it renders"));
    })
}

/// A parsed request cloned whole, as a transaction keeps its INVITE.
pub fn clone_request_lines(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    Box::new(move || {
        black_box(request.clone());
    })
}

/// One read of a single-valued header the request repeats on `n` lines.
pub fn read_single_repeated(n: usize) -> Op {
    let wire = wire::invite(&Shape::lines(0));
    let text = String::from_utf8(wire.to_vec()).expect("ascii");
    let lines = "Expires: 3600\r\n".repeat(n.max(1));
    let text = text.replacen("Max-Forwards: 70\r\n", &format!("Max-Forwards: 70\r\n{lines}"), 1);
    let request = wire::request(&Bytes::from(text.into_bytes()));
    Box::new(move || {
        black_box(request.header::<header::Expires>());
    })
}

fn respond(shape: Shape) -> Op {
    let request = wire::request(&wire::invite(&shape));
    let opts = GenerateResponseOpts {
        to_tag: Some("as83kf".to_string()),
        contact: Some(Contact::from_uri(Uri::sip_user("b2bua", "10.0.0.1").with_port(5060))),
        incoming_source: Some(("192.0.2.10".to_string(), 33000)),
        ..Default::default()
    };
    Box::new(move || {
        black_box(generate_response(&request, 200, "OK", &opts).expect("it builds"));
    })
}

pub fn response_lines(n: usize) -> Op {
    respond(Shape::lines(n))
}

pub fn response_vias(n: usize) -> Op {
    respond(Shape::vias(n, 1))
}

pub fn response_record_routes(n: usize) -> Op {
    respond(Shape::record_routes(n))
}

/// A BYE on a dialog whose route set holds `n` entries.
pub fn in_dialog_routes(n: usize) -> Op {
    let dialog = StackDialog {
        call_id: "a84b4c76e66710@pc33.example.com".to_string(),
        local_tag: "1928".to_string(),
        remote_tag: "as83kf".to_string(),
        local_uri: "sip:alice@example.com".to_string(),
        remote_uri: "sip:bob@example.com".to_string(),
        remote_target: "sip:bob@192.0.2.99:5060".to_string(),
        local_cseq: 314159,
        route_set: (0..n).map(|i| format!("<sip:rr{i}.example.net;lr>")).collect(),
    };
    let opts = GenerateInDialogRequestOpts {
        via: Some(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-abc123")),
        ..Default::default()
    };
    Box::new(move || {
        black_box(generate_in_dialog_request(InDialogMethod::Bye, &dialog, &opts).expect("ok"));
    })
}

/// The b-leg INVITE a B2BUA mints from a received one: the relayable headers
/// selected, then the origination recipe carrying them.
pub fn relay_mint_lines(n: usize) -> Op {
    let request = wire::request(&wire::invite(&Shape::lines(n)));
    let base = GenerateOutOfDialogRequestOpts {
        request_uri: Some(Uri::sip_user("bob", "example.com")),
        call_id: "b-leg-call-id@10.0.0.1".to_string(),
        from: Some(header::From::from_uri(Uri::sip_user("alice", "example.com")).with_tag("b1")),
        to: Some(To::from_uri(Uri::sip_user("bob", "example.com"))),
        cseq: 1,
        via: Some(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-b")),
        contact: Some(Contact::from_uri(Uri::sip_user("b2bua", "10.0.0.1").with_port(5060))),
        ..Default::default()
    };
    Box::new(move || {
        let extra_headers = relayable_request_headers(&request, RelayScope::request());
        let opts = GenerateOutOfDialogRequestOpts { extra_headers, ..base.clone() };
        black_box(generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts).expect("ok"));
    })
}

pub fn sdp_parse_sections_kib(kib: usize) -> Op {
    let sdp = wire::sdp_sections(kib * 1024);
    Box::new(move || {
        black_box(parse_sdp_body(&sdp));
    })
}

pub fn sdp_parse_attributes_kib(kib: usize) -> Op {
    let sdp = wire::sdp_attributes(kib * 1024);
    Box::new(move || {
        black_box(parse_sdp_body(&sdp));
    })
}

pub fn sdp_codec_profile_kib(kib: usize) -> Op {
    let sdp = wire::sdp_sections(kib * 1024);
    Box::new(move || {
        black_box(extract_codec_profile(&sdp));
    })
}

pub fn sdp_answer_sections_kib(kib: usize) -> Op {
    let offer = wire::sdp_sections(kib * 1024);
    let own = wire::sdp_sections(kib * 1024);
    Box::new(move || {
        black_box(answer_from_own(&offer, &own, FormatPreference::Offerer, None));
    })
}

pub fn sdp_answer_attributes_kib(kib: usize) -> Op {
    let offer = wire::sdp_attributes(kib * 1024);
    let own = wire::sdp_attributes(kib * 1024);
    Box::new(move || {
        black_box(answer_from_own(&offer, &own, FormatPreference::Offerer, None));
    })
}

pub fn multipart_decompose_kib(kib: usize) -> Op {
    let body = wire::multipart(kib * 1024);
    Box::new(move || {
        black_box(decompose_multipart(&body, wire::BOUNDARY));
    })
}

/// A request whose top Via carries `n` extension parameters.
pub fn parse_via_params(n: usize) -> Op {
    let params: String = (0..n).map(|k| format!(";p{k}=v{k}")).collect();
    let wire = wire::invite(&Shape::lines(0));
    let text = String::from_utf8(wire.to_vec()).expect("ascii");
    let text = text.replacen(";branch=z9hG4bK-h0", &format!(";branch=z9hG4bK-h0{params}"), 1);
    let datagram = Bytes::from(text.into_bytes());
    let parser = wire::parser();
    Box::new(move || {
        black_box(parser.parse_shared(datagram.clone()).expect("the fixture parses"));
    })
}

/// A request whose From URI carries `n` parameters.
pub fn parse_from_uri_params(n: usize) -> Op {
    let params: String = (0..n).map(|k| format!(";u{k}=v{k}")).collect();
    let wire = wire::invite(&Shape::lines(0));
    let text = String::from_utf8(wire.to_vec()).expect("ascii");
    let text =
        text.replacen("<sip:alice@example.com>", &format!("<sip:alice@example.com{params}>"), 1);
    let datagram = Bytes::from(text.into_bytes());
    let parser = wire::parser();
    Box::new(move || {
        black_box(parser.parse_shared(datagram.clone()).expect("the fixture parses"));
    })
}
