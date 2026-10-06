//! A doc with no HTTP row renders byte for byte as it did before the HTTP
//! plane existed: the fixtures are that renderer's output for this doc.

use seq_report::{
    render_global_txt, render_html, render_svg, Anomaly, Lane, LaneKind, RowKind, SeqDoc, SeqRow,
};

fn row(seq: u64, from: &str, to: Option<&str>, label: &str, kind: RowKind) -> SeqRow {
    SeqRow {
        at_ms: seq as i64 * 7,
        seq,
        from: from.into(),
        to: to.map(Into::into),
        label: label.into(),
        detail: Some(format!("{label} SIP/2.0\r\nCall-ID: c-1\r\n")),
        conn: seq.is_multiple_of(2).then(|| "c-1@host".to_string()),
        kind,
    }
}

/// SIP, replication and lifecycle rows, a lost frame, a grouped sub-lane and
/// both anomaly severities, one linked to a row.
pub fn sip_only_doc() -> SeqDoc {
    let sip = RowKind::Sip { delivered: true };
    SeqDoc {
        title: "sip-only".into(),
        description: Some("no HTTP plane".into()),
        passed: false,
        lanes: vec![
            Lane::new("alice", "alice (10.0.0.1:5060)", LaneKind::Ua),
            Lane::new("b1", "b1 (10.0.0.2:5060)", LaneKind::Node),
            Lane::new("b2", "b2 (10.0.0.3:5060)", LaneKind::Node),
            Lane::new("10.0.0.9:5070#bob", "bob", LaneKind::Ua).with_group("10.0.0.9:5070"),
        ],
        rows: vec![
            row(1, "alice", Some("b1"), "INVITE sip:bob@x", sip),
            row(2, "b1", Some("b2"), "Data[Create/bak]", RowKind::Repl { delivered: true }),
            row(3, "b1", None, "crash b1", RowKind::Lifecycle),
            row(4, "b1", Some("10.0.0.9:5070#bob"), "INVITE sip:bob@y", sip),
            row(5, "b2", Some("b1"), "PullRequest", RowKind::Repl { delivered: false }),
            row(6, "10.0.0.9:5070#bob", Some("b1"), "200 OK", RowKind::Sip { delivered: false }),
        ],
        anomalies: vec![
            Anomaly {
                check: "cseq-in-dialog-order".into(),
                detail: "CSeq went backwards".into(),
                lane: Some("alice".into()),
                endpoint: Some("alice".into()),
                advisory: Some(false),
                row_seqs: vec![4],
                rule_sourced: true,
            },
            Anomaly {
                check: "queueLeak".into(),
                detail: "1 datagram left".into(),
                lane: None,
                endpoint: None,
                advisory: Some(true),
                row_seqs: Vec::new(),
                rule_sourced: false,
            },
        ],
        views: vec![],
        epoch_base_ms: None,
    }
}

#[test]
fn a_sip_only_doc_renders_the_html_it_rendered_before() {
    assert_eq!(render_html(&sip_only_doc()), include_str!("fixtures/sip_only.html"));
}

#[test]
fn a_sip_only_doc_renders_the_svg_it_rendered_before() {
    assert_eq!(render_svg(&sip_only_doc()), include_str!("fixtures/sip_only.svg"));
}

#[test]
fn a_sip_only_doc_renders_the_text_it_rendered_before() {
    assert_eq!(render_global_txt(&sip_only_doc()), include_str!("fixtures/sip_only.global.txt"));
}
