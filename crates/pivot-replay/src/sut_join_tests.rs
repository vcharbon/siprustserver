use super::*;

/// A document of two calls, their caller legs A and C, each answered on its
/// own callee leg.
fn two_calls() -> PivotV3 {
    let d = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;
    let text = format!(
        r#"{{
          "pivot_version": 3,
          "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                    "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
          "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
          "calls": [
            {{ "id": "c1", "caller_leg": "A", "attempts": [
               {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }},
            {{ "id": "c2", "caller_leg": "C", "attempts": [
               {{ "branch": 0, "position": 0, "leg": "D", "callee": {{ "identity": "caller" }} }} ] }} ],
          "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
          "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                      {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
          "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                    {{ "id": "B", "actor": "uas1", "dir": "in" }},
                    {{ "id": "C", "actor": "uac1", "dir": "out" }},
                    {{ "id": "D", "actor": "uas1", "dir": "in" }} ],
          "flow": [
            {{ "id": "s1", "leg": "A", "op": "send", "msg": {{ "method": "INVITE" }}, "delay": {d} }},
            {{ "id": "s2", "leg": "B", "op": "expect", "msg": {{ "method": "INVITE" }}, "delay": {d} }},
            {{ "id": "s3", "leg": "C", "op": "send", "msg": {{ "method": "INVITE" }}, "delay": {d} }},
            {{ "id": "s4", "leg": "D", "op": "expect", "msg": {{ "method": "INVITE" }}, "delay": {d} }}
          ],
          "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
        }}"#
    );
    PivotV3::from_json(&text).expect("the document parses")
}

/// A cell mints one Call-ID per call of its document, each its caller leg's
/// under the cell's nonce: the Call-ID the SUT keys that call's state by.
#[test]
fn a_cell_mints_one_call_id_per_call_under_its_nonce() {
    let document = two_calls();
    let minted = minted(&document, "18dc2ba9a58faa871");
    let ids: Vec<&str> = minted.iter().map(|identity| identity.call_id.as_str()).collect();
    assert_eq!(ids, ["A-18dc2ba9a58faa871@pivot.invalid", "C-18dc2ba9a58faa871@pivot.invalid"]);
}

/// The join is written into the bundle as the file the harness reads back.
#[test]
fn the_join_is_written_into_the_bundle() {
    let out = std::env::temp_dir().join(format!("pivot-replay-sut-join-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let join = SutJoin {
        call_ids: vec!["A-1@pivot.invalid".into()],
        started_at_ms: 1_760_000_000_000,
        ended_at_ms: 1_760_000_004_500,
    };
    record(&out, &join).expect("written");
    let read: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(FILE)).unwrap()).unwrap();
    assert_eq!(
        read,
        serde_json::json!({
            "call_ids": ["A-1@pivot.invalid"],
            "started_at_ms": 1_760_000_000_000_i64,
            "ended_at_ms": 1_760_000_004_500_i64
        })
    );
    let _ = std::fs::remove_dir_all(&out);
}
