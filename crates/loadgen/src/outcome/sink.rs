//! Where outcome records go: a JSON-lines file, and optionally VictoriaLogs'
//! `/insert/jsonline`, written by one thread off the call path; plus the
//! per-stream counters of records written and calls never placed.

use std::io::{BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metric_catalogue::OpenRows;
use serde_json::Value;

use crate::catalogue::{
    CALLS_OFFERED, GENERATOR_FAILURES, NOT_PLACED, OUTCOME_RECORDS, RECORDS_DROPPED,
    RECORDS_PUSH_FAILED,
};

/// The bound on each step of a push (connect, write, read the answer).
const POST_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after the last push the next batch goes out.
const FLUSH_EVERY: Duration = Duration::from_millis(1000);

/// The most records one push carries.
const BATCH_MAX: usize = 1000;

/// Records waiting for the file writer, then for the pusher; one arriving at
/// a full queue is dropped and counted.
const QUEUE_MAX: usize = 65_536;

/// How long `close` lets the pusher deliver its backlog once the file holds
/// every record; what is left then is dropped and counted. `close` returns
/// within this plus one push attempt (three [`POST_TIMEOUT`] steps).
const CLOSE_BUDGET: Duration = Duration::from_secs(3);

/// The records of one generator run: the run, source and stream every record
/// names, the writer they go through, and the counters.
pub struct OutcomeSink {
    pub run: String,
    pub source: String,
    pub stream: String,
    /// What every Call-ID of the run carries: the run, then the call-id tag
    /// ([`Self::with_call_id_tag`]), so two generators of one run never share
    /// a Call-ID.
    pub call_id_nonce: String,
    tx: Mutex<Option<SyncSender<Value>>>,
    writers: Mutex<Vec<JoinHandle<()>>>,
    counts: Arc<Counts>,
}

/// The sink's counters, shared with its writer threads.
struct Counts {
    stream: String,
    records: OpenRows,
    not_placed: OpenRows,
    offered: OpenRows,
    generator_failures: OpenRows,
    push_failed: OpenRows,
    dropped: OpenRows,
    /// When the backlog's [`CLOSE_BUDGET`] runs out; set by `close`.
    close_deadline: Mutex<Option<Instant>>,
}

impl Counts {
    fn dropped(&self, stage: &str, n: usize) {
        if n > 0 {
            self.dropped.add(&[&self.stream, stage], n as u64);
        }
    }
}

/// A VictoriaLogs ingestion endpoint: `host:port` and the base path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogsTarget {
    pub authority: String,
    pub path: String,
}

impl LogsTarget {
    /// Parse `http://host:port[/base]`; `Err` names what does not read.
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("records URL {url:?}: only http:// is supported"))?;
        let (authority, base) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("records URL {url:?}: no host"));
        }
        let base = base.trim_end_matches('/');
        let path = format!(
            "{}/insert/jsonline?_stream_fields=run,source,stream",
            if base.is_empty() { String::new() } else { format!("/{base}") }
        );
        Ok(Self { authority: authority.to_string(), path })
    }
}

impl OutcomeSink {
    /// A sink appending to `file` (created with its directory) and, with
    /// `logs`, pushing each record to VictoriaLogs.
    pub fn new(
        run: &str,
        source: &str,
        stream: &str,
        file: &Path,
        logs: Option<LogsTarget>,
    ) -> std::io::Result<Self> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
        let counts = Arc::new(Counts {
            stream: stream.to_string(),
            records: OpenRows::new(&OUTCOME_RECORDS),
            not_placed: OpenRows::new(&NOT_PLACED),
            offered: OpenRows::new(&CALLS_OFFERED),
            generator_failures: OpenRows::new(&GENERATOR_FAILURES),
            push_failed: OpenRows::new(&RECORDS_PUSH_FAILED),
            dropped: OpenRows::new(&RECORDS_DROPPED),
            close_deadline: Mutex::new(None),
        });
        let mut writers = Vec::new();
        let push_tx = match logs {
            Some(target) => {
                let (tx, rx) = sync_channel(QUEUE_MAX);
                let c = counts.clone();
                writers.push(spawn("outcome-pusher", move || push_loop(rx, target, &c))?);
                Some(tx)
            }
            None => None,
        };
        let (tx, rx) = sync_channel(QUEUE_MAX);
        let (path, c) = (file.to_path_buf(), counts.clone());
        let file_writer = spawn("outcome-writer", move || {
            write_loop(rx, BufWriter::new(out), path, push_tx, &c)
        })?;
        writers.insert(0, file_writer);
        Ok(Self {
            run: run.to_string(),
            source: source.to_string(),
            stream: stream.to_string(),
            call_id_nonce: run.to_string(),
            tx: Mutex::new(Some(tx)),
            writers: Mutex::new(writers),
            counts,
        })
    }

    /// The sink of a generator whose Call-IDs carry `tag` after the run.
    pub fn with_call_id_tag(mut self, tag: &str) -> Self {
        self.call_id_nonce = format!("{}{tag}", self.run);
        self
    }

    /// Write one record (counted by its shape and ending); one arriving at a
    /// full queue is dropped and counted.
    pub fn write(&self, record: Value) {
        let shape = record["shape"].as_str().unwrap_or("").to_string();
        let ending = record["ending"].as_str().unwrap_or("").to_string();
        self.counts.records.add(&[&self.stream, &shape, &ending], 1);
        let queued = match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.try_send(record).is_ok(),
            None => false,
        };
        if !queued {
            self.counts.dropped("writer", 1);
        }
    }

    /// Count a call of `shape` offered: every slot of the rate, placed or not.
    pub fn offered(&self, shape: &str) {
        self.counts.offered.add(&[&self.stream, shape], 1);
    }

    /// Count a call of `shape` never placed (shed at the in-flight cap, or its
    /// key refused), for `reason`: neither an impact nor a generator failure.
    pub fn not_placed(&self, shape: &str, reason: &str) {
        self.counts.not_placed.add(&[&self.stream, shape, reason], 1);
    }

    /// Count a call of `shape` the generator itself failed (it wrote no
    /// record), for `reason`: a run with one is invalid.
    pub fn generator_failure(&self, shape: &str, reason: &str) {
        self.counts.generator_failures.add(&[&self.stream, shape, reason], 1);
    }

    /// Calls offered so far.
    pub fn offered_total(&self) -> u64 {
        self.counts.offered.sum()
    }

    /// Generator failures so far.
    pub fn generator_failures_total(&self) -> u64 {
        self.counts.generator_failures.sum()
    }

    /// Whether every call offered is accounted for: written as a record,
    /// counted not placed, or counted a generator failure. Read once the
    /// driver has drained.
    pub fn reconciles(&self) -> bool {
        self.offered_total()
            == self.records_written() + self.not_placed_total() + self.generator_failures_total()
    }

    /// Records written so far.
    pub fn records_written(&self) -> u64 {
        self.counts.records.sum()
    }

    /// Calls never placed so far.
    pub fn not_placed_total(&self) -> u64 {
        self.counts.not_placed.sum()
    }

    /// Append the sink's counter families to a `/metrics` body.
    pub fn render_prometheus(&self, out: &mut String) {
        self.counts.records.render(out);
        self.counts.not_placed.render(out);
        self.counts.offered.render(out);
        self.counts.generator_failures.render(out);
        self.counts.push_failed.render(out);
        self.counts.dropped.render(out);
    }

    /// Write every queued record to the file, give the pusher 3 s
    /// for its backlog (plus one push attempt), and stop. Later writes are counted
    /// dropped.
    pub fn close(&self) {
        self.counts.close_deadline.lock().unwrap().get_or_insert(Instant::now() + CLOSE_BUDGET);
        drop(self.tx.lock().unwrap().take());
        for writer in std::mem::take(&mut *self.writers.lock().unwrap()) {
            let _ = writer.join();
        }
    }
}

impl Drop for OutcomeSink {
    fn drop(&mut self) {
        self.close();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(f)
}

/// Append each record to the file and hand its log line to the pusher; the
/// file is flushed every [`FLUSH_EVERY`] and when the sink closes.
fn write_loop(
    rx: Receiver<Value>,
    mut out: BufWriter<std::fs::File>,
    path: PathBuf,
    push: Option<SyncSender<String>>,
    counts: &Counts,
) {
    loop {
        let record = match rx.recv_timeout(FLUSH_EVERY) {
            Ok(record) => record,
            Err(RecvTimeoutError::Timeout) => {
                let _ = out.flush();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Err(e) = writeln!(out, "{record}") {
            eprintln!("[loadgen] outcome records {}: {e}", path.display());
        }
        if let Some(push) = &push {
            if push.try_send(logs_line(record)).is_err() {
                counts.dropped("push", 1);
            }
        }
    }
    let _ = out.flush();
}

/// Push the log lines in batches of at most [`BATCH_MAX`], one batch per
/// [`FLUSH_EVERY`] after the last push. A failed push is retried once, then
/// its batch is dropped; both are counted. Once the file writer has stopped,
/// the backlog has [`CLOSE_BUDGET`]; what is left then is dropped and counted.
fn push_loop(rx: Receiver<String>, target: LogsTarget, counts: &Counts) {
    let mut pending: Vec<String> = Vec::new();
    let mut last_push = Instant::now();
    let mut drained = false;
    loop {
        let closing = *counts.close_deadline.lock().unwrap();
        // Closing, wait briefly for the file writer's last lines; else wait
        // until the next push is due.
        let wait = match closing {
            Some(_) => Duration::from_millis(20),
            None => FLUSH_EVERY.saturating_sub(last_push.elapsed()),
        };
        if pending.is_empty() || closing.is_none() {
            match rx.recv_timeout(wait) {
                Ok(line) => pending.push(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => drained = true,
            }
        }
        while pending.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(line) => pending.push(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    drained = true;
                    break;
                }
            }
        }
        let due =
            closing.is_some() || pending.len() >= BATCH_MAX || last_push.elapsed() >= FLUSH_EVERY;
        if !due {
            continue;
        }
        if pending.is_empty() {
            if drained {
                return;
            }
            last_push = Instant::now();
            continue;
        }
        let batch: Vec<String> = pending.drain(..pending.len().min(BATCH_MAX)).collect();
        if closing.is_some_and(|deadline| Instant::now() >= deadline) {
            counts.dropped("push", batch.len());
            continue;
        }
        let body = batch.join("\n") + "\n";
        let delivered = (0..2).any(|_| {
            let attempt = Instant::now() + POST_TIMEOUT * 3;
            let closing = *counts.close_deadline.lock().unwrap();
            let deadline = closing.map_or(attempt, |close| close.min(attempt));
            match post(&target, body.as_bytes(), deadline) {
                Ok(()) => true,
                Err(e) => {
                    counts.push_failed.add(&[&counts.stream], 1);
                    eprintln!("[loadgen] outcome records to VictoriaLogs: {e}");
                    false
                }
            }
        });
        if !delivered {
            counts.dropped("push", batch.len());
        }
        last_push = Instant::now();
    }
}

/// The VictoriaLogs line of a record, as every producer ships one (ADR-0049
/// §4.4): every field of the record at top level, the record verbatim in
/// `_msg` (a reader gets it back whole, types and nesting kept), and `_time`
/// its end.
pub(super) fn logs_line(record: Value) -> String {
    let msg = record.to_string();
    let time = record["ended_at_ms"].clone();
    let mut line = match record {
        Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    line.insert("_msg".to_string(), Value::String(msg));
    line.insert("_time".to_string(), time);
    Value::Object(line).to_string()
}

/// One HTTP/1.1 POST of `body`, every step bounded by [`POST_TIMEOUT`] and
/// by `deadline`; `Err` unless the answer is 2xx.
fn post(target: &LogsTarget, body: &[u8], deadline: Instant) -> std::io::Result<()> {
    let step = || {
        let left = deadline.saturating_duration_since(Instant::now()).min(POST_TIMEOUT);
        if left.is_zero() {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "push deadline passed"))
        } else {
            Ok(left)
        }
    };
    let addr = target.authority.to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::other(format!("{} resolves to no address", target.authority))
    })?;
    let mut stream = TcpStream::connect_timeout(&addr, step()?)?;
    stream.set_write_timeout(Some(step()?))?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/stream+json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        target.path,
        target.authority,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.set_read_timeout(Some(step()?))?;
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    let status = String::from_utf8_lossy(&answer).lines().next().unwrap_or("").to_string();
    if status.split(' ').nth(1).is_some_and(|c| c.starts_with('2')) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("answered {status:?}")))
    }
}

#[cfg(test)]
#[path = "sink_tests.rs"]
mod tests;
