use super::super::testkit::*;
use super::*;

/// A census reports a rule with no hits as zero rather than omitting it,
/// and merging two sweeps adds up.
#[test]
fn a_census_reports_every_rule_and_merges() {
    let clean = doc_of(vec![
        dg(1_000, A, B, request("INVITE", 1, "m1", "fa", None)),
        dg(4_000, B, A, response(200, "OK", 1, "INVITE", "m1", "fa", Some("tb"))),
        dg(4_500, A, B, request("ACK", 1, "m1", "fa", Some("tb"))),
    ]);
    let dirty = doc_of(vec![
        dg(1_000, A, B, request("INVITE", 1, "m2", "fa", None)),
        dg(4_000, A, B, request("CANCEL", 1, "m2", "fa", None)),
        dg(5_000, B, A, response(200, "OK", 1, "INVITE", "m2", "fa", Some("tb"))),
        dg(5_500, A, B, request("ACK", 1, "m2", "fa", Some("tb"))),
    ]);

    let mut first = Census::new();
    first.absorb("a.json", "cap-a", &clean);
    assert_eq!(first.rules["no-200-after-cancel"].hits, 0, "reported, not omitted");
    assert_eq!(
        first.rules["unacked-reliable-provisional"].hits, 0,
        "a rule this corpus never fires is still a reported result"
    );
    assert_eq!(first.rules["no-ack-to-dialog-creating-2xx"].hits, 0);

    let mut second = Census::new();
    second.absorb("b.json", "cap-b", &dirty);
    second.fail("c.json", "not JSON".to_string());

    first.merge(second);
    first.sort();
    assert_eq!(first.documents, 2);
    let tally = &first.rules["no-200-after-cancel"];
    assert_eq!((tally.hits, tally.documents), (1, 1));
    assert_eq!(tally.by_role["undetermined"], 1, "a single-leg call attributes neither side");
    assert_eq!(tally.buckets["gap <10ms"], 1);
    let cancelled =
        first.hits.iter().find(|h| h.hit.rule == RfcRule::No200AfterCancel).expect("the hit");
    assert_eq!(cancelled.capture, "cap-b");
    assert_eq!(
        first.captures.iter().map(String::as_str).collect::<Vec<_>>(),
        ["cap-a", "cap-b"],
        "the merged census covers both captures"
    );
    assert_eq!(first.failures.len(), 1);
    assert!(first.summary().contains("no-200-after-cancel: 1 hit(s)"));
    assert!(first.summary().contains("unacked-reliable-provisional: 0 hit(s)"));
}

/// Three rules in one document are tallied apart and all reach the report.
#[test]
fn every_rule_in_one_document_is_tallied_separately() {
    let both = doc_of(vec![
        // The PRACK rule's shape: 100rel offered, a reliable 180 taken,
        // never PRACKed, and the dialog lives on for seconds.
        dg(1_000_000, A, B, request_hdr("INVITE", 1, "x1", "fa", None, "Supported: 100rel\r\n")),
        dg(
            1_200_000,
            B,
            A,
            response_hdr(
                180,
                "Ringing",
                1,
                "INVITE",
                "x1",
                "fa",
                Some("tb"),
                "Require: 100rel\r\nRSeq: 1\r\n",
            ),
        ),
        // The cancel rule's shape, on the same transaction.
        dg(9_000_000, A, B, request("CANCEL", 1, "x1", "fa", None)),
        dg(9_100_000, B, A, response(200, "OK", 1, "CANCEL", "x1", "fa", Some("tb"))),
        dg(9_200_000, B, A, response(200, "OK", 1, "INVITE", "x1", "fa", Some("tb"))),
        // The ACK rule's shape: nobody ever confirms that dialog, and the
        // far side gives up 32 s later exactly as RFC 3261 §13.3.1.4 says.
        dg(41_200_000, B, A, request("BYE", 9, "x1", "tb", Some("fa"))),
        dg(41_300_000, A, B, response(200, "OK", 9, "BYE", "x1", "tb", Some("fa"))),
    ]);
    let mut census = Census::new();
    census.absorb("both.json", "cap-both", &both);
    census.sort();
    assert_eq!(census.rules["no-200-after-cancel"].hits, 1);
    assert_eq!(census.rules["unacked-reliable-provisional"].hits, 1);
    assert_eq!(census.rules["unacked-reliable-provisional"].buckets["window <10s"], 1);
    assert_eq!(census.rules["no-ack-to-dialog-creating-2xx"].hits, 1);
    assert_eq!(census.rules["no-ack-to-dialog-creating-2xx"].buckets["torn down un-ACKed"], 1);
    let three: Vec<&LocatedHit> = census
        .hits
        .iter()
        .filter(|h| {
            [
                RfcRule::No200AfterCancel,
                RfcRule::UnackedReliableProvisional,
                RfcRule::NoAckToDialogCreating2xx,
            ]
            .contains(&h.hit.rule)
        })
        .collect();
    assert_eq!(three.len(), 3);
    let json = serde_json::to_value(three[2]).unwrap();
    assert_eq!(json["rule"], "unacked-reliable-provisional");
    assert_eq!(json["emitter"], A, "the UAC that owed the PRACK");
    assert_eq!(json["rseq"], 1);
    let ack = serde_json::to_value(three[1]).unwrap();
    assert_eq!(ack["rule"], "no-ack-to-dialog-creating-2xx");
    assert_eq!(ack["emitter"], A, "the UAC that owed the ACK");
    assert_eq!(ack["to_tag"], "tb");
    assert_eq!(ack["bye_by"], B);
}

/// Every rule runs: a census names no candidates and still tallies a rule
/// beyond the first ones the vocabulary held, under its own token and
/// bucketed by its own measure, with the hit anchored on its message.
#[test]
fn every_rule_is_tallied_without_being_named() {
    let ok = response(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"));
    let recomposed = response_hdr(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"), "Server: x\r\n");
    let doc = doc_of(vec![
        dg(1_000_000, A, B, request("INVITE", 1, "r1", "fa", None)),
        dg(1_100_000, B, A, ok),
        dg(1_600_000, B, A, recomposed),
        dg(1_700_000, A, B, request("ACK", 1, "r1", "fa", Some("tb"))),
    ]);
    let mut census = Census::new();
    census.absorb("r.json", "cap-r", &doc);
    let tally = &census.rules["rung-byte-identical"];
    assert_eq!((tally.occasions, tally.decided, tally.hits), (1, 1, 1));
    assert_eq!(tally.buckets["2xx final"], 1);
    let rung = census
        .hits
        .iter()
        .find(|h| h.hit.rule == RfcRule::RungByteIdentical)
        .expect("the rung hit");
    let hit = serde_json::to_value(rung).unwrap();
    assert_eq!(hit["emitter"], B, "the UAS that re-composed its rung");
    assert_eq!(hit["region"], "head");
    assert_eq!(hit["anchor_msg"], 2, "anchored on the re-composed rung");
    assert!(census.summary().contains("rung-byte-identical: 1 hit(s)"));
    assert_eq!(census.rules.len(), RfcRule::ALL.len(), "every rule is a reported result");
}
