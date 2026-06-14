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

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use e2e_core::model::{self, ModelError};
use e2e_core::result::CellSummary;
use e2e_core::run::{self, JobHandle};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use tower_http::services::ServeDir;

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
        .route("/campaigns/{id}", get(campaign_detail).post(campaign_save))
        .route("/campaigns/{id}/runs", axum::routing::post(campaign_launch))
        .route("/runs", get(runs_index))
        .route("/runs/{campaign}/{ts}", get(run_status))
        .route("/runs/{campaign}/{ts}/cells/{cell}", get(cell_detail))
        .route("/runs/{campaign}/{ts}/cells/{cell}/files/{name}", get(cell_file))
        .route("/cases", get(cases_index))
        .route("/cases/{id}", get(case_view).post(case_save))
        // The registry's shapes (for the editor's shape-lens picker) and the
        // derived compatible-shapes of an in-progress buffer (the read-only panel).
        .route("/shapes", get(shapes_index))
        .route("/compat", axum::routing::post(case_compat))
        // The live model-generated JSON schema (never the on-disk copy) the
        // Monaco editor validates against — so authoring can't drift from code.
        // `?shape=<id>` narrows the test-case schema to one shape's vocabulary.
        .route("/schemas/{name}", get(schema_doc))
        // The vendored Monaco editor assets (the only client-side dependency
        // besides htmx). `CARGO_MANIFEST_DIR` keeps the path independent of cwd.
        .nest_service(
            "/static",
            ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/static")),
        )
        .with_state(state)
}

/// Serve a top-level doc schema by name (`test-case`, `campaign`, `check-set`,
/// `endpoint-config`; a trailing `.schema.json` is tolerated). `test-case` is
/// served from [`e2e_core::schema_gen`] — the live projection that narrows
/// `field`/`on` to the registry's vocabulary, optionally pinned to one shape via
/// `?shape=<id>`; the rest come straight from [`e2e_core::model::schemas`]. Both
/// are generated from code, so authoring can never drift from the model.
async fn schema_doc(
    UrlPath(name): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, HttpError> {
    let key = name.strip_suffix(".schema.json").unwrap_or(&name);
    if key == "test-case" {
        let registry = e2e_core::shapes::registry();
        let shape = q.get("shape").and_then(|id| registry.get(id));
        let schema = e2e_core::schema_gen::test_case_schema(shape.map(|b| b.as_ref()));
        return Ok(Json(schema).into_response());
    }
    let schema = model::schemas()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, s)| s)
        .ok_or_else(|| not_found(format!("schema {key:?}")))?;
    Ok(Json(schema).into_response())
}

/// The registry's shapes for the editor's shape-lens picker: id + the agents,
/// anchors and `<agent>.<anchor>` selectors each drives, and whether it exchanges
/// media. Always JSON (consumed by the editor, not browsed directly).
async fn shapes_index() -> Response {
    let registry = e2e_core::shapes::registry();
    let docs: Vec<_> = registry
        .iter()
        .map(|(id, s)| {
            serde_json::json!({
                "id": id,
                "agents": s.agents(),
                "anchors": s.anchors().iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "selectors": e2e_core::schema_gen::selectors_for(s.as_ref()),
                "media": matches!(s.media(), e2e_core::MediaMode::Exchange),
            })
        })
        .collect();
    Json(docs).into_response()
}

/// Derive the compatible shapes of an in-progress Test-case buffer (the editor's
/// read-only "this case also fits…" panel). A buffer that is not yet valid JSON
/// is a soft 422 — the editor just blanks the panel until it parses.
async fn case_compat(State(st): State<Arc<AppState>>, body: String) -> Result<Response, HttpError> {
    let case: model::TestCase = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("not a test case: {e}")))?;
    let check_sets = model::load_check_sets(&st.e2e_dir.join("checksets")).map_err(internal)?;
    let compat =
        e2e_core::schema_gen::compatible_shapes(&case, &e2e_core::shapes::registry(), &check_sets);
    Ok(Json(serde_json::json!({ "compatibleShapes": compat })).into_response())
}

/// A schema-aware Monaco JSON editor bound to `model_uri`, validating live
/// against `/schemas/{schema_name}`, seeded with `text`, with a "Validate &
/// save" button that POSTs the buffer back to the current path. Shared by the
/// Test-case and Campaign authoring pages.
///
/// When `shapes` is non-empty (the Test-case page) it also renders a **shape-lens
/// picker** — re-binding the schema to `/schemas/{schema_name}?shape=<id>` so
/// completion narrows to that shape's `<agent>.<anchor>` + field vocabulary — and
/// a read-only **compatible-shapes panel** recomputed from the buffer (debounced)
/// via `POST /compat`. `initial_shape` preselects the lens (the case's first
/// compatible shape); `None`/`""` leaves the base lens until the author picks.
fn json_editor(
    model_uri: &str,
    schema_name: &str,
    text: &str,
    shapes: &[String],
    initial_shape: Option<&str>,
) -> Markup {
    // JSON-encode the seed so any quotes/newlines embed safely in the script.
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let boot = format!(
        r#"require.config({{ paths: {{ vs: '/static/vs' }} }});
self.MonacoEnvironment = {{
  getWorkerUrl: function() {{
    return 'data:text/javascript;charset=utf-8,' + encodeURIComponent(
      "self.MonacoEnvironment={{baseUrl:'" + location.origin + "/static/'}};" +
      "importScripts('" + location.origin + "/static/vs/base/worker/workerMain.js');");
  }}
}};
require(['vs/editor/editor.main'], function() {{
  var uri = monaco.Uri.parse('{model_uri}');
  // The committed files carry `"$schema": "../schemas/{schema_name}.schema.json"`
  // (for on-disk editors). Relative to the model URI that resolves to the URI
  // below — register the live schema under THAT exact id so the in-document
  // $schema binds to it instead of triggering a (disabled) network fetch.
  var schemaUri = uri.with({{ path: '/schemas/{schema_name}.schema.json' }}).toString();
  var model = monaco.editor.createModel({text_json}, 'json', uri);

  // (Re)bind the live schema for the current shape lens (empty = base lens).
  function bindSchema(shape) {{
    var url = '/schemas/{schema_name}' + (shape ? ('?shape=' + encodeURIComponent(shape)) : '');
    return fetch(url).then(function(r){{ return r.json(); }}).then(function(schema){{
      monaco.languages.json.jsonDefaults.setDiagnosticsOptions({{
        validate: true, allowComments: false, enableSchemaRequest: false,
        schemas: [{{ uri: schemaUri, fileMatch: [uri.toString()], schema: schema }}]
      }});
    }});
  }}

  // The read-only "compatible shapes" panel, recomputed from the buffer.
  var compatEl = document.getElementById('compat-panel');
  function refreshCompat() {{
    if (!compatEl) return;
    fetch('/compat', {{ method: 'POST',
      headers: {{ 'content-type': 'application/json' }}, body: window.__editor.getValue() }})
      .then(function(r){{ return r.ok ? r.json() : null; }})
      .then(function(d){{
        if (!d) {{ compatEl.innerHTML = '<i>buffer not yet valid — fix errors to compute</i>'; return; }}
        var list = d.compatibleShapes.length
          ? d.compatibleShapes.map(function(s){{ return '<code>' + s + '</code>'; }}).join(' ')
          : '<i>none — no shape publishes these anchors</i>';
        compatEl.innerHTML = '<b>Compatible shapes</b> (computed from your checks): ' + list;
      }})
      .catch(function(){{}});
  }}

  window.__editor = monaco.editor.create(document.getElementById('editor'), {{
    model: model, automaticLayout: true, minimap: {{ enabled: false }}, scrollBeyondLastLine: false
  }});

  var picker = document.getElementById('shape-picker');
  if (picker) {{
    picker.addEventListener('change', function() {{ bindSchema(picker.value); }});
  }}
  var debounce;
  model.onDidChangeContent(function() {{ clearTimeout(debounce); debounce = setTimeout(refreshCompat, 400); }});

  bindSchema(picker ? picker.value : '').then(refreshCompat);

  document.getElementById('save-btn').onclick = function() {{
    fetch(location.pathname, {{ method: 'POST',
      headers: {{ 'content-type': 'application/json' }}, body: window.__editor.getValue() }})
      .then(function(r){{ return r.text().then(function(t){{
        document.getElementById('save-result').textContent = r.ok ? 'saved' : ('rejected: ' + t);
      }}); }});
  }};
}});"#
    );
    html! {
        script src="/static/vs/loader.js" {}
        @if !shapes.is_empty() {
            p {
                label { "Shape lens: "
                    select id="shape-picker" {
                        option value="" { "— pick a shape —" }
                        @for s in shapes {
                            @if Some(s.as_str()) == initial_shape {
                                option value=(s) selected { (s) }
                            } @else {
                                option value=(s) { (s) }
                            }
                        }
                    }
                }
                " " span .muted-inline { "narrows <agent>.<anchor> + field suggestions to the chosen shape" }
            }
            div id="compat-panel" .muted {}
        }
        p {
            button id="save-btn" type="button" { "Validate & save" }
            " " span id="save-result" {}
        }
        div id="editor" {}
        script { (PreEscaped(boot)) }
    }
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
#editor { width: 100%; height: 28rem; border: 1px solid #d1d5db; }
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
    let path = st.e2e_dir.join("campaigns").join(format!("{id}.json"));
    let campaign = load_campaign_by_id(&st, &id)?;
    if wants_json(&headers) {
        return Ok(Json(campaign).into_response());
    }
    let text = std::fs::read_to_string(&path).map_err(internal)?;
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
            // The layout knobs, made explicit (also present in the JSON below).
            h2 { "Concurrency (layout)" }
            p .muted {
                "fake cells: " b { (campaign.concurrency.fake) }
                " · real cells: " b { (campaign.concurrency.real) }
                " — how many cells of each Infra kind run at once."
            }
            form method="post" action={ "/campaigns/" (id) "/runs" } {
                button type="submit" { "Launch" }
            }
            // The full campaign JSON, schema-aware + editable (validated server-
            // side: referenced cases must exist and infra shapes must be known).
            h2 { "Edit" }
            (json_editor("inmemory://model/campaign.json", "campaign", &text, &[], None))
        },
    )
    .into_response())
}

/// Author/overwrite a campaign (mirrors [`case_save`]). The `deny_unknown_fields`
/// derive rejects typos; beyond that we referentially check that every case file
/// exists and every infra-shape id is known, so a launch can't fail late on a
/// dangling reference.
async fn campaign_save(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Result<Response, HttpError> {
    let campaign: model::Campaign = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("not a campaign: {e}")))?;
    if campaign.id != id {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("campaign id {:?} must match the path id {id:?}", campaign.id),
        ));
    }
    let mut problems = Vec::new();
    let cases_dir = st.e2e_dir.join("cases");
    for c in &campaign.cases {
        if !cases_dir.join(format!("{c}.json")).is_file() {
            problems.push(format!("unknown case {c:?}"));
        }
    }
    for shape in &campaign.infra_shapes {
        if e2e_core::infra::by_id(shape).is_none() {
            problems.push(format!(
                "unknown infra shape {shape:?} (known: {:?})",
                e2e_core::infra::known_ids()
            ));
        }
    }
    if !problems.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, problems.join("; ")));
    }
    let dir = st.e2e_dir.join("campaigns");
    std::fs::create_dir_all(&dir).map_err(internal)?;
    let pretty = serde_json::to_string_pretty(&campaign).map_err(internal)?;
    std::fs::write(dir.join(format!("{id}.json")), pretty + "\n").map_err(internal)?;
    Ok((StatusCode::OK, Json(campaign)).into_response())
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

/// Recursively collect cases under `root`, returning `(rel_dir, TestCase)` —
/// `rel_dir` is the folder path under `cases/` ("" at the root). Cases may be
/// organised into subdirectories; they are still referenced everywhere by `id`.
fn list_cases_grouped(root: &Path) -> Vec<(String, model::TestCase)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, model::TestCase)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().is_some_and(|e| e == "json") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(doc) = serde_json::from_str::<model::TestCase>(&text) {
                        let rel_dir = path
                            .parent()
                            .and_then(|p| p.strip_prefix(base).ok())
                            .map(|p| p.to_string_lossy().replace('\\', "/"))
                            .unwrap_or_default();
                        out.push((rel_dir, doc));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| (a.0.as_str(), a.1.id.as_str()).cmp(&(b.0.as_str(), b.1.id.as_str())));
    out
}

async fn cases_index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let cases = list_cases_grouped(&st.e2e_dir.join("cases"));
    if wants_json(&headers) {
        let docs: Vec<_> = cases.iter().map(|(_, c)| c).collect();
        return Json(docs).into_response();
    }
    // Group by folder for display; the BTreeMap keeps the sections ordered.
    let mut by_dir: std::collections::BTreeMap<&str, Vec<&model::TestCase>> =
        std::collections::BTreeMap::new();
    for (dir, c) in &cases {
        by_dir.entry(dir.as_str()).or_default().push(c);
    }
    page(
        "Test cases",
        html! {
            p .muted {
                "Organise cases into folders under " code { "e2e/cases/" }
                " — they are grouped by folder here and still referenced by id."
            }
            @for (dir, group) in &by_dir {
                h2 { "cases/" @if !dir.is_empty() { (dir) "/" } }
                table {
                    tr { th { "id" } th { "compatible shapes" } th { "description" } }
                    @for c in group {
                        tr {
                            td { a href={ "/cases/" (c.id) } { (c.id) } }
                            td { (c.compatible_shapes.join(", ")) }
                            td { (c.description.as_deref().unwrap_or("")) }
                        }
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
    // Resolve by id, searching subdirectories (cases may be organised in folders).
    let path = run::find_case_file(&st.e2e_dir, &id).ok_or_else(|| not_found(format!("case {id:?}")))?;
    let case = model::load_test_case(&path).map_err(|_| not_found(format!("case {id:?}")))?;
    if wants_json(&headers) {
        return Ok(Json(case).into_response());
    }
    let text = std::fs::read_to_string(&path).map_err(internal)?;
    // The shape-lens picker is driven by the live registry; preselect the case's
    // first declared compatible shape (a brand-new case has none → base lens).
    let shape_ids: Vec<String> = e2e_core::shapes::registry().into_keys().collect();
    let initial_shape = case.compatible_shapes.first().map(String::as_str);
    Ok(page(
        &format!("Case {id}"),
        html! {
            p { (case.description.as_deref().unwrap_or("")) }
            // Authoring: the schema-aware Monaco editor POSTs its buffer back as
            // raw JSON; the server re-validates against the compiled registries
            // before writing (the editor's live schema check is advisory).
            (json_editor("inmemory://model/case.json", "test-case", &text, &shape_ids, initial_shape))
        },
    )
    .into_response())
}

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
    // Preserve the case's folder when it already exists; a brand-new case lands
    // at the root of cases/.
    let path = run::find_case_file(&st.e2e_dir, &id)
        .unwrap_or_else(|| st.e2e_dir.join("cases").join(format!("{id}.json")));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(internal)?;
    }
    let pretty = serde_json::to_string_pretty(&case).map_err(internal)?;
    std::fs::write(&path, pretty + "\n").map_err(internal)?;
    Ok((StatusCode::OK, Json(case)).into_response())
}
