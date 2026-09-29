//! `canonical_form`: a description serialized by the stack itself — each
//! format's `rtpmap` then its `fmtp`, the other attributes as written, the
//! direction last and always stated (RFC 3264 §6.1, RFC 4566 §6).

use sip_message::canonical_form;

fn canonical(sdp: &str) -> String {
    String::from_utf8(canonical_form(sdp.as_bytes()).expect("a new form")).unwrap()
}

/// A format's `fmtp` written before the `rtpmap` lines moves behind its
/// `rtpmap`; the stated direction moves behind `a=ptime`.
#[test]
fn each_rtpmap_is_followed_by_its_fmtp_and_the_direction_comes_last() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8 18 101\r\n\
a=fmtp:18 annexb=no\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:18 G729/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=sendrecv\r\n\
a=ptime:20\r\n";
    assert_eq!(
        canonical(sdp),
        "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8 18 101\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:18 G729/8000\r\n\
a=fmtp:18 annexb=no\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n"
    );
}

/// A media section with no direction attribute states `a=sendrecv`, the
/// default (RFC 3264 §5.1), after its other attributes; the other attributes
/// keep their order.
#[test]
fn a_media_section_without_a_direction_states_sendrecv() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 18 101\r\n\
a=rtpmap:18 G729/8000\r\n\
a=fmtp:18 annexb=no\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=silenceSupp:off - - - -\r\n\
a=maxptime:30\r\n\
a=ptime:20\r\n";
    assert_eq!(
        canonical(sdp),
        format!("{sdp}a=sendrecv\r\n"),
        "only the direction is added, at the end"
    );
}

/// The session-level direction is the one each media section takes (RFC 4566
/// §6): stated explicitly in the section, and kept at session level.
#[test]
fn a_session_level_direction_is_stated_in_each_media_section() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
a=recvonly\r\n\
m=audio 49170 RTP/AVP 8 101\r\n\
c=IN IP4 192.0.2.1\r\n\
b=RR:0\r\n\
b=RS:0\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=maxptime:40\r\n\
a=ptime:20\r\n";
    assert_eq!(canonical(sdp), format!("{sdp}a=recvonly\r\n"));
}

/// A media section's own direction wins over the session-level one; each
/// section is put in form on its own.
#[test]
fn each_media_section_keeps_its_own_direction() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
a=sendonly\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=inactive\r\n\
a=rtpmap:0 PCMU/8000\r\n\
m=video 51372 RTP/AVP 96\r\n\
a=fmtp:96 profile-level-id=42e01f\r\n\
a=rtpmap:96 H264/90000\r\n";
    assert_eq!(
        canonical(sdp),
        "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
a=sendonly\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=inactive\r\n\
m=video 51372 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 profile-level-id=42e01f\r\n\
a=sendonly\r\n"
    );
}

/// An `rtpmap` or `fmtp` naming a format the `m=` line does not list is one of
/// the other attributes: it keeps its place among them.
#[test]
fn a_format_attribute_the_m_line_does_not_list_stays_among_the_others() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
a=fmtp:18 annexb=no\r\n\
a=ptime:20\r\n\
a=rtpmap:8 PCMA/8000\r\n";
    assert_eq!(
        canonical(sdp),
        "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=fmtp:18 annexb=no\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n"
    );
}

/// A description already in form is not rewritten; a body that is no
/// session description has no form.
#[test]
fn a_description_in_form_or_no_description_has_no_new_form() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8 101\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=fmtp:101 0-15\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n";
    assert_eq!(canonical_form(sdp.as_bytes()), None);
    assert_eq!(canonical_form(b"<xml/>"), None);
    assert_eq!(canonical_form(b""), None);
}

/// A description written with bare LF keeps LF; its last line gains one.
#[test]
fn the_line_ending_is_the_descriptions_own() {
    let sdp = "v=0\no=- 10 20 IN IP4 192.0.2.1\ns=-\nc=IN IP4 192.0.2.1\nt=0 0\nm=audio 49170 RTP/AVP 8\na=rtpmap:8 PCMA/8000";
    assert_eq!(
        canonical(sdp),
        "v=0\no=- 10 20 IN IP4 192.0.2.1\ns=-\nc=IN IP4 192.0.2.1\nt=0 0\nm=audio 49170 RTP/AVP 8\na=rtpmap:8 PCMA/8000\na=sendrecv\n"
    );
}

/// A description with no media section has nothing to put in form.
#[test]
fn a_description_without_media_is_as_written() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n";
    assert_eq!(canonical_form(sdp.as_bytes()), None);
}

/// A rejected stream (port 0) gains no direction: its attributes carry no
/// meaning (RFC 3264 §6). The live one beside it does.
#[test]
fn a_rejected_stream_gains_no_direction() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
m=video 0 RTP/AVP 96\r\n";
    assert_eq!(
        canonical(sdp),
        "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
a=sendrecv\r\n\
m=video 0 RTP/AVP 96\r\n"
    );
    let rejected = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 8\r\n";
    assert_eq!(canonical_form(rejected.as_bytes()), None);
}

/// A mix of CRLF and bare LF leaves as CRLF throughout; a blank line is left
/// out.
#[test]
fn a_mixed_line_ending_leaves_as_crlf_without_blank_lines() {
    let sdp = "v=0\r\no=- 10 20 IN IP4 192.0.2.1\ns=-\r\n\r\nt=0 0\nm=audio 49170 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\n";
    assert_eq!(
        canonical(sdp),
        "v=0\r\no=- 10 20 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 49170 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n"
    );
}
