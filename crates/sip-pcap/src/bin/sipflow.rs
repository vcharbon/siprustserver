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
//!   sipflow /tmp/sipcap --ruri 166601009 --final-status 5xx
//!   sipflow /tmp/sipcap --json > flows.json
//!   sipflow /tmp/sipcap --query update-rejected.json
//!   sipflow corpus/ --query-json '{"select":{"evidence_kind":"derived_call_id"},
//!                                  "project":{"mode":"summary","fields":["as_socket"]}}'

use std::path::PathBuf;

use clap::Parser as ClapParser;
use sip_message::SipMessage;
use sip_pcap::flow::{
    build_flows, CallGroup, CorrelateStrategy, FlowConfig, FlowLeg, DEFAULT_DEDUP_WINDOW_US,
};
use sip_pcap::query::{neighbours_of, select_groups, summary_row, Projection, Query};

#[derive(ClapParser, Debug)]
#[command(
    name = "sipflow",
    about = "capture → SIP callflow extractor with B2BUA leg correlation (see module doc)"
)]
struct Args {
    /// Capture files or directories — pcap/pcapng, plain or `.gz` (a directory
    /// expands to its capture files). Ring files are ordered oldest-first
    /// automatically.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

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
    /// decode counters) as pretty-printed JSON on stdout — schema documented
    /// on `sip_pcap::emit::flows_to_json`. Whole-capture emit: selection
    /// filters and text layout flags do not apply.
    #[arg(
        long,
        default_value_t = false,
        conflicts_with_all = [
            "call_id", "from", "to", "ruri", "method", "headers",
            "final_status", "token", "list", "full", "limit",
        ]
    )]
    json: bool,

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
    let args = Args::parse();
    let files = expand_inputs(&args.inputs);
    if files.is_empty() {
        eprintln!("no capture files found under {:?}", args.inputs);
        std::process::exit(2);
    }

    let (datagrams, stats) = match sip_pcap::read_capture_files(&files) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("capture read failed: {e}");
            std::process::exit(2);
        }
    };

    let mut strategies = vec![CorrelateStrategy::HeaderToken { headers: args.correlate.clone() }];
    if !args.no_param_correlate {
        for spec in &args.correlate_param {
            let Some((header, param)) = spec.split_once(':').filter(|(h, p)| !h.is_empty() && !p.is_empty())
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
    let cfg = FlowConfig { strategies, dedup_window_us: DEFAULT_DEDUP_WINDOW_US };

    if let Some(query) = load_query(&args) {
        // The query owns correlation when it says so: which strategies ran is
        // part of what a recorded query means, not ambient CLI state.
        let cfg = query.correlate.clone().unwrap_or(cfg);
        let flows = build_flows(&datagrams, &cfg);
        run_query(&flows, &stats, &query);
        return;
    }

    let flows = build_flows(&datagrams, &cfg);

    if args.json {
        // Pretty + deterministic field order: the emit is committed as
        // fixtures downstream, so its git/jq diffs must stay line-readable.
        let v = sip_pcap::emit::flows_to_json(&flows, &stats);
        println!("{}", serde_json::to_string_pretty(&v).expect("model JSON serializes"));
        return;
    }

    let selected: Vec<&CallGroup> =
        flows.groups.iter().filter(|g| group_matches(g, &flows.legs, &args)).collect();

    eprintln!(
        "# files={} {stats}\n# sip-messages={} capture-dups={} parse-failed={} non-sip={} legs={} call-groups={} matched={}",
        files.len(),
        flows.stats.sip_messages,
        flows.stats.capture_dups,
        flows.stats.parse_failed,
        flows.stats.non_sip,
        flows.legs.len(),
        flows.groups.len(),
        selected.len(),
    );

    if args.list {
        for (n, g) in selected.iter().enumerate() {
            print_list_line(n, g, &flows.legs);
        }
    } else {
        for (n, g) in selected.iter().take(args.limit).enumerate() {
            print_ladder(n, g, &flows.legs, args.full);
        }
        if selected.len() > args.limit {
            println!(
                "… {} more matching call groups (raise --limit or add filters; --list shows all)",
                selected.len() - args.limit
            );
        }
    }
}

/// Load `--query` / `--query-json`, exiting with the parse error's own path
/// and reason — a mistyped predicate must never widen the match set silently.
fn load_query(args: &Args) -> Option<Query> {
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
    match Query::from_json(&value) {
        Ok(q) => Some(q),
        Err(e) => {
            eprintln!("query rejected at {e}");
            std::process::exit(2);
        }
    }
}

/// Select, expand to neighbours, project. The three phases stay visible here
/// because they are what the query document is made of.
fn run_query(flows: &sip_pcap::flow::Flows, decode: &sip_pcap::DecodeStats, query: &Query) {
    let hits = select_groups(flows, query);
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
            let v = sip_pcap::emit::flows_to_json_selected(flows, decode, &wanted);
            println!("{}", serde_json::to_string_pretty(&v).expect("model JSON serializes"));
        }
    }
}

fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut in_dir: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| {
                            // Name-based, and deliberately loose: `.pcap`,
                            // `.pcapng` and either gzipped. The container and
                            // compression are re-detected from the bytes.
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.contains(".pcap"))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Ring files: oldest-first by mtime so fragment reassembly and
            // ladders see time monotonically.
            in_dir.sort_by_key(|p| {
                std::fs::metadata(p).and_then(|m| m.modified()).ok()
            });
            files.extend(in_dir);
        } else {
            files.push(p.clone());
        }
    }
    files
}

fn group_matches(group: &CallGroup, legs: &[FlowLeg], args: &Args) -> bool {
    let any_leg = |pred: &dyn Fn(&FlowLeg) -> bool| group.legs.iter().any(|&l| pred(&legs[l]));

    if let Some(cid) = &args.call_id {
        if !any_leg(&|l| l.call_id.contains(cid.as_str())) {
            return false;
        }
    }
    if let Some(f) = &args.from {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|inv| inv.from_uri.contains(f.as_str()))) {
            return false;
        }
    }
    if let Some(t) = &args.to {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|inv| inv.to_uri.contains(t.as_str()))) {
            return false;
        }
    }
    if let Some(r) = &args.ruri {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|inv| inv.ruri.contains(r.as_str()))) {
            return false;
        }
    }
    if let Some(m) = &args.method {
        if !any_leg(&|l| {
            l.msgs.iter().any(|rec| {
                matches!(&rec.parsed, SipMessage::Request(r) if r.method.as_str().eq_ignore_ascii_case(m))
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
                rec.parsed
                    .get_header(name)
                    .iter()
                    .any(|v| want.is_none_or(|w| v.contains(w)))
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

fn fmt_ts(ts_us: u64) -> String {
    let secs = ts_us / 1_000_000;
    let ms = (ts_us % 1_000_000) / 1_000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

fn leg_label(pos: usize) -> String {
    match pos {
        0 => "A".to_string(),
        n => format!("B{n}"),
    }
}

fn summary_line(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!("{} {} (CSeq {} {})", r.method, r.uri, r.cseq.seq, r.cseq.method),
        SipMessage::Response(r) => {
            format!("{} {} (CSeq {} {})", r.status, r.reason, r.cseq.seq, r.cseq.method)
        }
    }
}

fn final_str(leg: &FlowLeg) -> String {
    match (leg.invite.is_some(), leg.final_status) {
        (false, _) => "-".to_string(),
        (true, Some(c)) => c.to_string(),
        (true, None) => "NONE".to_string(),
    }
}

fn invite_strs(leg: &FlowLeg) -> (&str, &str, &str) {
    leg.invite
        .as_ref()
        .map(|inv| (inv.ruri.as_str(), inv.from_uri.as_str(), inv.to_uri.as_str()))
        .unwrap_or(("-", "-", "-"))
}

fn print_list_line(n: usize, group: &CallGroup, legs: &[FlowLeg]) {
    let a = &legs[group.legs[0]];
    let a_tokens = a.tokens();
    let token = a_tokens.iter().next().copied().unwrap_or("-");
    let (ruri, from, to) = invite_strs(a);
    let dur_ms = (legs[group.legs.last().copied().unwrap_or(group.legs[0])].t_last())
        .saturating_sub(a.t_first())
        / 1_000;
    println!(
        "#{n:<4} {} legs={} token={token} ruri={ruri} from={from} to={to} finals=[{}] ring={} term={} dur={dur_ms}ms msgs={}",
        fmt_ts(a.t_first()),
        group.legs.len(),
        group.legs.iter().map(|&l| final_str(&legs[l])).collect::<Vec<_>>().join(","),
        group.legs.iter().any(|&l| legs[l].saw_180),
        group
            .legs
            .iter()
            .filter_map(|&l| legs[l].terminated_by)
            .next()
            .map(|t| t.as_str())
            .unwrap_or("-"),
        group.legs.iter().map(|&l| legs[l].msgs.len()).sum::<usize>(),
    );
}

fn print_ladder(n: usize, group: &CallGroup, legs: &[FlowLeg], full: bool) {
    let t0 = legs[group.legs[0]].t_first();
    println!("\n━━━ call group #{n} ─ {} ─ legs={} ━━━", fmt_ts(t0), group.legs.len());
    for (pos, &l) in group.legs.iter().enumerate() {
        let leg = &legs[l];
        let tokens: Vec<&str> = leg.tokens().into_iter().collect();
        let (ruri, from, to) = invite_strs(leg);
        println!(
            "  leg {}: Call-ID {}\n         INVITE {}  {} -> {}\n         final: {}  term: {}  msgs: {}{}",
            leg_label(pos),
            leg.call_id,
            ruri,
            from,
            to,
            final_str(leg),
            leg.terminated_by.map(|t| t.as_str()).unwrap_or("-"),
            leg.msgs.len(),
            if tokens.is_empty() { String::new() } else { format!("  token: {}", tokens.join(",")) },
        );
    }
    // Merged chronological ladder across all legs of the group.
    let mut lines: Vec<(u64, String, Option<String>)> = Vec::new();
    for (pos, &l) in group.legs.iter().enumerate() {
        let leg = &legs[l];
        for m in &leg.msgs {
            let line = format!(
                "  {:>9.3}s {} {:>21} → {:<21} [{}] {}{}",
                (m.ts_us.saturating_sub(t0)) as f64 / 1e6,
                fmt_ts(m.ts_us),
                m.src.to_string(),
                m.dst.to_string(),
                leg_label(pos),
                summary_line(&m.parsed),
                if m.retx { "  (retx)" } else { "" },
            );
            let body = full.then(|| {
                String::from_utf8_lossy(m.raw())
                    .lines()
                    .map(|l| format!("      | {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            lines.push((m.ts_us, line, body));
        }
    }
    lines.sort_by_key(|line| line.0);
    for (_, line, body) in lines {
        println!("{line}");
        if let Some(b) = body {
            println!("{b}");
        }
    }
}
