//! The E2E test-management website (ADR-0018 Phase I): axum + maud + htmx over
//! the `e2e-core` registry. **One content-negotiated route set** — each handler
//! renders Maud HTML for a browser and the mirrored JSON when the client sends
//! `Accept: application/json` — so the website and the API can never drift.
//!
//! | Route | HTML | JSON |
//! |---|---|---|
//! | `GET /campaigns` | list + launch buttons | campaign index |
//! | `GET /campaigns/{id}` | detail + matrix preview | campaign def |
//! | `POST /campaigns/{id}/runs` | 303 → run page | `{runId}` |
//! | `GET /runs` | run history | run index |
//! | `GET /runs/{campaign}/{ts}` | live progress (htmx 1s poll) | status + cells |
//! | `GET /runs/{campaign}/{ts}/cells/{cell}` | SVG diagram + verdicts | cell result.json |
//! | `GET /cases` / `GET /cases/{id}` / `POST /cases/{id}` | view/author (validated) | case JSON |
//!
//! Launching POSTs into [`e2e_core::run::spawn_job`]; live jobs are polled via
//! their [`JobHandle`]s, finished runs are read back from the on-disk
//! `campaign.json` — the disk is the source of truth once a run completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use e2e_core::model::{self, ModelError};
use e2e_core::result::CellSummary;
use e2e_core::run::{self, JobHandle};
use maud::{DOCTYPE, Markup, PreEscaped, html};

/// Shared app state: the authored-files dir, the runs root, and the live jobs.
pub struct AppState {
    pub e2e_dir: PathBuf,
    pub runs_root: PathBuf,
    /// Live (spawned this process) jobs, keyed `"<campaign>/<ts>"`. Finished
    /// runs are served from disk; entries here only add live progress.
    jobs: Mutex<HashMap<String, JobHandle>>,
}

pub fn router(e2e_dir: PathBuf, runs_root: PathBuf) -> Router {
    let state = Arc::new(AppState { e2e_dir, runs_root, jobs: Mutex::new(HashMap::new()) });
    Router::new()
        .route("/", get(|| async { Redirect::to("/campaigns") }))
        .route("/campaigns", get(campaigns_index))
        .route("/campaigns/{id}", get(campaign_detail))
        .route("/campaigns/{id}/runs", axum::routing::post(campaign_launch))
        .route("/runs", get(runs_index))
        .route("/runs/{campaign}/{ts}", get(run_status))
        .route("/runs/{campaign}/{ts}/cells/{cell}", get(cell_detail))
        .route("/runs/{campaign}/{ts}/cells/{cell}/files/{name}", get(cell_file))
        .route("/cases", get(cases_index))
        .route("/cases/{id}", get(case_view).post(case_save))
        .with_state(state)
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"))
}

type HttpError = (StatusCode, String);

fn not_found(what: impl std::fmt::Display) -> HttpError {
    (StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn internal(err: impl std::fmt::Display) -> HttpError {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// The shared page chrome: nav + htmx (CDN; the only client-side dependency).
fn page(title: &str, body: Markup) -> Html<String> {
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (title) " — E2E" }
                script src="https://unpkg.com/htmx.org@1.9.12" {}
                style { (PreEscaped(CSS)) }
            }
            body {
                nav {
                    a href="/campaigns" { "Campaigns" } " · "
                    a href="/runs" { "Runs" } " · "
                    a href="/cases" { "Test cases" }
                }
                h1 { (title) }
                (body)
            }
        }
    };
    Html(markup.into_string())
}

const CSS: &str = r#"
body { font-family: system-ui, sans-serif; margin: 2rem; color: #111827; }
nav { margin-bottom: 1rem; color: #6b7280; }
table { border-collapse: collapse; margin: .75rem 0; }
th, td { border: 1px solid #e5e7eb; padding: .35rem .7rem; text-align: left; font-size: .95rem; }
th { background: #f9fafb; }
.pass { color: #059669; font-weight: 600; }
.fail { color: #dc2626; font-weight: 600; }
.crash { color: #b45309; font-weight: 600; }
.pending { color: #6b7280; }
.muted { color: #6b7280; font-size: .9rem; margin: .25rem 0 .5rem; max-width: 60rem; }
.muted-inline { color: #9ca3af; font-size: .85rem; }
.advisory { color: #6b7280; }
button { padding: .3rem .8rem; cursor: pointer; }
textarea { width: 100%; min-height: 24rem; font-family: ui-monospace, monospace; }
pre { background: #f9fafb; padding: .6rem; overflow-x: auto; }
"#;

fn verdict_markup(passed: bool, error: Option<&str>) -> Markup {
    match (passed, error) {
        (true, _) => html! { span .pass { "PASS" } },
        (false, None) => html! { span .fail { "FAIL" } },
        (false, Some(e)) => html! { span .crash title=(e) { "CRASH" } },
    }
}

// ---------------------------------------------------------------------------
// Campaigns
// ---------------------------------------------------------------------------

fn list_docs<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<(String, T)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(doc) = serde_json::from_str::<T>(&text) {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    out.push((stem, doc));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

async fn campaigns_index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let campaigns = list_docs::<model::Campaign>(&st.e2e_dir.join("campaigns"));
    if wants_json(&headers) {
        let docs: Vec<_> = campaigns.into_iter().map(|(_, c)| c).collect();
        return Json(docs).into_response();
    }
    page(
        "Campaigns",
        html! {
            table {
                tr { th { "id" } th { "cases" } th { "infra shapes" } th {} }
                @for (_, c) in &campaigns {
                    tr {
                        td { a href={ "/campaigns/" (c.id) } { (c.id) } }
                        td { (c.cases.join(", ")) }
                        td { (c.infra_shapes.join(", ")) }
                        td {
                            form method="post" action={ "/campaigns/" (c.id) "/runs" } style="margin:0" {
                                button type="submit" { "Launch" }
                            }
                        }
                    }
                }
            }
        },
    )
    .into_response()
}

fn load_campaign_by_id(st: &AppState, id: &str) -> Result<model::Campaign, HttpError> {
    model::load_campaign(&st.e2e_dir.join("campaigns").join(format!("{id}.json")))
        .map_err(|_| not_found(format!("campaign {id:?}")))
}

async fn campaign_detail(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let campaign = load_campaign_by_id(&st, &id)?;
    if wants_json(&headers) {
        return Ok(Json(campaign).into_response());
    }
    Ok(page(
        &format!("Campaign {id}"),
        html! {
            @if let Some(d) = &campaign.description { p { (d) } }
            h2 { "Matrix preview" }
            table {
                tr { th { "case" } @for i in &campaign.infra_shapes { th { (i) } } }
                @for c in &campaign.cases {
                    tr {
                        td { a href={ "/cases/" (c) } { (c) } }
                        @for _ in &campaign.infra_shapes { td .pending { "—" } }
                    }
                }
            }
            form method="post" action={ "/campaigns/" (id) "/runs" } {
                button type="submit" { "Launch" }
            }
        },
    )
    .into_response())
}

async fn campaign_launch(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let campaign_path = st.e2e_dir.join("campaigns").join(format!("{id}.json"));
    if !campaign_path.is_file() {
        return Err(not_found(format!("campaign {id:?}")));
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Suffix on collision so two launches in one second both get a run dir.
    let mut ts = format!("run-{secs}");
    {
        let jobs = st.jobs.lock().unwrap();
        let mut n = 1;
        while jobs.contains_key(&format!("{id}/{ts}"))
            || e2e_core::result::run_dir(&st.runs_root, &id, &ts).exists()
        {
            ts = format!("run-{secs}-{n}");
            n += 1;
        }
    }
    let spec = run::load_spec(&st.e2e_dir, &campaign_path, st.runs_root.clone(), ts.clone())
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let handle = run::spawn_job(spec);
    let run_id = format!("{id}/{ts}");
    st.jobs.lock().unwrap().insert(run_id.clone(), handle);

    if wants_json(&headers) {
        return Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "runId": run_id })))
            .into_response());
    }
    Ok(Redirect::to(&format!("/runs/{run_id}")).into_response())
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// The mirrored JSON of a run's state (live or from disk).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RunStatusDoc {
    campaign: String,
    ts: String,
    total: usize,
    finished: bool,
    cells: Vec<CellSummary>,
}

fn run_status_doc(st: &AppState, campaign: &str, ts: &str) -> Result<RunStatusDoc, HttpError> {
    let run_dir = e2e_core::result::run_dir(&st.runs_root, campaign, ts);
    // Finished runs: the disk aggregate is the source of truth.
    if let Ok(index) = e2e_core::result::read_campaign_index(&run_dir) {
        return Ok(RunStatusDoc {
            campaign: campaign.to_string(),
            ts: ts.to_string(),
            total: index.cells.len(),
            finished: true,
            cells: index.cells,
        });
    }
    // Else: a live job this process spawned.
    let jobs = st.jobs.lock().unwrap();
    let Some(handle) = jobs.get(&format!("{campaign}/{ts}")) else {
        return Err(not_found(format!("run {campaign}/{ts}")));
    };
    let status = handle.status();
    Ok(RunStatusDoc {
        campaign: campaign.to_string(),
        ts: ts.to_string(),
        total: status.total,
        finished: status.finished,
        cells: status.done,
    })
}

async fn runs_index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Disk history + any live jobs not yet aggregated.
    let mut runs: Vec<(String, String)> = Vec::new();
    if let Ok(campaigns) = std::fs::read_dir(&st.runs_root) {
        for c in campaigns.flatten() {
            if let Ok(tss) = std::fs::read_dir(c.path()) {
                for t in tss.flatten() {
                    runs.push((
                        c.file_name().to_string_lossy().to_string(),
                        t.file_name().to_string_lossy().to_string(),
                    ));
                }
            }
        }
    }
    for key in st.jobs.lock().unwrap().keys() {
        if let Some((c, t)) = key.split_once('/') {
            if !runs.iter().any(|(rc, rt)| rc == c && rt == t) {
                runs.push((c.to_string(), t.to_string()));
            }
        }
    }
    runs.sort();
    runs.reverse();

    if wants_json(&headers) {
        let docs: Vec<_> = runs
            .iter()
            .map(|(c, t)| serde_json::json!({ "campaign": c, "ts": t, "runId": format!("{c}/{t}") }))
            .collect();
        return Json(docs).into_response();
    }
    page(
        "Runs",
        html! {
            table {
                tr { th { "campaign" } th { "run" } }
                @for (c, t) in &runs {
                    tr {
                        td { (c) }
                        td { a href={ "/runs/" (c) "/" (t) } { (t) } }
                    }
                }
            }
        },
    )
    .into_response()
}

async fn run_status(
    State(st): State<Arc<AppState>>,
    UrlPath((campaign, ts)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let doc = run_status_doc(&st, &campaign, &ts)?;
    if wants_json(&headers) {
        return Ok(Json(doc).into_response());
    }
    let passed = doc.cells.iter().filter(|c| c.passed).count();
    // While running, the status block polls itself every second (htmx swaps
    // the fragment in place); once finished it renders statically.
    let body = html! {
        div id="run-status"
            hx-get={ "/runs/" (campaign) "/" (ts) }
            hx-trigger=[(!doc.finished).then_some("every 1s")]
            hx-select="#run-status" hx-swap="outerHTML" {
            p {
                @if doc.finished {
                    "Finished: " (passed) "/" (doc.total) " cell(s) passed."
                } @else {
                    (doc.cells.len()) "/" (doc.total) " cell(s) done…"
                }
            }
            table {
                tr { th { "cell" } th { "verdict" } }
                @for cell in &doc.cells {
                    tr {
                        td { a href={ "/runs/" (campaign) "/" (ts) "/cells/" (cell.dir) } { (cell.dir) } }
                        td { (verdict_markup(cell.passed, cell.error.as_deref())) }
                    }
                }
                @for _ in doc.cells.len()..doc.total {
                    tr { td .pending { "…" } td .pending { "running" } }
                }
            }
        }
    };
    Ok(page(&format!("Run {campaign}/{ts}"), body).into_response())
}

async fn cell_detail(
    State(st): State<Arc<AppState>>,
    UrlPath((campaign, ts, cell)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let cell_dir = e2e_core::result::run_dir(&st.runs_root, &campaign, &ts).join(&cell);
    // A crashed cell has error.txt and no result.json.
    if let Ok(error) = std::fs::read_to_string(cell_dir.join("error.txt")) {
        if wants_json(&headers) {
            return Ok(Json(serde_json::json!({ "cell": cell, "crashed": true, "error": error }))
                .into_response());
        }
        return Ok(page(
            &format!("Cell {cell}"),
            html! { p { (verdict_markup(false, Some(&error))) } pre { (error) } },
        )
        .into_response());
    }
    let result = e2e_core::result::read_result(&cell_dir)
        .map_err(|_| not_found(format!("cell {campaign}/{ts}/{cell}")))?;
    if wants_json(&headers) {
        // The mirrored JSON IS the persisted result.json.
        return Ok(Json(&result).into_response());
    }
    // The host-embeddable diagram: clickable messages with a fixed payload pane
    // on the right (see `seq_report::render_embed`).
    let diagram = seq_report::render_embed(&result.seq_doc);
    // Older runs can carry the same finding twice under different rule names (a
    // fixed re-fold bug); collapse exact (lane, detail) duplicates for display.
    // Gating rows (advisory == false) sort first — they are why the cell failed.
    let mut seen = std::collections::HashSet::new();
    let mut rfc: Vec<&seq_report::Anomaly> = result
        .rfc
        .iter()
        .filter(|a| seen.insert((a.lane.clone(), a.detail.clone())))
        .collect();
    rfc.sort_by_key(|a| !a.is_gating());
    let has_gating = rfc.iter().any(|a| a.is_gating());
    Ok(page(
        &format!("Cell {cell}"),
        html! {
            p { "Verdict: " (verdict_markup(result.passed, None)) }
            h2 { "Checks" }
            table {
                tr { th { "on" } th { "field" } th { "op" } th { "expected" } th { "actual" } th { "verdict" } th { "detail" } }
                @for v in &result.checks {
                    tr {
                        td { (v.on) }
                        td { (v.field) }
                        td { (format!("{:?}", v.op)) }
                        td { (v.expected.as_deref().unwrap_or("")) }
                        td { (v.actual.as_deref().unwrap_or("")) }
                        td { (verdict_markup(v.passed, None)) }
                        td { (v.detail) }
                    }
                }
            }
            @if !result.media.is_empty() {
                h2 { "Media" }
                table {
                    tr { th { "agent" } th { "heard" } th { "rms" } th { "recording" } }
                    @for m in &result.media {
                        tr {
                            td { (m.agent) }
                            td { (m.classify) }
                            td { (format!("{:.3}", m.rms)) }
                            td {
                                audio controls
                                    src={ "/runs/" (campaign) "/" (ts) "/cells/" (cell) "/files/" (m.wav) } {}
                            }
                        }
                    }
                }
            }
            @if !rfc.is_empty() {
                h2 { "RFC audit findings" }
                @if has_gating {
                    p .muted {
                        b { "Gating violations present — they FAILED this cell. " }
                        "The RFC suite runs role-aware over the recorded wire: each rule judges "
                        "only the endpoints whose declared role (UA / proxy) it governs. "
                        "Advisory rows are informational and never gate."
                    }
                } @else {
                    p .muted {
                        "Advisory / informational only — these did "
                        b { "not" }
                        " fail the test. The RFC suite runs role-aware over the recorded wire: "
                        "each rule judges only the endpoints whose declared role (UA / proxy) it "
                        "governs, so a proxy rule can no longer flag a UA lane. A gating "
                        "violation would FAIL the cell and show here in red."
                    }
                }
                table {
                    tr { th { "rule" } th { "endpoint" } th { "severity" } th { "detail" } }
                    @for a in &rfc {
                        tr {
                            td { code { (a.check) } }
                            td {
                                @if let Some(ep) = &a.endpoint { b { (ep) } " " }
                                span .muted-inline { (a.lane.as_deref().unwrap_or("")) }
                            }
                            td {
                                @if a.is_gating() { span .fail { "GATING" } }
                                @else { span .advisory { "advisory" } }
                            }
                            td { (a.detail) }
                        }
                    }
                }
            }
            h2 { "Call flow" }
            p .muted { "Click any message to inspect its full SIP payload in the pane on the right." }
            (PreEscaped(diagram))
        },
    )
    .into_response())
}

/// Serve a cell's sibling artifact (the `.wav` recordings). File names are
/// constrained to a single path segment (no traversal).
async fn cell_file(
    State(st): State<Arc<AppState>>,
    UrlPath((campaign, ts, cell, name)): UrlPath<(String, String, String, String)>,
) -> Result<Response, HttpError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "bad file name".to_string()));
    }
    let path = e2e_core::result::run_dir(&st.runs_root, &campaign, &ts).join(&cell).join(&name);
    let bytes = std::fs::read(&path).map_err(|_| not_found(format!("file {name:?}")))?;
    let content_type =
        if name.ends_with(".wav") { "audio/wav" } else { "application/octet-stream" };
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

// ---------------------------------------------------------------------------
// Test cases (view + authoring)
// ---------------------------------------------------------------------------

async fn cases_index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let cases = list_docs::<model::TestCase>(&st.e2e_dir.join("cases"));
    if wants_json(&headers) {
        let docs: Vec<_> = cases.into_iter().map(|(_, c)| c).collect();
        return Json(docs).into_response();
    }
    page(
        "Test cases",
        html! {
            table {
                tr { th { "id" } th { "compatible shapes" } th { "description" } }
                @for (_, c) in &cases {
                    tr {
                        td { a href={ "/cases/" (c.id) } { (c.id) } }
                        td { (c.compatible_shapes.join(", ")) }
                        td { (c.description.as_deref().unwrap_or("")) }
                    }
                }
            }
        },
    )
    .into_response()
}

async fn case_view(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let path = st.e2e_dir.join("cases").join(format!("{id}.json"));
    let case = model::load_test_case(&path).map_err(|_| not_found(format!("case {id:?}")))?;
    if wants_json(&headers) {
        return Ok(Json(case).into_response());
    }
    let text = std::fs::read_to_string(&path).map_err(internal)?;
    Ok(page(
        &format!("Case {id}"),
        html! {
            p { (case.description.as_deref().unwrap_or("")) }
            // Authoring: the textarea body POSTs back as raw JSON; the server
            // re-validates against the compiled registries before writing.
            form onsubmit=(PreEscaped(SAVE_JS)) {
                textarea id="case-json" { (text) }
                p { button type="submit" { "Validate & save" } " " span id="save-result" {} }
            }
        },
    )
    .into_response())
}

/// Posts the textarea as a JSON body and shows the validation outcome inline.
const SAVE_JS: &str = "event.preventDefault();\
 fetch(location.pathname, {method:'POST', headers:{'content-type':'application/json'},\
 body:document.getElementById('case-json').value})\
 .then(r=>r.text().then(t=>{document.getElementById('save-result').textContent=(r.ok?'saved':('rejected: '+t))}));";

async fn case_save(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Result<Response, HttpError> {
    let case: model::TestCase = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("not a test case: {e}")))?;
    if case.id != id {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("case id {:?} must match the path id {id:?}", case.id),
        ));
    }
    let check_sets =
        model::load_check_sets(&st.e2e_dir.join("checksets")).map_err(internal)?;
    if let Err(e @ ModelError::Invalid(_)) =
        model::validate_case(&case, &e2e_core::shapes::registry(), &check_sets)
    {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, e.to_string()));
    }
    let dir = st.e2e_dir.join("cases");
    std::fs::create_dir_all(&dir).map_err(internal)?;
    let pretty = serde_json::to_string_pretty(&case).map_err(internal)?;
    std::fs::write(dir.join(format!("{id}.json")), pretty + "\n").map_err(internal)?;
    Ok((StatusCode::OK, Json(case)).into_response())
}
