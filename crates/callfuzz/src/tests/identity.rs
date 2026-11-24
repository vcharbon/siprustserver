//! The identity draws ([`crate::identity`]): every value reads back under its
//! grammar, all chars or not; the targets and header lists bound what is
//! drawn; a fuzzed call drawing them all runs to a legal ending.

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
                    let read = PAssertedIdentity::parse(&raw);
                    assert!(read.is_ok_and(|a| a.display().is_some()), "seed {seed}: {value:?}");
                } else if name == "Route" {
                    let read = NameAddr::parse_as(&raw, AddrForm::NameAddrOnly, UriRole::Target);
                    assert!(read.is_ok(), "seed {seed}: Route {value:?}");
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

#[tokio::test(start_paused = true)]
async fn an_all_chars_user_is_read_as_written() {
    let mut rng = Rng::new(11);
    for _ in 0..200 {
        let user = crate::chars::user(&mut rng);
        let uri = Uri::parse(&SipStr::owned(&format!("sip:{user}@example.com")));
        assert!(uri.is_ok(), "{user:?}");
    }
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
