//! The probes of the headers whose values are protocol vocabulary — feature
//! tags, Info Package and service names, media types — and of the identities
//! written among that vocabulary: a host or a name in a quoted or bare
//! feature value, a media type parameter, a service list, a Reason's second
//! protocol slot, a vendor header carrying a feature tag, and the parameter
//! names of a dialog identifier header. Every value is invented. The
//! identities go, the vocabulary keeps its spelling, the datagram keeps its
//! length and parses, and the leak checks count nothing; untouched, they
//! count the identities. Peer addresses are public-looking (85.12.34.x) on
//! purpose: the scope maps only public addresses.

use super::refusal_probe_tests::{document, leaks, wire};
use crate::anonymize::{anonymize_in, parse_ok_map, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;

/// One probe: its extra header lines, the identities that must go, and the
/// spellings that must stay.
struct Probe {
    lines: &'static [&'static str],
    gone: &'static [&'static str],
    kept: &'static [&'static str],
}

const PROBES: &[Probe] = &[
    Probe {
        lines: &[
            "Accept-Contact: *;+sip.description=\"Desk Fakey McFakename hq.fakecorp-test.de\";explicit",
            "Feature-Caps: *;+g.3gpp.atcf-path=\"atcf1.notreal-ims.de\";+g.3gpp.srvcc",
        ],
        gone: &["McFakename", "fakecorp-test", "notreal-ims"],
        kept: &["+sip.description=\"", "+g.3gpp.atcf-path=\"", "+g.3gpp.srvcc", ";explicit"],
    },
    Probe {
        lines: &[
            "P-Asserted-Service: urn:urn-7:3gpp-service.ims.icsi.mmtel, svc.notreal-op.de",
            "Recv-Info: g.3gpp.zz-vndr-pkg, nodex.notreal-op.de",
        ],
        gone: &["notreal-op"],
        kept: &["urn:urn-7:3gpp-service.ims.icsi.mmtel", "g.3gpp.zz-vndr-pkg"],
    },
    Probe {
        lines: &[
            "Recv-Info: vnd.vendorx.order-pkg",
            "X-Body-Kind: application/vnd.vendorx.order",
            "Accept: application/sdp, application/vnd.vendorx.order",
        ],
        gone: &[],
        kept: &[
            "Recv-Info: vnd.vendorx.order-pkg",
            "X-Body-Kind: application/vnd.vendorx.order",
            "application/vnd.vendorx.order\r\n",
        ],
    },
    Probe {
        lines: &["Reason: Q.850;cause=16, sbc3.fakecarrier-net.de;cause=1"],
        gone: &["fakecarrier-net"],
        kept: &["Reason: Q.850;cause=16, "],
    },
    Probe {
        lines: &[
            "X-Dev: <sip:x@8.8.8.11>;+sip.instance=\"<urn:uuid:0f6c2a1e-1b2c-4d5e-8f90-a1b2c3d4e5f6>\"",
            "Accept-Contact: *;+sip.instance=\"<urn:uuid:0f6c2a1e-1b2c-4d5e-8f90-a1b2c3d4e5f6>\";require",
        ],
        gone: &["0f6c2a1e-1b2c"],
        kept: &["+sip.instance=\"<urn:uuid:", ";require"],
    },
    Probe {
        lines: &["Session-ID: ab12cd34ef56ab12cd34ef56ab12cd34;remote=9f8e7d6c5b4a39281706f5e4d3c2b1a0"],
        gone: &["ab12cd34ef56", "9f8e7d6c5b4a"],
        kept: &[";remote="],
    },
];

/// A document whose last header line carries a media type parameter.
fn with_media_type() -> crate::model::Flows {
    let mut raw = document(&[], "c1@cidhost.callerdom.zz", "v=0\r\n");
    let text = wire(&raw).replace(
        "Content-Type: application/sdp",
        "Content-Type: application/sdp;origin=media7.fakecorp-gw.de",
    );
    let width = text.len() - wire(&raw).len();
    crate::model::set_wire(&mut raw.legs[0].msgs[0], text.as_bytes());
    assert_eq!(width, ";origin=media7.fakecorp-gw.de".len());
    raw
}

#[test]
fn every_identity_among_vocabulary_is_gone_and_the_vocabulary_stays() {
    let docs = PROBES
        .iter()
        .map(|p| (document(p.lines, "c1@cidhost.callerdom.zz", ""), p.gone, p.kept))
        .chain([(with_media_type(), &["fakecorp-gw"][..], &["application/sdp;origin="][..])]);
    for (i, (raw, gone, kept)) in docs.enumerate() {
        let mut out = raw.clone();
        let parsed = parse_ok_map(&out, &crate::testflow::reading());
        let report = anonymize_in(
            &mut out,
            &crate::testflow::plan(),
            &Seed::from_bytes([9; 32]),
            &Scope::default(),
        )
        .expect("anonymises");
        let text = wire(&out);
        for id in gone {
            assert!(
                !text.to_lowercase().contains(&id.to_lowercase()),
                "probe {i}: {id} survives:\n{text}"
            );
        }
        for spelling in kept {
            assert!(text.contains(spelling), "probe {i}: {spelling} lost:\n{text}");
        }
        assert_eq!(wire(&raw).len(), text.len(), "probe {i}: length");
        assert_eq!(
            parse_ok_map(&out, &crate::testflow::reading()),
            parsed,
            "probe {i}: still parses"
        );
        assert_eq!(leaks(&raw, &out, &report.fakes), 0, "probe {i}: verify counts a leak:\n{text}");
    }
}

/// A letterless device id (an IMEI) is faked whole, its separators kept: no
/// group of its digits — the type allocation head included — stays, where a
/// reading as a number with separators would keep a country-code-looking head.
#[test]
fn a_letterless_device_id_is_faked_whole() {
    let imei = "35000099-524190-1";
    let raw = document(
        &[
            &format!("Contact: <sip:4321@8.8.8.20:5060>;+sip.instance=\"<urn:gsma:imei:{imei}>\"")
                [..],
        ],
        "c1@cidhost.callerdom.zz",
        "",
    );
    let mut out = raw.clone();
    let report = anonymize_in(
        &mut out,
        &crate::testflow::plan(),
        &Seed::from_bytes([9; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    let text = wire(&out);
    let at = text.find("urn:gsma:imei:").expect("the instance id stays a URN") + 14;
    let faked = &text[at..at + imei.len()];
    assert_eq!(faked.match_indices('-').map(|(i, _)| i).collect::<Vec<_>>(), [8, 15], "{faked}");
    for group in imei.split('-').filter(|g| g.len() > 1) {
        assert!(!faked.contains(group), "{group} stays in {faked}");
    }
    assert!(!faked.starts_with(&imei[..3]), "the head stays: {faked}");
    let whole = crate::fake::names::fake_host(&Seed::from_bytes([9; 32]), imei);
    assert_eq!(faked, whole, "faked whole, word by word like a host");
    assert_eq!(leaks(&raw, &out, &report.fakes), 0, "verify counts a leak:\n{text}");
}

/// A body's media type keeps its spelling, a vendor tree's included: the cut
/// reads a part by it.
#[test]
fn a_media_type_keeps_its_spelling() {
    let mut raw = document(&[], "c1@cidhost.callerdom.zz", "v=0\r\n");
    let text = wire(&raw)
        .replace("Content-Type: application/sdp", "Content-Type: application/vnd.vendorx.order");
    crate::model::set_wire(&mut raw.legs[0].msgs[0], text.as_bytes());
    let mut out = raw.clone();
    anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([9; 32]), &Scope::default())
        .expect("anonymises");
    let text = wire(&out);
    assert!(text.contains("Content-Type: application/vnd.vendorx.order\r\n"), "{text}");
}

#[test]
fn verify_counts_an_identity_among_vocabulary_when_nothing_anonymises_it() {
    let docs = [&PROBES[0], &PROBES[1], &PROBES[3]]
        .into_iter()
        .map(|p| document(p.lines, "c1@cidhost.callerdom.zz", ""))
        .chain([with_media_type()]);
    let base = document(&[], "c1@cidhost.callerdom.zz", "v=0\r\n");
    let reading = crate::testflow::reading();
    let floor =
        crate::verify::leaks::leaks(&base, &Scope::default(), &Fakes::default(), &reading).total();
    for (i, raw) in docs.enumerate() {
        let counted =
            crate::verify::leaks::leaks(&raw, &Scope::default(), &Fakes::default(), &reading);
        assert!(counted.total() > floor, "probe {i}: {counted:?}");
    }
}
