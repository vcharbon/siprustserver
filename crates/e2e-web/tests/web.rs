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
        let dst = root.join(sub);
        std::fs::create_dir_all(&dst).unwrap();
        if let Ok(entries) = std::fs::read_dir(committed.join(sub)) {
            for e in entries.flatten() {
                std::fs::copy(e.path(), dst.join(e.file_name())).unwrap();
            }
        }
    }
    root
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
    assert!(html.contains("textarea"));
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
