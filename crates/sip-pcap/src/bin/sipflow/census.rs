//! The RFC-violation census sweep over already emitted flows documents.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use sip_pcap::doc::FlowsDoc;
use sip_pcap::rfc::Census;

use crate::walk::{capture_of, expand_flows_documents};

/// Sweep the census over flows documents and print it.
///
/// Every document is either scanned or recorded as a read failure: a census
/// that dropped a document it could not parse would understate its own counts.
pub(crate) fn run(inputs: &[PathBuf], jobs: usize, candidates: &[rfc_rules::RuleId]) {
    let files = expand_flows_documents(inputs);
    if files.is_empty() {
        eprintln!("no *.flows.json found under {inputs:?}");
        std::process::exit(2);
    }
    let workers = jobs.clamp(1, files.len());
    eprintln!("# rfc-census: {} document(s), {workers} worker(s)", files.len());

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let census = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (next, done, files) = (&next, &done, &files);
                scope.spawn(move || {
                    let mut local = Census::with_candidates(candidates);
                    while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let name = path.display().to_string();
                        let capture = capture_of(path);
                        match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(
                            |t| serde_json::from_str::<FlowsDoc>(&t).map_err(|e| e.to_string()),
                        ) {
                            Ok(doc) => local.absorb(&name, &capture, &doc),
                            Err(reason) => local.fail(&name, reason),
                        }
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if n % 500 == 0 {
                            eprintln!("# {n}/{} scanned", files.len());
                        }
                    }
                    local
                })
            })
            .collect();
        let mut merged = Census::with_candidates(candidates);
        for handle in handles {
            merged.merge(handle.join().expect("a census worker panicked"));
        }
        merged
    });
    let mut census = census;
    census.sort();

    eprint!("{}", census.summary());
    println!("{}", serde_json::to_string_pretty(&census).expect("census serializes"));
}
