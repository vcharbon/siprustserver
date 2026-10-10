use super::*;

fn lane(id: &str, kind: LaneKind) -> Lane {
    Lane::new(id, format!("{id} (lane)"), kind)
}

/// A small mixed-plane doc: alice→b1 INVITE, a crash band, then a b1→b2
/// repl Data, out of input order to prove the renderer sorts.
fn mixed_doc() -> SeqDoc {
    SeqDoc {
        title: "mixed".into(),
        description: Some("three planes".into()),
        passed: true,
        lanes: vec![
            lane("alice", LaneKind::Ua),
            lane("b1", LaneKind::Node),
            lane("b2", LaneKind::Node),
        ],
        rows: vec![
            SeqRow {
                at_ms: 200,
                seq: 3,
                from: "b1".into(),
                to: Some("b2".into()),
                label: "Data[Create/bak] x".into(),
                detail: None,
                conn: None,
                kind: RowKind::Repl { delivered: true },
            },
            SeqRow {
                at_ms: 0,
                seq: 1,
                from: "alice".into(),
                to: Some("b1".into()),
                label: "INVITE sip:bob".into(),
                detail: Some("INVITE sip:bob SIP/2.0\r\n".into()),
                conn: None,
                kind: RowKind::Sip { delivered: true },
            },
            SeqRow {
                at_ms: 100,
                seq: 2,
                from: "b1".into(),
                to: None,
                label: "crash b1".into(),
                detail: None,
                conn: None,
                kind: RowKind::Lifecycle,
            },
        ],
        anomalies: vec![],
        views: vec![],
        epoch_base_ms: None,
    }
}

/// A lifecycle marker recorded at the SAME `at_ms` as a following message
/// must render BEFORE it — order follows the global `seq` (true recording
/// order), NOT `at_ms`:
/// the `reboot` band must precede the `PullRequest[Bootstrap]` it triggers
/// even though both land on one paused-clock millisecond.
#[test]
fn lifecycle_row_sorts_before_a_same_ms_message_by_global_seq() {
    let doc = SeqDoc {
        title: "tie".into(),
        description: None,
        passed: true,
        lanes: vec![lane("b1", LaneKind::Node), lane("b2", LaneKind::Node)],
        // Both at the SAME at_ms; the marker was recorded FIRST (lower seq).
        // Pushed in REVERSE order to prove the renderer does not rely on input
        // order and orders by seq, not at_ms.
        rows: vec![
            SeqRow {
                at_ms: 1_618,
                seq: 8,
                from: "b2".into(),
                to: Some("b1".into()),
                label: "PullRequest[Bootstrap] caller=b2".into(),
                detail: None,
                conn: None,
                kind: RowKind::Repl { delivered: true },
            },
            SeqRow {
                at_ms: 1_618,
                seq: 7,
                from: "b2".into(),
                to: None,
                label: "reboot b2 restart empty, higher gen".into(),
                detail: None,
                conn: None,
                kind: RowKind::Lifecycle,
            },
        ],
        anomalies: vec![],
        views: vec![],
        epoch_base_ms: None,
    };
    let txt = render_global_txt(&doc);
    let reboot = txt.find("reboot b2").expect("reboot band present");
    let pull = txt.find("PullRequest[Bootstrap]").expect("pull present");
    assert!(reboot < pull, "lower-seq lifecycle row must sort before a same-at_ms message: {txt}");
    // The HTML view must agree (both renderers share `sorted_rows`).
    let html = render_html(&doc);
    let h_reboot = html.find("reboot b2").expect("html reboot present");
    let h_pull = html.find("PullRequest[Bootstrap]").expect("html pull present");
    assert!(h_reboot < h_pull, "HTML honors the same global-seq order: {html}");
}

#[test]
fn rows_render_in_time_order_not_input_order() {
    let doc = mixed_doc();
    let txt = render_global_txt(&doc);
    let invite = txt.find("INVITE").expect("INVITE present");
    let crash = txt.find("crash b1").expect("crash present");
    let data = txt.find("Data[Create/bak]").expect("Data present");
    assert!(invite < crash && crash < data, "sorted by (at_ms, seq): {txt}");
}

#[test]
fn lifecycle_row_renders_as_a_band_in_both_outputs() {
    let doc = mixed_doc();
    let txt = render_global_txt(&doc);
    assert!(txt.contains("=== crash b1 ==="), "text band: {txt}");
    let html = render_html(&doc);
    // The band is a full-width SVG annotation (no payload, not a `.seq-msg`);
    // its label appears in the diagram.
    assert!(html.contains("crash b1"), "html band label present");
    // A lifecycle band is NOT a clickable message and has no payload block.
    assert!(
        !html.contains("⏻ crash b1") || !html.contains("id=\"evt-1\""),
        "the lifecycle band (ord 1) must not get an #evt-1 payload block"
    );
}

#[test]
fn all_three_planes_appear_with_distinct_styling() {
    let doc = mixed_doc();
    let html = render_html(&doc);
    // SIP and REPL messages are clickable `.seq-msg` groups carrying a
    // plane class so they are visually distinguishable.
    assert!(html.contains("seq-msg seq-sip"), "sip message class");
    assert!(html.contains("seq-msg seq-repl"), "repl message class");
    // The lifecycle band renders (full-width, no class) — its label is shown.
    assert!(html.contains("crash b1"), "lifecycle band label");
    // The legend names all three planes.
    assert!(html.contains("class=\"legend\""));
    // (The Replication legend entry carries a per-socket-color note, so
    // match its prefix rather than an exact `>Replication<`.)
    assert!(html.contains(">SIP<") && html.contains(">Replication"));
    assert!(html.contains("Lifecycle"));
}

#[test]
fn format_epoch_utc_renders_time_of_day_with_millis() {
    // 1970-01-01 + (12h34m56.789s) → 12:34:56.789Z.
    let ms = ((12 * 3600 + 34 * 60 + 56) * 1000 + 789) as i64;
    assert_eq!(format_epoch_utc(ms), "12:34:56.789Z");
    // Wraps within the day (only time-of-day is shown).
    assert_eq!(format_epoch_utc(ms + 86_400_000), "12:34:56.789Z");
}

#[test]
fn wall_clock_doc_shows_absolute_utc_reference() {
    // A wall-clock-aligned doc renders the absolute-UTC anchor in the header
    // AND an absolute time next to each row's relative stamp. A virtual-clock
    // doc (epoch_base_ms = None) shows neither (relative-only).
    let mut doc = mixed_doc();
    // Anchor the base (at_ms 0) to a known epoch ms.
    let t0 = ((8 * 3600 + 48 * 60 + 29) * 1000 + 135) as i64; // 08:48:29.135Z
    doc.epoch_base_ms = Some(t0);
    let html = render_html(&doc);
    assert!(html.contains("Timeline t0"), "header states the absolute anchor");
    assert!(html.contains("08:48:29.135Z"), "absolute UTC anchor shown");
    // The repl row at at_ms=200 → t0 + 200ms = 08:48:29.335Z somewhere.
    assert!(html.contains("08:48:29.335Z"), "per-row absolute time shown");

    // Without the anchor, no absolute time leaks in (relative-only).
    let plain = render_html(&mixed_doc());
    assert!(!plain.contains("Timeline t0"));
    assert!(!plain.contains("08:48:29"));
}

#[test]
fn lanes_are_columns_in_declared_order() {
    let doc = mixed_doc();
    let html = render_html(&doc);
    let a = html.find("alice (lane)").unwrap();
    let b1 = html.find("b1 (lane)").unwrap();
    let b2 = html.find("b2 (lane)").unwrap();
    assert!(a < b1 && b1 < b2, "columns in declared order");
}

#[test]
fn two_pane_layout_with_fixed_detail_panel() {
    // The viewport-filling two-pane shell: a scrollable diagram on the left,
    // a FIXED, always-visible Message-Detail panel on the right whose
    // scrollable `.detail-body` shows the clicked payload.
    let doc = mixed_doc();
    let html = render_html(&doc);
    assert!(html.contains("class=\"main\""), "two-pane flex container present");
    assert!(html.contains("class=\"diagram-panel\""), "left diagram panel present");
    assert!(html.contains("class=\"detail-panel\""), "right detail panel present");
    assert!(html.contains("class=\"detail-body\""), "scrollable detail body present");
    assert!(html.contains("Message Detail"), "detail header captioned");
    assert!(
        html.contains("class=\"detail-placeholder\""),
        "placeholder shown until a message is clicked"
    );
}

#[test]
fn payload_lives_in_hidden_block_keyed_by_message_ordinal() {
    // The full wire text lives in a HIDDEN, HTML-escaped block keyed by the
    // message's diagram ordinal: `<div class="payload" id="evt-{N}" hidden>`.
    let doc = mixed_doc();
    let html = render_html(&doc);
    assert!(html.contains("class=\"payload "), "payload block present");
    assert!(html.contains("id=\"evt-0\""), "payload keyed by ordinal");
    // It is hidden (the detail panel pulls its innerHTML on click).
    let pay = html.find("id=\"evt-0\"").expect("evt-0 present");
    let pay_block = &html[pay..pay + html[pay..].find("</div>").unwrap_or(html.len() - pay)];
    // The opening tag of the evt-0 block carries `hidden`.
    let open_end = html[pay..].find('>').unwrap() + pay;
    assert!(
        html[pay..open_end].contains("hidden"),
        "payload block is hidden: {}",
        &html[pay..open_end]
    );
    let _ = pay_block;
    assert!(html.contains("INVITE sip:bob SIP/2.0"), "wire detail embedded");
}

#[test]
fn diagram_message_is_a_seq_msg_keyed_to_its_payload_block() {
    // A SIP message in the SVG diagram is a clickable `<g class="seq-msg"
    // data-idx="N">` whose ordinal matches its `#evt-N` payload block. The
    // click `<script>` wires `.seq-msg` → `.detail-body`.
    let doc = mixed_doc();
    let html = render_html(&doc);

    // INVITE sorts first (at_ms 0) → ord 0.
    let svg_end = html.find("</svg>").expect("svg present");
    let svg = &html[..svg_end];
    assert!(
        svg.contains("class=\"seq-msg seq-sip\" data-idx=\"0\""),
        "first message is a .seq-msg group with data-idx=\"0\": {svg}"
    );
    // The group with data-idx="0" wraps the INVITE arrow label.
    let g = svg.find("data-idx=\"0\"").expect("first message has data-idx=0");
    let g_close = svg[g..].find("</g>").expect("group closes");
    let group_span = &svg[g..g + g_close];
    assert!(
        group_span.contains("INVITE sip:bob"),
        "the data-idx=0 group carries the INVITE arrow label: {group_span}"
    );
    // It contains a transparent full-row hit rect so the whole row clicks.
    assert!(
        group_span.contains("fill=\"transparent\""),
        "group has a transparent hit target: {group_span}"
    );

    // The matching hidden payload block (id=evt-0) carries the full wire text.
    let pay = html.find("id=\"evt-0\"").expect("payload evt-0 present");
    assert!(
        html[pay..].contains("INVITE sip:bob SIP/2.0"),
        "payload evt-0 holds the message's full detail"
    );

    // The click handler wires `.seq-msg` clicks into the `.detail-body`.
    assert!(html.contains("querySelectorAll('.seq-msg')"), "click handler binds .seq-msg groups");
    assert!(
        html.contains("getElementById('evt-' + g.dataset.idx)"),
        "click handler resolves the matching #evt-{{idx}} payload"
    );
    assert!(
        html.contains("querySelector('.detail-body').innerHTML"),
        "click handler populates the .detail-body"
    );
}

#[test]
fn embed_is_a_self_contained_clickable_fragment() {
    // `render_embed` is the host-embeddable view (the E2E cell page): no
    // `<html>` chrome, but the SVG messages ARE wired to reveal their
    // payload on click — everything scoped under `.seq-embed`.
    let doc = mixed_doc();
    let embed = render_embed(&doc);

    // A fragment, not a full document.
    assert!(!embed.contains("<!DOCTYPE"), "embed is a fragment, no doctype");
    assert!(embed.contains("class=\"seq-embed\""), "scoped root present");
    // The clickable message group + its matching hidden payload block.
    assert!(
        embed.contains("class=\"seq-msg seq-sip\" data-idx=\"0\""),
        "first message is a clickable group: {embed}"
    );
    assert!(embed.contains("id=\"evt-0\""), "matching payload block keyed by ordinal");
    assert!(embed.contains("INVITE sip:bob SIP/2.0"), "payload carries the wire text");
    // The detail pane + a click handler scoped to the embed root (so a host
    // page with other content is never touched).
    assert!(embed.contains("class=\"detail-body\""), "detail pane present");
    assert!(
        embed.contains("document.currentScript.closest('.seq-embed')"),
        "click handler scopes to the embed root"
    );
}

#[test]
fn status_and_anomalies_surface() {
    let mut doc = mixed_doc();
    doc.passed = false;
    doc.anomalies.push(Anomaly {
        check: "rfc.cseqInDialogOrder".into(),
        detail: "out of order".into(),
        lane: Some("b1".into()),
        endpoint: None,
        advisory: Some(false),
        row_seqs: Vec::new(),
        rule_sourced: true,
    });
    let html = render_html(&doc);
    assert!(html.contains("FAIL"));
    assert!(html.contains("rfc.cseqInDialogOrder"));
    let txt = render_global_txt(&doc);
    assert!(txt.contains("FAIL"));
    assert!(txt.contains("rfc.cseqInDialogOrder"));
}

/// A finding with `row_seqs` renders as a clickable `.linked` list item
/// carrying the resolved diagram ordinals, the linked row gets a ⚠ badge in
/// the SVG, and its payload block carries the anomaly context — the three
/// pieces the anomaly↔message navigation is built from.
#[test]
fn linked_anomaly_ties_list_row_badge_and_payload_together() {
    let mut doc = mixed_doc();
    doc.passed = false;
    doc.anomalies.push(Anomaly {
        check: "non-contiguous-rseq".into(),
        detail: "RSeq gap".into(),
        lane: Some("b1".into()),
        endpoint: None,
        advisory: Some(false),
        // The INVITE row: seq 1 → diagram ordinal 0.
        row_seqs: vec![1],
        rule_sourced: true,
    });
    let html = render_html(&doc);

    // List: gating severity badge + the resolved data-rows ordinal + a jump
    // affordance naming the linked message.
    assert!(html.contains("class=\"anomaly gating linked\" data-rows=\"0\""), "{html}");
    assert!(html.contains(">GATING</span>"), "{html}");
    assert!(html.contains("→ INVITE sip:bob @ T+0.000s"), "{html}");
    // SVG: the linked row's group carries the ⚠ badge in the gating color.
    let svg_end = html.find("</svg>").unwrap();
    assert!(html[..svg_end].contains(">⚠</text>"), "row badge present: {html}");
    // Payload: the anomaly rides the row's hidden payload block, so the
    // detail panel states the finding when the message is clicked.
    let pay = html.find("id=\"evt-0\"").unwrap();
    let pay_end = pay + html[pay..].find("</pre>").unwrap();
    assert!(
        html[pay..pay_end].contains("payload-anoms")
            && html[pay..pay_end].contains("non-contiguous-rseq"),
        "payload carries the anomaly context: {}",
        &html[pay..pay_end]
    );
    // Header: severity split next to the raw count.
    assert!(html.contains("1 gating"), "{html}");
    assert!(html.contains("0 advisory"), "{html}");
    // The click handler wires linked anomalies to the diagram.
    assert!(html.contains("querySelectorAll('.anomaly.linked')"), "{html}");
}

/// Gating findings list before advisory ones regardless of recorded order,
/// and an unlinked / lifecycle-seq finding renders without a link.
#[test]
fn anomalies_sort_gating_first_and_unlinked_stay_unlinked() {
    let mut doc = mixed_doc();
    doc.passed = false;
    doc.anomalies.push(Anomaly {
        check: "rfc3261.rportEcho".into(),
        detail: "empty rport".into(),
        lane: None,
        endpoint: None,
        advisory: Some(true),
        row_seqs: Vec::new(),
        rule_sourced: true,
    });
    doc.anomalies.push(Anomaly {
        check: "rfc3261.cseqInDialogOrder".into(),
        detail: "reuse".into(),
        lane: None,
        endpoint: None,
        advisory: Some(false),
        // seq 2 is the lifecycle band — not a message row, so no link.
        row_seqs: vec![2],
        rule_sourced: true,
    });
    let html = render_html(&doc);
    let gating = html.find("rfc3261.cseqInDialogOrder").unwrap();
    let advisory = html.find("rfc3261.rportEcho").unwrap();
    assert!(gating < advisory, "gating listed first: {html}");
    // Neither resolves a row: no linked item, no data-rows.
    assert!(!html.contains("anomaly gating linked"), "{html}");
    assert!(!html.contains("anomaly advisory linked"), "{html}");
    assert!(!html.contains("data-rows"), "{html}");
}

/// consecutive lanes sharing a `group` render one bracketing
/// socket header above their individual captions.
#[test]
fn grouped_sub_lanes_render_a_shared_socket_header() {
    let mut doc = mixed_doc();
    doc.lanes = vec![
        lane("alice", LaneKind::Ua),
        Lane::new("10.0.0.9:5070#callee", "callee", LaneKind::Ua).with_group("10.0.0.9:5070"),
        Lane::new("10.0.0.9:5070#alt", "alt", LaneKind::Ua).with_group("10.0.0.9:5070"),
    ];
    doc.rows.truncate(0);
    doc.rows.push(SeqRow {
        at_ms: 0,
        seq: 1,
        from: "alice".into(),
        to: Some("10.0.0.9:5070#callee".into()),
        label: "INVITE sip:x".into(),
        detail: None,
        conn: None,
        kind: RowKind::Sip { delivered: true },
    });
    let html = render_html(&doc);
    // The shared socket appears once as the group header; both sub-lane
    // captions render individually.
    assert!(html.contains("10.0.0.9:5070</text>"), "group header text:\n{html}");
    assert!(html.contains(">callee</text>"));
    assert!(html.contains(">alt</text>"));
}

/// A doc carrying beliefs renders the third piece of the views plane in BOTH
/// outputs: the in-sequence chip on the observer's own column, the views
/// table, and the disagreements list naming the conflicting beliefs.
#[test]
fn views_render_as_chip_table_and_disagreement_list() {
    let mut doc = mixed_doc();
    doc.views = vec![
        ViewChange {
            at_ms: 50,
            seq: 4,
            observer: "b1".into(),
            lane: "b1".into(),
            subject: "b1".into(),
            belief: "running gen 1".into(),
            stance: "present".into(),
            signal: "core".into(),
        },
        ViewChange {
            at_ms: 100,
            seq: 5,
            observer: "b2".into(),
            lane: "b2".into(),
            subject: "b1".into(),
            belief: "absent".into(),
            stance: "absent".into(),
            signal: "withdraw".into(),
        },
    ];
    let html = render_html(&doc);
    // (a) the chip rides the sequence, anchored on the observer's column.
    assert!(html.contains("b1: b1 = running gen 1 (core)"), "view chip in the diagram");
    // (b) the views table names the observers as columns.
    assert!(html.contains("class=\"views\""), "views section present");
    assert!(html.contains("<th>b1</th>") && html.contains("<th>b2</th>"), "{html}");
    // (c) the disagreement is listed with both beliefs and their signals.
    assert!(html.contains("Disagreements (1)"), "{html}");
    assert!(html.contains("believes absent"), "{html}");
    assert!(html.contains("class=\"disputed"), "the conflicting cells are tinted");

    let txt = render_global_txt(&doc);
    assert!(txt.contains("[VIEW] b1: b1 = running gen 1 (core)"), "{txt}");
    assert!(txt.contains("Views — what each observer believed"), "{txt}");
    assert!(txt.contains("Disagreements (1)"), "{txt}");
    assert!(txt.contains("b2 believes absent (withdraw)"), "{txt}");
}

/// A doc with no beliefs renders no views section and no disagreements
/// list.
#[test]
fn a_doc_without_views_renders_no_views_sections() {
    let html = render_html(&mixed_doc());
    assert!(!html.contains("class=\"views\""));
    assert!(!html.contains("Disagreements"));
    let txt = render_global_txt(&mixed_doc());
    assert!(!txt.contains("Disagreements"));
}

/// SIP rows carrying a `conn` (the Call-ID) are coloured per
/// flow and named in the legend; the long id never renders inline on the
/// arrow label.
#[test]
fn sip_rows_colour_by_call_id_with_legend() {
    let mut doc = mixed_doc();
    let cid = "b-1-1234567890abcdef@10.244.0.7";
    doc.rows[1].conn = Some(cid.into());
    let html = render_html(&doc);
    // Legend chip names the (truncated) Call-ID.
    assert!(html.contains("Call-ID b-1-1234567890abcdef@10.244…"), "legend chip:\n{html}");
    // The arrow label itself stays clean (no inline Call-ID).
    assert!(html.contains(">INVITE sip:bob</text>"), "inline label unchanged:\n{html}");
    // The arrow is drawn with a conn-coloured arrowhead, not the SIP one.
    assert!(html.contains("url(#ah-conn-"), "conn-coloured arrowhead used:\n{html}");
}
