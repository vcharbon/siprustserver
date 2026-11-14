//! The texts a network profile keeps (`kept_texts`, the strings the SUT
//! itself writes): kept as written wherever they stand whole, accepted by
//! verify, and faked like any other text under a scope that does not keep
//! them. Every value invented; the media address is public-looking on
//! purpose, as the scope maps only public addresses.

use super::refusal_probe_tests::{document, wire};
use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::Flows;

fn probe() -> Flows {
    document(
        &["Server: Fakeswitch", "User-Agent: Notreal Gateway"],
        "c1@cidhost.callerdom.zz",
        "v=0\r\no=- 1 1 IN IP4 8.8.8.30\r\ns= Fakeswitch\r\nc=IN IP4 8.8.8.30\r\nt=0 0\r\n\
         m=audio 4000 RTP/AVP 0\r\n",
    )
}

fn anonymised(scope: &Scope) -> (Flows, Fakes) {
    let mut out = probe();
    let report =
        anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([13; 32]), scope)
            .expect("anonymises");
    (out, report.fakes)
}

/// Every check of verify on the probe's output, under `scope`.
fn verdict(out: &Flows, fakes: &Fakes, scope: &Scope) -> usize {
    let raw = probe();
    crate::verify::survivors(&raw, out, &crate::testflow::plan(), scope)
        + crate::verify::names::name_survivors(&raw, out, scope)
        + crate::verify::leaks::leaks(out, scope, fakes, &crate::testflow::reading()).total()
}

#[test]
fn a_kept_text_stays_as_written_and_verify_accepts_it() {
    let scope = Scope::default().keeping_texts(&["fakeSWITCH".to_string()]);
    let (out, fakes) = anonymised(&scope);
    let text = wire(&out);
    assert!(text.contains("Server: Fakeswitch\r\n"), "{text}");
    assert!(text.contains("s= Fakeswitch\r\n"), "{text}");
    assert!(!text.contains("Notreal"), "a text the profile does not keep goes:\n{text}");
    assert_eq!(verdict(&out, &fakes, &scope), 0, "{text}");
}

#[test]
fn a_profile_keeps_the_session_name_the_sut_writes() {
    let profile = crate::profile::Profile::from_json_str(
        r#"{"sut":["192.0.2.0/24"],"kept_texts":["Fakeswitch"]}"#,
    )
    .expect("the profile reads");
    assert!(profile.scope().keeps_text(" FAKESWITCH"));
    assert!(!profile.scope().keeps_text("Notreal Gateway"));
}

#[test]
fn a_text_no_profile_keeps_is_faked_and_verify_counts_it_left() {
    let (out, fakes) = anonymised(&Scope::default());
    assert!(!wire(&out).contains("Fakeswitch"), "{}", wire(&out));
    let kept = Scope::default().keeping_texts(&["Fakeswitch".to_string()]);
    let (left, _) = anonymised(&kept);
    assert!(
        verdict(&left, &fakes, &Scope::default()) > 0,
        "verify counts a kept text it was not told of"
    );
}
