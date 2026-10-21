//! The re-parse sweep: every message of already emitted flows documents put
//! through today's parser, each refusal reported and the sweep continued.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use sip_message::parser::SipParser;
use sip_message::CustomParser;
use sip_pcap::doc::{FlowsDoc, Summary};

use crate::walk::{capture_of, expand_flows_documents};

#[path = "header_errors.rs"]
mod header_errors;

/// Re-parse every message under `inputs` and print one JSON line per message
/// the parser refuses (its document, capture, position, start-line facts,
/// reason and head), and one per non-fatal header reading of an accepted
/// message that failed ([`header_errors::of`], the line carrying `header` and
/// that header's `values`), then the totals on stderr.
///
/// A document that cannot be read is printed as one `error` line and counted,
/// never skipped: the totals cover every document the walk found.
pub(crate) fn run(inputs: &[PathBuf], jobs: usize) {
    let files = expand_flows_documents(inputs);
    if files.is_empty() {
        eprintln!("no *.flows.json found under {inputs:?}");
        std::process::exit(2);
    }
    let workers = jobs.clamp(1, files.len());
    eprintln!("# reparse: {} document(s), {workers} worker(s)", files.len());

    let next = AtomicUsize::new(0);
    let totals = Mutex::new(Totals::default());
    let out = Mutex::new(std::io::stdout());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (next, totals, out, files) = (&next, &totals, &out, &files);
            scope.spawn(move || {
                let parser = CustomParser::new();
                while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let document = path.display().to_string();
                    let capture = capture_of(path);
                    let (lines, counted) = sweep_document(&parser, &document, &capture, path);
                    let mut stdout = out.lock().unwrap_or_else(|e| e.into_inner());
                    for line in &lines {
                        let _ = writeln!(stdout, "{line}");
                    }
                    drop(stdout);
                    let mut t = totals.lock().unwrap_or_else(|e| e.into_inner());
                    t.absorb(&counted);
                    if t.documents % 500 == 0 {
                        eprintln!("# {}/{} re-parsed", t.documents, files.len());
                    }
                }
            });
        }
    });
    let t = totals.into_inner().unwrap_or_else(|e| e.into_inner());
    eprintln!(
        "# reparse: documents={} unreadable={} messages={} refused={} documents-with-refusal={} header-errors={}",
        t.documents, t.unreadable, t.messages, t.refused, t.refusing_documents, t.header_errors
    );
}

#[derive(Default)]
struct Totals {
    documents: usize,
    unreadable: usize,
    messages: usize,
    refused: usize,
    refusing_documents: usize,
    header_errors: usize,
}

impl Totals {
    fn absorb(&mut self, one: &Totals) {
        self.documents += one.documents;
        self.unreadable += one.unreadable;
        self.messages += one.messages;
        self.refused += one.refused;
        self.refusing_documents += one.refusing_documents;
        self.header_errors += one.header_errors;
    }
}

fn sweep_document(
    parser: &CustomParser,
    document: &str,
    capture: &str,
    path: &std::path::Path,
) -> (Vec<String>, Totals) {
    let mut counted = Totals { documents: 1, ..Totals::default() };
    let doc = match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<FlowsDoc>(&t).map_err(|e| e.to_string()))
    {
        Ok(doc) => doc,
        Err(error) => {
            counted.unreadable = 1;
            let line =
                serde_json::json!({ "document": document, "capture": capture, "error": error });
            return (vec![line.to_string()], counted);
        }
    };
    let mut lines = Vec::new();
    let mut refused_here = false;
    for (li, leg) in doc.legs.iter().enumerate() {
        for (mi, msg) in leg.msgs.iter().enumerate() {
            counted.messages += 1;
            let (kind, method, status) = match &msg.summary {
                Summary::Request { method, .. } => ("request", method.clone(), None),
                Summary::Response { status, cseq, .. } => {
                    ("response", cseq.method.clone(), Some(*status))
                }
            };
            let reason = match msg.payload.bytes() {
                Ok(raw) => match parser.parse(&raw) {
                    Ok(parsed) => {
                        let errors = header_errors::of(&parsed);
                        counted.header_errors += errors.len();
                        for error in errors {
                            let line = serde_json::json!({
                                "document": document,
                                "capture": capture,
                                "leg": li,
                                "msg": mi,
                                "kind": kind,
                                "method": method,
                                "status": status,
                                "header": error.name.as_wire_str(),
                                "reason": error.reason,
                                "values": error.values,
                            });
                            lines.push(line.to_string());
                        }
                        continue;
                    }
                    Err(e) => (e.reason, head_of(&raw)),
                },
                Err(e) => (format!("payload: {e}"), String::new()),
            };
            counted.refused += 1;
            refused_here = true;
            let line = serde_json::json!({
                "document": document,
                "capture": capture,
                "leg": li,
                "msg": mi,
                "kind": kind,
                "method": method,
                "status": status,
                "reason": reason.0,
                "head": reason.1,
            });
            lines.push(line.to_string());
        }
    }
    counted.refusing_documents = usize::from(refused_here);
    (lines, counted)
}

/// The message up to the blank line that ends its headers, as text.
fn head_of(raw: &[u8]) -> String {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    String::from_utf8_lossy(raw.get(..end).unwrap_or(raw)).into_owned()
}
