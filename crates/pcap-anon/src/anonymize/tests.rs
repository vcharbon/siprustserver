use super::*;
use crate::model::{set_wire, wire, Summary};
use crate::testflow::{bare_leg, document, message};
use std::collections::BTreeMap;

/// [`anonymize_display`] over a UTF-8 name, read back as text.
fn mask(dn: &str, subs: &DigitSubs) -> String {
    String::from_utf8(anonymize_display(dn.as_bytes(), subs)).expect("ASCII-only rewrite")
}

#[test]
fn anonymize_display_masks_letters_and_maps_numbers() {
    // 85245 is a discovered number → its fake; the standalone orphan run
    // 35 → same-length zeros; basic-ASCII letters → x/X.
    let subs = DigitSubs::new(vec![(b"85245".to_vec(), b"00091".to_vec())]);
    assert_eq!(mask("MeetMe 85245 (35)", &subs), "XxxxXx 00091 (00)");
    // Every byte of an accented letter masked, the orphan '7' zeroed.
    assert_eq!(mask("Ünïcode-Café 7", &subs), "xxxxxxxxx-Xxxxx 0");
    // With no discovered numbers, an unclassifiable run is an orphan → zeros.
    assert_eq!(mask("6940w", &DigitSubs::new(vec![])), "0000x");
    // A zero-led run maps its embedded number; a run shaped like a fake is a
    // run like any other, never kept for its shape.
    assert_eq!(mask("00085245", &subs), "00000091");
    assert_eq!(mask("0009123", &subs), "0000000");
}

/// [`anonymize_display`] with no number mapping: `dn` masks to `expected`
/// and keeps its byte length.
fn assert_masked(dn: &[u8], expected: &[u8]) {
    let once = anonymize_display(dn, &DigitSubs::new(vec![]));
    assert_eq!(String::from_utf8_lossy(&once), String::from_utf8_lossy(expected));
    assert_eq!(once.len(), dn.len(), "the byte length is kept");
}

#[test]
fn a_display_name_in_a_non_latin_script_keeps_no_letter() {
    assert_masked("Фейк Тестов".as_bytes(), b"xxxxxxxx xxxxxxxxxxxx");
    assert_masked("Ψεύτο".as_bytes(), b"xxxxxxxxxx");
    assert_masked("假名字".as_bytes(), b"xxxxxxxxx");
    assert_masked("مزيف 42".as_bytes(), b"xxxxxxxx 00");
}

#[test]
fn a_mixed_accented_display_name_keeps_no_letter() {
    assert_masked("Fakë Fäkero-Ønreal".as_bytes(), b"Xxxxx Xxxxxxx-xxxxxxx");
}

#[test]
fn an_emoji_display_name_keeps_no_symbol() {
    assert_masked("Foo 🌹 ✈".as_bytes(), b"Xxx xxxx xxx");
}

/// A supplementary character written as two UTF-8-encoded surrogates
/// (CESU-8) and a lone Latin-1 byte are not UTF-8; every byte is masked.
#[test]
fn a_display_name_in_non_utf8_bytes_keeps_no_letter() {
    let mut cesu = b"Fake ".to_vec();
    cesu.extend_from_slice(&[0xED, 0xA0, 0xBC, 0xED, 0xBC, 0xB9]);
    assert_masked(&cesu, b"Xxxx xxxxxx");
    assert_masked(b"Fak\xE9", b"Xxxx");
}

#[test]
fn a_junk_plus_nsn_display_run_lands_in_the_reserved_space() {
    // A display run of junk digits FOLLOWED by a discovered NSN: the NSN maps
    // and the junk is zeroed, so the whole run lands in the 000-space.
    let subs = DigitSubs::new(vec![(b"22345678".to_vec(), b"000900001".to_vec())]);
    assert_eq!(mask("cf 9922345678", &subs), "xx 00000900001");
}

#[test]
fn group_in_order_keeps_first_occurrence_order() {
    let groups = group_in_order([("b", 1), ("a", 2), ("b", 3), ("c", 4), ("a", 5)]);
    assert_eq!(groups, vec![("b", vec![1, 3]), ("a", vec![2, 5]), ("c", vec![4])]);
}

fn entry(original: &str, fake: &str) -> MappingEntry {
    MappingEntry { class: "private".into(), original: original.into(), fake: fake.into() }
}

/// A flows document whose datagrams carry `keys` as private user-parts,
/// one INVITE per key, in the given order (only `raw` matters to
/// discovery).
fn flows_with_users(keys: &[String]) -> Flows {
    let msgs = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let raw = format!(
                "INVITE sip:{k}@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{i}\r\n\
                     From: <sip:{k}@10.0.0.1>;tag=t{i}\r\nTo: <sip:{k}@h>\r\nCall-ID: c\r\n\
                     CSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContact: <sip:10.0.0.1:5060>\r\n\
                     Content-Length: 0\r\n\r\n"
            );
            let summary = Summary::Request {
                method: "INVITE".into(),
                uri: format!("sip:{k}@h"),
                cseq: crate::model::CSeq { seq: 1, method: "INVITE".into() },
                from: crate::model::Party { uri: format!("sip:{k}@10.0.0.1"), tag: None },
                to: crate::model::Party { uri: format!("sip:{k}@h"), tag: None },
            };
            message(i as u64, "10.0.0.1:5060", "10.0.0.2:5060", raw.as_bytes(), summary)
        })
        .collect();
    document(vec![bare_leg("c", msgs)])
}

/// A peer may echo a display name with each supplementary character as two
/// UTF-8-encoded surrogates (CESU-8), which is not UTF-8: the datagram
/// rides the opaque arm, and its display name is still masked like the
/// same name in the UTF-8 datagram beside it.
#[test]
fn a_display_name_in_a_non_utf8_datagram_is_masked() {
    let user = "062345678";
    let party = |tag: Option<&str>| crate::model::Party {
        uri: format!("sip:{user}@10.0.0.1"),
        tag: tag.map(str::to_string),
    };
    let cseq = || crate::model::CSeq { seq: 1, method: "INVITE".into() };
    let msg = |ts_us: u64, raw: Vec<u8>, summary: Summary| {
        message(ts_us, "10.0.0.1:5060", "10.0.0.2:5060", &raw, summary)
    };
    let invite = format!(
        "INVITE sip:{user}@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK0\r\n\
             From: \"Fakey McFakename 🌹\" <sip:{user}@10.0.0.1>;tag=t0\r\nTo: <sip:{user}@h>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    let mut echoed = b"SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK0\r\n\
            From: \"Fakey McFakename "
        .to_vec();
    // U+1F339 as CESU-8: the high and the low surrogate, each UTF-8-encoded.
    echoed.extend_from_slice(&[0xED, 0xA0, 0xBC, 0xED, 0xBC, 0xB9]);
    echoed.extend_from_slice(
        format!(
            "\" <sip:{user}@10.0.0.1>;tag=t0\r\nTo: <sip:{user}@h>\r\nCall-ID: c\r\n\
                 CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(std::str::from_utf8(&echoed).is_err(), "the fixture is not UTF-8");
    let mut flows = flows_with_users(&[user.to_string()]);
    flows.legs[0].msgs = vec![
        msg(
            0,
            invite.into_bytes(),
            Summary::Request {
                method: "INVITE".into(),
                uri: format!("sip:{user}@h"),
                cseq: cseq(),
                from: party(Some("t0")),
                to: party(None),
            },
        ),
        msg(
            1,
            echoed,
            Summary::Response {
                status: 100,
                reason: "Trying".into(),
                cseq: cseq(),
                from: party(Some("t0")),
                to: party(None),
            },
        ),
    ];
    anonymize(
        &mut flows,
        &crate::testflow::plan(),
        &Seed::from_hex(crate::fake::GOLDEN_SEED_HEX).unwrap(),
    )
    .unwrap();
    for msg in &flows.legs[0].msgs {
        let raw = wire(msg);
        let text = String::from_utf8_lossy(&raw);
        assert!(
            !text.contains("Fakey") && !text.contains("McFakename"),
            "a display name survives: {text}"
        );
    }
}

/// Every display name of every identity header, in a UTF-8 datagram and in
/// a CESU-8 one, a quoted name with escaped quotes included, keeps no
/// letter but the masks: no non-ASCII byte survives, every datagram keeps
/// its byte length, and the anonymised document is refused a second pass.
#[test]
fn every_display_name_keeps_no_letter_in_any_script() {
    let user = "062345678";
    let invite = format!(
        "INVITE sip:{user}@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK0\r\n\
             From: \"Фейк Тестов\" <sip:{user}@10.0.0.1>;tag=t0\r\n\
             To: \"假名字\" <sip:{user}@h>\r\n\
             P-Asserted-Identity: \"Fakë Fäkero\" <sip:{user}@10.0.0.1>\r\n\
             Contact: \"Foo 🌹\" <sip:{user}@10.0.0.1:5060>\r\n\
             Referred-By: \"Fakey \\\"FF\\\" McFakename\" <sip:{user}@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    let mut echoed = b"SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK0\r\n\
            From: \"Fake "
        .to_vec();
    echoed.extend_from_slice(&[0xED, 0xA0, 0xBC, 0xED, 0xBC, 0xB9]);
    echoed.extend_from_slice(
        format!(
            "\" <sip:{user}@10.0.0.1>;tag=t0\r\nTo: <sip:{user}@h>\r\nCall-ID: c\r\n\
                 CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    let mut flows = flows_with_users(&[user.to_string(), user.to_string()]);
    set_wire(&mut flows.legs[0].msgs[0], invite.as_bytes());
    set_wire(&mut flows.legs[0].msgs[1], &echoed);
    flows.legs[0].msgs[1].summary = Summary::Response {
        status: 100,
        reason: "Trying".into(),
        cseq: crate::model::CSeq { seq: 1, method: "INVITE".into() },
        from: crate::model::Party { uri: format!("sip:{user}@10.0.0.1"), tag: None },
        to: crate::model::Party { uri: format!("sip:{user}@h"), tag: None },
    };
    let lengths: Vec<usize> = flows.legs[0].msgs.iter().map(|m| wire(m).len()).collect();
    let (plan, seed) =
        (crate::testflow::plan(), Seed::from_hex(crate::fake::GOLDEN_SEED_HEX).unwrap());
    anonymize(&mut flows, &plan, &seed).unwrap();
    for (msg, len) in flows.legs[0].msgs.iter().zip(lengths) {
        let raw = wire(msg);
        let text = String::from_utf8_lossy(&raw);
        assert_eq!(raw.len(), len, "the datagram keeps its byte length: {text}");
        assert!(raw.is_ascii(), "a non-ASCII byte survives: {text}");
        for name in ["Fakey", "FF", "McFakename", "Foo", "Fake", "Fak", "kero"] {
            assert!(!text.contains(name), "{name} survives: {text}");
        }
    }
    assert!(matches!(
        anonymize(&mut flows, &plan, &seed),
        Err(AnonymizeError::AlreadyRewritten(_))
    ));
}

/// One INVITE whose datagram is `raw` (only `raw` matters to discovery).
fn single_invite(raw: &str) -> Flows {
    let mut flows = flows_with_users(&["5678".to_string()]);
    set_wire(&mut flows.legs[0].msgs[0], raw.as_bytes());
    flows
}

/// Anonymize `flows` under the golden seed and hold the run to its contract:
/// no number of the raw document survives, no display name keeps a letter,
/// the same raw document under the same seed comes out byte for byte the
/// same, and the output is refused a second pass. Returns the datagrams.
fn assert_anonymised_whole(raw: &str) -> Vec<String> {
    let (plan, seed) =
        (crate::testflow::plan(), Seed::from_hex(crate::fake::GOLDEN_SEED_HEX).unwrap());
    let mut flows = single_invite(raw);
    anonymize(&mut flows, &plan, &seed).unwrap();
    let mut again = single_invite(raw);
    anonymize(&mut again, &plan, &seed).unwrap();
    let text = |f: &Flows| -> Vec<String> {
        f.legs[0].msgs.iter().map(|m| String::from_utf8_lossy(&wire(m)).into_owned()).collect()
    };
    assert_eq!(text(&again), text(&flows), "the kept seed reproduces the output");
    assert_eq!(
        crate::verify::survivors(&single_invite(raw), &flows, &plan, &Scope::default()),
        0,
        "{:?}",
        text(&flows)
    );
    assert_eq!(crate::verify::unmasked_displays(&flows, &Scope::default()), 0);
    assert!(matches!(
        anonymize(&mut flows, &plan, &seed),
        Err(AnonymizeError::AlreadyRewritten(_))
    ));
    text(&flows)
}

/// A user-part shaped like a fake (`+CC` then the fake space) is a number of
/// the raw document like any other: it maps, and a shorter key spelt inside
/// it never splits it.
#[test]
fn a_run_shaped_like_a_fake_is_anonymised_like_any_number() {
    let once = assert_anonymised_whole(
        "INVITE sip:+280009123456781234@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:2800@10.0.0.1>;tag=t1\r\nTo: <sip:+8350009991@h>\r\n\
             Contact: <sip:8350@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(!once[0].contains("0009123456781234"), "{}", once[0]);
    assert!(!once[0].contains("8350009991"), "{}", once[0]);
    assert!(!once[0].contains("sip:2800@") && !once[0].contains("sip:8350@"), "{}", once[0]);
}

/// A display digit run equal to its URI user-part, an unrecognised key
/// composed around a recognised number, shows the user-part's fake, and a
/// second pass leaves both alone.
#[test]
fn a_display_run_equal_to_its_user_part_is_a_fixed_point() {
    let once = assert_anonymised_whole(
        "INVITE sip:+2822345678@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:5678@10.0.0.1>;tag=t1\r\nTo: <sip:+2822345678@h>\r\n\
             P-Asserted-Identity: \"00835622345678\" <sip:00835622345678@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    let pai = once[0].lines().find(|l| l.starts_with("P-Asserted-Identity")).unwrap();
    let (display, user) = (&pai[22..36], &pai[43..57]);
    assert_eq!(display, user, "{pai}");
    assert!(!pai.contains("00835622345678"), "{pai}");
}

/// A display name masked to its user-part's fake is not split afterwards
/// by a shorter key its fake digits spell: the display keeps the
/// user-part's fake and a second pass leaves it alone.
#[test]
fn a_masked_display_is_never_split_by_a_shorter_key() {
    let once = assert_anonymised_whole(
        "INVITE sip:+2822345678@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: \"+8312345678909\" <sip:+8312345678909@10.0.0.1>;tag=t1\r\n\
             To: <sip:+2822345678@h>\r\nContact: <sip:1000@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    let from = once[0].lines().find(|l| l.starts_with("From")).unwrap();
    let (display, user) = (&from[7..21], &from[28..42]);
    assert_eq!(display, user, "{from}");
    assert!(!from.contains("12345678909"), "{from}");
}

/// A composed user-part (bare CC + short code) maps whole to its form
/// with the key's fake: a shorter key spelt across its CC and its key
/// never splits it, and a second pass reads the short code's fake.
#[test]
fn a_composed_user_part_is_never_split_by_a_key_across_its_parts() {
    let once = assert_anonymised_whole(
        "INVITE sip:+2822345678@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:2812345@10.0.0.1>;tag=t1\r\nTo: <sip:+2822345678@h>\r\n\
             Contact: <sip:8123@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    let from = once[0].lines().find(|l| l.starts_with("From")).unwrap();
    assert!(from.starts_with("From: <sip:280009"), "{from}");
}

/// An unrecognised run composed around two recognised keys maps to their
/// fakes behind zeros, a run in the reserved space that also spells a
/// national number: a second pass reads it as the fake it is.
#[test]
fn a_reserved_run_spelling_a_national_number_is_no_key() {
    let once = assert_anonymised_whole(
        "INVITE sip:56781234560000000000@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:5678@10.0.0.1>;tag=t1\r\nTo: <sip:56781234560000000000@h>\r\n\
             Contact: <sip:23456@10.0.0.1>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(once[0].starts_with("INVITE sip:09"), "{}", once[0]);
}

/// A PASSporT is redacted whole — header, claims and signature — while the
/// numbers it claims map wherever else they stand.
#[test]
fn a_passport_is_redacted_whole_and_its_claimed_numbers_map_elsewhere() {
    let payload =
        crate::passport::encode(br#"{"orig":{"tn":"2862345678"},"dest":{"tn":["2822345678"]}}"#, 0);
    let once = assert_anonymised_whole(&format!(
        "INVITE sip:+2822345678@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:+2862345678@10.0.0.1>;tag=t1\r\nTo: <sip:+2822345678@h>\r\n\
             Identity: eyJhbGciOiJFUzI1NiJ9.{payload}.c2ln;info=<https://cert.example/a.pem>\r\n\
             Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    ));
    let identity = once[0].lines().find(|l| l.starts_with("Identity: ")).unwrap();
    let token = identity["Identity: ".len()..].split(';').next().unwrap();
    assert!(token.bytes().all(|b| b == b'_' || b == b'.'), "{identity}");
    assert!(!once[0].contains("62345678") && !once[0].contains("22345678"), "{}", once[0]);
}

/// ≥ 500 keys per width 5…12, discovered in two orders under one seed, map
/// identically; another seed maps them differently. Widths 5…12 are all private
/// under a plan that admits them.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn keyed_mapping_is_discovery_order_independent_and_seed_dependent() {
    let plan = Plan::from_json_str(
        r#"{ "default_cc": "28", "countries": [ { "cc": "28", "nsn_len": 20 } ],
                 "private_len": { "min": 5, "max": 12 }, "max_composed_prefix": 0,
                 "fake_prefix": "000" }"#,
    )
    .unwrap();
    // Distinct keys, never 0-leading, 500 per width.
    let mut keys: Vec<String> = Vec::new();
    for width in 5..=12usize {
        for n in 0..500u64 {
            keys.push(format!(
                "{}{:0>w$}",
                1 + n % 9,
                n * 7919 % 10u64.pow((width - 1) as u32),
                w = width - 1
            ));
        }
    }
    keys.sort();
    keys.dedup();
    assert!(keys.len() >= 8 * 500 - 8 * 20, "the key set must stay dense: {}", keys.len());
    let mut reversed = keys.clone();
    reversed.reverse();

    let seed = Seed::from_bytes([42; 32]);
    let mapping = |order: &[String], seed: &Seed| -> BTreeMap<String, String> {
        discover(&flows_with_users(order), &plan, seed, &crate::identities::BodyTypes::default())
            .entries
            .into_iter()
            .map(|e| (e.original, e.fake))
            .collect()
    };
    let forward = mapping(&keys, &seed);
    assert_eq!(forward.len(), keys.len(), "every key is discovered once");
    assert_eq!(forward, mapping(&reversed, &seed), "discovery order must not matter");
    for (key, fake) in &forward {
        assert_eq!(fake.len(), key.len());
        assert!(fake.starts_with("0009"), "{fake} is outside the reserved space");
    }
    let other = mapping(&keys, &Seed::from_bytes([43; 32]));
    let moved = forward.iter().filter(|(k, f)| other[*k] != **f).count();
    assert!(moved > forward.len() / 2, "another seed must remap most keys, moved {moved}");
}

#[test]
fn shared_fakes_are_counted_per_key_and_nested_fakes_per_pair() {
    let entries = vec![
        entry("41001", "00091"),
        entry("41002", "00091"),
        entry("41003", "00092"),
        entry("22345678", "000913456"),
        entry("62345678", "000923456"),
        entry("62345679", "000923456"),
    ];
    let report = Report {
        collisions: count_collisions(&entries),
        entries,
        merged_call_ids: 0,
        uui_skipped: 0,
        uui_redacted: 0,
        names: Vec::new(),
        name_collisions: 0,
        fakes: Fakes::default(),
        redacted_parts: 0,
    };
    assert_eq!(
        report.collisions,
        vec![
            Collision::Shared {
                fake: "00091".into(),
                originals: vec!["41001".into(), "41002".into()]
            },
            Collision::Shared {
                fake: "000923456".into(),
                originals: vec!["62345678".into(), "62345679".into()]
            },
            Collision::Nested { shorter: "00091".into(), longer: "000913456".into() },
            Collision::Nested { shorter: "00092".into(), longer: "000923456".into() },
        ]
    );
    assert_eq!(report.shared_keys(), 4);
    assert_eq!(report.nested_fakes(), 2);
}

#[test]
fn a_colliding_capture_is_rewritten_consistently_not_refused() {
    // Widths 5 and 9 under a seed: with more 5-digit keys than the width
    // has hash values, some share a fake and the run still succeeds, every
    // key rewritten to exactly its own fake.
    let plan = crate::testflow::plan();
    let seed = Seed::from_bytes([7; 32]);
    let keys: Vec<String> = (0..30).map(|n| format!("4{n:04}")).collect();
    let mut flows = flows_with_users(&keys);
    let report = anonymize(&mut flows, &plan, &seed).unwrap();
    assert_eq!(report.entries.len(), 30);
    assert!(report.shared_keys() > 0, "30 five-digit keys over 10 hash values must share");
    for (key, msg) in keys.iter().zip(&flows.legs[0].msgs) {
        let fake = &report.entries.iter().find(|e| e.original == *key).unwrap().fake;
        let raw = String::from_utf8(wire(msg)).unwrap();
        assert!(raw.contains(&format!("sip:{fake}@h")), "{key}: {raw}");
        assert!(!raw.contains(key.as_str()), "{key} leaked: {raw}");
    }
}

/// A failed header reading names the raw Call-ID and quotes the raw value:
/// the anonymised document keeps how many readings failed, never which.
#[test]
fn the_failed_header_readings_leave_only_their_count() {
    let key = "062345678".to_string();
    let mut flows = flows_with_users(std::slice::from_ref(&key));
    flows.flow_stats.header_failed = 1;
    flows.flow_stats.header_failures.push(sip_pcap::doc::HeaderFailureJson {
        call_id: "c".into(),
        msg: 0,
        header: "P-Asserted-Identity".into(),
        reason: format!("unterminated quoted value in <sip:{key}@h>"),
    });
    let reading = crate::testflow::reading();
    anonymize(&mut flows, &crate::testflow::plan(), &Seed::from_bytes([3; 32])).unwrap();
    let emitted = crate::model::to_json_string(&flows, &reading).unwrap();
    assert!(!emitted.contains(&key), "{emitted}");
    let stats = serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["flow_stats"].clone();
    assert_eq!(stats["header_failed"], 1, "{stats}");
    assert!(stats.get("header_failures").is_none(), "{stats}");
}

#[test]
fn a_capture_whose_cseq_equals_a_private_key_passes_the_numeric_guard() {
    let plan = crate::testflow::plan();
    let seed = Seed::from_bytes([3; 32]);
    let keys = vec!["41001".to_string()];
    let mut flows = flows_with_users(&keys);
    let raw = String::from_utf8(wire(&flows.legs[0].msgs[0])).unwrap();
    set_wire(
        &mut flows.legs[0].msgs[0],
        raw.replace("CSeq: 1 INVITE", "CSeq: 41001 INVITE").as_bytes(),
    );
    if let Summary::Request { cseq, .. } = &mut flows.legs[0].msgs[0].summary {
        cseq.seq = 41001;
    }
    let report = anonymize(&mut flows, &plan, &seed).expect("the CSeq coincidence is exempt");
    assert_eq!(report.entries.len(), 1);
    let raw = String::from_utf8(wire(&flows.legs[0].msgs[0])).unwrap();
    assert!(raw.contains("CSeq: 41001 INVITE"), "{raw}");
    assert!(raw.contains(&format!("sip:{}@h", report.entries[0].fake)), "{raw}");
    assert!(!raw.contains("sip:41001@"), "{raw}");
}

/// Through the whole pass: a short code inside a timestamp origin stays, a
/// session id that is a national number maps.
#[test]
fn a_short_code_inside_an_origin_stays_and_a_national_session_id_maps() {
    let plan = crate::testflow::plan();
    let seed = Seed::from_bytes([9; 32]);
    let keys = vec!["+2839491".to_string(), "+2862345678".to_string()];
    let mut flows = flows_with_users(&keys);
    let origins = ["o=- 17897195439490 17897195439491", "o=- 2862345678 1"];
    for (msg, origin) in flows.legs[0].msgs.iter_mut().zip(origins) {
        let sdp = format!("v=0\r\n{origin} IN IP4 h\r\n");
        let head = String::from_utf8(wire(msg)).unwrap().replace(
            "Content-Length: 0\r\n",
            &format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", sdp.len()),
        );
        set_wire(msg, format!("{head}{sdp}").as_bytes());
    }
    anonymize(&mut flows, &plan, &seed).unwrap();
    let raw = |i: usize| String::from_utf8(wire(&flows.legs[0].msgs[i])).unwrap();
    assert!(!raw(0).contains("sip:+2839491@"), "{}", raw(0));
    assert!(raw(0).contains("o=- 17897195439490 17897195439491 IN"), "{}", raw(0));
    assert!(!raw(1).contains("62345678") && raw(1).contains(" 1 IN"), "{}", raw(1));
}

#[test]
fn call_ids_spelling_keys_that_share_a_fake_never_merge() {
    // Forty legs whose Call-IDs are forty private keys over ten fakes: the
    // number pass merges the keys, the identifier pass keeps the Call-IDs apart.
    let plan = crate::testflow::plan();
    let seed = Seed::from_bytes([5; 32]);
    let keys: Vec<String> = (0..40).map(|n| format!("5{n:04}")).collect();
    let mut flows = flows_with_users(&keys);
    // One leg per message, Call-ID = the key.
    let msgs = std::mem::take(&mut flows.legs[0].msgs);
    flows.legs = msgs
        .into_iter()
        .zip(&keys)
        .map(|(mut msg, key)| {
            let raw = String::from_utf8(wire(&msg)).unwrap();
            set_wire(
                &mut msg,
                raw.replace("Call-ID: c\r\n", &format!("Call-ID: {key}\r\n")).as_bytes(),
            );
            bare_leg(key, vec![msg])
        })
        .collect();
    let report = anonymize(&mut flows, &plan, &seed).unwrap();
    assert!(report.shared_keys() > 0, "40 five-digit keys over 10 hash values must share");
    assert_eq!(report.merged_call_ids, 0);
    let distinct: HashSet<&str> = flows.legs.iter().map(|l| l.call_id.as_str()).collect();
    assert_eq!(distinct.len(), 40);
    assert!(keys.iter().all(|k| !distinct.contains(k.as_str())), "every Call-ID is rewritten");
}

/// A production-shaped document: `messages` INVITEs over `numbers`
/// distinct identities — NSNs in their `+CC` and `0` forms, private
/// extensions, quoted and unquoted display names carrying digits, keys
/// glued into Call-IDs and SDP — every message a leg of its own.
fn synthetic_capture(messages: usize, numbers: usize) -> Flows {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    let numbers: Vec<String> = (0..numbers)
        .map(|n| match n % 8 {
            0 => format!("4{:04}", n % 10_000),
            1 => format!("5{:05}", n % 100_000),
            2 => format!("6{:06}", n % 1_000_000),
            _ => format!("{}{:07}", 1 + n % 9, (n as u64 * 7_919_311) % 10_000_000),
        })
        .collect();
    let uri = |n: &str, host: &str| {
        if n.len() == 8 {
            format!("sip:+28{n}@{host}")
        } else {
            format!("sip:{n}@{host}")
        }
    };
    let mut flows = flows_with_users(&[]);
    flows.legs.clear();
    for i in 0..messages {
        let (from, to, third) = (
            numbers[next(numbers.len())].clone(),
            numbers[next(numbers.len())].clone(),
            numbers[next(numbers.len())].clone(),
        );
        let from_uri = uri(&from, "10.0.0.1");
        let to_uri = uri(&to, "carrier.example");
        let display = match next(3) {
            0 => format!("\"{from}\" "),
            1 => format!("Desk {third} "),
            _ => String::new(),
        };
        let call_id = format!("{third}-{i}@10.0.0.1");
        let raw = format!(
            "INVITE {to_uri} SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{i}\r\n\
                 Max-Forwards: 70\r\nFrom: {display}<{from_uri}>;tag=t{i}\r\nTo: <{to_uri}>\r\n\
                 P-Asserted-Identity: \"{third}\" <{}>\r\nCall-ID: {call_id}\r\n\
                 CSeq: {} INVITE\r\nContact: <sip:{from}@10.0.0.1:5060>\r\n\
                 Content-Type: application/sdp\r\nContent-Length: 0\r\n\r\n\
                 v=0\r\no={from} {i} {i} IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 4{:04} RTP/AVP 8\r\n",
            uri(&third, "10.0.0.2"),
            1 + i % 1000,
            i % 10_000
        );
        let summary = Summary::Request {
            method: "INVITE".into(),
            uri: to_uri.clone(),
            cseq: crate::model::CSeq { seq: (1 + i % 1000) as u32, method: "INVITE".into() },
            from: crate::model::Party { uri: from_uri, tag: Some(format!("t{i}")) },
            to: crate::model::Party { uri: to_uri, tag: None },
        };
        let msg = message(i as u64, "10.0.0.1:5060", "10.0.0.2:5060", raw.as_bytes(), summary);
        flows.legs.push(bare_leg(&call_id, vec![msg]));
    }
    flows
}

/// A capture-sized document — 50 k messages over 5 k distinct numbers —
/// anonymised under one seed: the report, the document and the wall time.
fn anonymize_capture_sized() -> (Report, Flows, std::time::Duration) {
    anonymize_synthetic(50_000)
}

/// [`anonymize_capture_sized`] over `messages` messages.
fn anonymize_synthetic(messages: usize) -> (Report, Flows, std::time::Duration) {
    let plan = crate::testflow::plan();
    let seed = Seed::from_bytes([181; 32]);
    let mut flows = synthetic_capture(messages, 5_000);
    let bytes: usize = flows.legs.iter().flat_map(|l| &l.msgs).map(|m| wire(m).len()).sum();
    let started = std::time::Instant::now();
    let report = anonymize(&mut flows, &plan, &seed).unwrap();
    let elapsed = started.elapsed();
    eprintln!("anonymised {bytes} bytes, {} identities in {elapsed:?}", report.entries.len());
    (report, flows, elapsed)
}

/// A capture-sized document anonymises completely — every identity discovered,
/// no Call-ID keeping the number it was spelt from, every SDP origin's
/// username in the fake space, no original digits in the output. The wall time is printed, not asserted: the test lane shares its
/// box.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn a_capture_sized_document_anonymises_completely() {
    let (report, flows, _) = anonymize_capture_sized();
    assert!(report.entries.len() >= 4_900, "{} identities discovered", report.entries.len());
    // The identifier pass owns the Call-IDs: no Call-ID keeps the number it
    // was spelt from, and the wire and the model agree.
    let originals: HashSet<&str> = report.entries.iter().map(|e| e.original.as_str()).collect();
    for leg in &flows.legs {
        let raw = String::from_utf8(wire(&leg.msgs[0])).unwrap();
        let head = leg.call_id.split(['-', '@']).next().unwrap_or_default();
        assert!(!originals.contains(head), "{}", leg.call_id);
        assert!(raw.contains(&format!("Call-ID: {}", leg.call_id)), "{raw}");
        assert!(raw.contains("\r\no=0009"), "{raw}");
    }
    // No number of the raw document stays where it stood, protocol counters
    // (a port, an origin's version) aside.
    let raw = synthetic_capture(50_000, 5_000);
    assert_eq!(
        crate::verify::survivors(&raw, &flows, &crate::testflow::plan(), &Scope::default()),
        0
    );
}

/// The time grows linearly with the document: four times the messages take
/// about four times as long, where the quadratic scan this guards against
/// took sixteen. The ratio holds on a shared, loaded box where a wall-time
/// bound does not (the capture-sized run measured 45–49 s in a debug build
/// and 4.7–5.6 s in release, at a load average of 13–15).
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn a_capture_sized_document_anonymises_in_linear_time() {
    let (_, _, quarter) = anonymize_synthetic(12_500);
    let (_, _, whole) = anonymize_capture_sized();
    let ratio = whole.as_secs_f64() / quarter.as_secs_f64();
    eprintln!("12.5 k messages in {quarter:?}, 50 k in {whole:?}: x{ratio:.1}");
    assert!(ratio < 8.0, "four times the messages took x{ratio:.1}");
}
