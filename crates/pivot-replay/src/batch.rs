//! `replay batch <batch-spec.json>`: ONE process per batch on a hosted lane. It
//! stands what the product registers for the batch ([`Registered`]): the
//! backend double the SUT asks its decisions of ([`BackendDouble`]) and the
//! gates its end answers. It runs every cell of the batch as a task of its
//! own, each on its own thread so a cell's panic is its own.
//!
//! The process raises its soft descriptor limit to the hard one before
//! anything stands (`crate::nofile`).
//!
//! The protocol is lines of JSON on stdout: `{"ready":…}` once the backend
//! double is served, stating the descriptor limit; one `{"cell":…}` per cell
//! as it ends, carrying the lines that cell wrote on stderr; `{"end":…}` once
//! stdin closed, every cell ended and the batch's own gates ran ([`End`]),
//! with what the backend double served and how fast (`served`). Each stdin
//! line is the path of one cell's run-spec. A SIGTERM or SIGINT ends the
//! process at once.
//!
//! Every wait of the batch's end is bounded, the bound beside it; a wait
//! that lasts is stated on stderr every 30 s (`waiting`), the harness's stdin
//! and the cells still running included.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pivot_interpreter::DialogIdentity;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::verifications::Gates;
use crate::{exit_code, nofile, Failure, RunSummary};

pub use cell::{guarded, line_of, run_cell};
pub use end::{End, Ended, BACKEND_SETTLE};
pub use lines::{cell_line, end_line, ready_line, CellLine};
pub use placed::Placed;
pub use served::{Quantiles, Served};
pub use waiting::Waiting;
pub use wiring::{Wiring, MISWIRED};

/// The service a product's SUT asks its decisions of, as the batch serves or
/// relays it: where it is reached, and the exchanges it recorded, each keyed
/// by the call it served.
pub trait BackendDouble: Send + Sync + 'static {
    /// One recorded exchange.
    type Row;

    /// Where the SUT reaches it.
    fn addr(&self) -> SocketAddr;

    /// Every exchange recorded so far.
    fn rows(&self) -> Vec<Self::Row>;

    /// The exchanges recorded so far for the calls `keys` name, in arrival
    /// order.
    fn rows_of(&self, keys: &BTreeSet<String>) -> Vec<Self::Row>;

    /// The key its exchanges for the call placed under `call` carry.
    fn key_of(&self, call: &DialogIdentity) -> String;

    /// How long it took from the request's receipt to its answer, in µs,
    /// where the row states it.
    fn process_us(row: &Self::Row) -> Option<u64>;

    /// Whether it relays exchanges the batch's end waits out ([`Self::settled`]).
    fn relays(&self) -> bool {
        false
    }

    /// Whether the last exchange it carries ended within `within`.
    fn settled(&self, within: Duration) -> impl Future<Output = bool> {
        let _ = within;
        async { true }
    }
}

/// What a product registers for a batch on its hosted lanes (ADR-0060
/// appendix §13): stood once per batch from the batch-spec, it runs each
/// cell and answers the batch's end gates. The host derefs to it.
pub trait Registered: Sized + Send + Sync + 'static {
    /// The batch-spec the harness writes.
    type Spec: DeserializeOwned;
    /// The backend double the batch serves its SUT.
    type Backend: BackendDouble;
    /// What the `end` line states beside the gates, flattened into it.
    type Report: Serialize;

    /// The lane every cell of the batch names.
    fn lane(spec: &Self::Spec) -> &str;

    /// The gate lines a cell's verdict splits into.
    fn gates() -> &'static Gates;

    /// Stands the registered parts and the backend double for `spec`, on
    /// the current runtime.
    fn stand(
        spec: &Self::Spec,
        seed: u64,
    ) -> impl Future<Output = Result<(Self, Self::Backend), Failure>>;

    /// Once the `ready` line is out, before the first cell: a baseline the
    /// end compares against, read. Answers what the run output states of it.
    fn before(host: &Host<Self>) -> impl Future<Output = Option<String>> {
        let _ = host;
        async { None }
    }

    /// Runs the cell whose run-spec is at `path`, the batch's `ordinal`-th,
    /// on the calling thread.
    fn cell(host: &Host<Self>, path: &Path, ordinal: u64) -> Result<RunSummary, Failure>;

    /// The batch's end gates over the cells of `placed`: each gate's answer,
    /// its charges written to the cells' verdicts, how the end stands on
    /// each line it answers for every cell.
    fn end(host: &Host<Self>, placed: &[Placed]) -> impl Future<Output = Ended<Self::Report>>;

    /// Each cell that recorded, with the epoch its record is rebased on,
    /// once the end's gates ran and the backend double's last exchange is
    /// recorded.
    fn recorded(host: &Host<Self>, cells: Vec<(PathBuf, i64)>) {
        let _ = (host, cells);
    }
}

/// What a batch holds for its cells: the product's registered parts (which
/// it derefs to), the backend double, the cells placed and recorded.
pub struct Host<P: Registered> {
    /// The lane every cell names.
    pub lane: String,
    /// What each cell's ids derive from, with its ordinal.
    pub seed: u64,
    pub backend: P::Backend,
    registered: P,
    /// Every cell that recorded, and the epoch its record is rebased on.
    finished: Mutex<Vec<(PathBuf, i64)>>,
    /// Every cell's bundle with the calls it placed.
    minted: Mutex<Vec<Placed>>,
    /// What the batch waits for, stated while a wait lasts.
    pub waiting: Waiting,
    /// Whether the SUT calls the backend double the batch serves.
    pub wiring: Wiring,
}

impl<P: Registered> Deref for Host<P> {
    type Target = P;

    fn deref(&self) -> &P {
        &self.registered
    }
}

impl<P: Registered> DerefMut for Host<P> {
    fn deref_mut(&mut self) -> &mut P {
        &mut self.registered
    }
}

impl<P: Registered> Host<P> {
    /// Stands the registered parts and the backend double, on the current
    /// runtime.
    pub async fn stand(spec: &P::Spec, seed: u64) -> Result<Host<P>, Failure> {
        let (registered, backend) = P::stand(spec, seed).await?;
        Ok(Host {
            lane: P::lane(spec).to_string(),
            seed,
            backend,
            registered,
            finished: Mutex::new(Vec::new()),
            minted: Mutex::new(Vec::new()),
            waiting: Waiting::start(waiting::EVERY, |line| eprintln!("{line}")),
            wiring: Wiring::default(),
        })
    }

    /// The cell whose bundle is `out_dir` places its calls under `seeds`.
    pub fn minted(&self, out_dir: &Path, seeds: &[DialogIdentity]) {
        let mut minted = self.minted.lock().unwrap_or_else(|p| p.into_inner());
        minted.push(Placed { bundle: out_dir.to_path_buf(), seeds: seeds.to_vec() });
    }

    /// Every cell placed so far, in the order each placed its calls.
    pub fn placed(&self) -> Vec<Placed> {
        self.minted.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// A cell recorded its bundle at `out_dir`, its record rebased on
    /// `epoch_ms`: [`Registered::recorded`] takes it again at batch end.
    pub fn finished(&self, out_dir: &Path, epoch_ms: i64) {
        let mut finished = self.finished.lock().unwrap_or_else(|p| p.into_inner());
        finished.push((out_dir.to_path_buf(), epoch_ms));
    }

    /// The keys the backend double's exchanges for `placed`'s calls carry.
    pub fn keys_of(&self, placed: &Placed) -> BTreeSet<String> {
        placed.seeds.iter().map(|seed| self.backend.key_of(seed)).collect()
    }
}

/// The batch's scratch directory when the harness names none.
pub fn default_work_dir(seed: u64) -> PathBuf {
    std::env::temp_dir().join(format!("pivot-replay-batch-{seed:x}"))
}

/// A seed no earlier batch on this box took: the clock.
pub fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        .max(1)
}

/// The host's runtime: the backend double and the batch's gates run on it.
pub fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| Failure::Environment(format!("build runtime: {e}")))
}

/// Where the protocol lines go.
pub type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// One JSON line, flushed.
fn emit(out: &Out, value: &serde_json::Value) {
    let mut out = out.lock().unwrap_or_else(|p| p.into_inner());
    let _ = writeln!(out, "{value}").and_then(|()| out.flush());
}

/// `replay batch <batch-spec.json>`: the process's exit code — 0 once the
/// batch ended (its gates are in the end line), 2 a spec refused, 3 a host
/// that could not stand.
pub fn run<P: Registered>(spec_path: &Path) -> i32 {
    let served = read_spec::<P>(spec_path).and_then(|spec| {
        let out: Out = Arc::new(Mutex::new(Box::new(std::io::stdout())));
        let nofile = nofile::raise().unwrap_or_else(|refused| refused);
        serve::<P>(&spec, std::io::stdin().lock(), out, Process { nofile, signals: true })
    });
    match served {
        Ok(()) => 0,
        Err(failure) => {
            eprintln!("replay batch: {failure}");
            exit_code(&Err(failure))
        }
    }
}

fn read_spec<P: Registered>(spec_path: &Path) -> Result<P::Spec, Failure> {
    let text = std::fs::read_to_string(spec_path)
        .map_err(|e| Failure::Environment(format!("read {}: {e}", spec_path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| Failure::Refused(format!("{} is not a batch-spec: {e}", spec_path.display())))
}

/// What the batch states of the process it runs in.
pub struct Process {
    /// The descriptor limit, as `nofile::raise` stated it.
    pub nofile: String,
    /// Whether SIGTERM / SIGINT end the process.
    pub signals: bool,
}

/// The batch: the host stood, one cell per line of `cells`, the protocol
/// lines on `out`.
pub fn serve<P: Registered>(
    spec: &P::Spec,
    cells: impl BufRead,
    out: Out,
    process: Process,
) -> Result<(), Failure> {
    let rt = runtime()?;
    let seed = fresh_seed();
    let host = Arc::new(rt.block_on(Host::<P>::stand(spec, seed))?);
    if process.signals {
        rt.spawn(on_signal());
    }
    let ready = ready_line(&host.backend.addr().to_string(), &process.nofile, seed);
    emit(&out, &ready);
    if let Some(note) = rt.block_on(P::before(&host)) {
        eprintln!("replay batch: {note}");
    }

    let received = Arc::new(AtomicUsize::new(0));
    let live: Arc<Mutex<BTreeSet<String>>> = Arc::default();
    {
        let (received, live) = (received.clone(), live.clone());
        host.waiting.on(move || {
            format!(
                "the harness to close stdin: {} cell(s) received, {}",
                received.load(Ordering::SeqCst),
                running_of(&live)
            )
        });
    }
    let mut running = Vec::new();
    for (index, line) in cells.lines().enumerate() {
        let Ok(line) = line else { break };
        let path = line.trim().to_string();
        if path.is_empty() {
            continue;
        }
        received.fetch_add(1, Ordering::SeqCst);
        live.lock().unwrap_or_else(|p| p.into_inner()).insert(path.clone());
        let (cell_host, cell_out, cell_live) = (host.clone(), out.clone(), live.clone());
        let ordinal = index as u64 + 1;
        let spawned = std::thread::Builder::new()
            .name(format!("cell-{ordinal}"))
            .stack_size(8 << 20)
            .spawn(move || {
                let line = run_cell(&cell_host, &path, ordinal);
                emit(&cell_out, &cell_line(&line));
                cell_live.lock().unwrap_or_else(|p| p.into_inner()).remove(&path);
            });
        match spawned {
            Ok(handle) => running.push(handle),
            Err(e) => {
                live.lock().unwrap_or_else(|p| p.into_inner()).remove(line.trim());
                emit(
                    &out,
                    &cell_line(&CellLine {
                        spec: line.trim().to_string(),
                        exit: 3,
                        summary: format!("environment: spawn the cell's thread: {e}"),
                        stderr: String::new(),
                    }),
                )
            }
        }
    }
    {
        let live = live.clone();
        host.waiting.on(move || format!("the cells to end: {}", running_of(&live)));
    }
    for handle in running {
        let _ = handle.join();
    }
    let end = rt.block_on(host.finish());
    emit(&out, &end_line(&end));
    Ok(())
}

/// How many cells of `live` still run, the first few named.
fn running_of(live: &Mutex<BTreeSet<String>>) -> String {
    const NAMED: usize = 5;
    let live = live.lock().unwrap_or_else(|p| p.into_inner());
    if live.is_empty() {
        return "none running".to_string();
    }
    let names: Vec<&str> = live
        .iter()
        .take(NAMED)
        .map(|spec| Path::new(spec).file_name().and_then(|n| n.to_str()).unwrap_or(spec))
        .collect();
    let more = live.len().saturating_sub(NAMED);
    let tail = if more == 0 { String::new() } else { format!(" and {more} more") };
    format!("{} running: {}{tail}", live.len(), names.join(", "))
}

/// Ends the process on SIGTERM or SIGINT.
pub async fn on_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) =
        (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
    else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    eprintln!("replay batch: signalled; ending with its cells in flight");
    std::process::exit(143);
}

mod cell;
mod end;
pub mod gates;
mod lines;
mod placed;
mod served;
mod waiting;
mod wiring;

#[cfg(test)]
mod tests;
