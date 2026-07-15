//! Stateless Tier-1 overload rejection: template a **503 Service
//! Unavailable** by byte-slicing the mandatory RFC 3261 §8.2.6.2 header
//! lines verbatim out of an inbound INVITE — no parse, no transaction, no
//! `To`-tag — plus the jittered `Retry-After` seam that feeds it.

use super::bytes::find_subslice;

/// Compute a jittered `Retry-After` value (seconds).
///
/// Randomness is **injected**: `roll` yields a fresh value in
/// `[0, u64::MAX]` (e.g. `rand::random::<u64>()` at the network-layer call
/// site), keeping this function deterministic and unit-testable.
///
/// Returns `base_sec` unchanged when `jitter_sec == 0`, otherwise
/// `base_sec + (roll % (jitter_sec + 1))` — a uniform offset in the
/// inclusive range `[0, jitter_sec]`.
pub fn jittered_retry_after(base_sec: u32, jitter_sec: u32, roll: impl FnOnce() -> u64) -> u32 {
    if jitter_sec == 0 {
        return base_sec;
    }
    // `jitter_sec + 1` fits in u64 (jitter_sec: u32); the modulus is in
    // [0, jitter_sec] so the sum cannot exceed base_sec + jitter_sec.
    let offset = (roll() % (u64::from(jitter_sec) + 1)) as u32;
    base_sec + offset
}

/// Build a stateless **503 Service Unavailable** response by byte-slicing the
/// mandatory header lines verbatim out of an inbound INVITE datagram — no SIP
/// parse, no transaction allocation, no `To`-tag. The Tier-1 (UDP
/// pre-ingress) cheap-rejection path that sheds load before the parser runs.
///
/// Per RFC 3261 §8.2.6.2 the five headers a UAS MUST copy into a response are
/// taken verbatim from the request (first occurrence of each; compact forms
/// `v`/`f`/`t`/`i` accepted on input):
///   - `Via` (topmost — the UAC matches the response on it),
///   - `From` (echoed),
///   - `To` (echoed **without** adding our own tag — see below),
///   - `Call-ID`,
///   - `CSeq`.
///
/// We deliberately add **no** `To`-tag. The UAC's resulting ACK therefore
/// carries no dialog context, so it matches nothing in the transaction
/// layer's dialog index and is dropped at the orphan-ACK rule — that is the
/// cheap-rejection contract (and why this is distinct from the Tier-3
/// `b2bua::router::build_stateless_overload_503`, which runs *after* a parse,
/// reuses the full `generate_response` machinery, and DOES add a `To`-tag).
///
/// Returns `None` when the buffer does not look like a SIP **request** we can
/// template (no header terminator, fewer than two lines, a non-request first
/// line, or any of the five required headers missing) — the caller then
/// accepts the packet and lets the normal pipeline reject it, rather than
/// emitting a malformed reply.
///
/// Output header names are normalised to canonical casing; the value (after
/// the first `:`) is copied byte-for-byte from the request line, with no
/// high-bit masking. The reply is CRLF-terminated regardless of whether the
/// request used LF-only separators.
pub fn build_stateless_reject_503_buffer(raw: &[u8], retry_after_sec: u32) -> Option<Vec<u8>> {
    // Locate the header-section terminator: CRLFCRLF, or LFLF as a fallback.
    // The chosen terminator dictates the intra-section line separator.
    let (header_end, line_sep): (usize, &[u8]) = match find_subslice(raw, b"\r\n\r\n") {
        Some(end) => (end, b"\r\n"),
        None => match find_subslice(raw, b"\n\n") {
            Some(end) => (end, b"\n"),
            None => return None,
        },
    };

    let header_section = &raw[..header_end];
    let lines: Vec<&[u8]> = split_on(header_section, line_sep);
    if lines.len() < 2 {
        return None;
    }

    // First line must look like a SIP message: we only template replies to
    // requests, and a request line ends with `SIP/2.0`. (A response status
    // line also contains the token; callers only feed request datagrams.)
    find_subslice(lines[0], b"SIP/2.0")?;

    let mut via_line: Option<&[u8]> = None;
    let mut from_line: Option<&[u8]> = None;
    let mut to_line: Option<&[u8]> = None;
    let mut call_id_line: Option<&[u8]> = None;
    let mut cseq_line: Option<&[u8]> = None;

    for line in &lines[1..] {
        if line.is_empty() {
            continue;
        }
        // A continuation line (RFC 3261 line folding — starts with SP/HTAB)
        // belongs to the previous header; the verbatim single-line copy is the
        // contract, so skip it.
        if line[0] == b' ' || line[0] == b'\t' {
            continue;
        }
        let colon = match find_subslice(line, b":") {
            Some(c) => c,
            None => continue,
        };
        // Header-name match is case-INsensitive (RFC 3261 §7.3.1). The
        // `if ….is_none()` guard on each arm is the "first occurrence wins"
        // contract (a later duplicate is ignored).
        let name = trim_ascii(&line[..colon]).to_ascii_lowercase();
        match name.as_slice() {
            b"via" | b"v" if via_line.is_none() => via_line = Some(line),
            b"from" | b"f" if from_line.is_none() => from_line = Some(line),
            b"to" | b"t" if to_line.is_none() => to_line = Some(line),
            b"call-id" | b"i" if call_id_line.is_none() => call_id_line = Some(line),
            b"cseq" if cseq_line.is_none() => cseq_line = Some(line),
            _ => {}
        }

        if via_line.is_some()
            && from_line.is_some()
            && to_line.is_some()
            && call_id_line.is_some()
            && cseq_line.is_some()
        {
            break;
        }
    }

    let (via_line, from_line, to_line, call_id_line, cseq_line) =
        match (via_line, from_line, to_line, call_id_line, cseq_line) {
            (Some(v), Some(f), Some(t), Some(i), Some(c)) => (v, f, t, i, c),
            _ => return None,
        };

    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + 80);
    out.extend_from_slice(b"SIP/2.0 503 Service Unavailable\r\n");
    push_normalized_header(&mut out, b"Via", via_line);
    push_normalized_header(&mut out, b"From", from_line);
    push_normalized_header(&mut out, b"To", to_line);
    push_normalized_header(&mut out, b"Call-ID", call_id_line);
    push_normalized_header(&mut out, b"CSeq", cseq_line);
    out.extend_from_slice(b"Reason: SIP;cause=503;text=\"overload\"\r\n");
    out.extend_from_slice(b"Retry-After: ");
    out.extend_from_slice(retry_after_sec.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    Some(out)
}

/// Append `canonical_name` + the verbatim value tail of `line` (from its
/// first `:` onward, including the colon and any surrounding whitespace) +
/// CRLF. `line` is a header line known to contain a `:`.
fn push_normalized_header(out: &mut Vec<u8>, canonical_name: &[u8], line: &[u8]) {
    let colon = find_subslice(line, b":").expect("caller passes a line containing ':'");
    out.extend_from_slice(canonical_name);
    out.extend_from_slice(&line[colon..]);
    out.extend_from_slice(b"\r\n");
}

/// Split `data` on every occurrence of `sep`, returning the (possibly empty)
/// pieces between separators; a trailing separator yields a trailing empty
/// piece. `sep` is always non-empty here.
fn split_on<'a>(data: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while let Some(rel) = find_subslice(&data[start..], sep) {
        let at = start + rel;
        pieces.push(&data[start..at]);
        start = at + sep.len();
    }
    pieces.push(&data[start..]);
    pieces
}

/// Trim leading/trailing ASCII whitespace from a byte slice. Avoids a UTF-8
/// round-trip in the brake hot path.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod jitter_tests {
    //! Pins [`jittered_retry_after`]: the zero-jitter identity path (the
    //! brake's `retry_after_jitter_sec = 0` config) and the injected-roll
    //! modular arithmetic.

    use super::jittered_retry_after;

    #[test]
    fn zero_jitter_returns_base_unchanged() {
        // No randomness: the roll closure is never invoked.
        let mut rolled = false;
        let v = jittered_retry_after(2, 0, || {
            rolled = true;
            999
        });
        assert_eq!(v, 2);
        assert!(!rolled, "zero jitter must not consult the roll source");
    }

    #[test]
    fn roll_is_reduced_modulo_jitter_plus_one() {
        // base=10, jitter=4 → offset ∈ [0, 4]; roll=7 ⇒ 7 % 5 = 2 ⇒ 12.
        assert_eq!(jittered_retry_after(10, 4, || 7), 12);
        // roll exactly at a multiple of (jitter+1) ⇒ offset 0 ⇒ base.
        assert_eq!(jittered_retry_after(10, 4, || 5), 10);
        // roll = jitter ⇒ max offset ⇒ base + jitter.
        assert_eq!(jittered_retry_after(10, 4, || 4), 14);
    }

    #[test]
    fn offset_is_bounded_to_zero_through_jitter_inclusive() {
        let (base, jitter) = (30u32, 6u32);
        for roll in 0u64..50 {
            let v = jittered_retry_after(base, jitter, || roll);
            assert!(
                (base..=base + jitter).contains(&v),
                "roll={roll} produced {v}, outside [{base}, {}]",
                base + jitter
            );
        }
    }

    #[test]
    fn large_roll_does_not_overflow() {
        // u64::MAX % (jitter+1) is still a small offset — no panic, in-range.
        let v = jittered_retry_after(1, 9, || u64::MAX);
        assert!((1..=10).contains(&v));
    }
}

#[cfg(test)]
mod build_503_tests {
    //! Pins the byte-slicing contract of [`build_stateless_reject_503_buffer`]:
    //! the five mandatory headers copied verbatim with canonical names, the
    //! deliberate absence of a To-tag, the Reason/Retry-After/Content-Length
    //! trailer, the LF-only fallback, compact header forms, and every `None`
    //! rejection branch.

    use super::build_stateless_reject_503_buffer;

    const FLOODER_IP: &str = "10.0.0.1";
    const FLOODER_PORT: u16 = 5555;
    const B2BUA_IP: &str = "127.0.0.1";
    const B2BUA_PORT: u16 = 5060;

    /// A well-formed non-emergency INVITE (CRLF separators, canonical names).
    fn invite_buf(i: u32) -> Vec<u8> {
        format!(
            "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{FLOODER_IP}:{FLOODER_PORT}>\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// Split a built response into its CRLF-delimited lines.
    fn resp_lines(buf: &[u8]) -> Vec<String> {
        String::from_utf8(buf.to_vec())
            .unwrap()
            .split("\r\n")
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn templates_a_503_from_a_well_formed_invite() {
        let resp = build_stateless_reject_503_buffer(&invite_buf(0), 5).expect("should template");
        let lines = resp_lines(&resp);

        assert_eq!(lines[0], "SIP/2.0 503 Service Unavailable");

        // The five required headers, canonical-named, value copied verbatim.
        assert_eq!(
            lines[1],
            "Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-0"
        );
        assert_eq!(lines[2], "From: <sip:alice@flooder.test>;tag=alice-tag-0");
        assert_eq!(lines[3], "To: <sip:bob@b2bua.test>");
        assert_eq!(lines[4], "Call-ID: brake-test-0@10.0.0.1");
        assert_eq!(lines[5], "CSeq: 1 INVITE");

        // Overload trailer.
        assert_eq!(lines[6], "Reason: SIP;cause=503;text=\"overload\"");
        assert_eq!(lines[7], "Retry-After: 5");
        assert_eq!(lines[8], "Content-Length: 0");
        // Header section terminated by a blank line (CRLFCRLF → trailing "", "").
        assert_eq!(lines[9], "");
        assert_eq!(lines[10], "");
    }

    #[test]
    fn does_not_add_a_to_tag() {
        // The cheap-rejection contract: the echoed To carries NO ;tag= (so the
        // UAC's ACK has no dialog context and is dropped as an orphan ACK).
        let resp = build_stateless_reject_503_buffer(&invite_buf(1), 7).unwrap();
        let to_line = resp_lines(&resp).into_iter().find(|l| l.starts_with("To:")).unwrap();
        assert!(!to_line.contains(";tag="), "Tier-1 503 must not add a To-tag: {to_line:?}");
    }

    #[test]
    fn retry_after_value_is_rendered() {
        let resp = build_stateless_reject_503_buffer(&invite_buf(2), 30).unwrap();
        assert!(resp_lines(&resp).iter().any(|l| l == "Retry-After: 30"));
    }

    #[test]
    fn topmost_via_is_the_one_copied() {
        // Two Via headers (a forwarded request); the response echoes only the
        // first (topmost) — what the UAC matches on.
        let raw = b"INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
Via: SIP/2.0/UDP top.example:5060;branch=z9hG4bK-top\r\n\
Via: SIP/2.0/UDP bottom.example:5060;branch=z9hG4bK-bot\r\n\
From: <sip:a@x>;tag=ft\r\n\
To: <sip:b@y>\r\n\
Call-ID: cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let via = resp_lines(&resp).into_iter().find(|l| l.starts_with("Via:")).unwrap();
        assert_eq!(via, "Via: SIP/2.0/UDP top.example:5060;branch=z9hG4bK-top");
    }

    #[test]
    fn compact_header_forms_are_accepted_and_normalized() {
        // Compact forms v/f/t/i on input → canonical names on output, value
        // copied verbatim (including the original spacing after the colon).
        let raw = b"INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
v: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-c\r\n\
f: <sip:a@x>;tag=cf\r\n\
t: <sip:b@y>\r\n\
i: compact-cid@x\r\n\
CSeq: 7 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-c");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=cf");
        assert_eq!(lines[3], "To: <sip:b@y>");
        assert_eq!(lines[4], "Call-ID: compact-cid@x");
        assert_eq!(lines[5], "CSeq: 7 INVITE");
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        // Mixed-case inbound header names are still matched (RFC 3261 §7.3.1).
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
VIA: SIP/2.0/UDP h:5060;branch=z9hG4bK-u\r\n\
fRoM: <sip:a@x>;tag=u\r\n\
To: <sip:b@y>\r\n\
CALL-ID: u-cid@x\r\n\
cSeQ: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-u");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=u");
        assert_eq!(lines[4], "Call-ID: u-cid@x");
    }

    #[test]
    fn first_occurrence_of_each_header_wins() {
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-1\r\n\
From: <sip:first@x>;tag=one\r\n\
From: <sip:second@x>;tag=two\r\n\
To: <sip:b@y>\r\n\
Call-ID: cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let from = resp_lines(&resp).into_iter().find(|l| l.starts_with("From:")).unwrap();
        assert_eq!(from, "From: <sip:first@x>;tag=one");
    }

    #[test]
    fn lf_only_separators_are_accepted_and_output_is_crlf() {
        // LFLF fallback: the request uses bare LF, but the templated reply is
        // always CRLF-terminated.
        let raw = b"INVITE sip:bob SIP/2.0\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-lf\n\
From: <sip:a@x>;tag=lf\n\
To: <sip:b@y>\n\
Call-ID: lf-cid@x\n\
CSeq: 1 INVITE\n\
Content-Length: 0\n\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).expect("LF-only should template");
        assert!(resp.starts_with(b"SIP/2.0 503 Service Unavailable\r\n"));
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-lf");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=lf");
    }

    #[test]
    fn folded_continuation_line_is_skipped_not_misparsed() {
        // A folded Via value (continuation line starts with SP): the
        // continuation is skipped as a header line and the first physical Via
        // line is copied verbatim — the copy is single-physical-line, not the
        // unfolded value.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060\r\n ;branch=z9hG4bK-folded\r\n\
From: <sip:a@x>;tag=fold\r\n\
To: <sip:b@y>\r\n\
Call-ID: fold-cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060");
        // From/To/etc. still found after the continuation.
        assert_eq!(lines[2], "From: <sip:a@x>;tag=fold");
    }

    // ---- None / rejection branches -------------------------------------

    #[test]
    fn no_header_terminator_returns_none() {
        // No CRLFCRLF and no LFLF anywhere → cannot find the header section.
        let raw = b"INVITE sip:bob SIP/2.0\r\nVia: SIP/2.0/UDP h\r\nFrom: x";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn first_line_without_sip_version_returns_none() {
        // Pins the missing-`SIP/2.0`-token guard, NOT response-rejection: the
        // function keys only off whether the first line contains the `SIP/2.0`
        // substring. A genuine response status line DOES carry it, so this
        // guard does not by itself reject inbound responses — the brake only
        // feeds it request datagrams.
        let raw = b"GARBAGE LINE NO VERSION\r\nVia: x\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn missing_required_header_returns_none() {
        // Has Via/From/To/Call-ID but NO CSeq → not templatable.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-n\r\n\
From: <sip:a@x>;tag=n\r\n\
To: <sip:b@y>\r\n\
Call-ID: n-cid@x\r\n\
Content-Length: 0\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn header_section_with_only_a_request_line_returns_none() {
        // CRLFCRLF immediately after the request line → fewer than two lines.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn empty_buffer_returns_none() {
        assert!(build_stateless_reject_503_buffer(b"", 5).is_none());
    }
}
