//! The **Run Job executor** (ADR-0018 Phase G): expand a Campaign into cells
//! {Test case × compatible Callflow shape × Infra shape}, run them with a
//! per-`InfraKind` concurrency cap (fake fans out wide; real cells share one
//! external cluster), persist each cell as it finishes, and write the
//! `campaign.json` aggregate.
//!
//! Threading model: `Harness` (and therefore `InfraShape`/`CallflowShape`) is
//! `!Send`, so every cell runs on its **own OS thread with its own
//! current-thread tokio runtime** — `start_paused(true)` for a fake cell (the
//! library analogue of `#[tokio::test(start_paused = true)]`), a normal
//! runtime for a real one. A cell panic (a harness assertion or the RFC hard
//! gate at `finish()`) is caught at the thread join and recorded as a crashed
//! cell (`error.txt`, `passed: false`) — it never kills the job.
//!
//! Two drivers over one core: [`run_blocking`] (the CLI) and [`spawn_job`]
//! (the web layer polls [`JobHandle::status`] as summaries land).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::infra::{self, EndpointConfig, InfraKind};
use crate::model::{Campaign, CheckSet, ModelError, TestCase, validate_case};
use crate::result::{self, CampaignIndex, CellId, CellSummary, RunResult};
use crate::{checks, shapes};

/// Everything one campaign run needs, fully resolved (no file I/O during the
/// run except result persistence). Built by hand or via
/// [`load_spec`](crate::run::load_spec).
#[derive(Debug, Clone)]
pub struct CampaignSpec {
    pub campaign: Campaign,
    /// The campaign's Test cases, keyed by id.
    pub cases: BTreeMap<String, TestCase>,
    pub check_sets: BTreeMap<String, CheckSet>,
    /// One Endpoint config per Infra-shape id the campaign runs over.
    pub endpoint_configs: BTreeMap<String, EndpointConfig>,
    /// Parent of `<campaign>/<ts>/` (usually the repo's `e2e/runs`).
    pub runs_root: PathBuf,
    /// The `<ts>` path segment — caller-supplied so the core stays clock-free.
    pub ts: String,
}

/// One expanded cell, self-contained and `Send` (ids, not trait objects — the
/// cell thread resolves them via the registries).
#[derive(Debug, Clone)]
struct CellSpec {
    cell: CellId,
    case: TestCase,
    check_sets: BTreeMap<String, CheckSet>,
    cfg: EndpointConfig,
    kind: InfraKind,
    run_dir: PathBuf,
}

/// A finished campaign run.
#[derive(Debug, Clone)]
pub struct CampaignResult {
    pub index: CampaignIndex,
    pub run_dir: PathBuf,
}

impl CampaignResult {
    pub fn passed(&self) -> bool {
        self.index.passed()
    }
}

/// Validate the spec and expand the {case × compatible shape × infra} matrix.
/// Every problem is reported (unknown ids, incompatible cases, missing
/// endpoint configs) before anything runs.
fn expand(spec: &CampaignSpec) -> Result<Vec<CellSpec>, ModelError> {
    let shapes = shapes::registry();
    let mut problems = Vec::new();
    let mut cells = Vec::new();
    let run_dir = result::run_dir(&spec.runs_root, &spec.campaign.id, &spec.ts);

    // Resolve every infra the campaign names, with its endpoint config.
    let mut infras: Vec<(String, InfraKind, EndpointConfig)> = Vec::new();
    for infra_id in &spec.campaign.infra_shapes {
        let Some(shape) = infra::by_id(infra_id) else {
            problems.push(format!(
                "campaign {:?}: unknown Infra shape {infra_id:?} (known: {:?})",
                spec.campaign.id,
                infra::known_ids()
            ));
            continue;
        };
        match spec.endpoint_configs.get(infra_id) {
            Some(cfg) => infras.push((infra_id.clone(), shape.kind(), cfg.clone())),
            None => problems.push(format!(
                "campaign {:?}: no endpoint config supplied for Infra shape {infra_id:?}",
                spec.campaign.id
            )),
        }
    }

    for case_id in &spec.campaign.cases {
        let Some(case) = spec.cases.get(case_id) else {
            problems.push(format!(
                "campaign {:?}: unknown Test case {case_id:?}",
                spec.campaign.id
            ));
            continue;
        };
        if let Err(ModelError::Invalid(p)) = validate_case(case, &shapes, &spec.check_sets) {
            problems.extend(p);
            continue;
        }
        for shape_id in &case.compatible_shapes {
            for (infra_id, kind, cfg) in &infras {
                cells.push(CellSpec {
                    cell: CellId {
                        case: case_id.clone(),
                        shape: shape_id.clone(),
                        infra: infra_id.clone(),
                    },
                    case: case.clone(),
                    check_sets: spec.check_sets.clone(),
                    cfg: cfg.clone(),
                    kind: *kind,
                    run_dir: run_dir.clone(),
                });
            }
        }
    }

    if problems.is_empty() { Ok(cells) } else { Err(ModelError::Invalid(problems)) }
}

/// Run one cell to a [`CellSummary`], persisting `result.json` (or `error.txt`
/// on a crash). Runs on the caller's thread — the executor gives each call its
/// own thread; the panic boundary is that thread's join.
fn run_cell(spec: &CellSpec) -> CellSummary {
    let dir_name = spec.cell.dir_name();
    let runtime = {
        let mut b = tokio::runtime::Builder::new_current_thread();
        b.enable_all();
        if spec.kind == InfraKind::Fake {
            b.start_paused(true);
        }
        b.build().expect("build cell runtime")
    };

    let result: RunResult = runtime.block_on(async {
        let infra = infra::by_id(&spec.cell.infra).expect("infra id validated at expand");
        let shapes = shapes::registry();
        let shape = shapes.get(spec.cell.shape.as_str()).expect("shape id validated at expand");

        let mut rt = infra.build(&dir_name, &spec.cfg).await;
        let lb_vip = rt.lb_vip;
        shape.run(&mut rt, &spec.case.input.core).await;
        let report = rt.finish().await;

        let verdicts = checks::evaluate_case(
            &spec.case,
            &spec.check_sets,
            &report,
            &checks::Bindings { input: &spec.case.input, lb_vip },
        );
        RunResult::from_run(spec.cell.clone(), &report, verdicts)
    });

    result::write_result(&spec.run_dir, &result).expect("persist result.json");
    CellSummary {
        cell: spec.cell.clone(),
        passed: result.passed,
        dir: dir_name,
        error: None,
    }
}

/// The crash fallback: record the panic as a failed cell with an `error.txt`.
fn crashed_summary(spec: &CellSpec, panic_msg: String) -> CellSummary {
    let dir_name = spec.cell.dir_name();
    let dir = spec.run_dir.join(&dir_name);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("error.txt"), format!("{panic_msg}\n"));
    CellSummary {
        cell: spec.cell.clone(),
        passed: false,
        dir: dir_name,
        error: Some(panic_msg),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "cell panicked (non-string payload)".to_string()
    }
}

/// Live progress of a running job. `done` grows as cells finish (any order
/// within a kind pool); `finished` flips once the aggregate is written.
#[derive(Debug, Clone, Default)]
pub struct JobStatus {
    pub total: usize,
    pub done: Vec<CellSummary>,
    pub finished: bool,
}

/// Run a whole campaign on the calling thread (cells still get their own
/// threads), returning once `campaign.json` is written.
pub fn run_blocking(spec: &CampaignSpec) -> Result<CampaignResult, ModelError> {
    execute(spec, &Arc::new(Mutex::new(JobStatus::default())))
}

/// A spawned campaign run, for the web layer: poll [`status`](Self::status)
/// while it runs, [`join`](Self::join) for the final aggregate.
pub struct JobHandle {
    status: Arc<Mutex<JobStatus>>,
    thread: std::thread::JoinHandle<Result<CampaignResult, ModelError>>,
}

impl JobHandle {
    pub fn status(&self) -> JobStatus {
        self.status.lock().unwrap().clone()
    }
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
    pub fn join(self) -> Result<CampaignResult, ModelError> {
        self.thread.join().expect("job driver thread never panics")
    }
}

/// Run a campaign on a background thread; the same core as [`run_blocking`].
pub fn spawn_job(spec: CampaignSpec) -> JobHandle {
    let status = Arc::new(Mutex::new(JobStatus::default()));
    let st = status.clone();
    let thread = std::thread::spawn(move || execute(&spec, &st));
    JobHandle { status, thread }
}

/// The shared driver: expand, run each kind's cells under its concurrency cap
/// (both pools in parallel), persist as cells finish, aggregate at the end.
fn execute(
    spec: &CampaignSpec,
    status: &Arc<Mutex<JobStatus>>,
) -> Result<CampaignResult, ModelError> {
    let cells = expand(spec)?;
    let run_dir = result::run_dir(&spec.runs_root, &spec.campaign.id, &spec.ts);
    status.lock().unwrap().total = cells.len();

    let (fake, real): (Vec<_>, Vec<_>) = cells.into_iter().partition(|c| c.kind == InfraKind::Fake);
    let (tx, rx) = mpsc::channel::<CellSummary>();
    let mut expected = 0usize;

    std::thread::scope(|scope| {
        for (pool, cap) in [
            (fake, spec.campaign.concurrency.fake.max(1)),
            (real, spec.campaign.concurrency.real.max(1)),
        ] {
            expected += pool.len();
            let queue = Arc::new(Mutex::new(pool));
            for _ in 0..cap.min(queue.lock().unwrap().len()) {
                let queue = queue.clone();
                let tx = tx.clone();
                scope.spawn(move || {
                    loop {
                        let Some(cell) = queue.lock().unwrap().pop() else { break };
                        // One inner thread per cell = the panic boundary: a
                        // crashed cell is recorded, the worker moves on.
                        let summary = std::thread::scope(|inner| {
                            inner
                                .spawn(|| run_cell(&cell))
                                .join()
                                .unwrap_or_else(|p| crashed_summary(&cell, panic_message(p)))
                        });
                        if tx.send(summary).is_err() {
                            break;
                        }
                    }
                });
            }
        }
        drop(tx);

        // Collect on the driver thread, feeding live status as cells land.
        let mut summaries = Vec::with_capacity(expected);
        while let Ok(summary) = rx.recv() {
            status.lock().unwrap().done.push(summary.clone());
            summaries.push(summary);
        }

        // Deterministic aggregate order regardless of completion order.
        summaries.sort_by(|a, b| a.dir.cmp(&b.dir));
        let index = CampaignIndex {
            campaign: spec.campaign.id.clone(),
            ts: spec.ts.clone(),
            cells: summaries,
        };
        result::write_campaign_index(&run_dir, &index).map_err(|e| {
            ModelError::Io { path: run_dir.clone(), source: e }
        })?;
        status.lock().unwrap().finished = true;
        Ok(CampaignResult { index, run_dir: run_dir.clone() })
    })
}

/// Load a fully-resolved [`CampaignSpec`] from the conventional `e2e/` layout:
/// the campaign file itself, `cases/<id>.json` for each referenced case,
/// every `checksets/*.json`, and `infra/<infra-id>.json` per Infra shape.
/// `ts` is caller-supplied (e.g. unix seconds).
pub fn load_spec(
    e2e_dir: &std::path::Path,
    campaign_path: &std::path::Path,
    runs_root: PathBuf,
    ts: String,
) -> Result<CampaignSpec, ModelError> {
    let campaign = crate::model::load_campaign(campaign_path)?;
    let mut cases = BTreeMap::new();
    for case_id in &campaign.cases {
        let case = crate::model::load_test_case(&e2e_dir.join("cases").join(format!("{case_id}.json")))?;
        if case.id != *case_id {
            return Err(ModelError::Invalid(vec![format!(
                "case file cases/{case_id}.json declares id {:?} (must match its file name)",
                case.id
            )]));
        }
        cases.insert(case_id.clone(), case);
    }
    let check_sets = crate::model::load_check_sets(&e2e_dir.join("checksets"))?;
    let mut endpoint_configs = BTreeMap::new();
    for infra_id in &campaign.infra_shapes {
        let cfg = crate::model::load_endpoint_config(
            &e2e_dir.join("infra").join(format!("{infra_id}.json")),
        )?;
        endpoint_configs.insert(infra_id.clone(), cfg);
    }
    Ok(CampaignSpec { campaign, cases, check_sets, endpoint_configs, runs_root, ts })
}
