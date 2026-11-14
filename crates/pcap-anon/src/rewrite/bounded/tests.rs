use super::*;

fn subs(pairs: &[(&str, &str)]) -> BoundedSubs {
    BoundedSubs::new(
        pairs.iter().map(|(a, b)| (a.as_bytes().to_vec(), b.as_bytes().to_vec())).collect(),
    )
}

fn apply(subs: &BoundedSubs, input: &str) -> String {
    String::from_utf8(subs.apply(input.as_bytes())).expect("ASCII")
}

#[test]
fn a_name_is_replaced_where_it_stands_alone_or_inside_a_dotted_name() {
    let s = subs(&[("alpha.zz", "qwert.ab")]);
    assert_eq!(apply(&s, "sip:a@alpha.zz;x"), "sip:a@qwert.ab;x");
    assert_eq!(apply(&s, "ims.alpha.zz"), "ims.qwert.ab");
    assert_eq!(apply(&s, "myalpha.zz"), "myalpha.zz", "inside another word");
    assert_eq!(apply(&s, "alpha.zz-2"), "alpha.zz-2", "a word byte after it");
    assert_eq!(apply(&s, "abc%40alpha.zz%3Bx"), "abc%40qwert.ab%3Bx", "behind an escape");
}

#[test]
fn a_name_is_never_replaced_inside_a_longer_dotted_quad_the_address_pass_owns() {
    let s = subs(&[("33", "86"), ("8.1.104.83", "0.46.35.37")]);
    assert_eq!(
        apply(&s, "Contact: <sip:203.0.113.33:5061>;x=33"),
        "Contact: <sip:203.0.113.33:5061>;x=86"
    );
    assert_eq!(apply(&s, "33.1.2.3 1.2.3.33 a.33.b"), "33.1.2.3 1.2.3.33 a.86.b");
    assert_eq!(
        apply(&s, "Server: X 8.1.104.83 (y)"),
        "Server: X 0.46.35.37 (y)",
        "a quad that is the name itself"
    );
    assert!(s.found(b"203.0.113.33").is_empty(), "the leak check reads it as the rewrite does");
}

#[test]
fn a_name_is_found_in_any_case_and_its_fake_takes_that_case() {
    let s = subs(&[("core.alpha.zz", "qwer.tyuio.pa")]);
    assert_eq!(
        apply(&s, "Via: SIP/2.0/UDP Core.Alpha.ZZ:5060"),
        "Via: SIP/2.0/UDP Qwer.Tyuio.PA:5060"
    );
    assert_eq!(apply(&s, "CORE.ALPHA.ZZ"), "QWER.TYUIO.PA");
}

#[test]
fn the_longest_name_wins_where_two_overlap() {
    let s = subs(&[("alpha.zz", "qwert.ab"), ("ims.alpha.zz", "xcv.bnmqw.rt")]);
    assert_eq!(apply(&s, "<sip:ims.alpha.zz>"), "<sip:xcv.bnmqw.rt>");
    assert_eq!(s.found(b"a ims.alpha.zz b alpha.zz"), vec![1, 0]);
}

#[test]
fn no_name_is_no_rewrite() {
    let s = subs(&[]);
    assert_eq!(apply(&s, "anything"), "anything");
    assert!(s.found(b"anything").is_empty());
}

#[test]
fn a_delimited_name_carries_its_own_boundary() {
    let s = BoundedSubs::new(vec![(b">Fakey Testo<".to_vec(), b">qwert asdfg<".to_vec())]);
    assert_eq!(s.apply(b"<who>Fakey Testo</who>"), b"<who>Qwert Asdfg</who>".to_vec());
    assert_eq!(s.apply(b"<who>Fakey Testos</who>"), b"<who>Fakey Testos</who>".to_vec());
}
