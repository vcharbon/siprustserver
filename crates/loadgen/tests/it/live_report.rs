//! The live report on the metrics socket: the page rendered from the
//! reporter on demand, its sample pages from memory, its index as JSON, and
//! the rate form answered by a redirect back to the page.

use std::sync::Arc;
use std::time::Duration;

use loadgen::{
    serve_metrics_on, CallEvidence, CallOutcome, Canaries, LiveReport, LoadRunIndex, LoadRunMeta,
    MetricsSurface, RateHandle, Reporter, ReporterCfg,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn http(bind: std::net::SocketAddr, req: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(bind).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn the_metrics_socket_serves_the_live_report() {
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 2, background_record_every: 0 }));
    let failed = CallOutcome::RfcAuditFail(vec![sip_net_testkit::RfcFinding {
        rule: "rule-x".into(),
        lane: "192.0.2.10:5060".into(),
        detail: "a 2xx never ACKed".into(),
        advisory: false,
        offending: None,
        charged: None,
    }]);
    let sample = loadgen::report::RenderedSample {
        html: Some(Arc::from("<html><body>the recorded flow</body></html>")),
        detail: failed.detail(),
        e2e_ms: 0.0,
    };
    let evidence = CallEvidence { sample: Some(sample), ..CallEvidence::default() };
    reporter.record("basic_call", &failed, &failed.case(None), Duration::ZERO, &[], evidence);
    let refused = CallOutcome::Rejected("key_in_flight");
    let none = CallEvidence::default();
    reporter.record("basic_call", &refused, &refused.case(None), Duration::ZERO, &[], none);

    let rate = RateHandle::new(20.0);
    let (index_reporter, page_reporter) = (reporter.clone(), reporter.clone());
    let meta = LoadRunMeta {
        target: "192.0.2.10:5060".into(),
        cps: 20.0,
        duration_secs: 60,
        command_line: vec!["loadgen".into(), "--cps".into(), "20".into()],
        ..LoadRunMeta::default()
    };
    let surface = MetricsSurface {
        metrics: Arc::new(|| "loadgen_calls_total 1\n".to_string()),
        rate: Some(rate.clone()),
        report: Some(LiveReport {
            index: Arc::new(move || index_reporter.build_index(meta.clone(), Canaries::default())),
            page: Arc::new(move |rel| page_reporter.sample_page(rel)),
        }),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    tokio::spawn(serve_metrics_on(listener, surface));

    let page = http(bind, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"), "{page}");
    assert!(page.contains("<pre class=\"cmd\">loadgen --cps 20</pre>"), "{page}");
    assert!(page.contains("<meta http-equiv=\"refresh\" content=\"60\">"));
    assert!(page.contains("action=\"rate?refresh=60\""), "live controls");
    let link = "callflows/basic_call/rfc_audit_fail/rule-x/0.html";
    assert!(page.contains(&format!("href=\"{link}\"")), "the sample linked: {page}");

    let frozen = http(bind, "GET /?refresh=0 HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(!frozen.contains("http-equiv=\"refresh\""), "frozen");

    let flow = http(bind, &format!("GET /{link} HTTP/1.1\r\nHost: x\r\n\r\n")).await;
    assert!(flow.contains("200 OK") && flow.contains("the recorded flow"), "{flow}");
    let missing = http(bind, "GET /callflows/x/0.html HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");

    let json = http(bind, "GET /load-result.json HTTP/1.1\r\nHost: x\r\n\r\n").await;
    let body = &json[json.find("\r\n\r\n").unwrap() + 4..];
    let idx: LoadRunIndex = serde_json::from_str(body).unwrap();
    assert_eq!((idx.progress.failed, idx.progress.rejected), (1, 1));
    let row = idx.counts.iter().find(|c| c.class == "rfc_audit_fail").unwrap();
    assert_eq!(row.error.as_deref(), Some("rfc audit: a 2xx never ACKed"));
    let failures: Vec<_> = idx.recent_failures.iter().map(|f| f.page.as_deref()).collect();
    assert_eq!(failures, vec![Some(link)], "the failure, not the refused key");

    let metrics = http(bind, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(metrics.contains("loadgen_calls_total 1"), "{metrics}");

    let form =
        "POST /rate?refresh=60 HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\n\
                Content-Length: 6\r\n\r\ncps=40";
    let posted = http(bind, form).await;
    assert!(
        posted.starts_with("HTTP/1.1 303") && posted.contains("Location: ./?refresh=60\r\n"),
        "{posted}"
    );
    assert_eq!(rate.cps(), 40.0, "the form re-targets the rate");
    let (events, _) = rate.events_since(reporter.started_at());
    assert_eq!((events.len(), events[0].from_cps, events[0].to_cps), (1, 20.0, 40.0));
}
