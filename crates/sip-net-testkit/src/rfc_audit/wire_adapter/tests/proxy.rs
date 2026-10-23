//! The live surfacing of the proxy family rules.

use super::*;

/// §16.7 step 5 at the taker: one 100 per INVITE sent is owed, and only the
/// copy past the budget is reported — once, whatever the count.
#[test]
fn an_extra_100_is_reported_once_at_the_taker() {
    let owed = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 1),
    ];
    assert!(live(RuleId::Proxy100TryingNotForwarded).check_positioned(&owed).is_empty());

    let mut extra = owed;
    extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 2));
    extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 3));
    let out = live(RuleId::Proxy100TryingNotForwarded).check_positioned(&extra);
    assert_eq!(out.len(), 1, "one finding for the transaction: {out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took them");
    assert!(out[0].1.contains("received 3 100 Trying"), "{}", out[0].1);
    assert!(out[0].1.contains("against 1 INVITE(s) sent"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the first excess 100");
}

/// §16.3: advisory, `{Proxy}`-only, and a §16.6 forward on a FRESH branch
/// is a resolved target, not a no-target outcome.
#[test]
fn no_target_404_is_advisory_proxy_scoped_and_survives_a_branch_rewrite() {
    assert!(live(RuleId::NoTarget404).force_advisory());
    assert_eq!(live(RuleId::NoTarget404).subject(), HashSet::from([UaRole::Proxy]));

    let unforwarded = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, resp(500, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
    ];
    let out = live(RuleId::NoTarget404).check_positioned(&unforwarded);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("expected 404"), "{}", out[0].1);

    let answered_404 = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, resp(404, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
    ];
    assert!(live(RuleId::NoTarget404).check_positioned(&answered_404).is_empty());

    // The downstream that answered is no recorded bind: a row scoped to
    // proxies never falls back to judge it as one.
    let forwarded = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-out", 1, None), BOB, 1),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-out"), BOB, 2),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 3),
    ];
    assert!(
        live(RuleId::NoTarget404).check_positioned(&forwarded).is_empty(),
        "{:?}",
        live(RuleId::NoTarget404).check_positioned(&forwarded)
    );

    let relayed = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-other"), BOB, 1),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 2),
    ];
    assert!(
        live(RuleId::NoTarget404).check_positioned(&relayed).is_empty(),
        "a relayed downstream rejection resolved a target"
    );
}

/// §16.4 is proxy behaviour: a `{Proxy}`-declared lane is judged, a UA lane
/// is not — the subject, not the body, is what keeps a UA off this rule.
#[test]
fn strict_route_is_judged_only_on_a_declared_proxy_lane() {
    let wire =
        vec![recv(SUT, req_with("INVITE", "z9hG4bK-in", "Route: <sip:strict@h>\r\n"), ALICE, 1)];
    let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
    proxy.extend(wire.clone());
    let found: Vec<RfcFinding> = evaluate_rfc_findings(&proxy)
        .into_iter()
        .filter(|f| f.rule == live(RuleId::StrictRouteRewriteHandled).name())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lane, SUT);
    assert!(found[0].detail.contains("strict-route"), "{}", found[0].detail);
    assert_eq!(found[0].offending, Some(1), "offending points at the request taken");

    let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
    ua.extend(wire);
    assert!(
        !evaluate_rfc_findings(&ua)
            .iter()
            .any(|f| f.rule == live(RuleId::StrictRouteRewriteHandled).name()),
        "a UA lane forwards nothing and is not judged",
    );
}

/// §16.7 stays ADVISORY and proxy-scoped: a paused test clock advances
/// virtual time no real latency corresponds to.
#[test]
fn the_100_trying_grace_is_advisory_and_proxy_scoped() {
    assert!(live(RuleId::Proxy100WithinGrace).force_advisory());
    assert_eq!(live(RuleId::Proxy100WithinGrace).subject(), HashSet::from([UaRole::Proxy]));

    let silent = vec![recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0)];
    let f = live(RuleId::Proxy100WithinGrace).check_positioned(&silent);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("100 Trying") && f[0].1.contains("no response sent"), "{}", f[0].1);

    // Answered inside the grace: §16.7 owes nothing.
    let prompt = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 5),
    ];
    assert!(live(RuleId::Proxy100WithinGrace).check_positioned(&prompt).is_empty());

    // Past it, the finding names the Δ actually observed.
    let late = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 350),
    ];
    let f = live(RuleId::Proxy100WithinGrace).check_positioned(&late);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("Δ=350ms"), "{}", f[0].1);
}
