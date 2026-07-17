//! sipflow — extract full SIP callflows (including the B2BUA a-leg/b-leg
//! crossing) from tcpdump ring files.
//!
//! Pipeline: decode pcaps (`sip_pcap`, with IP-fragment reassembly) → build
//! the flow model (`sip_pcap::flow`: parse with the real `sip-message`
//! parser, group messages into **legs** by Call-ID, correlate legs into
//! **calls** by relayed token with a conservative From/To+host fallback) →
//! filter and print. This bin is a presenter — selection and text layout
//! only; everything model-shaped lives in the library.
//!
//! Filters select whole call groups (give an a-leg Call-ID, get the b-leg
//! ladder too). `--final-status none` finds calls whose initial INVITE never
//! got a final response — the "everything times out" triage query.
//!
//! Examples:
//!   sipflow /tmp/sipcap --list
//!   sipflow /tmp/sipcap --call-id 7f3a... --full
//!   sipflow /tmp/sipcap --final-status none
//!   sipflow /tmp/sipcap --ruri 166601009 --final-status 5xx
//!   sipflow /tmp/sipcap --json > flows.json

use std::path::PathBuf;

use clap::Parser as ClapParser;
use sip_message::SipMessage;
use sip_pcap::flow::{build_flows, CallGroup, FlowConfig, FlowLeg};

#[derive(ClapParser, Debug)]
#[command(
    name = "sipflow",
    about = "pcap → SIP callflow extractor with B2BUA leg correlation (see module doc)"
)]
struct Args {
    /// pcap files or directories (a directory expands to its *.pcap* files).
    /// Ring files are ordered oldest-first automatically.
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

    /// Correlation headers whose (relayed) value ties B2BUA legs together.
    #[arg(long = "correlate", default_values_t = ["X-Loadgen-Id".to_string(), "X-Api-Call".to_string()])]
    correlate: Vec<String>,

    /// Disable the From/To+host adjacency fallback (token-only correlation).
    #[arg(long, default_value_t = false)]
    no_fromto_fallback: bool,

    /// Emit the FULL flow model (raw payloads base64-encoded, parsed
    /// summaries, hops, match evidence, decode counters) as JSON on stdout —
    /// schema documented on `sip_pcap::emit::flows_to_json`. Whole-capture
    /// emit: selection filters and text layout flags do not apply.
    #[arg(
        long,
        default_value_t = false,
        conflicts_with_all = [
            "call_id", "from", "to", "ruri", "method", "headers",
            "final_status", "token", "list", "full", "limit",
        ]
    )]
    json: bool,

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
        eprintln!("no pcap files found under {:?}", args.inputs);
        std::process::exit(2);
    }

    let (datagrams, stats) = match sip_pcap::read_pcap_files(&files) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pcap read failed: {e}");
            std::process::exit(2);
        }
    };

    let cfg = FlowConfig {
        correlate_headers: args.correlate.clone(),
        fromto_fallback: !args.no_fromto_fallback,
    };
    let flows = build_flows(&datagrams, &cfg);

    if args.json {
        println!("{}", sip_pcap::emit::flows_to_json(&flows, &stats));
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

fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut in_dir: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| {
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
        if !any_leg(&|l| l.tokens.iter().any(|t| t.contains(tok.as_str()))) {
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
    let token = a.tokens.iter().next().map(|s| s.as_str()).unwrap_or("-");
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
        let tokens: Vec<&str> = leg.tokens.iter().map(|s| s.as_str()).collect();
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
