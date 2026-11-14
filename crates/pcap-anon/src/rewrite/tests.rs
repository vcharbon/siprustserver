use super::*;

fn digits(pairs: &[(&[u8], &[u8])]) -> DigitSubs {
    DigitSubs::new(pairs.iter().map(|(f, t)| (f.to_vec(), t.to_vec())).collect())
}

fn displays(pairs: &[(&str, &str)]) -> DisplaySubs {
    DisplaySubs::new(
        pairs.iter().map(|(f, t)| (f.as_bytes().to_vec(), t.as_bytes().to_vec())).collect(),
    )
}

#[test]
fn digit_substitution_is_longest_first_and_non_cascading() {
    let subs = digits(&[(b"82217", b"00002"), (b"22345678", b"00000001")]);
    assert_eq!(
        subs.apply(b"+15550102822345678 and 82217", None),
        b"+15550102800000001 and 00002".to_vec()
    );
    // A fake that is itself a key never re-matches: scanned bytes are not
    // revisited.
    let subs = digits(&[(b"41001", b"41002"), (b"41002", b"41003")]);
    assert_eq!(subs.apply(b"41001 41002", None), b"41002 41003".to_vec());
    let subs = digits(&[
        (b"8623", b"0912"),
        (b"62345678", b"00091234"),
        (b"1555001", b"1555001"),
        (b"3001", b"0934"),
        (b"12345", b"00091"),
        (b"34567", b"00092"),
    ]);
    assert_eq!(
        subs.apply(b"+2862345678 +288623 +15550012862345678 3001 1234567", None),
        b"+2800091234 +280912 +15550012800091234 0934 0009167".to_vec(),
        "of two overlapping occurrences the longer key is the identity even when the shorter \
         starts first: the short code spelling the CC's last digit and the NSN's head never \
         splits it, on its own it maps; a trunk id mapped to itself is consumed whole over a \
         short key inside it; equal lengths: leftmost"
    );
    assert_eq!(subs.apply_zeroing(b"926234567899"), b"000009123400".to_vec());
    let subs = DigitSubs::with_weak(
        vec![(b"62345678".to_vec(), b"00091234".to_vec())],
        vec![
            (b"286234567".to_vec(), b"280009123".to_vec()),
            (b"0622623456789".to_vec(), b"0620000912340".to_vec()),
        ],
    );
    assert_eq!(
        subs.apply(b"+2862345678 +286234567 +0622623456789", None),
        b"+2800091234 +280009123 +0620000912340".to_vec(),
        "a weak key yields to a strong one it partially overlaps (the truncated `+CC` form \
         beside the NSN it is a prefix of) and wins over one it contains (its composed form \
         carries that one)"
    );
    // The CC behind a `+` is an anchor: a short code spelling its last
    // digit and the head of a service prefix yields, the NSN behind the
    // prefix maps, and the composed form keeps its CC; the same digits
    // elsewhere (bare, or behind a CC they do not overlap) map; a run
    // containing the CC (a truncated weak one) keeps it.
    let subs = DigitSubs::anchored(
        vec![(b"8100".to_vec(), b"0912".to_vec()), (b"62345678".to_vec(), b"00091234".to_vec())],
        vec![(b"281234567".to_vec(), b"280009123".to_vec())],
        vec![b"28".to_vec(), b"1".to_vec()],
    );
    assert_eq!(
        subs.apply(b"+281001262345678 sip:8100@h +288100 +281234567 +181001", None),
        b"+281001200091234 sip:0912@h +280912 +280009123 +109121".to_vec()
    );
}

#[test]
fn empty_substitution_sets_are_identity() {
    assert_eq!(digits(&[]).apply(b"sip:41001@h", None), b"sip:41001@h".to_vec());
    assert_eq!(digits(&[]).apply_zeroing(b"6940"), b"0000".to_vec());
    assert_eq!(displays(&[]).apply(b"\"A\" <sip:41001@h>"), b"\"A\" <sip:41001@h>".to_vec());
}

#[test]
fn zeroing_collapses_every_byte_no_key_consumed() {
    let subs = digits(&[(b"22345678", b"00090001")]);
    assert_eq!(subs.apply_zeroing(b"9922345678"), b"0000090001".to_vec());
    assert_eq!(subs.apply_zeroing(b"223456789"), b"000900010".to_vec());
}

#[test]
fn the_cseq_value_is_exempt_from_the_rewrite_and_no_identity_is() {
    // The key is rewritten wherever its digits occur — composed, inside a
    // longer run — but in the CSeq value (and the RAck echoing it), a port and
    // an identifier (a Call-ID, a tag: the identifier pass's).
    let subs = digits(&[(b"57073", b"00091")]);
    let keep = Some((b"857073".as_slice(), b"INVITE".as_slice()));
    assert_eq!(
        subs.apply(
            b"CSeq: 857073 INVITE\r\nRAck: 1 857073 invite\r\nCall-ID: 857073x\r\n\
                  From: <sip:+2857073@h>;tag=57073\r\nContact: <sip:57073@h:57073>",
            keep
        ),
        b"CSeq: 857073 INVITE\r\nRAck: 1 857073 invite\r\nCall-ID: 857073x\r\n\
              From: <sip:+2800091@h>;tag=57073\r\nContact: <sip:00091@h:57073>"
            .to_vec()
    );
    // A CSeq EQUAL to the key: it stays, the Call-ID is the identifier pass's.
    let keep = Some((b"57073".as_slice(), b"INVITE".as_slice()));
    assert_eq!(
        subs.apply(
            b"CSeq: 57073 INVITE\r\nTo: <sip:57073@h>\r\nCall-ID: 57073\r\n\r\no=57073 1 IN",
            keep
        ),
        b"CSeq: 57073 INVITE\r\nTo: <sip:00091@h>\r\nCall-ID: 57073\r\n\r\no=00091 1 IN".to_vec()
    );
    // Another method after the number is not this message's CSeq value.
    assert_eq!(subs.apply(b"X-A: 57073 INVITER", keep), b"X-A: 00091 INVITER".to_vec());
    // A key nested twice in the CSeq run: the whole run stays.
    let keep = Some((b"5707357073".as_slice(), b"BYE".as_slice()));
    assert_eq!(
        subs.apply(b"CSeq: 5707357073 BYE\r\nX-A: 5707357073", keep),
        b"CSeq: 5707357073 BYE\r\nX-A: 0009100091".to_vec()
    );
}

#[test]
fn counters_named_by_field_are_exempt_and_identities_beside_them_are_not() {
    let subs = digits(&[(b"8000", b"0912"), (b"1234", b"0934"), (b"41001", b"00091")]);
    let keep = Some((b"1".as_slice(), b"INVITE".as_slice()));
    assert_eq!(
        subs.apply(
            b"CSeq: 1 INVITE\r\nContact: <sip:8000@h:1234>\r\nContent-Length: 1234\r\n\
                  l: 8000\r\nX-Length: 1234\r\nCall-ID: 1234@h\r\n\r\n\
                  o=8000 1234 41001 IN IP4 10.0.0.1\r\nm=audio 8000 RTP/AVP 8 101\r\n\
                  a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000/1234\r\n\
                  a=fmtp:101 0-15 8000\r\n",
            keep
        ),
        b"CSeq: 1 INVITE\r\nContact: <sip:0912@h:1234>\r\nContent-Length: 1234\r\n\
              l: 8000\r\nX-Length: 0934\r\nCall-ID: 1234@h\r\n\r\n\
              o=0912 1234 41001 IN IP4 10.0.0.1\r\nm=audio 8000 RTP/AVP 8 101\r\n\
              a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000/1234\r\n\
              a=fmtp:101 0-15 8000\r\n"
            .to_vec()
    );
    // Without a CSeq to keep (a mirrored model string), the fields still hold.
    assert_eq!(subs.apply(b"Content-Length: 1234", None), b"Content-Length: 1234".to_vec());
}

/// `3401` is a short number of the capture, `62345678` a national one.
#[test]
fn an_origin_s_version_stays_its_session_id_maps_a_phone_number_and_its_username_maps() {
    let subs = digits(&[(b"3401", b"0919"), (b"62345678", b"00091267")])
        .mapping_phones_in_origin(&std::collections::HashSet::from([b"62345678".to_vec()]));
    let keep = Some((b"1".as_slice(), b"ACK".as_slice()));
    for origin in [
        "o=- 1789719543401 1789719543401 IN IP4 h",
        "o=- 1789719543401 1789719543402 IN IP4 h",
        "o=- 34013401 34013402 IN IP4 h",
        "o=- 3401 3401 IN IP4 h",
        "o=- 1 062345678 IN IP4 h",
    ] {
        let sdp = format!("CSeq: 1 ACK\r\n\r\nv=0\r\n{origin}\r\n");
        assert_eq!(subs.apply(sdp.as_bytes(), keep), sdp.as_bytes(), "{origin}");
    }
    // The username, the fourth field and a header that only looks like an
    // origin map.
    assert_eq!(
        subs.apply(
            b"CSeq: 1 ACK\r\nX-o: a 13401 1 b\r\n\r\no=3401 3401 3401 13401 IP4 h\r\n\
                  o=-  3401 5 IN IP4 h\r\n",
            keep
        ),
        b"CSeq: 1 ACK\r\nX-o: a 10919 1 b\r\n\r\no=0919 3401 3401 10919 IP4 h\r\n\
              o=-  3401 5 IN IP4 h\r\n"
            .to_vec()
    );
    // A session id a phone-width key spells is that number and maps.
    assert_eq!(
        subs.apply(b"CSeq: 1 ACK\r\no=- 2862345678 1 IN IP4 h\r\n", keep),
        b"CSeq: 1 ACK\r\no=- 2800091267 1 IN IP4 h\r\n".to_vec()
    );
    // Without a CSeq to keep (a mirrored model string), the fields still hold.
    assert_eq!(subs.apply(b"o=- 13401 1 IN", None), b"o=- 13401 1 IN".to_vec());
}

#[test]
fn display_substitution_handles_quoted_and_unquoted_forms() {
    let subs = displays(&[("MeetMe 85245", "XxxxXx 85245")]);
    assert_eq!(
        subs.apply(b"Contact: \"MeetMe 85245\" <sip:85245@h>"),
        b"Contact: \"XxxxXx 85245\" <sip:85245@h>".to_vec()
    );
    assert_eq!(
        subs.apply(b"From: MeetMe 85245 <sip:85245@h>"),
        b"From: XxxxXx 85245 <sip:85245@h>".to_vec()
    );
    // Not followed by '<': left alone.
    assert_eq!(subs.apply(b"note MeetMe 85245 today"), b"note MeetMe 85245 today".to_vec());
}

#[test]
fn display_substitution_falls_back_to_a_shorter_name_the_context_admits() {
    // The longest name at a position is tried first; when its context
    // fails, a shorter name at the same position still rewrites.
    let subs = displays(&[("AB", "XX"), ("AB <", "XX <")]);
    assert_eq!(subs.apply(b"From: AB <sip:41001@h>"), b"From: XX <sip:41001@h>".to_vec());
    assert_eq!(subs.apply(b"From: \"AB\" <sip:41001@h>"), b"From: \"XX\" <sip:41001@h>".to_vec());
    // Two names starting at the same position, both in context: the
    // longest wins; a name at the very end of the input is not a match.
    let subs = displays(&[("Foo", "Xxx"), ("Foo Testy", "Xxx Xxxxx")]);
    assert_eq!(subs.apply(b"\"Foo Testy\"<sip:x> Foo"), b"\"Xxx Xxxxx\"<sip:x> Foo".to_vec());
    assert_eq!(subs.apply(b"Foo\t<sip:x>;Foo Testy"), b"Xxx\t<sip:x>;Foo Testy".to_vec());
}

#[test]
fn nested_pairs_are_substring_pairs_in_sorted_index_order() {
    let fakes = ["00091", "000913456", "00092", "0009100091", "000923456"];
    assert_eq!(nested_pairs(&fakes), vec![(0, 1), (0, 3), (2, 4)]);
    assert!(nested_pairs(&[]).is_empty());
}

#[test]
fn nested_pairs_equal_the_pairwise_scan_on_a_hashed_fake_set() {
    // Distinct fakes of every width 5…12 under one seed, as a run's
    // report holds them; the pairwise `contains` scan is the reference.
    let plan = crate::testflow::plan();
    let seed = crate::fake::Seed::from_bytes([181; 32]);
    let mut fakes: Vec<String> = (5..=12usize)
        .flat_map(|width| {
            (0..300u64).map(move |n| format!("{}{:0>w$}", 1 + n % 9, n * 7919, w = width - 1))
        })
        .map(|key| crate::fake::fake_for(&plan, &seed, &key))
        .collect();
    let mut seen = std::collections::HashSet::new();
    fakes.retain(|f| seen.insert(f.clone()));
    let fakes: Vec<&str> = fakes.iter().map(String::as_str).collect();
    let mut expected = Vec::new();
    for (si, shorter) in fakes.iter().enumerate() {
        for (li, longer) in fakes.iter().enumerate() {
            if longer.len() > shorter.len() && longer.contains(shorter) {
                expected.push((si, li));
            }
        }
    }
    assert!(expected.len() > 100, "{} nested pairs", expected.len());
    assert_eq!(nested_pairs(&fakes), expected);
}

mod discovered;
mod reference;

/// One message of the equivalence corpus: its bytes, its CSeq exemption
/// and the all-digit display runs its display names carry.
struct Sample {
    raw: Vec<u8>,
    keep: Option<(Vec<u8>, Vec<u8>)>,
    runs: Vec<Vec<u8>>,
}

/// A corpus the three scans are compared on: the messages and the
/// substitution sets, in the reference's own priority order.
struct Corpus {
    samples: Vec<Sample>,
    digit_pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The unrecognised keys: weak in the selection.
    weak_pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// The plan's CCs: anchors behind a `+`.
    anchors: Vec<Vec<u8>>,
    display_pairs: Vec<(String, String)>,
}

/// The embedded plan's CCs as anchors.
fn plan_anchors() -> Vec<Vec<u8>> {
    crate::testflow::plan().anchors().into_iter().map(String::into_bytes).collect()
}

/// The all-digit runs of a display name (what the display masking hands
/// to the zeroing scan).
fn digit_runs(dn: &str) -> Vec<Vec<u8>> {
    dn.as_bytes()
        .split(|b| !b.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// The display mask the reference derives: letters to `x`/`X`, every
/// digit run through the reference zeroing scan.
fn reference_mask(
    dn: &str,
    digit_pairs: &[(Vec<u8>, Vec<u8>)],
    weak_pairs: &[(Vec<u8>, Vec<u8>)],
    anchors: &[Vec<u8>],
) -> String {
    let mut out = Vec::new();
    let bytes = dn.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.extend(reference::substitute_digits_zeroing(
                &bytes[start..i],
                digit_pairs,
                weak_pairs,
                anchors,
            ));
        } else {
            out.push(match bytes[i] {
                b'a'..=b'z' => b'x',
                b'A'..=b'Z' => b'X',
                other => other,
            });
            i += 1;
        }
    }
    String::from_utf8(out).expect("only ASCII bytes are rewritten")
}

/// Display pairs in the reference's priority order.
fn prioritized_names(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut pairs = pairs;
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    pairs
}

/// A dense synthetic corpus: keys that nest (5-digit keys inside 8-digit
/// ones, keys glued into Call-IDs and CSeq values), display names that are
/// prefixes of one another and digit-only, in both forms, over a
/// deterministic generator. Its weak (unrecognised) keys are truncated
/// `+CC` forms that partially overlap the NSNs they are prefixes of, and
/// ingress runs that contain an NSN whole, in the composed form.
fn synthetic_corpus() -> Corpus {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let nsns: Vec<String> =
        (0..120).map(|n| format!("2{:07}", n * 7_919_311 % 10_000_000)).collect();
    let mut keys: Vec<String> = nsns.clone();
    // Every 5-digit window of the first NSNs is a key too: dense nesting.
    for nsn in &nsns[..40] {
        for at in 0..=nsn.len() - 5 {
            keys.push(nsn[at..at + 5].to_string());
        }
    }
    let zero_led_national_forms = nsns[60..90].iter().map(|n| format!("0{n}"));
    keys.extend(zero_led_national_forms);
    keys.sort();
    keys.dedup();
    let digit_pairs = prioritized(
        keys.iter()
            .enumerate()
            .map(|(i, k)| {
                (k.clone().into_bytes(), format!("0009{i:0>w$}", w = k.len() - 4).into_bytes())
            })
            .collect(),
    );
    let names: Vec<String> =
        ["Foo", "Foo Testy", "Foo Testy <", "AB", "AB <", "cf 99", "0612", "MeetMe"]
            .iter()
            .map(|s| s.to_string())
            .chain(keys.iter().step_by(7).map(|k| format!("cf {k}")))
            .chain(nsns.iter().step_by(5).map(|n| format!("0{n}")))
            .collect();
    let weak_pairs = prioritized(
        nsns[..30]
            .iter()
            .map(|n| {
                (format!("28{}", &n[..7]).into_bytes(), format!("28{}", "000912").into_bytes())
            })
            .chain(nsns[30..60].iter().enumerate().map(|(i, n)| {
                (format!("835237028{n}").into_bytes(), format!("8350000000009{i:05}").into_bytes())
            }))
            .collect(),
    );
    let display_pairs = prioritized_names(
        names
            .iter()
            .map(|dn| (dn.clone(), reference_mask(dn, &digit_pairs, &weak_pairs, &plan_anchors())))
            .collect(),
    );
    let mut samples = Vec::new();
    for i in 0..1500u64 {
        let key = |n: &mut dyn FnMut(u64) -> u64| keys[n(keys.len() as u64) as usize].clone();
        let name = |n: &mut dyn FnMut(u64) -> u64| names[n(names.len() as u64) as usize].clone();
        let (from, to, cid_key, seq_key) =
            (key(&mut next), key(&mut next), key(&mut next), key(&mut next));
        let seq = match next(3) {
            0 => seq_key.clone(),
            1 => format!("8{seq_key}"),
            _ => (i + 1).to_string(),
        };
        let origin_tail = ["", "7", "00"][next(3) as usize];
        let (dn1, dn2) = (name(&mut next), name(&mut next));
        let quoted = |dn: &str, q: u64| if q == 0 { format!("\"{dn}\"") } else { dn.to_string() };
        let ws = ["", " ", "\t", "  "][next(4) as usize];
        let method = ["INVITE", "BYE", "MESSAGE", "UPDATE"][next(4) as usize];
        let start = match next(4) {
            0 => "SIP/2.0 180 Ringing".to_string(),
            1 => "SIP/2.0 486 Busy Here".to_string(),
            _ => format!("{method} sip:+28{to}@h SIP/2.0"),
        };
        let raw = format!(
            "{start}\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{i}\r\n\
                 From: {}{ws}<sip:{from}@10.0.0.1>;tag={from}{i}\r\nTo: {}{ws}<sip:0{to}@h>\r\n\
                 Call-ID: {cid_key}-{i}@{from}\r\nCSeq: {seq} {method}\r\nRAck: 1 {seq} INVITE\r\n\
                 P-Asserted-Identity: \"{dn1}\" <sip:{seq}@x>, {dn2} {dn1}\r\nContact: <sip:{seq_key}@h:{seq_key}>\r\n\
                 Diversion: <sip:+28{}@h>, <sip:+835237028{}@h>, <sip:+28{}@h>\r\n\
                 Content-Length: {}\r\n\r\nv=0\r\no={seq_key}{cid_key} {seq} 1 IN IP4 10.0.0.1\r\n\
                 o=- 9{cid_key} {seq_key}{origin_tail} IN IP4 10.0.0.1\r\n\
                 a=rtpmap:8 PCMA/{}\r\n",
            quoted(&dn1, next(2)),
            quoted(&dn2, next(2)),
            &nsns[next(30) as usize][..7],
            nsns[30 + next(30) as usize],
            nsns[next(30) as usize],
            keys[next(keys.len() as u64) as usize],
            keys[next(keys.len() as u64) as usize],
        );
        samples.push(Sample {
            raw: raw.into_bytes(),
            keep: Some((seq.into_bytes(), method.as_bytes().to_vec())),
            runs: vec![digit_runs(&format!("{dn1}{seq_key}"))[0].clone()],
        });
    }
    // A long digit run of one repeating key, as the CSeq value and not:
    // the exemption is judged once per run, the reference judges it at
    // every position (kept at 4 KB so the reference stays quick).
    let key = keys.iter().find(|k| k.len() == 5).unwrap();
    let run = key.repeat(800);
    let raw = format!("CSeq: {run} INVITE\r\nx: {run}\r\ny: 1{run}\r\n\r\n{run}");
    samples.push(Sample {
        raw: raw.into_bytes(),
        keep: Some((run.clone().into_bytes(), b"INVITE".to_vec())),
        runs: vec![run.into_bytes()],
    });

    Corpus { samples, digit_pairs, weak_pairs, anchors: plan_anchors(), display_pairs }
}

/// The one-pass rewrite equals the naive reference scan byte for byte on
/// `corpus`, which the empty substitution sets leave as it is: the digit
/// pass under each message's protocol-counter exemption, the zeroing pass on
/// every display run, the display pass in both forms. How many messages the
/// exemption changed, the digit pass rewrote and the display pass rewrote.
///
/// The exemption differs in mechanics only: the reference drops every
/// occurrence inside an exempt run before choosing, the one-pass scan
/// chooses first and then drops. They agree because the exemption is a
/// property of the whole digit run around the occurrence and no key
/// spans a non-digit, so every occurrence overlapping an exempt one lies
/// in the same run and is exempt too — the run is emitted verbatim
/// either way. An identifier's or an escape's digits depend on the
/// occurrence's own bounds, so the reference drops them after choosing, as
/// the scan does. Both judge fields by the scan's classifiers.
fn assert_equivalent(name: &str, corpus: &Corpus) -> (usize, usize, usize) {
    assert!(!corpus.digit_pairs.is_empty(), "{name}: mapping");
    let empty = DigitSubs::anchored(Vec::new(), Vec::new(), corpus.anchors.clone());
    for sample in &corpus.samples {
        assert_eq!(empty.apply(&sample.raw, None), sample.raw, "{name}: empty sets");
    }
    let digits = DigitSubs::anchored(
        corpus.digit_pairs.clone(),
        corpus.weak_pairs.clone(),
        corpus.anchors.clone(),
    );
    let displays = DisplaySubs::new(
        corpus
            .display_pairs
            .iter()
            .map(|(f, t)| (f.clone().into_bytes(), t.clone().into_bytes()))
            .collect(),
    );
    let (mut exempt, mut digits_rewritten, mut displays_rewritten) = (0, 0, 0);
    for (n, sample) in corpus.samples.iter().enumerate() {
        let keep = sample.keep.as_ref().map(|(s, m)| (s.as_slice(), m.as_slice()));
        let expected = reference::substitute_digits(
            &sample.raw,
            &corpus.digit_pairs,
            &corpus.weak_pairs,
            &corpus.anchors,
            keep,
        );
        assert_eq!(digits.apply(&sample.raw, keep), expected, "{name} msg {n}: digit pass");
        digits_rewritten += usize::from(expected != sample.raw);
        let unstated = reference::substitute_digits(
            &sample.raw,
            &corpus.digit_pairs,
            &corpus.weak_pairs,
            &corpus.anchors,
            None,
        );
        assert_eq!(digits.apply(&sample.raw, None), unstated, "{name} msg {n}: no CSeq stated");
        let unexempt = reference::substitute_digits_unexempt(
            &sample.raw,
            &corpus.digit_pairs,
            &corpus.weak_pairs,
            &corpus.anchors,
        );
        exempt += usize::from(expected != unexempt);
        let expected = reference::substitute_displays(&sample.raw, &corpus.display_pairs);
        assert_eq!(displays.apply(&sample.raw), expected, "{name} msg {n}: display pass");
        displays_rewritten += usize::from(expected != sample.raw);
        for run in &sample.runs {
            assert_eq!(
                digits.apply_zeroing(run),
                reference::substitute_digits_zeroing(
                    run,
                    &corpus.digit_pairs,
                    &corpus.weak_pairs,
                    &corpus.anchors,
                ),
                "{name} msg {n}: zeroing pass on {}",
                String::from_utf8_lossy(run)
            );
        }
    }
    (exempt, digits_rewritten, displays_rewritten)
}

/// [`assert_equivalent`] on a dense synthetic corpus of requests of several
/// methods and responses, leading-zero national keys among its keys.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn one_pass_rewrite_equals_the_reference_scan_on_a_synthetic_corpus() {
    let (exempt, digits, displays) = assert_equivalent("synthetic", &synthetic_corpus());
    assert!(exempt > 100, "{exempt} messages exercise the exemption");
    assert!(digits > 1000, "{digits} messages map digits");
    assert!(displays > 1000, "{displays} messages map names");
}

/// [`assert_equivalent`] on whole calls, under the pairs discovery makes.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn one_pass_rewrite_equals_the_reference_scan_on_discovered_pairs() {
    let corpus = discovered::corpus();
    let (exempt, digits, displays) = assert_equivalent("discovered", &corpus);
    let pairs = corpus.digit_pairs.iter().chain(&corpus.weak_pairs);
    let zero_led = pairs.filter(|(key, _)| key.starts_with(b"0")).count();
    assert!(zero_led > 100, "{zero_led} keys open on a zero");
    assert!(corpus.weak_pairs.len() > 100, "{} unrecognised keys", corpus.weak_pairs.len());
    assert!(exempt > 0, "{exempt} messages exercise the exemption");
    assert!(digits * 10 > corpus.samples.len() * 9, "{digits} messages map digits");
    assert!(displays > 100, "{displays} messages map names");
}
