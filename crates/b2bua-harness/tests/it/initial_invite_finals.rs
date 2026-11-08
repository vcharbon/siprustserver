//! Every final this stack mints for an initial INVITE, read off the sources and
//! pinned to its class, so one with no answer behind it cannot appear unnoticed.
//! A caller past her 100 acts on the class alone (RFC 3261 §17.1.1.2):
//!
//! - **internal error**: anything the stack cannot do. A 5xx with no `Reason`
//!   (RFC 3326): nothing is known to be wrong with the call.
//! - **stated**: the status a decision engine or a peer stated.
//! - **refused answer**: an engine's answer whose status is no final it may state,
//!   the plain 500 under the headers that engine stated.
//! - **RFC 3261's own**: 400 (§8.1.1.3, §8.1.1.8), 420 (§8.2.2.3), 482 (§8.2.2.2), 483
//!   (§16.3), 487 (§9.2).
//! - **no callee answered**: the generic defaults of a callee's silence, 480 or 408.
//!
//! The scan reads the places a final is built: [`SITES`], [`RESPOND`] and the
//! [`FOLD_CODE`] of a fold's payload. A new one fails
//! [`every_final_the_stack_mints_for_an_initial_invite_has_its_class`] until a row
//! states its class, and a class holds only its own statuses.

use std::path::{Path, PathBuf};

use Class::*;

/// What a final is to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    InternalError,
    Stated,
    RefusedAnswer,
    Rfc3261,
    NoCalleeAnswered,
    /// Answers another request than an initial INVITE: not this table's rule.
    OtherRequest,
    /// Carries the status its caller states.
    Builder,
}

/// One site: its file under `crates/`, the status token it states and its class.
type Row = (&'static str, &'static str, Class);

const FINALS: &[Row] = &[
    ("b2bua-core/src/decision/apply_reject.rs", "500", RefusedAnswer),
    ("b2bua-core/src/decision/apply_reject.rs", "500", RefusedAnswer),
    ("b2bua-core/src/decision/apply_reject.rs", "code", Stated),
    ("b2bua-core/src/decision/apply_reject.rs", "redirect.code", Stated),
    ("b2bua-core/src/decision/apply_reject.rs", "reject.reject_code", Stated),
    // A Relay treatment with no captured failure to relay.
    ("b2bua-core/src/decision/failover_fold.rs", "480", NoCalleeAnswered),
    ("b2bua-core/src/decision/failover_fold.rs", "503", InternalError),
    // RFC 3261 §16.3, a required extension the call withholds (§8.2.2.3), the
    // destination allow-list, a route that does not mint.
    ("b2bua-core/src/decision/route_fold.rs", "483", Rfc3261),
    ("b2bua-core/src/decision/route_fold.rs", "400", Rfc3261),
    ("b2bua-core/src/decision/route_fold.rs", "420", Rfc3261),
    ("b2bua-core/src/decision/route_fold.rs", "503", InternalError),
    ("b2bua-core/src/decision/route_fold.rs", "500", InternalError),
    // A Relay treatment with no captured failure to relay; an unavailable
    // decision; a redirect whose target does not read.
    ("b2bua-core/src/initial_invite.rs", "480", NoCalleeAnswered),
    ("b2bua-core/src/initial_invite.rs", "503", InternalError),
    ("b2bua-core/src/initial_invite.rs", "500", RefusedAnswer),
    ("b2bua-core/src/initial_invite.rs", "status", Builder),
    // What the callee answered, relayed.
    ("b2bua-core/src/rules/actions/relay_response.rs", "status", Stated),
    ("b2bua-core/src/rules/actions/relay_response.rs", "status", Stated),
    ("b2bua-core/src/rule_chain/message_cap.rs", "503", InternalError),
    ("b2bua-core/src/rules/invariants.rs", "503", InternalError),
    ("b2bua-core/src/rules/operator_release.rs", "503", InternalError),
    // An undecided call no node asks again.
    ("b2bua-core/src/undecided.rs", "503", InternalError),
    // A BYE on the caller's early dialog ends her INVITE (RFC 3261 §15.1.2).
    ("b2bua-core/src/rules/defaults/core_rules.rs", "487", Rfc3261),
    // A no-answer deadline with no consult to raise, and the setup deadline.
    ("b2bua-core/src/rules/defaults/core_rules.rs", "480", NoCalleeAnswered),
    ("b2bua-core/src/rules/defaults/core_rules.rs", "408", NoCalleeAnswered),
    // The let-stand of a timeout: `timeout_final`, 480 or 408.
    ("b2bua-core/src/rules/defaults/failover_rules.rs", "status", NoCalleeAnswered),
    ("b2bua-core/src/rules/defaults/failover_rules.rs", "500", RefusedAnswer),
    ("b2bua-core/src/rules/defaults/failover_rules.rs", "status", Stated),
    ("b2bua-core/src/rules/defaults/failover_rules.rs", "status", Stated),
    ("b2bua-core/src/rules/defaults/failover_rules.rs", "500", RefusedAnswer),
    ("b2bua-core/src/rules/relay/a_leg_response.rs", "status", Builder),
    ("b2bua-core/src/rules/relay/cancelled_final.rs", "487", Rfc3261),
    ("b2bua-core/src/rules/actions/respond.rs", "status", Builder),
    ("b2bua-core/src/rules/actions/respond.rs", "status", Builder),
    ("b2bua-core/src/rules/actions/respond.rs", "status", Builder),
    ("b2bua-core/src/rules/actions/respond.rs", "status", Builder),
    ("b2bua-core/src/rules/actions/respond.rs", "status", Builder),
    // The admission ladder's refusal.
    ("b2bua/src/admission/render.rs", "503", InternalError),
    // The turn's own refusals: no From tag; a cordon or a spent setup budget.
    ("b2bua/src/router/initial_turn.rs", "400", Rfc3261),
    ("b2bua/src/router/initial_turn.rs", "503", InternalError),
    ("b2bua/src/router/responses.rs", "482", Rfc3261),
    ("b2bua/src/router/responses.rs", "500", InternalError),
    ("b2bua/src/router/responses.rs", "500", InternalError),
    ("b2bua/src/router/responses.rs", "487", OtherRequest),
    ("b2bua/src/router/responses.rs", "481", OtherRequest),
    ("b2bua/src/router/responses.rs", "405", OtherRequest),
    ("b2bua/src/router/responses.rs", "500", OtherRequest),
    // The in-dialog request of a turn past a per-call cap (ADR-0057 §4).
    ("b2bua/src/router/responses.rs", "500", OtherRequest),
    ("b2bua/src/router/responses.rs", "status", OtherRequest),
    ("b2bua/src/router/store_fault_gate.rs", "500", OtherRequest),
    // A fold's stated code: the bare 503 of an unanswered consult and of a chain
    // with no way to fail over, named once; a trace's reading of a refusal.
    ("b2bua-core/src/failure_unavailable.rs", "STATUS", Builder),
    ("b2bua/src/router/callouts.rs", "failure_unavailable::STATUS", Builder),
    ("b2bua/src/trace/intake.rs", "code", OtherRequest),
    ("sip-txn/src/layer/server_cancel.rs", "487", Rfc3261),
    ("sip-txn/src/layer/unanswered.rs", "status", OtherRequest),
];

/// A call that builds a final, and which of its arguments states the status.
const SITES: [(&str, usize); 3] =
    [("reject_call(", 2), ("response_to_a_leg(", 3), ("generate_response(", 1)];

/// The struct a rule states a final with, by its `status` field.
const RESPOND: &str = "RespondToALeg {";

/// The field a fold's JSON payload states a final's status in.
const FOLD_CODE: &str = "\"code\":";

/// The sources a final is minted in, under `crates/`.
const ROOTS: [&str; 3] = ["b2bua-core/src", "b2bua/src", "sip-txn/src"];

/// A path that holds tests or the deterministic drivers they run on.
fn is_test_source(path: &str) -> bool {
    path.contains("test") || path.contains("turn_driver") || path.contains("/testkit")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a source directory").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `source` with its comments and its inline test module left out.
fn code_of(source: &str) -> String {
    let code = source.split("#[cfg(test)]\nmod tests {").next().unwrap_or_default();
    code.lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The top-level arguments of the call whose text starts just past its `(`.
fn arguments(call: &str) -> Vec<String> {
    let (mut depth, mut current, mut args) = (0i32, String::new(), Vec::new());
    for c in call.chars() {
        match c {
            ')' | ']' | '}' if depth == 0 => break,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    args.push(current.trim().to_string());
    args
}

/// One minted final found in the sources: its status token, and whether the
/// site itself states it carries no header of a decision's.
#[derive(Debug, Clone)]
struct Found {
    file: String,
    status: String,
    bare: bool,
}

/// The finals `code` of `file` mints: a status of 300 and up, or stated by name.
fn finals_in(file: &str, code: &str, found: &mut Vec<Found>) {
    let mut push = |status: String, bare: bool| {
        let provisional_or_success =
            status.parse::<u16>().is_ok_and(|literal| !(300..=699).contains(&literal));
        if !provisional_or_success {
            found.push(Found { file: file.to_string(), status, bare });
        }
    };
    for (site, at) in SITES {
        for (start, _) in code.match_indices(site) {
            if code[..start].ends_with("fn ") {
                continue;
            }
            let args = arguments(&code[start + site.len()..]);
            let bare = match site {
                "reject_call(" => args.get(4).is_some_and(|a| a == "None"),
                _ => true,
            };
            push(args.get(at).cloned().unwrap_or_default(), bare);
        }
    }
    for (start, _) in code.match_indices(RESPOND) {
        let fields = arguments(&code[start + RESPOND.len()..]);
        let Some(status) = fields.iter().find(|f| f.starts_with("status")) else {
            continue; // `{ .. }`: a match on the action, no final.
        };
        let after = &code[start..];
        let closes = after.find('}').map_or(after, |end| &after[end + 1..]);
        if closes.trim_start().starts_with("=>") {
            continue; // Its interpreter, which mints what the rules state.
        }
        let status = status.strip_prefix("status:").unwrap_or(status).trim().to_string();
        let bare = fields.iter().any(|f| f == "header_updates: vec![]");
        push(status, bare);
    }
    for (start, _) in code.match_indices(FOLD_CODE) {
        let value = code[start + FOLD_CODE.len()..].split([',', '\n', '}']).next();
        push(value.unwrap_or_default().trim().to_string(), true);
    }
}

fn found_in_sources() -> Vec<Found> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    for root in ROOTS {
        let mut sources = Vec::new();
        rust_sources(&crates.join(root), &mut sources);
        for source in sources {
            let relative = source.strip_prefix(&crates).expect("under crates/");
            let file = relative.to_string_lossy().into_owned();
            if is_test_source(&file) {
                continue;
            }
            let text = std::fs::read_to_string(&source).expect("a readable source");
            finals_in(&file, &code_of(&text), &mut found);
        }
    }
    found
}

/// Whether `status` may hold `class`, a site's own statement of it.
fn fits(class: Class, status: &str, bare: bool) -> bool {
    let literal = status.parse::<u16>().ok();
    match class {
        InternalError => bare && literal.is_some_and(|s| (500..=599).contains(&s)),
        Stated | Builder => literal.is_none(),
        RefusedAnswer => literal == Some(500),
        Rfc3261 => matches!(literal, Some(400 | 420 | 482 | 483 | 487)),
        NoCalleeAnswered => matches!(literal, Some(480 | 408)) || status == "status",
        OtherRequest => true,
    }
}

/// `items` less one occurrence of each of `taken`.
fn without<T: Clone + PartialEq>(items: &[T], taken: &[T]) -> Vec<T> {
    let mut left = items.to_vec();
    for item in taken {
        if let Some(at) = left.iter().position(|l| l == item) {
            left.remove(at);
        }
    }
    left
}

#[test]
fn every_final_the_stack_mints_for_an_initial_invite_has_its_class() {
    let found = found_in_sources();

    let mut sites: Vec<(String, String)> =
        found.iter().map(|f| (f.file.clone(), f.status.clone())).collect();
    let mut rows: Vec<(String, String)> =
        FINALS.iter().map(|(file, status, _)| (file.to_string(), status.to_string())).collect();
    sites.sort();
    rows.sort();
    let unlisted = without(&sites, &rows);
    let stale = without(&rows, &sites);
    assert!(unlisted.is_empty(), "a final with no row states no class: {unlisted:?}");
    assert!(stale.is_empty(), "a row whose final is gone: {stale:?}");

    for (file, status, class) in FINALS {
        assert!(
            found
                .iter()
                .filter(|f| f.file == *file && f.status == *status)
                .any(|f| fits(*class, status, f.bare)),
            "{file}: {status} is not a {class:?}"
        );
    }
}

/// The scan sees a new final at every place one is built.
#[test]
fn the_scan_finds_a_final_at_each_place_one_is_built() {
    let code = "fn reject_call(call: Call, status: u16) {}\n\
                reject_call(call, invite, 404, None, None, &[]);\n\
                response_to_a_leg(call, fx, &invite, 410, \"Gone\");\n\
                generate_response(req, 415, \"Unsupported\", &opts);\n\
                RuleAction::RespondToALeg { status: 420, reason: r, header_updates: vec![] };\n\
                RuleAction::RespondToALeg { .. } => {}\n\
                generate_response(req, 200, \"OK\", &opts);\n\
                json!({ \"code\": 430, \"origin\": \"x\" });\n";
    let mut found = Vec::new();
    finals_in("new.rs", code, &mut found);
    let mut statuses: Vec<_> = found.iter().map(|f| f.status.as_str()).collect();
    statuses.sort_unstable();
    assert_eq!(statuses, ["404", "410", "415", "420", "430"]);
}

/// A 4xx the class table does not name has no row to hide in.
#[test]
fn no_class_holds_a_4xx_outside_its_own() {
    for class in [InternalError, Rfc3261, RefusedAnswer, NoCalleeAnswered] {
        assert!(!fits(class, "486", true), "{class:?} holds a 486");
        assert!(!fits(class, "404", true), "{class:?} holds a 404");
    }
    assert!(!fits(InternalError, "503", false), "an internal error states no decision's header");
}
