//! sipflow — extract full SIP callflows (including the B2BUA a-leg/b-leg
//! crossing) from capture files: classic pcap or pcapng, plain or gzipped.
//!
//! Pipeline: decode captures (`sip_pcap`, with IP-fragment reassembly) → build
//! the flow model (`sip_pcap::flow`: parse with the real `sip-message`
//! parser, group messages into **legs** by Call-ID, correlate legs into
//! **calls** through the ordered strategy pipeline — relayed token headers,
//! header-param tokens such as the IMS P-Charging-Vector icid, identity
//! adjacency, Call-ID derivation) → filter and print. This bin is a presenter
//! — selection and text layout only; everything model-shaped lives in the
//! library.
//!
//! Two selection surfaces. The FLAGS are leg-level and independent: handy for
//! triage, but they cannot express a predicate that spans a request and its
//! response. `--query` runs a JSON predicate tree (`sip_pcap::query`) that
//! can — it binds to a transaction, so "the UPDATE that was rejected" and
//! "the re-INVITE whose 200 carried this SDP" mean what they say. A query
//! also chooses its own projection: named fields per match for a screening
//! sweep, or the full model for extraction.
//!
//! Examples:
//!   sipflow /tmp/sipcap --list
//!   sipflow /tmp/sipcap --call-id 7f3a... --full
//!   sipflow /tmp/sipcap --final-status none
//!   sipflow /tmp/sipcap --ruri 15550100 --final-status 5xx
//!   sipflow /tmp/sipcap --json > flows.json
//!   sipflow /tmp/sipcap --query update-rejected.json
//!   sipflow corpus/ --query-json '{"select":{"evidence_kind":"derived_call_id"},
//!                                  "project":{"mode":"summary","fields":["as_socket"]}}'
//!   sipflow /tmp/sipcap --call-id 7f3a... --sut 10.0.0.9 --rfc --rfc-rules all
//!   sipflow /tmp/sipcap --sut auto --rfc --rfc-json review.json --rfc-doc review.flows.json
//!   sipflow --to-pcap capture.anon.flows.json --out capture.anon.pcap
//!   sipflow --list-rules
//!   sipflow index capture.flows.json
//!   sipflow get capture-dir/ --call-id 7f3a... --cseq 2 --method BYE --raw
//!
//! `--rfc` reviews the SELECTED calls: the same rules the census runs, on the
//! document the selection would emit, with every hit placed on a side by
//! `--sut` — so the review answers "what did the platform break on these
//! calls", not "what is in this capture".

use std::path::PathBuf;

use clap::Parser as ClapParser;
use sip_message::header::HeaderName;
use sip_message::SipMessage;
use sip_message::ToolReading;
use sip_pcap::doc::FlowsDoc;
use sip_pcap::enrich::{enrich_str, EnrichOptions};
use sip_pcap::flow::{
    build_flows, CallGroup, CorrelateStrategy, FlowConfig, FlowLeg, DEFAULT_DEDUP_WINDOW_US,
};
use sip_pcap::query::{neighbours_of, select_groups, summary_row, Projection, Query};
use sip_pcap::rfc::{Census, LocatedHit, SutSet};

#[path = "sipflow/captures.rs"]
mod captures;
use captures::expand_inputs;
#[path = "sipflow/census.rs"]
mod census;
#[path = "sipflow/get.rs"]
mod get;
#[path = "sipflow/ladder.rs"]
mod ladder;
use ladder::{fmt_ts, print_ladder, print_list_line};
#[path = "sipflow/reparse.rs"]
mod reparse;
#[path = "sipflow/walk.rs"]
mod walk;

/// The reading the file at `path` states, or every problem with it.
fn stated_reading(path: &std::path::Path) -> Result<ToolReading, Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| vec![e.to_string()])?;
    ToolReading::stated(&text)
}

/// Serialize an emitted document, exiting with the derivation error a
/// document that cannot be re-derived must never be printed past.
fn emit_json(doc: Result<FlowsDoc, String>) -> String {
    match doc {
        Ok(doc) => serde_json::to_string_pretty(&doc).expect("model JSON serializes"),
        Err(e) => {
            eprintln!("flows enrichment failed: {e}");
            std::process::exit(2);
        }
    }
}

#[derive(ClapParser, Debug)]
#[command(
    name = "sipflow",
    about = "capture → SIP callflow extractor with B2BUA leg correlation (see module doc)"
)]
struct Args {
    /// The reading every message's bytes are parsed under, wherever sipflow
    /// parses them (extraction, `--enrich`, `--reparse`, `--rfc`): a file of
    /// `key = value` lines naming `exemptions` and the `max-header-length`,
    /// `max-uri-length`, `max-header-lines` and `max-datagram` bounds
    /// (`sip_pcap::reading`). A line that does not read refuses the run.
    /// Without it, the RFC: no exemption, the parser's default bounds.
    #[arg(long, value_name = "FILE")]
    reading: Option<PathBuf>,

    /// Capture files or directories — pcap/pcapng, plain or `.gz` (a directory
    /// expands to its capture files). Ring files are ordered oldest-first
    /// automatically. Required except with `--schema` / `--enrich`, which read
    /// no capture.
    inputs: Vec<PathBuf>,

    /// Read the inputs as ONE consecutive capture — ring files of one tap,
    /// oldest first — and refuse them (exit 2) unless each file starts where
    /// the previous one ended: no overlap, and no gap wider than this many
    /// milliseconds. Without it, several inputs are read as whatever they
    /// are, a corpus of unrelated captures included.
    #[arg(long, value_name = "MAX_GAP_MS")]
    contiguous: Option<u64>,

    /// Select call groups containing a leg whose Call-ID contains this substring.
    #[arg(long)]
    call_id: Option<String>,

    /// Substring match on the INVITE From URI.
    #[arg(long)]
    from: Option<String>,

    /// Substring match on the INVITE To URI.
    #[arg(long)]
    to: Option<String>,

    /// Substring match on the INVITE Request-URI.
    #[arg(long)]
    ruri: Option<String>,

    /// Select groups containing a request with this method (e.g. REFER).
    #[arg(long)]
    method: Option<String>,

    /// Select groups where some message carries this header, `Name` or
    /// `Name=substring`. Repeatable (all must match).
    #[arg(long = "header")]
    headers: Vec<String>,

    /// Initial-INVITE final response filter: an exact code (486), a class
    /// (4xx/5xx/6xx), or `none` (no final response ever seen — timeout triage).
    /// Matches if ANY leg of the group qualifies.
    #[arg(long)]
    final_status: Option<String>,

    /// Substring match on a correlation token.
    #[arg(long)]
    token: Option<String>,

    /// The system under test's own addresses, comma-separated IPs
    /// (port-insensitive, no CIDR), or `auto` — the sockets that
    /// re-originated a call under a derived Call-ID, as the correlation
    /// evidence names them. Selects the call groups the SUT touched, and
    /// places every `--rfc` hit on a side (`platform` / `peer`).
    #[arg(long, value_delimiter = ',')]
    sut: Vec<String>,

    /// Correlation headers whose (relayed) value ties B2BUA legs together —
    /// whole-value equality, the pipeline's first strategy.
    #[arg(long = "correlate", default_values_t = ["X-Loadgen-Id".to_string(), "X-Api-Call".to_string()])]
    correlate: Vec<String>,

    /// Header-param correlation as `Header:param` — equality of one named
    /// `;`-param (sibling params may mutate). Repeatable; each spec appends
    /// one strategy, in order, after the token headers and before identity
    /// adjacency. Default: the IMS end-to-end charging id.
    #[arg(long = "correlate-param", default_values_t = ["P-Charging-Vector:icid-value".to_string()])]
    correlate_param: Vec<String>,

    /// Disable header-param correlation (drop those strategies from the
    /// pipeline).
    #[arg(long, default_value_t = false)]
    no_param_correlate: bool,

    /// Disable Call-ID derivation correlation (the `1-<original>` application
    /// server loopback).
    #[arg(long, default_value_t = false)]
    no_derived_call_id: bool,

    /// Pair a derived Call-ID even when its INVITE does not depart on a hop
    /// the base leg traversed — looser, and no longer evidence of a loopback.
    #[arg(long, default_value_t = false)]
    derived_call_id_any_hop: bool,

    /// Max first-activity distance, in milliseconds, for the strategies that
    /// bound one (Call-ID derivation, identity adjacency).
    #[arg(long, default_value_t = 5_000)]
    pair_window_ms: u64,

    /// Disable the identity-adjacency fallback (drop the last pipeline
    /// strategy — token/param-only correlation).
    #[arg(long, default_value_t = false)]
    no_identity_adjacency: bool,

    /// Emit the FULL flow model (raw payloads as text where valid UTF-8,
    /// base64 for binary parts, parsed summaries, hops, match evidence,
    /// per-message and per-call enrichment, decode counters) as pretty-printed
    /// JSON on stdout — the document is `sip_pcap::doc::FlowsDoc`, whose JSON
    /// Schema `--schema` prints. Whole-capture emit: selection filters and text
    /// layout flags do not apply.
    #[arg(
        long,
        default_value_t = false,
        conflicts_with_all = [
            "call_id", "from", "to", "ruri", "method", "headers",
            "final_status", "token", "list", "full", "limit",
        ]
    )]
    json: bool,

    /// Headers projected onto every emitted message as structured
    /// `msgs[].headers` entries, comma-separated. Matching is by header
    /// IDENTITY, so casing and RFC 3261 §7.3.3 compact forms collapse. A
    /// consumer that needs a header a correlation rule names passes it here
    /// instead of re-parsing the wire text.
    #[arg(long = "emit-headers", value_delimiter = ',')]
    emit_headers: Vec<String>,

    /// Re-derive the enrichment of an ALREADY EMITTED flows document and print
    /// it. The pass is a pure function of the message bytes, so a document a
    /// tool rewrote (anonymized, filtered) comes back in step with its bytes.
    #[arg(long, conflicts_with_all = ["json", "query", "query_json", "list", "full"])]
    enrich: Option<PathBuf>,

    /// Print the JSON Schema of the emitted flows document and exit.
    #[arg(long, default_value_t = false)]
    schema: bool,

    /// Print every RFC rule token (`rfc_rules::RuleId`), one per line, and exit.
    #[arg(long, default_value_t = false)]
    list_rules: bool,

    /// Encode an ALREADY EMITTED flows document as a classic pcap (Ethernet /
    /// IP / UDP, the document's own sockets and timestamps) at `--out` — an
    /// anonymized document back into a capture every reader takes. Reads no
    /// capture.
    #[arg(long = "to-pcap", requires = "out", conflicts_with_all = ["json", "query", "query_json", "list", "full", "enrich", "rfc_census", "rfc"])]
    to_pcap: Option<PathBuf>,

    /// Where `--to-pcap` writes.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Also write every datagram ingest set aside (`parse-failed`, and a copy
    /// of each `header-failed` one), exactly as captured, to this classic pcap.
    #[arg(long = "parse-failed-pcap")]
    parse_failed_pcap: Option<PathBuf>,

    /// Run the RFC-violation census (`sip_pcap::rfc`) over ALREADY EMITTED flows
    /// documents — files, or directories walked for `*.flows.json`, each capture
    /// at its current generation — and print the report as JSON on stdout, its
    /// summary on stderr. Reads no capture, and modifies no document.
    #[arg(
        long = "rfc-census",
        value_delimiter = ',',
        conflicts_with_all = ["json", "query", "query_json", "list", "full", "enrich"]
    )]
    rfc_census: Vec<PathBuf>,

    /// Worker threads the census sweep runs, each parsing one document at a
    /// time.
    #[arg(long, default_value_t = 4)]
    rfc_census_jobs: usize,

    /// Re-parse every message of ALREADY EMITTED flows documents — files, or
    /// directories walked as `--rfc-census` walks them — and print one JSON
    /// line per message the parser refuses and per failed non-fatal header of
    /// an accepted one, with the totals on stderr. Reads no capture.
    #[arg(
        long,
        value_delimiter = ',',
        conflicts_with_all = ["json", "query", "query_json", "list", "full", "enrich", "rfc_census"]
    )]
    reparse: Vec<PathBuf>,

    /// Worker threads the re-parse sweep runs, each reading one document at a
    /// time.
    #[arg(long, default_value_t = 4)]
    reparse_jobs: usize,

    /// Rule tokens (`rfc_rules::RuleId`) named beside the census vocabulary.
    /// The census runs every rule with a body already, so naming one changes
    /// nothing the report carries.
    #[arg(long = "rfc-census-with", value_delimiter = ',', requires = "rfc_census")]
    rfc_census_with: Vec<rfc_rules::RuleId>,

    /// Review the SELECTED call groups against the RFC rules (`sip_pcap::rfc`)
    /// in the same run: the hits under each ladder (or list line), the census
    /// summary on stderr. Whole-capture `--json` is the one mode it does not
    /// combine with — `--rfc-doc` writes the reviewed document instead.
    #[arg(long, default_value_t = false, conflicts_with_all = ["json", "enrich", "rfc_census"])]
    rfc: bool,

    /// Which rules `--rfc` runs: `wire` (the corpus-backed census vocabulary),
    /// `all` (every rule `rfc-rules` holds), or rule tokens, comma-separated.
    /// Rules beyond `wire` are triage input: a report carrying them is not
    /// one the census consumers read.
    #[arg(long = "rfc-rules", value_delimiter = ',', default_values_t = ["wire".to_string()], requires = "rfc")]
    rfc_rules: Vec<String>,

    /// Write the `--rfc` report — the census shape, one document — here.
    #[arg(long = "rfc-json", requires = "rfc")]
    rfc_json: Option<PathBuf>,

    /// Write the reviewed flows document (the selection, schema 5) here — what
    /// the report's `document` names, so a consumer can take the cut it
    /// speaks of.
    #[arg(long = "rfc-doc", requires = "rfc")]
    rfc_doc: Option<PathBuf>,

    /// Run a JSON query (see `sip_pcap::query`): a predicate tree over calls,
    /// legs, transactions and messages, plus the projection of a match. The
    /// query's own `correlate` block, when present, replaces the pipeline the
    /// flags above build.
    #[arg(long, conflicts_with_all = ["json", "list", "full"])]
    query: Option<PathBuf>,

    /// The same, given inline instead of in a file.
    #[arg(long, conflicts_with_all = ["json", "list", "full", "query"])]
    query_json: Option<String>,

    /// One summary line per call group instead of full ladders.
    #[arg(long, default_value_t = false)]
    list: bool,

    /// Print every message in full (raw bytes) under its ladder line.
    #[arg(long, default_value_t = false)]
    full: bool,

    /// Max call groups printed in ladder mode (list mode prints all).
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

fn main() {
    get::dispatch();
    let args = Args::parse();
    if args.list_rules {
        for rule in rfc_rules::RuleId::ALL {
            println!("{}", rule.token());
        }
        return;
    }
    if args.schema {
        let schema = schemars::schema_for!(sip_pcap::doc::FlowsDoc);
        println!("{}", serde_json::to_string_pretty(&schema).expect("schema serializes"));
        return;
    }
    let reading = match &args.reading {
        None => ToolReading::plain_rfc(),
        Some(path) => match stated_reading(path) {
            Ok(reading) => reading,
            Err(problems) => {
                for problem in problems {
                    eprintln!("--reading {}: {problem}", path.display());
                }
                std::process::exit(2);
            }
        },
    };
    let opts = EnrichOptions::with_headers(&args.emit_headers, reading.clone());
    if let Some(path) = &args.enrich {
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| enrich_str(&t, &opts))
        {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("cannot re-enrich {}: {e}", path.display());
                std::process::exit(2);
            }
        }
        return;
    }
    if let Some(path) = &args.to_pcap {
        let out = args.out.as_ref().expect("clap: --to-pcap requires --out");
        if let Err(e) = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<FlowsDoc>(&t).map_err(|e| e.to_string()))
            .and_then(|doc| sip_pcap::pcapout::doc_to_pcap(&doc))
            .and_then(|bytes| std::fs::write(out, bytes).map_err(|e| e.to_string()))
        {
            eprintln!("cannot encode {} as a capture: {e}", path.display());
            std::process::exit(2);
        }
        return;
    }
    if !args.reparse.is_empty() {
        reparse::run(&args.reparse, args.reparse_jobs, &reading);
        return;
    }
    if !args.rfc_census.is_empty() {
        census::run(&args.rfc_census, args.rfc_census_jobs, &args.rfc_census_with);
        return;
    }
    let rfc_rules = args.rfc.then(|| match rule_set(&args.rfc_rules) {
        Ok(rules) => rules,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    });
    if args.inputs.is_empty() {
        eprintln!(
            "no capture given (see --help; --schema, --enrich, --reparse and --rfc-census read no capture)"
        );
        std::process::exit(2);
    }
    let files = expand_inputs(&args.inputs);
    if files.is_empty() {
        eprintln!("no capture files found under {:?}", args.inputs);
        std::process::exit(2);
    }

    let read = match args.contiguous {
        Some(max_gap_ms) => sip_pcap::read_capture_set(&files, max_gap_ms.saturating_mul(1_000)),
        None => sip_pcap::read_capture_files(&files),
    };
    let (datagrams, stats) = match read {
        Ok(v) => v,
        Err(e) => {
            eprintln!("capture read failed: {e}");
            std::process::exit(2);
        }
    };

    let mut strategies = vec![CorrelateStrategy::HeaderToken { headers: args.correlate.clone() }];
    if !args.no_param_correlate {
        for spec in &args.correlate_param {
            let Some((header, param)) =
                spec.split_once(':').filter(|(h, p)| !h.is_empty() && !p.is_empty())
            else {
                eprintln!("--correlate-param wants Header:param, got {spec:?}");
                std::process::exit(2);
            };
            strategies.push(CorrelateStrategy::HeaderParam {
                header: header.to_string(),
                param: param.to_string(),
            });
        }
    }
    let window_us = args.pair_window_ms.saturating_mul(1_000);
    if !args.no_derived_call_id {
        strategies.push(CorrelateStrategy::DerivedCallId {
            window_us,
            require_shared_hop: !args.derived_call_id_any_hop,
            min_base_len: 8,
        });
    }
    if !args.no_identity_adjacency {
        strategies.push(CorrelateStrategy::IdentityAdjacency { window_us });
    }
    let cfg = FlowConfig { strategies, dedup_window_us: DEFAULT_DEDUP_WINDOW_US, reading };

    let capture = capture_name(&files);

    if let Some(query) = load_query(&args, &cfg.reading) {
        // The query owns correlation when it says so: which strategies ran is
        // part of what a recorded query means, not ambient CLI state.
        let cfg = query.correlate.clone().unwrap_or(cfg);
        let flows = build_flows(&datagrams, &cfg);
        let sut = sut_of(&args, &flows);
        let hits = run_query(&flows, &stats, &query, &opts, sut.as_ref());
        if let Some(rules) = &rfc_rules {
            let review = review(&flows, &stats, &hits, &opts, rules, sut.as_ref(), &capture, &args);
            print_review(&review, &flows, &hits, &args, true);
        }
        return;
    }

    let flows = build_flows(&datagrams, &cfg);
    let sut = sut_of(&args, &flows);

    if let Some(path) = &args.parse_failed_pcap {
        if let Err(e) = sip_pcap::pcapout::set_aside_to_pcap(&flows.stats, &datagrams)
            .and_then(|bytes| std::fs::write(path, bytes).map_err(|e| e.to_string()))
        {
            eprintln!("cannot write the rejected datagrams to {}: {e}", path.display());
            std::process::exit(2);
        }
    }

    if args.json {
        // Pretty + deterministic field order: the emit is committed as
        // fixtures downstream, so its git/jq diffs must stay line-readable.
        println!("{}", emit_json(sip_pcap::emit::flows_to_doc(&flows, &stats, &opts)));
        return;
    }

    let selected: Vec<usize> = (0..flows.groups.len())
        .filter(|&g| group_matches(&flows.groups[g], &flows.legs, &args, sut.as_ref()))
        .collect();

    eprintln!(
        "# files={} {stats}\n# sip-messages={} capture-dups={} parse-failed={} header-failed={} non-sip={} legs={} call-groups={} matched={}",
        files.len(),
        flows.stats.sip_messages,
        flows.stats.capture_dups,
        flows.stats.parse_failed,
        flows.stats.header_failed,
        flows.stats.non_sip,
        flows.legs.len(),
        flows.groups.len(),
        selected.len(),
    );
    for a in &flows.stats.aligned_probes {
        eprintln!(
            "# aligned-probe={} reference={} from-us={} offset-ms={:.3} pairs={}",
            a.probe,
            a.reference,
            a.from_us,
            a.offset_us as f64 / 1000.0,
            a.pairs
        );
    }

    // The review is over EVERY selected group, whatever `--limit` prints: the
    // limit bounds the reader's screen, not the question.
    let review = rfc_rules.as_ref().map(|rules| {
        review(&flows, &stats, &selected, &opts, rules, sut.as_ref(), &capture, &args)
    });

    if args.list {
        for (n, &g) in selected.iter().enumerate() {
            print_list_line(n, &flows.groups[g], &flows.legs);
        }
    } else {
        for (n, &g) in selected.iter().take(args.limit).enumerate() {
            print_ladder(n, &flows.groups[g], &flows.legs, args.full);
            if let Some(review) = &review {
                print_group_hits(n, review);
            }
        }
        if selected.len() > args.limit {
            println!(
                "… {} more matching call groups (raise --limit or add filters; --list shows all)",
                selected.len() - args.limit
            );
        }
    }
    if let Some(review) = &review {
        print_review(review, &flows, &selected, &args, args.list);
    }
}

/// The capture the review names: the one file's name, or the directory's
/// when several ring files were read.
fn capture_name(files: &[PathBuf]) -> String {
    match files {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        many => many
            .first()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The SUT set `--sut` states, or the one the capture names under `auto`.
/// Exits when `auto` finds no derivation: a review that guessed the platform
/// would place every hit on a side nobody stated.
fn sut_of(args: &Args, flows: &sip_pcap::flow::Flows) -> Option<SutSet> {
    match args.sut.as_slice() {
        [] => None,
        [one] if one == "auto" => match SutSet::minted_in(flows) {
            Some(sut) => Some(sut),
            None => {
                eprintln!(
                    "--sut auto: no call group carries derived-Call-ID evidence, so the capture \
                     names no platform; state the addresses"
                );
                std::process::exit(2);
            }
        },
        stated => match SutSet::stated(stated) {
            Ok(sut) => Some(sut),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
    }
}

/// The `--rfc-rules` words as candidate rule ids: `wire` is the census
/// vocabulary and needs no candidates; `all` names every rule beyond it; a
/// token names one.
fn rule_set(words: &[String]) -> Result<Vec<rfc_rules::RuleId>, String> {
    let mut out = Vec::new();
    for word in words {
        match word.as_str() {
            "wire" => {}
            "all" => out.extend(rfc_rules::RuleId::ALL),
            token => {
                let id = token
                    .parse::<rfc_rules::RuleId>()
                    .map_err(|_| format!("--rfc-rules: unknown rule {token:?}"))?;
                out.push(id);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// One selection's review: the emitted document scanned into a one-document
/// census, its hits keyed by the position each group has in `selected` — the
/// same `#n` the ladders print.
struct Review {
    census: Census,
    doc: FlowsDoc,
}

fn review(
    flows: &sip_pcap::flow::Flows,
    decode: &sip_pcap::DecodeStats,
    selected: &[usize],
    opts: &EnrichOptions,
    candidates: &[rfc_rules::RuleId],
    sut: Option<&SutSet>,
    capture: &str,
    args: &Args,
) -> Review {
    let doc = match sip_pcap::emit::flows_to_doc_selected(flows, decode, selected, opts) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("flows enrichment failed: {e}");
            std::process::exit(2);
        }
    };
    let document = args
        .rfc_doc
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("{capture}#selected"));
    let mut census = Census::with_candidates(candidates);
    census.absorb_with_sut(&document, capture, &doc, sut);
    census.sort();
    Review { census, doc }
}

/// The hits of the group printed as `#n`, under its ladder.
fn print_group_hits(n: usize, review: &Review) {
    let hits: Vec<&LocatedHit> = review.census.hits.iter().filter(|h| h.hit.group == n).collect();
    if hits.is_empty() {
        println!("  rfc: no hit");
        return;
    }
    for h in hits {
        println!("  rfc: {}", hit_line(h, &review.doc));
    }
}

/// One hit on one line: the rule, who is charged and on which side, the
/// message the evidence anchors on, and the evidence itself as flat JSON —
/// the rule's own proof, in the rule's own words.
fn hit_line(h: &LocatedHit, doc: &FlowsDoc) -> String {
    let hit = &h.hit;
    let side = if hit.side.is_unattributed() {
        String::new()
    } else {
        format!(" side={}", hit.side.token())
    };
    let anchor = doc
        .legs
        .get(hit.leg)
        .and_then(|l| l.msgs.get(hit.evidence.anchor()))
        .map(|m| format!("{} {} → {} {}", fmt_ts(m.ts_us), m.src, m.dst, summary_json(&m.summary)))
        .unwrap_or_default();
    format!(
        "{} emitter={} role={}{}{} taker={} cseq={} leg={} @ {}  {}",
        hit.rule.token(),
        hit.emitter,
        hit.emitter_role.token(),
        side,
        relayed_note(hit),
        hit.taker,
        hit.cseq,
        hit.call_id,
        anchor,
        serde_json::to_string(&hit.evidence).expect("evidence serializes"),
    )
}

/// ` relayed`, naming the origin's emitter and anchor where the census paired
/// the hit with the one it carries on; empty on an originated hit.
fn relayed_note(hit: &sip_pcap::rfc::Hit) -> String {
    match (&hit.relays, hit.relayed) {
        (Some(o), _) => format!(" relayed-from={}@leg{}/msg{}", o.emitter, o.leg, o.anchor_msg),
        (None, true) => " relayed".to_string(),
        (None, false) => String::new(),
    }
}

fn summary_json(s: &sip_pcap::doc::Summary) -> String {
    match s {
        sip_pcap::doc::Summary::Request { method, uri, cseq, .. } => {
            format!("{method} {uri} (CSeq {} {})", cseq.seq, cseq.method)
        }
        sip_pcap::doc::Summary::Response { status, reason, cseq, .. } => {
            format!("{status} {reason} (CSeq {} {})", cseq.seq, cseq.method)
        }
    }
}

/// The review's tail: in list mode every hit (the ladders were not printed,
/// so nothing carried them), then the census summary on stderr, then the
/// files `--rfc-json` / `--rfc-doc` asked for.
fn print_review(
    review: &Review,
    flows: &sip_pcap::flow::Flows,
    selected: &[usize],
    args: &Args,
    hits_here: bool,
) {
    if hits_here {
        for h in &review.census.hits {
            let leg0 = selected.get(h.hit.group).map(|&g| flows.groups[g].legs[0]);
            let t0 = leg0.map(|l| fmt_ts(flows.legs[l].t_first())).unwrap_or_default();
            println!("rfc #{:<4} {t0} {}", h.hit.group, hit_line(h, &review.doc));
        }
    }
    let sut = review
        .census
        .sut
        .as_ref()
        .map(|s| format!(" sut={} ({})", s.addresses.join(","), s.decided_by))
        .unwrap_or_default();
    eprint!(
        "# rfc review: {} selected group(s), rules={}{sut}\n{}",
        selected.len(),
        args.rfc_rules.join(","),
        review.census.summary_measured()
    );
    if let Some(path) = &args.rfc_json {
        let text = serde_json::to_string_pretty(&review.census).expect("census serializes");
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("cannot write {}: {e}", path.display());
            std::process::exit(2);
        }
    }
    if let Some(path) = &args.rfc_doc {
        let text = serde_json::to_string_pretty(&review.doc).expect("document serializes");
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("cannot write {}: {e}", path.display());
            std::process::exit(2);
        }
    }
}

/// Load `--query` / `--query-json`, exiting with the parse error's own path
/// and reason — a mistyped predicate must never widen the match set silently.
fn load_query(args: &Args, reading: &ToolReading) -> Option<Query> {
    let text = match (&args.query, &args.query_json) {
        (Some(path), _) => match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cannot read query {}: {e}", path.display());
                std::process::exit(2);
            }
        },
        (None, Some(inline)) => inline.clone(),
        (None, None) => return None,
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("query is not valid JSON: {e}");
            std::process::exit(2);
        }
    };
    match Query::from_json(&value, reading) {
        Ok(q) => Some(q),
        Err(e) => {
            eprintln!("query rejected at {e}");
            std::process::exit(2);
        }
    }
}

/// Select, expand to neighbours, project. The three phases stay visible here
/// because they are what the query document is made of.
fn run_query(
    flows: &sip_pcap::flow::Flows,
    decode: &sip_pcap::DecodeStats,
    query: &Query,
    opts: &EnrichOptions,
    sut: Option<&SutSet>,
) -> Vec<usize> {
    let mut hits = select_groups(flows, query);
    if let Some(sut) = sut {
        hits.retain(|&g| {
            let legs: Vec<&FlowLeg> =
                flows.groups[g].legs.iter().map(|&l| &flows.legs[l]).collect();
            sut.touches(&legs)
        });
    }
    eprintln!(
        "# legs={} call-groups={} matched={}{}",
        flows.legs.len(),
        flows.groups.len(),
        hits.len(),
        query.name.as_ref().map(|n| format!(" query={n}")).unwrap_or_default(),
    );

    let expanded = query.neighbours.as_ref().map(|spec| neighbours_of(flows, &hits, spec));

    match &query.project {
        Projection::Count => println!("{}", hits.len()),
        Projection::Summary { fields } => {
            for (n, &g) in hits.iter().enumerate() {
                let mut row = summary_row(flows, g, fields);
                if let Some(near) = expanded.as_ref().and_then(|e| e.get(n)) {
                    let rows: Vec<_> =
                        near.1.iter().map(|&x| summary_row(flows, x, fields)).collect();
                    if let Some(obj) = row.as_object_mut() {
                        obj.insert("neighbours".into(), serde_json::Value::Array(rows));
                    }
                }
                println!("{row}");
            }
        }
        Projection::Full => {
            // Neighbours join the emitted set: a hit's context is part of what
            // was asked for, and the document must stay self-consistent.
            let mut wanted = hits.clone();
            if let Some(e) = &expanded {
                wanted.extend(e.iter().flat_map(|(_, near)| near.iter().copied()));
            }
            wanted.sort_unstable();
            wanted.dedup();
            println!(
                "{}",
                emit_json(sip_pcap::emit::flows_to_doc_selected(flows, decode, &wanted, opts))
            );
        }
    }
    hits
}

fn group_matches(group: &CallGroup, legs: &[FlowLeg], args: &Args, sut: Option<&SutSet>) -> bool {
    let any_leg = |pred: &dyn Fn(&FlowLeg) -> bool| group.legs.iter().any(|&l| pred(&legs[l]));

    if let Some(sut) = sut {
        let members: Vec<&FlowLeg> = group.legs.iter().map(|&l| &legs[l]).collect();
        if !sut.touches(&members) {
            return false;
        }
    }

    if let Some(cid) = &args.call_id {
        if !any_leg(&|l| l.call_id.contains(cid.as_str())) {
            return false;
        }
    }
    if let Some(f) = &args.from {
        if !any_leg(&|l| {
            l.invite.as_ref().is_some_and(|inv| inv.from_uri.text().contains(f.as_str()))
        }) {
            return false;
        }
    }
    if let Some(t) = &args.to {
        if !any_leg(&|l| {
            l.invite.as_ref().is_some_and(|inv| inv.to_uri.text().contains(t.as_str()))
        }) {
            return false;
        }
    }
    if let Some(r) = &args.ruri {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|inv| inv.ruri.text().contains(r.as_str())))
        {
            return false;
        }
    }
    if let Some(m) = &args.method {
        if !any_leg(&|l| {
            l.msgs.iter().any(|rec| {
                matches!(&rec.parsed, SipMessage::Request(r) if r.method().as_str().eq_ignore_ascii_case(m))
            })
        }) {
            return false;
        }
    }
    if let Some(tok) = &args.token {
        if !any_leg(&|l| l.tokens().iter().any(|t| t.contains(tok.as_str()))) {
            return false;
        }
    }
    for spec in &args.headers {
        let (name, want) = match spec.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (spec.as_str(), None),
        };
        if !any_leg(&|l| {
            l.msgs.iter().any(|rec| {
                rec.parsed.raw(HeaderName::from(name)).any(|v| want.is_none_or(|w| v.contains(w)))
            })
        }) {
            return false;
        }
    }
    if let Some(fs) = &args.final_status {
        let matches_leg = |l: &FlowLeg| -> bool {
            if l.invite.is_none() {
                return false;
            }
            match (fs.as_str(), l.final_status) {
                ("none", None) => true,
                ("none", Some(_)) => false,
                (_, None) => false,
                (spec, Some(code)) => {
                    if let Some(class) = spec.strip_suffix("xx") {
                        class.parse::<u16>().is_ok_and(|c| code / 100 == c)
                    } else {
                        spec.parse::<u16>().is_ok_and(|c| code == c)
                    }
                }
            }
        };
        if !any_leg(&matches_leg) {
            return false;
        }
    }
    true
}

#[cfg(all(test, unix))]
#[path = "sipflow/tests.rs"]
mod tests;
