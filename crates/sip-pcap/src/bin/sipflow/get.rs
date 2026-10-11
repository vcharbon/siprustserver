//! `sipflow index` and `sipflow get`: one call of an emitted flows document,
//! served from its per-call index (`sip_pcap::callindex`) instead of a read of
//! the whole document.
//!
//!   sipflow index <flows.json>
//!     → writes `<flows.json>.idx`.
//!   sipflow get <flows.json | directory> --call-id X [--cseq N] [--method M] [--raw]
//!     → the call's messages, one line each (`m<i> +<seconds> src -> dst |
//!       start line | CSeq`), or with `--raw` their wire text. A directory
//!       names the one `*.flows.json` it holds. A stale or missing index is
//!       replaced by a scan of the document, and the run says so on stderr.

use std::path::{Path, PathBuf};

use clap::Parser as ClapParser;
use sip_pcap::callindex;
use sip_pcap::doc::{LegJson, MsgJson, Payload, Summary};

#[derive(ClapParser, Debug)]
#[command(name = "sipflow index", about = "write the per-call index of a flows document")]
struct IndexArgs {
    /// The flows document.
    doc: PathBuf,
}

#[derive(ClapParser, Debug)]
#[command(name = "sipflow get", about = "one call of a flows document, from its per-call index")]
struct GetArgs {
    /// The flows document, or a directory holding exactly one `*.flows.json`.
    doc: PathBuf,
    /// The Call-ID of the leg, whole.
    #[arg(long)]
    call_id: String,
    /// Only the messages of this CSeq number.
    #[arg(long)]
    cseq: Option<u32>,
    /// Only the messages of this CSeq method (a request and its responses).
    #[arg(long)]
    method: Option<String>,
    /// Print each message's wire text instead of its one-line summary.
    #[arg(long)]
    raw: bool,
}

/// Run `sipflow get` or `sipflow index` and exit when the command line names
/// one; return otherwise.
pub fn dispatch() {
    match std::env::args().nth(1).as_deref() {
        Some("get") => std::process::exit(get(std::env::args().skip(1))),
        Some("index") => std::process::exit(index(std::env::args().skip(1))),
        _ => {}
    }
}

/// `sipflow index …` (`argv[1]` already matched); the exit code.
pub fn index(argv: impl Iterator<Item = String>) -> i32 {
    let args = IndexArgs::parse_from(argv);
    match callindex::write(&args.doc) {
        Ok(index) => {
            eprintln!("indexed {} leg(s) of {}", index.legs.len(), args.doc.display());
            0
        }
        Err(e) => {
            eprintln!("sipflow index: {e}");
            2
        }
    }
}

/// `sipflow get …` (`argv[1]` already matched); the exit code.
pub fn get(argv: impl Iterator<Item = String>) -> i32 {
    let args = GetArgs::parse_from(argv);
    match run(&args) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("sipflow get: {e}");
            2
        }
    }
}

fn run(args: &GetArgs) -> Result<String, String> {
    let doc = document(&args.doc)?;
    let (index, served) = callindex::load(&doc)?;
    if !served {
        eprintln!("# no current index beside {}: scanned the document", doc.display());
    }
    let legs = callindex::legs(&doc, &index, &args.call_id)?;
    if legs.is_empty() {
        return Err(format!(
            "no leg has this Call-ID in {} ({} legs indexed)",
            doc.display(),
            index.legs.len()
        ));
    }
    Ok(render(&legs, args))
}

/// The flows document `path` names: itself, or the one a directory holds.
fn document(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    let docs: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_str().is_some_and(|n| n.ends_with(".flows.json")) && p.is_file())
        .collect();
    match docs.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("{} holds no *.flows.json", path.display())),
        _ => Err(format!("{} holds {} *.flows.json; name one", path.display(), docs.len())),
    }
}

/// The selected messages of `legs`, as `get` prints them.
fn render(legs: &[LegJson], args: &GetArgs) -> String {
    let mut out = String::new();
    for leg in legs {
        let first = leg.msgs.first().map_or(0, |m| m.ts_us);
        out.push_str(&format!("leg {} ({} message(s))\n", leg.call_id, leg.msgs.len()));
        for (i, msg) in leg.msgs.iter().enumerate() {
            if !callindex::selects(msg, args.cseq, args.method.as_deref()) {
                continue;
            }
            let at = msg.ts_us.saturating_sub(first) as f64 / 1e6;
            out.push_str(&format!("m{i} +{at:.6} {} -> {} | {}\n", msg.src, msg.dst, line(msg)));
            if args.raw {
                out.push_str(&wire(&msg.payload));
                out.push('\n');
            }
        }
    }
    out
}

/// A message's start line and CSeq.
fn line(msg: &MsgJson) -> String {
    match &msg.summary {
        Summary::Request { method, uri, cseq, .. } => {
            format!("{method} {uri} | CSeq {} {}", cseq.seq, cseq.method)
        }
        Summary::Response { status, reason, cseq, .. } => {
            format!("{status} {reason} | CSeq {} {}", cseq.seq, cseq.method)
        }
    }
}

/// A message's wire text; a binary body or an opaque datagram is named, not
/// printed.
fn wire(payload: &Payload) -> String {
    match payload {
        Payload::Text { raw } => raw.clone(),
        Payload::HeadBody { head, body_b64 } => {
            format!("{head}[a binary body, {} base64 characters]\n", body_b64.len())
        }
        Payload::Opaque { raw_b64 } => {
            format!("[an opaque datagram, {} base64 characters]\n", raw_b64.len())
        }
    }
}

#[cfg(test)]
#[path = "get/tests.rs"]
mod tests;
