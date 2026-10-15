//! Where outcome records go: a JSON-lines file, and optionally VictoriaLogs'
//! `/insert/jsonline`, written by one thread off the call path; plus the
//! per-stream counters of records written and calls never placed.

use std::io::{BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metric_catalogue::OpenRows;
use serde_json::Value;

use crate::catalogue::{NOT_PLACED, OUTCOME_RECORDS};

/// The bound on each step of a push (connect, write, read the answer): an
/// unreachable endpoint delays the writer by this much per batch, never more.
const POST_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the writer flushes the file and pushes a batch.
const FLUSH_EVERY: Duration = Duration::from_millis(1000);

/// The records of one generator run: the run, source and stream every record
/// names, the writer they go through, and the counters.
pub struct OutcomeSink {
    pub run: String,
    pub source: String,
    pub stream: String,
    tx: Mutex<Option<Sender<Value>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
    records: OpenRows,
    not_placed: OpenRows,
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
        let (tx, rx) = channel();
        let path = file.to_path_buf();
        let writer = std::thread::Builder::new()
            .name("outcome-writer".into())
            .spawn(move || write_loop(rx, BufWriter::new(out), path, logs))?;
        Ok(Self {
            run: run.to_string(),
            source: source.to_string(),
            stream: stream.to_string(),
            tx: Mutex::new(Some(tx)),
            writer: Mutex::new(Some(writer)),
            records: OpenRows::new(&OUTCOME_RECORDS),
            not_placed: OpenRows::new(&NOT_PLACED),
        })
    }

    /// Write one record (counted by its shape and ending).
    pub fn write(&self, record: Value) {
        let shape = record["shape"].as_str().unwrap_or("").to_string();
        let ending = record["ending"].as_str().unwrap_or("").to_string();
        self.records.add(&[&self.stream, &shape, &ending], 1);
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(record);
        }
    }

    /// Count a call of `shape` never placed, for `reason`.
    pub fn not_placed(&self, shape: &str, reason: &str) {
        self.not_placed.add(&[&self.stream, shape, reason], 1);
    }

    /// Records written so far.
    pub fn records_written(&self) -> u64 {
        self.records.sum()
    }

    /// Calls never placed so far.
    pub fn not_placed_total(&self) -> u64 {
        self.not_placed.sum()
    }

    /// Append the two counter families to a `/metrics` body.
    pub fn render_prometheus(&self, out: &mut String) {
        self.records.render(out);
        self.not_placed.render(out);
    }

    /// Flush every record written and stop the writer. Later writes are
    /// counted but go nowhere.
    pub fn close(&self) {
        drop(self.tx.lock().unwrap().take());
        if let Some(writer) = self.writer.lock().unwrap().take() {
            let _ = writer.join();
        }
    }
}

impl Drop for OutcomeSink {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_loop(
    rx: Receiver<Value>,
    mut out: BufWriter<std::fs::File>,
    path: PathBuf,
    logs: Option<LogsTarget>,
) {
    let mut batch = String::new();
    let mut flushed = Instant::now();
    loop {
        let closed = match rx.recv_timeout(FLUSH_EVERY.saturating_sub(flushed.elapsed())) {
            Ok(record) => {
                if let Err(e) = writeln!(out, "{record}") {
                    eprintln!("[loadgen] outcome records {}: {e}", path.display());
                }
                if logs.is_some() {
                    batch.push_str(&logs_line(record));
                    batch.push('\n');
                }
                if flushed.elapsed() < FLUSH_EVERY {
                    continue;
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        flushed = Instant::now();
        let _ = out.flush();
        if let (Some(target), false) = (&logs, batch.is_empty()) {
            if let Err(e) = post(target, batch.as_bytes()) {
                eprintln!("[loadgen] outcome records to VictoriaLogs: {e}");
            }
            batch.clear();
        }
        if closed {
            return;
        }
    }
}

/// The VictoriaLogs line of a record: the record, its `_time` (its end), and a
/// `_msg` naming the shape and ending.
fn logs_line(mut record: Value) -> String {
    let msg = format!(
        "{} {}",
        record["shape"].as_str().unwrap_or(""),
        record["ending"].as_str().unwrap_or("")
    );
    record["_time"] = record["ended_at_ms"].clone();
    record["_msg"] = Value::String(msg);
    record.to_string()
}

/// One HTTP/1.1 POST of `body`; `Err` unless the answer is 2xx.
fn post(target: &LogsTarget, body: &[u8]) -> std::io::Result<()> {
    let addr = target.authority.to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::other(format!("{} resolves to no address", target.authority))
    })?;
    let mut stream = TcpStream::connect_timeout(&addr, POST_TIMEOUT)?;
    stream.set_read_timeout(Some(POST_TIMEOUT))?;
    stream.set_write_timeout(Some(POST_TIMEOUT))?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/stream+json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        target.path,
        target.authority,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
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
