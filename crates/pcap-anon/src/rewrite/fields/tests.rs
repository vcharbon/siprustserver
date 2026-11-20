use super::*;

/// Whether the first occurrence of the digit run `run` in `text` is a counter.
fn counter(text: &str, run: &str) -> bool {
    let start = text.find(run).expect("the run is in the text");
    is_counter(text.as_bytes(), start, start + run.len())
}

#[test]
fn counters_are_named_by_field() {
    let head = "INVITE sip:+15556765432@host.example.com:5070;user=phone SIP/2.0\r\n\
                Via: SIP/2.0/UDP 192.0.2.1:5080;branch=z9hG4bK7777;rport=5090\r\n\
                Call-ID: 4444-55@192.0.2.1\r\nCSeq: 1234 INVITE\r\nRAck: 1 2345 INVITE\r\n\
                Max-Forwards: 7000\r\nExpires: 3600\r\nDate: Sat, 13 Nov 2010 23:29:00 GMT\r\n\
                Contact: <sip:[2001:db8::1]:5100>;expires=1800\r\nX-Called: 6666\r\n\
                Content-Length: 1500\r\n\r\n";
    for run in
        ["5070", "5080", "5090", "1234", "2345", "7000", "3600", "2010", "5100", "1800", "1500"]
    {
        assert!(counter(head, run), "{run}");
    }
    for run in ["15556765432", "7777", "4444", "6666"] {
        assert!(!counter(head, run), "{run}");
    }
}

#[test]
fn the_media_stack_lines_of_a_description_are_counters_and_the_rest_is_not() {
    let sdp = "v=0\r\no=- 1111 2222 IN IP4 192.0.2.1\r\ns=Room 3333\r\nt=4444 0\r\n\
               m=audio 5555 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=ssrc:6666 cname:x\r\n\
               a=x-num:7777\r\np=+1 555 0100\r\n";
    for run in ["4444", "5555", "8000", "6666"] {
        assert!(counter(sdp, run), "{run}");
    }
    for run in ["3333", "7777", "0100"] {
        assert!(!counter(sdp, run), "{run}");
    }
}

#[test]
fn a_port_follows_a_host_never_a_scheme_or_a_user() {
    assert!(counter("<sip:media.example.com:5060>", "5060"));
    assert!(counter("http://192.0.2.9:8080/x", "8080"));
    assert!(counter("192.0.2.1:1234", "1234"), "a socket");
    assert!(!counter("<sip:5060@example.com>", "5060"));
    assert!(!counter("<tel:5060;phone-context=example.com>", "5060"));
    assert!(!counter("<urn:uuid:5060-1>", "5060"));
    assert!(!counter("at 12:5060 sharp", "5060"));
    assert!(!counter("a=x-num:5060\r\n", "5060"));
    assert!(!counter("SIP/2.0 200 OK\r\nX-Called:5060\r\n", "5060"));
}

#[test]
fn a_percent_escape_splits_digit_runs() {
    let text = b"88666%4010.112%3B4567";
    let runs: Vec<&[u8]> = digit_runs(text).into_iter().map(|r| &text[r]).collect();
    assert_eq!(runs, [&b"88666"[..], b"10", b"112", b"4567"]);
}
