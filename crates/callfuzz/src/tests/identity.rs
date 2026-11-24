//! The identity draws ([`crate::identity`]): every value reads back under its
//! grammar, all chars or not; the targets and header lists bound what is
//! drawn; a fuzzed call drawing them all runs to a legal ending.

use sip_message::char_class;
use sip_message::header::{
    AddrForm, HeaderName, HeaderValue, NameAddr, PAssertedIdentity, Uri, UriRole,
};
use sip_message::sip_str::SipStr;

use super::*;

fn every_target(all_chars: bool) -> FuzzConfig {
    FuzzConfig {
        all_chars,
        text_headers: &["X-Fuzz-Free", "Organization"],
        header_budget: usize::MAX,
        ..payloads_always()
    }
}

/// The display name `d` reads back whole once written in a name-addr.
fn display_round_trips(d: &str) -> bool {
    let written = PAssertedIdentity::new(NameAddr::new(Uri::sip("example.com")).with_display(d));
    PAssertedIdentity::parse(&SipStr::owned(&written.to_string()))
        .is_ok_and(|read| read.display() == Some(d))
}

#[tokio::test(start_paused = true)]
async fn every_drawn_identity_value_reads_back() {
    let (h, alice, bob) = agents("callfuzz-identity-reads").await;
    let env = env(&alice, &bob, "t");
    for all_chars in [false, true] {
        let config = every_target(all_chars);
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..300 {
            let mut call = basic(&env);
            let mut notes = Vec::new();
            perturb(&mut Rng::new(seed), &mut call, &config, &mut notes);
            seen.extend(notes.iter().map(|n| n.split(':').next().unwrap_or("").to_string()));
            let plan = invite_plan(&call);
            let p = &plan.presentation;
            for d in [&p.from_display, &p.to_display].into_iter().flatten() {
                assert!(display_round_trips(d), "seed {seed}: display {d:?}");
            }
            for (name, value) in &plan.headers {
                let raw = SipStr::owned(value);
                if name == "P-Asserted-Identity" {
                    let read = PAssertedIdentity::parse(&raw).expect(value);
                    assert!(read.display().is_some(), "seed {seed}: {value:?}");
                    assert_eq!(read.uri().host(), "example.com", "seed {seed}: {value:?}");
                    let user = read.uri().user().expect(value);
                    assert!(drawn_user(user), "seed {seed}: user {user:?} of {value:?}");
                } else if name == "Route" {
                    let read = NameAddr::parse_as(&raw, AddrForm::NameAddrOnly, UriRole::Target)
                        .expect(value);
                    let uri = read.uri();
                    let (host, port) = (plan.via.ip().to_string(), plan.via.port());
                    assert_eq!((uri.host(), uri.port()), (host.as_str(), Some(port)), "{value}");
                    assert!(uri.param("lr").is_some(), "seed {seed}: {value:?}");
                    let x_fz = value.split(";x-fz=").nth(1).expect(value).trim_end_matches('>');
                    assert!(
                        char_class::is_run_of(x_fz, char_class::is_paramchar, true),
                        "seed {seed}: {value:?}"
                    );
                }
                assert!(!value.contains(['\r', '\n', '\0']), "seed {seed}: {name}: {value:?}");
            }
            for (canonical, wire) in &p.name_forms {
                assert!(
                    wire.eq_ignore_ascii_case(canonical) || wire.len() == 1,
                    "seed {seed}: {canonical} spelled {wire:?}"
                );
            }
        }
        for target in ["from-dn", "to-dn", "addr", "text", "route-self", "name"] {
            assert!(seen.contains(target), "all_chars {all_chars}: {target} never drawn: {seen:?}");
        }
    }
    h.finish().await;
}

/// Whether `user`, as a URI reader read it, is one the fuzzer draws whole: a
/// listed one, or a run of `user` characters and escapes holding all of them.
fn drawn_user(user: &str) -> bool {
    let all_chars =
        (0u8..0x80).filter(|&b| char_class::is_user_char(b)).all(|b| user.contains(char::from(b)));
    crate::identity::USERS.iter().any(|(u, _)| *u == user)
        || all_chars && char_class::is_run_of(user, char_class::is_user_char, true)
}

/// The URI reader reads back `user` as the whole user part.
fn read_as_written(user: &str) -> bool {
    Uri::parse(&SipStr::owned(&format!("sip:{user}@example.com")))
        .is_ok_and(|uri| uri.user() == Some(user))
}

#[test]
fn every_listed_and_all_chars_user_is_read_as_written() {
    for (user, _) in crate::identity::USERS {
        assert!(read_as_written(user), "{user:?}");
    }
    let mut rng = Rng::new(11);
    for _ in 0..200 {
        let user = crate::chars::user(&mut rng);
        assert!(read_as_written(&user), "{user:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_configured_list_never_adds_a_header_the_stack_writes() {
    let (h, alice, bob) = agents("callfuzz-identity-stack").await;
    let env = env(&alice, &bob, "t");
    let config = FuzzConfig {
        addr_headers: &["From", "m"],
        text_headers: &["Call-ID", "cseq", "Max-Forwards"],
        ..every_target(true)
    };
    for seed in 0..100 {
        let mut plan = invite_plan(&basic(&env));
        let before = plan.headers.len();
        let mut notes = Vec::new();
        crate::identity::draw(&mut Rng::new(seed), &mut plan, &config, &mut notes);
        let added: Vec<_> = plan.headers[before..].iter().map(|(n, _)| n.as_str()).collect();
        assert!(added.iter().all(|n| *n == "Route"), "seed {seed}: {added:?}");
        assert!(!notes.iter().any(|n| n.starts_with("addr:") || n.starts_with("text:")));
    }
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn no_target_draws_nothing_into_the_identity_fields() {
    let (h, alice, bob) = agents("callfuzz-identity-none").await;
    let env = env(&alice, &bob, "t");
    let config = FuzzConfig { targets: Targets::NONE, addr_headers: &[], ..every_target(true) };
    for seed in 0..100 {
        let mut call = basic(&env);
        let mut notes = Vec::new();
        perturb(&mut Rng::new(seed), &mut call, &config, &mut notes);
        let plan = invite_plan(&call);
        assert_eq!(plan.presentation, Default::default(), "seed {seed}");
        assert!(!plan.headers.iter().any(|(n, _)| n == "Route"), "seed {seed}");
        assert!(!notes.iter().any(|n| n.starts_with("addr:")), "seed {seed}: {notes:?}");
    }
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_drawn_header_never_repeats_one_the_invite_holds_in_another_spelling() {
    let (h, alice, bob) = agents("callfuzz-identity-spelling").await;
    let env = env(&alice, &bob, "t");
    let config = FuzzConfig { text_headers: &["Subject"], ..every_target(true) };
    for seed in 0..100 {
        let mut plan = invite_plan(&basic(&env));
        plan.headers.push(("s".into(), "compact subject".into()));
        crate::identity::draw(&mut Rng::new(seed), &mut plan, &config, &mut Vec::new());
        let subjects = plan.headers.iter().filter(|(n, _)| HeaderName::Subject.matches(n)).count();
        assert_eq!(subjects, 1, "seed {seed}: {:?}", plan.headers);
    }
    h.finish().await;
}

#[test]
fn targets_parse_from_a_comma_list() {
    assert_eq!(
        Targets::parse("from, route").unwrap(),
        Targets { from: true, route: true, ..Targets::NONE }
    );
    assert_eq!(Targets::parse("none").unwrap(), Targets::NONE);
    assert_eq!(Targets::parse("from,to,route,names").unwrap(), Targets::ALL);
    assert!(Targets::parse("from,cseq").is_err());
    assert!(Targets::parse("none,from").is_err());
}

/// Fuzzed basic calls drawing every target with all chars, alice to bob: each
/// ends on an accepted ending and the RFC audit charges nothing.
#[tokio::test(start_paused = true)]
async fn all_chars_calls_run_to_a_legal_ending() {
    let config = FuzzConfig { header_budget: 6000, ..every_target(true) };
    for seed in 0..24u64 {
        let name = format!("callfuzz-all-chars-{seed}");
        let (h, alice, bob) = agents(&name).await;
        let env = env(&alice, &bob, &name);
        let call =
            wrap(FuzzConfig { run_seed: seed, ..config }).build(&env).expect("the call builds");
        let verdict = run_built_actor_call(call, &env, &CallCtx::new()).await;
        let (_report, findings) = h.finish_collecting().await;
        assert!(matches!(verdict, ScenarioVerdict::Success { .. }), "seed {seed}: {verdict:?}");
        let gating: Vec<_> = findings.iter().filter(|f| !f.advisory).collect();
        assert!(gating.is_empty(), "seed {seed}: {gating:?}");
    }
}
