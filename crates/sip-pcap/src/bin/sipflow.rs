//! sipflow — extract full SIP callflows (including the B2BUA a-leg/b-leg
//! crossing) from tcpdump ring files.
//!
//! Pipeline: decode pcaps (`sip_pcap`, with IP-fragment reassembly) → parse
//! every UDP datagram with the real `sip-message` `CustomParser` → group
//! messages into **legs** by Call-ID → correlate legs into **calls**:
//!   1. shared relayed correlation-header token (`X-Loadgen-Id`, `X-Api-Call`
//!      — the B2BUA's `relay_headers` mint point copies these onto every
//!      originated leg, see b2bua/src/rules/relay.rs), then
//!   2. a conservative fallback for token-less captures: legs with identical
//!      From/To URIs whose INVITEs cross one shared host (a-leg dst ip ==
//!      b-leg src ip) within 5 s, joined only when the pairing is unambiguous.
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

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser as ClapParser;
use sip_message::parser::SipParser;
use sip_message::{CustomParser, Method, SipMessage};

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

/// One captured SIP message.
struct MsgRec {
    ts_us: u64,
    src: SocketAddr,
    dst: SocketAddr,
    msg: SipMessage,
    /// Same transaction-key seen before in this leg (SIP retransmission).
    retx: bool,
}

/// All messages sharing one Call-ID.
struct Leg {
    call_id: String,
    msgs: Vec<MsgRec>,
    /// From the first INVITE (if any): (ruri, from-uri, to-uri, cseq).
    invite: Option<(String, String, String, u32)>,
    /// First final (>=200) response to that INVITE.
    final_status: Option<u16>,
    saw_180: bool,
    terminated_by: Option<&'static str>,
    tokens: BTreeSet<String>,
}

impl Leg {
    fn t_first(&self) -> u64 {
        self.msgs.first().map(|m| m.ts_us).unwrap_or(0)
    }
    fn t_last(&self) -> u64 {
        self.msgs.last().map(|m| m.ts_us).unwrap_or(0)
    }
    /// (src, dst) of the first INVITE — the leg's direction of establishment.
    fn invite_addrs(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.msgs
            .iter()
            .find(|m| matches!(&m.msg, SipMessage::Request(r) if r.method == Method::Invite))
            .map(|m| (m.src, m.dst))
    }
}

fn main() {
    let args = Args::parse();
    let files = expand_inputs(&args.inputs);
    if files.is_empty() {
        eprintln!("no pcap files found under {:?}", args.inputs);
        std::process::exit(2);
    }

    let (mut datagrams, stats) = match sip_pcap::read_pcap_files(&files) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pcap read failed: {e}");
            std::process::exit(2);
        }
    };
    datagrams.sort_by_key(|d| d.ts_us);

    // Capture-level dedup: `-i any` sees the same packet on veth AND bridge.
    // A genuine SIP retransmission is >=500 ms (Timer T1) away; identical
    // (src,dst,payload) within 200 ms is the capture stack, not the peer.
    let mut dedup: HashMap<u64, u64> = HashMap::new();
    let mut capture_dups = 0u64;
    let mut parse_fail = 0u64;
    let mut non_sip = 0u64;

    let parser = CustomParser::new();
    let mut legs: Vec<Leg> = Vec::new();
    let mut leg_by_call_id: HashMap<String, usize> = HashMap::new();

    for d in &datagrams {
        if !looks_like_sip(&d.payload) {
            non_sip += 1;
            continue;
        }
        let key = hash_datagram(d);
        if let Some(prev) = dedup.get(&key) {
            if d.ts_us.saturating_sub(*prev) < 200_000 {
                capture_dups += 1;
                continue;
            }
        }
        dedup.insert(key, d.ts_us);

        let msg = match parser.parse(&d.payload) {
            Ok(m) => m,
            Err(_) => {
                parse_fail += 1;
                continue;
            }
        };
        let call_id = match &msg {
            SipMessage::Request(r) => r.call_id.clone(),
            SipMessage::Response(r) => r.call_id.clone(),
        };
        let idx = *leg_by_call_id.entry(call_id.clone()).or_insert_with(|| {
            legs.push(Leg {
                call_id,
                msgs: Vec::new(),
                invite: None,
                final_status: None,
                saw_180: false,
                terminated_by: None,
                tokens: BTreeSet::new(),
            });
            legs.len() - 1
        });
        ingest(&mut legs[idx], d.ts_us, d.src, d.dst, msg, &args.correlate);
    }

    // --- correlate legs into call groups -----------------------------------
    let group_of = correlate(&legs, args.no_fromto_fallback);
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (leg_idx, g) in group_of.iter().enumerate() {
        groups.entry(*g).or_default().push(leg_idx);
    }
    let mut ordered: Vec<Vec<usize>> = groups.into_values().collect();
    for g in &mut ordered {
        g.sort_by_key(|&l| legs[l].t_first());
    }
    ordered.sort_by_key(|g| legs[g[0]].t_first());

    // --- filters ------------------------------------------------------------
    let selected: Vec<&Vec<usize>> =
        ordered.iter().filter(|g| group_matches(g, &legs, &args)).collect();

    // --- output --------------------------------------------------------------
    eprintln!(
        "# files={} {stats}\n# sip-messages={} capture-dups={} parse-failed={} non-sip={} legs={} call-groups={} matched={}",
        files.len(),
        legs.iter().map(|l| l.msgs.len()).sum::<usize>(),
        capture_dups,
        parse_fail,
        non_sip,
        legs.len(),
        ordered.len(),
        selected.len(),
    );

    if args.list {
        for (n, g) in selected.iter().enumerate() {
            print_list_line(n, g, &legs);
        }
    } else {
        for (n, g) in selected.iter().take(args.limit).enumerate() {
            print_ladder(n, g, &legs, args.full);
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

/// Cheap pre-filter so RTP/STUN/DNS on captured ports never reaches the parser.
fn looks_like_sip(payload: &[u8]) -> bool {
    if payload.len() < 16 {
        return false;
    }
    let head = &payload[..payload.len().min(256)];
    (head.starts_with(b"SIP/2.0 ") || head.windows(9).any(|w| w == b" SIP/2.0\r"))
        && payload[0].is_ascii_uppercase()
}

fn hash_datagram(d: &sip_pcap::Datagram) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.src.hash(&mut h);
    d.dst.hash(&mut h);
    d.payload.hash(&mut h);
    h.finish()
}

fn ingest(
    leg: &mut Leg,
    ts_us: u64,
    src: SocketAddr,
    dst: SocketAddr,
    msg: SipMessage,
    correlate: &[String],
) {
    for name in correlate {
        for v in msg.get_header(name) {
            let v = v.trim();
            if !v.is_empty() {
                leg.tokens.insert(v.to_string());
            }
        }
    }
    match &msg {
        SipMessage::Request(r) => {
            if r.method == Method::Invite && leg.invite.is_none() {
                leg.invite = Some((
                    r.uri.clone(),
                    r.from.uri.clone(),
                    r.to.uri.clone(),
                    r.cseq.seq,
                ));
            }
            if r.method == Method::Bye && leg.terminated_by.is_none() {
                leg.terminated_by = Some("BYE");
            }
            if r.method == Method::Cancel && leg.terminated_by.is_none() {
                leg.terminated_by = Some("CANCEL");
            }
        }
        SipMessage::Response(r) => {
            if r.cseq.method == Method::Invite {
                if r.status == 180 {
                    leg.saw_180 = true;
                }
                let initial = leg.invite.as_ref().map(|(_, _, _, seq)| *seq);
                if r.status >= 200
                    && leg.final_status.is_none()
                    && (initial.is_none() || initial == Some(r.cseq.seq))
                {
                    leg.final_status = Some(r.status);
                }
            }
        }
    }
    // Retransmission tag: same direction + same transaction key already seen.
    let retx = leg.msgs.iter().any(|m| {
        m.src == src
            && m.dst == dst
            && match (&m.msg, &msg) {
                (SipMessage::Request(a), SipMessage::Request(b)) => {
                    a.method == b.method
                        && a.cseq.seq == b.cseq.seq
                        && a.via.first().branch == b.via.first().branch
                }
                (SipMessage::Response(a), SipMessage::Response(b)) => {
                    a.status == b.status
                        && a.cseq == b.cseq
                        && a.via.first().branch == b.via.first().branch
                }
                _ => false,
            }
    });
    leg.msgs.push(MsgRec { ts_us, src, dst, msg, retx });
}

/// Union-find correlation: tokens first, then the conservative From/To+host
/// adjacency fallback for token-less captures.
fn correlate(legs: &[Leg], no_fromto_fallback: bool) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..legs.len()).collect();
    fn find(parent: &mut Vec<usize>, mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    fn union(parent: &mut Vec<usize>, a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }

    let mut by_token: HashMap<&str, usize> = HashMap::new();
    for (i, leg) in legs.iter().enumerate() {
        for t in &leg.tokens {
            match by_token.get(t.as_str()) {
                Some(&j) => union(&mut parent, i, j),
                None => {
                    by_token.insert(t, i);
                }
            }
        }
    }

    if !no_fromto_fallback {
        // Candidate pairs: same From+To URIs, INVITEs crossing one shared host
        // (a-leg's INVITE destination ip == b-leg's INVITE source ip — the
        // B2BUA), within 5 s. Joined greedily nearest-first, one partner per
        // leg, and only for legs not already token-correlated.
        let mut pairs: Vec<(u64, usize, usize)> = Vec::new();
        for i in 0..legs.len() {
            if !legs[i].tokens.is_empty() {
                continue;
            }
            let (Some((_, fi, ti, _)), Some((si, di))) = (&legs[i].invite, legs[i].invite_addrs())
            else {
                continue;
            };
            for j in i + 1..legs.len() {
                if !legs[j].tokens.is_empty() {
                    continue;
                }
                let (Some((_, fj, tj, _)), Some((sj, dj))) =
                    (&legs[j].invite, legs[j].invite_addrs())
                else {
                    continue;
                };
                if fi != fj || ti != tj {
                    continue;
                }
                // The B2BUA crossing: one leg's INVITE lands on the host the
                // other leg's INVITE departs from (either orientation).
                if di.ip() != sj.ip() && dj.ip() != si.ip() {
                    continue;
                }
                let dt = legs[j].t_first().abs_diff(legs[i].t_first());
                if dt <= 5_000_000 {
                    pairs.push((dt, i, j));
                }
            }
        }
        pairs.sort_by_key(|(dt, _, _)| *dt);
        let mut used: Vec<bool> = vec![false; legs.len()];
        for (_, i, j) in pairs {
            if !used[i] && !used[j] {
                used[i] = true;
                used[j] = true;
                union(&mut parent, i, j);
            }
        }
    }

    (0..legs.len()).map(|i| find(&mut parent, i)).collect()
}

fn group_matches(group: &[usize], legs: &[Leg], args: &Args) -> bool {
    let any_leg = |pred: &dyn Fn(&Leg) -> bool| group.iter().any(|&l| pred(&legs[l]));

    if let Some(cid) = &args.call_id {
        if !any_leg(&|l| l.call_id.contains(cid.as_str())) {
            return false;
        }
    }
    if let Some(f) = &args.from {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|(_, fu, _, _)| fu.contains(f.as_str()))) {
            return false;
        }
    }
    if let Some(t) = &args.to {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|(_, _, tu, _)| tu.contains(t.as_str()))) {
            return false;
        }
    }
    if let Some(r) = &args.ruri {
        if !any_leg(&|l| l.invite.as_ref().is_some_and(|(ru, _, _, _)| ru.contains(r.as_str()))) {
            return false;
        }
    }
    if let Some(m) = &args.method {
        if !any_leg(&|l| {
            l.msgs.iter().any(|rec| {
                matches!(&rec.msg, SipMessage::Request(r) if r.method.as_str().eq_ignore_ascii_case(m))
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
                rec.msg
                    .get_header(name)
                    .iter()
                    .any(|v| want.is_none_or(|w| v.contains(w)))
            })
        }) {
            return false;
        }
    }
    if let Some(fs) = &args.final_status {
        let matches_leg = |l: &Leg| -> bool {
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

fn final_str(leg: &Leg) -> String {
    match (leg.invite.is_some(), leg.final_status) {
        (false, _) => "-".to_string(),
        (true, Some(c)) => c.to_string(),
        (true, None) => "NONE".to_string(),
    }
}

fn print_list_line(n: usize, group: &[usize], legs: &[Leg]) {
    let a = &legs[group[0]];
    let token = a.tokens.iter().next().map(|s| s.as_str()).unwrap_or("-");
    let (ruri, from, to) = a
        .invite
        .as_ref()
        .map(|(r, f, t, _)| (r.as_str(), f.as_str(), t.as_str()))
        .unwrap_or(("-", "-", "-"));
    let dur_ms = (legs[group.last().copied().unwrap_or(group[0])].t_last())
        .saturating_sub(a.t_first())
        / 1_000;
    println!(
        "#{n:<4} {} legs={} token={token} ruri={ruri} from={from} to={to} finals=[{}] ring={} term={} dur={dur_ms}ms msgs={}",
        fmt_ts(a.t_first()),
        group.len(),
        group.iter().map(|&l| final_str(&legs[l])).collect::<Vec<_>>().join(","),
        group.iter().any(|&l| legs[l].saw_180),
        group
            .iter()
            .filter_map(|&l| legs[l].terminated_by)
            .next()
            .unwrap_or("-"),
        group.iter().map(|&l| legs[l].msgs.len()).sum::<usize>(),
    );
}

fn print_ladder(n: usize, group: &[usize], legs: &[Leg], full: bool) {
    let t0 = legs[group[0]].t_first();
    println!("\n━━━ call group #{n} ─ {} ─ legs={} ━━━", fmt_ts(t0), group.len());
    for (pos, &l) in group.iter().enumerate() {
        let leg = &legs[l];
        let tokens: Vec<&str> = leg.tokens.iter().map(|s| s.as_str()).collect();
        let (ruri, from, to) = leg
            .invite
            .as_ref()
            .map(|(r, f, t, _)| (r.as_str(), f.as_str(), t.as_str()))
            .unwrap_or(("-", "-", "-"));
        println!(
            "  leg {}: Call-ID {}\n         INVITE {}  {} -> {}\n         final: {}  term: {}  msgs: {}{}",
            leg_label(pos),
            leg.call_id,
            ruri,
            from,
            to,
            final_str(leg),
            leg.terminated_by.unwrap_or("-"),
            leg.msgs.len(),
            if tokens.is_empty() { String::new() } else { format!("  token: {}", tokens.join(",")) },
        );
    }
    // Merged chronological ladder across all legs of the group.
    let mut lines: Vec<(u64, String, Option<String>)> = Vec::new();
    for (pos, &l) in group.iter().enumerate() {
        let leg = &legs[l];
        for m in &leg.msgs {
            let line = format!(
                "  {:>9.3}s {} {:>21} → {:<21} [{}] {}{}",
                (m.ts_us.saturating_sub(t0)) as f64 / 1e6,
                fmt_ts(m.ts_us),
                m.src.to_string(),
                m.dst.to_string(),
                leg_label(pos),
                summary_line(&m.msg),
                if m.retx { "  (retx)" } else { "" },
            );
            let body = full.then(|| {
                let raw = match &m.msg {
                    SipMessage::Request(r) => &r.raw,
                    SipMessage::Response(r) => &r.raw,
                };
                String::from_utf8_lossy(raw)
                    .lines()
                    .map(|l| format!("      | {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            lines.push((m.ts_us, line, body));
        }
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, line, body) in lines {
        println!("{line}");
        if let Some(b) = body {
            println!("{b}");
        }
    }
}
