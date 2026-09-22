//! HTTP exchanges on the unified timeline: a request row from the requester's
//! lane to a service lane and a reply row back, placed between the SIP rows by
//! their global `seq`, in every renderer, and absent from the normalized doc.

use seq_report::{
    normalize, render_global_txt, render_html, render_svg, role_map_from_lanes, Anomaly, Lane,
    LaneKind, RowKind, SeqDoc, SeqRow,
};

fn row(seq: u64, from: &str, to: &str, label: &str, kind: RowKind) -> SeqRow {
    SeqRow {
        at_ms: seq as i64 * 10,
        seq,
        from: from.into(),
        to: Some(to.into()),
        label: label.into(),
        detail: Some(format!("detail of {label}")),
        conn: None,
        kind,
    }
}

/// alice INVITEs the SUT, the SUT asks the service (answered), then asks again
/// (no reply), then relays the INVITE on. Rows pushed out of order.
fn doc() -> SeqDoc {
    let sip = RowKind::Sip { delivered: true };
    SeqDoc {
        title: "http".into(),
        description: None,
        passed: false,
        lanes: vec![
            Lane::new("alice", "alice (10.0.0.1:5060)", LaneKind::Ua),
            Lane::new("sut", "sut (10.0.0.2:5060)", LaneKind::Sut),
            Lane::new("svc", "routing (10.0.0.9:8080)", LaneKind::Service),
            Lane::new("bob", "bob (10.0.0.3:5060)", LaneKind::Ua),
        ],
        rows: vec![
            row(6, "sut", "bob", "INVITE sip:bob@x", sip),
            row(2, "sut", "svc", "POST /route", RowKind::Http { delivered: true }),
            row(1, "alice", "sut", "INVITE sip:bob@x", sip),
            row(3, "svc", "sut", "200", RowKind::Http { delivered: true }),
            row(4, "sut", "svc", "POST /route/next", RowKind::Http { delivered: true }),
            row(5, "svc", "sut", "no reply", RowKind::Http { delivered: false }),
        ],
        anomalies: vec![Anomaly {
            check: "http.unmatched".into(),
            detail: "no script opens POST /route/next".into(),
            lane: None,
            endpoint: None,
            advisory: Some(false),
            row_seqs: vec![4],
            rule_sourced: false,
        }],
        views: vec![],
        epoch_base_ms: None,
    }
}

#[test]
fn the_text_timeline_tags_http_rows_between_the_sip_rows() {
    let txt = render_global_txt(&doc());
    assert!(txt.contains("http=4"), "the header counts the HTTP rows:\n{txt}");
    let at = |needle: &str| txt.find(needle).unwrap_or_else(|| panic!("{needle} in\n{txt}"));
    let post = at("[HTTP] sut (10.0.0.2:5060) -> routing (10.0.0.9:8080)  POST /route");
    let reply = at("[HTTP] routing (10.0.0.9:8080) -> sut (10.0.0.2:5060)  200");
    let lost = at("no reply  ✗ [LOST IN TRANSIT]");
    let first_invite = at("[SIP ] alice");
    let relayed = at("[SIP ] sut");
    assert!(first_invite < post && post < reply && reply < lost && lost < relayed, "{txt}");
    assert!(txt.contains("[HTTP] exchange"), "the legend names the plane:\n{txt}");
}

#[test]
fn a_doc_without_http_rows_keeps_its_text_header() {
    let mut d = doc();
    d.rows.retain(|r| !matches!(r.kind, RowKind::Http { .. }));
    d.lanes.retain(|l| l.kind != LaneKind::Service);
    let txt = render_global_txt(&d);
    assert!(txt.contains("(sip=2, repl=0, lifecycle=0)\n"), "{txt}");
    assert!(!txt.contains("[HTTP]"), "{txt}");
}

#[test]
fn the_svg_draws_http_rows_as_messages_to_the_service_lane() {
    let svg = render_svg(&doc());
    assert_eq!(svg.matches("seq-msg seq-http").count(), 4, "{svg}");
    assert!(svg.contains("POST /route"), "{svg}");
    assert!(svg.contains("routing (10.0.0.9:8080)"), "the service lane has a column: {svg}");
    assert!(svg.contains("no reply ✗ lost"), "an undelivered reply reads as one: {svg}");
}

#[test]
fn the_html_links_an_anomaly_to_its_http_row_and_carries_its_payload() {
    let html = render_html(&doc());
    // Sorted ordinals: 0 alice INVITE, 1 POST, 2 200, 3 POST next, 4 no reply.
    assert!(html.contains("data-rows=\"3\""), "the finding links the request row:\n{html}");
    assert!(html.contains("detail of POST /route/next"), "the payload block is there");
    assert!(html.contains(">HTTP<"), "the legend names the plane");
}

#[test]
fn normalize_drops_http_rows_and_the_lanes_only_they_use() {
    let d = doc();
    let n = normalize(&d, &role_map_from_lanes(&d.lanes));
    assert!(n.rows.iter().all(|r| matches!(r.kind, RowKind::Sip { .. })), "{:?}", n.rows);
    assert_eq!(n.rows.len(), 2);
    assert!(n.lanes.iter().all(|l| l.kind != LaneKind::Service), "{:?}", n.lanes);

    let mut plain = d.clone();
    plain.rows.retain(|r| !matches!(r.kind, RowKind::Http { .. }));
    plain.lanes.retain(|l| l.kind != LaneKind::Service);
    plain.anomalies.clear();
    let m = normalize(&plain, &role_map_from_lanes(&plain.lanes));
    assert_eq!(n, m, "a doc with HTTP rows normalizes as the same doc without them");
}

#[test]
fn normalize_drops_a_requester_lane_that_only_http_rows_reference() {
    let mut d = doc();
    d.lanes.push(Lane::new("10.0.0.2", "http client 10.0.0.2", LaneKind::Sut));
    d.rows.push(row(7, "10.0.0.2", "svc", "POST /late", RowKind::Http { delivered: true }));
    let n = normalize(&d, &role_map_from_lanes(&d.lanes));
    assert!(n.lanes.iter().all(|l| l.id != "http client 10.0.0.2"), "{:?}", n.lanes);
}

#[test]
fn the_row_kind_and_lane_kind_serialize_as_http_and_service() {
    let json = serde_json::to_string(&RowKind::Http { delivered: false }).unwrap();
    assert_eq!(json, r#"{"http":{"delivered":false}}"#);
    assert_eq!(serde_json::to_string(&LaneKind::Service).unwrap(), r#""service""#);
}
