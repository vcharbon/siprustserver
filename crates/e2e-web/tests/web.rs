//! Phase I acceptance (ADR-0018): launch the smoke campaign through the web
//! surface, poll the run page until the rows settle, open the cell (SVG +
//! check verdicts), and verify the SAME routes mirror JSON under
//! `Accept: application/json` — driven in-process via `tower::ServiceExt`.

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

/// A self-contained copy of the committed `e2e/` authored files (cases,
/// checksets, campaigns, infra — NOT runs/schemas) so authoring tests never
/// write into the repo.
fn temp_e2e(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("e2e-web-test-{tag}-{}", std::process::id()));
    let committed = workspace_root().join("e2e");
    for sub in ["cases", "checksets", "campaigns", "infra"] {
        copy_tree(&committed.join(sub), &root.join(sub));
    }
    root
}

/// Recursively mirror `src` into `dst` (cases may be organised into folders).
fn copy_tree(src: &std::path::Path, dst: &PathBuf) {
    std::fs::create_dir_all(dst).unwrap();
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            copy_tree(&path, &dst.join(e.file_name()));
        } else {
            std::fs::copy(&path, dst.join(e.file_name())).unwrap();
        }
    }
}

fn app(e2e_dir: &PathBuf) -> Router {
    e2e_web::router(e2e_dir.clone(), e2e_dir.join("runs"))
}

async fn get(app: &Router, uri: &str, json: bool) -> (StatusCode, String) {
    let mut req = Request::builder().uri(uri);
    if json {
        req = req.header(header::ACCEPT, "application/json");
    }
    let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn post(app: &Router, uri: &str, body: &str, json: bool) -> (StatusCode, String) {
    let mut req = Request::builder().method("POST").uri(uri);
    if json {
        req = req.header(header::ACCEPT, "application/json");
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The full browser path: list → launch → poll live progress → cell detail
/// with the SVG diagram — and every step's JSON mirror.
#[tokio::test(flavor = "multi_thread")]
async fn launch_poll_and_inspect_a_run() {
    let e2e = temp_e2e("run");
    let app = app(&e2e);

    // List: HTML has the launch button; JSON mirrors the campaign defs.
    let (st, html) = get(&app, "/campaigns", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("/campaigns/smoke/runs"), "launch form present");
    let (st, json) = get(&app, "/campaigns", true).await;
    assert_eq!(st, StatusCode::OK);
    let docs: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(docs.iter().any(|d| d["id"] == "smoke"), "{docs:?}");

    // Launch (API flavor → 202 + runId; the browser flavor would 303).
    let (st, body) = post(&app, "/campaigns/smoke/runs", "", true).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let run_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();

    // Poll the run page until the job finishes (the cell runs a real fake-infra
    // call on its own thread; this is wall-clock fast).
    let mut status_doc = serde_json::Value::Null;
    for _ in 0..600 {
        let (st, body) = get(&app, &format!("/runs/{run_id}"), true).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        status_doc = serde_json::from_str(&body).unwrap();
        if status_doc["finished"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(status_doc["finished"], true, "job must finish: {status_doc}");
    assert_eq!(status_doc["cells"][0]["passed"], true);
    let cell_dir = status_doc["cells"][0]["dir"].as_str().unwrap().to_string();

    // The HTML run page shows the verdict row (now static — no more polling).
    let (st, html) = get(&app, &format!("/runs/{run_id}"), false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("PASS"), "{html}");
    assert!(!html.contains("every 1s"), "finished page stops polling");

    // Cell detail: HTML embeds the SVG call diagram + check verdicts; JSON IS
    // the persisted result.json.
    let cell_url = format!("/runs/{run_id}/cells/{cell_dir}");
    let (st, html) = get(&app, &cell_url, false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("<svg"), "inline diagram");
    assert!(html.contains("from.userInfo"), "check verdicts table");
    let (st, json) = get(&app, &cell_url, true).await;
    assert_eq!(st, StatusCode::OK);
    let from_api: serde_json::Value = serde_json::from_str(&json).unwrap();
    let on_disk: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            e2e.join("runs/smoke")
                .join(run_id.split('/').nth(1).unwrap())
                .join(&cell_dir)
                .join("result.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(from_api, on_disk, "the cell API is the result.json, verbatim");

    std::fs::remove_dir_all(&e2e).ok();
}

/// Phase J on the web: the media cell's page embeds <audio> players and the
/// file route serves the actual RIFF/WAVE recording.
#[tokio::test(flavor = "multi_thread")]
async fn media_cell_serves_audio() {
    let e2e = temp_e2e("media");
    let app = app(&e2e);

    let (st, body) = post(&app, "/campaigns/full/runs", "", true).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let run_id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();
    let mut status_doc = serde_json::Value::Null;
    for _ in 0..600 {
        let (_, body) = get(&app, &format!("/runs/{run_id}"), true).await;
        status_doc = serde_json::from_str(&body).unwrap();
        if status_doc["finished"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(status_doc["finished"], true, "{status_doc}");
    let media_cell = status_doc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["dir"].as_str().unwrap().starts_with("basic-call-media"))
        .expect("media cell ran");
    assert_eq!(media_cell["passed"], true, "{media_cell}");
    let dir = media_cell["dir"].as_str().unwrap();

    let (st, html) = get(&app, &format!("/runs/{run_id}/cells/{dir}"), false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("<audio"), "audio players on the cell page");

    let (st, wav) =
        get(&app, &format!("/runs/{run_id}/cells/{dir}/files/alice.received.wav"), false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(wav.as_bytes().starts_with(b"RIFF"), "a real WAV is served");

    std::fs::remove_dir_all(&e2e).ok();
}

/// Authoring: an invalid case is rejected with the precise validation message
/// (nothing written); a valid one is persisted and re-served.
#[tokio::test(flavor = "multi_thread")]
async fn case_authoring_validates_then_writes() {
    let e2e = temp_e2e("author");
    let app = app(&e2e);

    // View the committed case (both flavors).
    let (st, html) = get(&app, "/cases/basic-call-identity", false).await;
    assert_eq!(st, StatusCode::OK);
    // The schema-aware Monaco editor, bound to the test-case schema.
    assert!(html.contains("/static/vs/loader.js"), "loads vendored Monaco");
    assert!(html.contains("/schemas/test-case"), "validates against the live schema");
    let (st, _) = get(&app, "/cases/basic-call-identity", true).await;
    assert_eq!(st, StatusCode::OK);

    // Reject: anchor basic-call does not publish; file untouched.
    let before = std::fs::read_to_string(e2e.join("cases/basic-call-identity.json")).unwrap();
    let bad = serde_json::json!({
        "id": "basic-call-identity",
        "compatibleShapes": ["basic-call"],
        "checks": [ { "on": "bob1.prack", "checks": [ { "field": "rack", "op": "exists" } ] } ]
    });
    let (st, body) = post(&app, "/cases/basic-call-identity", &bad.to_string(), true).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("prack"), "precise rejection: {body}");
    assert_eq!(
        std::fs::read_to_string(e2e.join("cases/basic-call-identity.json")).unwrap(),
        before
    );

    // Accept: a valid edit lands on disk and round-trips through GET.
    let mut good: serde_json::Value = serde_json::from_str(&before).unwrap();
    good["description"] = "edited from the web".into();
    let (st, _) = post(&app, "/cases/basic-call-identity", &good.to_string(), true).await;
    assert_eq!(st, StatusCode::OK);
    let (_, json) = get(&app, "/cases/basic-call-identity", true).await;
    let served: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(served["description"], "edited from the web");

    std::fs::remove_dir_all(&e2e).ok();
}

/// The Monaco editor's validation source: `/schemas/{name}` serves the live
/// model-generated JSON Schema (with or without the `.schema.json` suffix);
/// an unknown name 404s.
#[tokio::test(flavor = "multi_thread")]
async fn schema_route_serves_live_schemas() {
    let e2e = temp_e2e("schema");
    let app = app(&e2e);
    for name in ["test-case", "campaign", "check-set.schema.json"] {
        let (st, body) = get(&app, &format!("/schemas/{name}"), false).await;
        assert_eq!(st, StatusCode::OK, "{name}");
        let schema: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(schema.get("properties").is_some(), "{name} is a JSON schema object");
    }
    let (st, _) = get(&app, "/schemas/nope", false).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&e2e).ok();
}

/// Campaign authoring: the detail page shows the editor + visible concurrency,
/// a dangling case/shape reference is rejected, and a valid edit round-trips.
#[tokio::test(flavor = "multi_thread")]
async fn campaign_authoring_validates_then_writes() {
    let e2e = temp_e2e("campaign-author");
    let app = app(&e2e);

    // Detail page: matrix + the layout knobs + the schema-aware editor.
    let (st, html) = get(&app, "/campaigns/smoke", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("Concurrency (layout)"), "layout params visible");
    assert!(html.contains("/schemas/campaign"), "campaign editor schema-bound");

    // Reject: a dangling case + unknown infra shape; file untouched.
    let before = std::fs::read_to_string(e2e.join("campaigns/smoke.json")).unwrap();
    let bad = serde_json::json!({
        "id": "smoke",
        "cases": ["does-not-exist"],
        "infraShapes": ["no-such-infra"]
    });
    let (st, body) = post(&app, "/campaigns/smoke", &bad.to_string(), true).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("does-not-exist") && body.contains("no-such-infra"), "{body}");
    assert_eq!(std::fs::read_to_string(e2e.join("campaigns/smoke.json")).unwrap(), before);

    // Accept: a valid edit lands and round-trips.
    let mut good: serde_json::Value = serde_json::from_str(&before).unwrap();
    good["description"] = "edited from the web".into();
    let (st, _) = post(&app, "/campaigns/smoke", &good.to_string(), true).await;
    assert_eq!(st, StatusCode::OK);
    let (_, json) = get(&app, "/campaigns/smoke", true).await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["description"],
        "edited from the web"
    );
    std::fs::remove_dir_all(&e2e).ok();
}

/// Cases organised into a subfolder are discovered, grouped by folder in the
/// index, and still resolve + save by id (the folder is preserved on save).
#[tokio::test(flavor = "multi_thread")]
async fn cases_can_be_organised_into_folders() {
    let e2e = temp_e2e("folders");
    // Move a committed case into a subfolder (filename still equals its id).
    let nested = e2e.join("cases/group-a");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::rename(
        e2e.join("cases/basic-call-identity.json"),
        nested.join("basic-call-identity.json"),
    )
    .unwrap();
    let app = app(&e2e);

    // Index groups by folder and still links the nested case by id.
    let (st, html) = get(&app, "/cases", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("cases/group-a/"), "folder section shown: {html}");
    assert!(html.contains("/cases/basic-call-identity"), "nested case linked by id");

    // It resolves by id regardless of folder.
    let (st, json) = get(&app, "/cases/basic-call-identity", true).await;
    assert_eq!(st, StatusCode::OK);
    let mut doc: serde_json::Value = serde_json::from_str(&json).unwrap();

    // Saving preserves the folder (does not duplicate at the root).
    doc["description"] = "edited in folder".into();
    let (st, _) = post(&app, "/cases/basic-call-identity", &doc.to_string(), true).await;
    assert_eq!(st, StatusCode::OK);
    assert!(nested.join("basic-call-identity.json").is_file(), "saved back into the folder");
    assert!(
        !e2e.join("cases/basic-call-identity.json").exists(),
        "not duplicated at the cases/ root"
    );
    std::fs::remove_dir_all(&e2e).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_resources_are_404() {
    let e2e = temp_e2e("missing");
    let app = app(&e2e);
    for uri in ["/campaigns/nope", "/runs/smoke/never-ran", "/cases/nope"] {
        let (st, _) = get(&app, uri, true).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
    }
    std::fs::remove_dir_all(&e2e).ok();
}

// ---------------------------------------------------------------------------
// Load runs
// ---------------------------------------------------------------------------

/// The load-result.json for one fixture run: a genuine check_fail reinvite (with
/// a sampled callflow page) beside an OK basic_call, plus latency, a check
/// tally, and canaries. Kept as authored JSON so the test also exercises the
/// wire shape the loadgen writes.
fn fixture_load_result() -> serde_json::Value {
    serde_json::json!({
        "meta": {
            "startedMs": 1_700_000_000_000i64,
            "finishedMs": 1_700_000_060_000i64,
            "finished": true,
            "target": "172.20.255.250:5060",
            "cps": 20.0,
            "durationSecs": 60,
            "maxInFlight": 2000,
            "egress": "transparent",
            "profile": "endurance baseline"
        },
        "counts": [
            { "scenario": "basic_call", "class": "ok", "chaos": "clear", "count": 1180, "ok": true },
            { "scenario": "reinvite", "class": "check_fail", "chaos": "clear", "count": 1, "ok": false }
        ],
        "latency": [
            { "scenario": "basic_call", "n": 1180, "meanMs": 12.5, "p50Ms": 10.0, "p90Ms": 25.0, "p99Ms": 40.0, "maxMs": 88.0 }
        ],
        "checkpoints": [
            { "scenario": "basic_call", "checkpoint": "ringing", "n": 1180, "p50Ms": 3.0, "p90Ms": 8.0, "p99Ms": 15.0 }
        ],
        "checks": [
            { "scenario": "reinvite", "passed": 7, "failed": 1 }
        ],
        "canaries": {
            "orphans": 0, "shed": 4, "drops": 11, "ringingExpected": 1185, "ringingReceived": 1184
        },
        "samples": [
            { "scenario": "reinvite", "class": "check_fail", "chaos": "clear",
              "pages": ["callflows/reinvite/check_fail/clear/0.html"] }
        ]
    })
}

/// A load-runs root with one run dir (`endurance-1`) holding a load-result.json
/// and its one sampled callflow page. Returns `(root, app)`.
fn load_run_app(tag: &str) -> (PathBuf, Router) {
    let root = std::env::temp_dir().join(format!("e2e-web-load-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let run = root.join("endurance-1");
    let flow_dir = run.join("callflows/reinvite/check_fail/clear");
    std::fs::create_dir_all(&flow_dir).unwrap();
    std::fs::write(
        run.join("load-result.json"),
        serde_json::to_string_pretty(&fixture_load_result()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        flow_dir.join("0.html"),
        "<html><body><h3>reinvite check_fail sample</h3></body></html>",
    )
    .unwrap();
    // A partial run (no load-result.json yet) must NOT appear in the listing.
    std::fs::create_dir_all(root.join("half-baked")).unwrap();

    let e2e = std::env::temp_dir().join(format!("e2e-web-load-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&e2e).unwrap();
    let app = e2e_web::router_with_load_runs(e2e, root.join("nope-runs"), root.clone());
    (root, app)
}

/// The list + detail + JSON-negotiation path for load runs: the index lists the
/// run (skipping the index-less dir); the detail HTML renders the counts table,
/// latency, checks, canaries + a sample link; and the same detail route mirrors
/// the persisted load-result.json under `Accept: application/json`.
#[tokio::test(flavor = "multi_thread")]
async fn load_runs_list_detail_and_json() {
    let (root, app) = load_run_app("listdetail");

    // Index HTML: the run is linked; the half-baked dir is not.
    let (st, html) = get(&app, "/load", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("/load/endurance-1"), "run linked: {html}");
    assert!(!html.contains("half-baked"), "index-less dir hidden");

    // Index JSON mirrors {run, totalCalls, clearFailures, finished}.
    let (st, json) = get(&app, "/load", true).await;
    assert_eq!(st, StatusCode::OK);
    let docs: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let doc = docs.iter().find(|d| d["run"] == "endurance-1").expect("run in index");
    assert_eq!(doc["totalCalls"], 1181);
    assert_eq!(doc["clearFailures"], 1);
    assert_eq!(doc["finished"], true);

    // Detail HTML: counts table + latency + checks + canaries + sample link.
    let (st, html) = get(&app, "/load/endurance-1", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("check_fail"), "counts table: {html}");
    assert!(html.contains("172.20.255.250:5060"), "target echoed");
    assert!(html.contains("Latency"), "latency section");
    assert!(html.contains("Canaries"), "canaries section");
    assert!(
        html.contains("/load/endurance-1/files/callflows/reinvite/check_fail/clear/0.html"),
        "sample page linked: {html}"
    );

    // Detail JSON IS the persisted load-result.json, verbatim.
    let (st, json) = get(&app, "/load/endurance-1", true).await;
    assert_eq!(st, StatusCode::OK);
    let from_api: serde_json::Value = serde_json::from_str(&json).unwrap();
    let on_disk: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("endurance-1/load-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(from_api, on_disk, "detail JSON is the load-result.json");

    std::fs::remove_dir_all(&root).ok();
}

/// A sampled callflow page is served statically from the run dir; a missing run
/// 404s; and a traversal attempt is rejected (never reads outside the run dir).
#[tokio::test(flavor = "multi_thread")]
async fn load_run_files_serve_and_reject_traversal() {
    let (root, app) = load_run_app("files");

    // The sampled page renders.
    let (st, html) =
        get(&app, "/load/endurance-1/files/callflows/reinvite/check_fail/clear/0.html", false).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains("reinvite check_fail sample"), "served the callflow page");

    // A missing run 404s (both flavors).
    let (st, _) = get(&app, "/load/does-not-exist", true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Traversal attempts never escape the run dir.
    for uri in [
        "/load/endurance-1/files/../../load-result.json",
        "/load/endurance-1/files/../half-baked",
        "/load/endurance-1/files/callflows/../../../load-result.json",
    ] {
        let (st, _) = get(&app, uri, false).await;
        assert!(
            st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND,
            "traversal {uri} must be refused, got {st}"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}
