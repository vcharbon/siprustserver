//! The run ladder: a compiled plan driven over a real B2BUA on a paused clock,
//! producing a complete run bundle.
//!
//! One rung per shape, simplest first. Each rung proves the same three things —
//! the flow ran, the run SETTLED, and the bundle on disk holds every datagram —
//! and adds one construct.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// The ladder's second callee socket.
const IDLE_PORT: u16 = 5090;

/// Where this lane's media booking starts handing out RTP ports: the first
/// active `m=` line of the first leg to offer takes exactly this one (§8.3
/// `m=port`), which is what lets a rung name the port it expects on the wire.
const LANE_RTP_BASE: u16 = 40000;

/// This ladder's lane: the upstream demo B2BUA on a paused clock. Its own name,
/// not a deployment's — a document's `origin_lane` is compared against it, and
/// two lanes sharing a token would silently stop downgrading (§9.1).
const DEMO_LANE: &str = "upstream-demo";

use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{B2buaScene, B2buaSut, BOB_PORT};
use call::model::cdr::CdrEventType;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpTransport, SimulatedHttpNetwork};
use pivot_interpreter::plan::Plan;
use pivot_interpreter::{
    Booking, ClockMode, CloseOwed, Failure, IdentityBindings, Lane, Outcome, RunConfig, Sut,
    UriComposer, VerdictStatus,
};
use pivot_schema::bundle::recording::Dir;
use pivot_schema::bundle::{Arrived, GatedOn};
use pivot_schema::msg::Header;
use pivot_schema::must_fail::{DeclaredFailure, MustFail};
use pivot_schema::placement::{BackgroundMatch, BackgroundPolicy, BackgroundResponse, CountBound};
use pivot_schema::scoping::CheckClass;
use pivot_schema::violation::RfcRule;
use pivot_schema::PivotV3;
use sip_clock::Clock;
use sip_message::header::Uri;

/// The demo lane's URI composition: a Request-URI is addressed at the system
/// under test, a From/To at the lane's own domain.
struct DemoComposer {
    ingress: SocketAddr,
    domain: String,
    numbers: BTreeMap<String, String>,
    /// The ingress carrier trunk a `trunk-composed` Request-URI is dialled over.
    /// A numbering-plan fact, which is why the LANE composes it and the
    /// interpreter only asks.
    trunk: String,
}

impl UriComposer for DemoComposer {
    fn compose(&self, pos: &str, form: Option<&str>, is_ruri: bool) -> Option<String> {
        let number = self.numbers.get(pos)?;
        let user = match (is_ruri, form) {
            (true, Some("trunk-composed")) => format!("+{}{number}", self.trunk),
            _ => number.clone(),
        };
        Some(if is_ruri {
            format!("sip:{user}@{}", self.ingress)
        } else {
            format!("sip:{user}@{}", self.domain)
        })
    }

    /// A frozen value replays verbatim. Three shapes, and the middle one is the
    /// reason this is not a one-liner: a value carrying a scheme IS a URI; a
    /// value the cut classified (`kind`), or one already carrying a HOST (RFC
    /// 3323 §4.1.1.3's `anonymous@anonymous.invalid`), is a URI missing only
    /// its scheme, and composing the lane host onto it would mint a second `@`
    /// and put an unparseable address on the wire; a bare userpart composes
    /// over the host. Scheme detection is §25.1 grammar, never a bare colon —
    /// a port colon read as a scheme colon emits no URI at all.
    fn frozen(&self, value: &str, kind: Option<&str>, is_ruri: bool) -> String {
        if Uri::value_has_scheme(value) {
            return value.to_string();
        }
        if kind.is_some() || value.contains('@') {
            return format!("sip:{value}");
        }
        let host = if is_ruri { self.ingress.to_string() } else { self.domain.clone() };
        format!("sip:{value}@{host}")
    }
}

/// The Request-URI userpart a tier-2 ref resolves to on this lane — the same
/// composition [`DemoComposer`] performs, read back so the driver can bind the
/// claim to it.
/// Ask the COMPOSER what it will put on the wire and read the userpart back, so
/// the claim can never drift from the emission: a `trunk-composed` form dials a
/// different userpart than the bare number, and binding the claim to the number
/// would silently stop matching.
/// A recorded datagram RENDERED as text, for assertions on its head: the
/// recording holds bytes, and a head is ASCII whatever the body holds.
fn text(m: &pivot_schema::bundle::RecordedMessage) -> String {
    String::from_utf8_lossy(m.wire()).into_owned()
}

fn ruri_user(r: &pivot_schema::msg::Ref, composer: &DemoComposer) -> Option<String> {
    let uri = match r {
        pivot_schema::msg::Ref::Positional(p) => {
            composer.compose(&p.pos, p.form.as_deref(), true)?
        }
        pivot_schema::msg::Ref::Frozen(f) => composer.frozen(&f.frozen, f.kind.as_deref(), true),
    };
    let after_scheme = uri.split_once(':').map(|(_, rest)| rest).unwrap_or(&uri);
    after_scheme.rsplit_once('@').map(|(user, _)| user.to_string())
}

/// The lane's number allocation for one document (§4.3, the driver's job): every
/// tier-2 POSITION the document names, bound to a number, plus every identity
/// bound in each dial form it declares.
///
/// A captured document's `identities[].observed` is the anonymized value the
/// capture carried; this lane allocates it back, because the system routes by
/// its own decision and the number only has to be consistent and dialable.
fn lane_numbers(plan: &Plan) -> (BTreeMap<String, String>, IdentityBindings) {
    let document = plan.document();
    let qualified = document.calls.len() > 1;
    let mut numbers = BTreeMap::new();
    let mut bindings = IdentityBindings::new();
    let allocate = |name: &str, position: String, bindings: &mut IdentityBindings| {
        let identity = document.identities.iter().find(|i| i.name == name);
        let number = identity
            .and_then(|i| i.observed.clone())
            .map(|observed| observed.split('@').next().unwrap_or(&observed).to_string())
            .unwrap_or_else(|| name.replace(['-', '.'], ""));
        for form in identity.map(|i| i.forms.clone()).unwrap_or_default() {
            *bindings = std::mem::take(bindings).bind(name, form, number.clone());
        }
        (position, number)
    };
    // EVERY identity the registry declares, in every form it declares. A
    // `${num:…}` may name a party no attempt dials — a Refer-To's transfer
    // target is exactly that — and an identity the lane left unbound refuses at
    // emission rather than composing an empty user part.
    for identity in &document.identities {
        let number = identity
            .observed
            .clone()
            .map(|observed| observed.split('@').next().unwrap_or(&observed).to_string())
            .unwrap_or_else(|| identity.name.replace(['-', '.'], ""));
        for form in &identity.forms {
            bindings = bindings.bind(&identity.name, form.clone(), number.clone());
        }
    }
    for call in &document.calls {
        let prefix = if qualified { format!("{}.", call.id) } else { String::new() };
        if let Some(name) = document
            .legs
            .iter()
            .find(|l| l.id == call.caller_leg)
            .and_then(|l| document.actors.iter().find(|a| a.id == l.actor))
            .and_then(|a| a.identity.clone())
        {
            let (position, number) = allocate(&name, format!("{prefix}caller"), &mut bindings);
            numbers.insert(position, number);
        }
        for attempt in &call.attempts {
            let (position, number) = allocate(
                &attempt.callee.identity,
                format!("{prefix}called[{}][{}]", attempt.branch, attempt.position),
                &mut bindings,
            );
            numbers.insert(position, number);
        }
    }
    (numbers, bindings)
}

/// What this lane egresses ONE attempt under (§4.3): the Request-URI user part
/// the system dials its callee at, the socket it dials, and the ring timer it
/// arms before giving up.
struct Egress {
    /// The call this attempt belongs to: a directive is per CALL (§4.3).
    call: String,
    identity: String,
    user: String,
    /// `host:port`, as the route decision states a destination.
    dest: String,
    /// The whole-second `no_answer_timeout_sec` the attempt's `no_answer_ms`
    /// lowers to, where the attempt states one.
    no_answer_sec: Option<i64>,
}

/// The route timer this lane arms for an attempt's stated ring, in the whole
/// seconds its decision backend speaks.
///
/// This lane's grain is one second, so a dwell the document measured between two
/// seconds is armed at the NEAREST one and the difference rides the run's stated
/// timing tolerance (§9.2). A difference the tolerance does not cover is REFUSED
/// rather than rounded: a run whose ring is half a second short of what the
/// document measured proves a different thing than the document does, and
/// quietly proving something else is the one outcome §14 rules out.
fn no_answer_sec(identity: &str, ms: u64, tolerance_ms: u64) -> i64 {
    let secs = (ms as f64 / 1000.0).round() as i64;
    let armed = (secs * 1000) as u64;
    assert!(
        armed.abs_diff(ms) <= tolerance_ms,
        "{identity}: this lane arms whole seconds, so {ms} ms arms at {armed} ms — \
         a difference of {} ms, and this run states a tolerance of ±{tolerance_ms} ms",
        armed.abs_diff(ms)
    );
    secs
}

/// The lane's egress for every attempt the document declares (§4.3).
///
/// Position 0 of a branch is a RELAY: the caller dialled that callee and this
/// lane's decision hands the user part straight through, so it reads the
/// document's own opening INVITE. Every LATER position is a REROUTE the
/// system's own decision composes, so it reads the identity registry instead —
/// deriving it from the caller's Request-URI would bind two callees to one
/// number and leave a `ruri-pos` claim ambiguous between them
/// (`claim/same-number-ambiguous`).
fn lane_egress(
    plan: &Plan,
    composer: &DemoComposer,
    identities: &IdentityBindings,
    tolerance_ms: u64,
    agents: &BTreeMap<String, scenario_harness::Agent>,
) -> Vec<Egress> {
    let document = plan.document();
    let mut out = Vec::new();
    for call in &document.calls {
        let dialled = plan
            .steps()
            .into_iter()
            .find(|step| {
                step.leg == call.caller_leg
                    && step.msg.method.as_deref() == Some("INVITE")
                    && step.msg.ruri.is_some()
            })
            .and_then(|s| s.msg.ruri.as_ref().and_then(|r| ruri_user(r, composer)));
        for attempt in &call.attempts {
            let name = &attempt.callee.identity;
            let registered = plan
                .forms(name)
                .and_then(|forms| forms.iter().find_map(|f| identities.resolve(name, f).ok()))
                .map(str::to_string);
            let user = match attempt.position {
                0 => dialled.clone().or(registered),
                _ => registered,
            };
            let Some(user) = user else { continue };
            // Where the lane BOUND that callee, which is the only address the
            // system can reach it at. `endpoints[].observed` is the socket the
            // capture saw and answers only when this lane happens to bind the
            // same one; a directive naming it otherwise dials an empty port.
            let actor =
                document.legs.iter().find(|leg| leg.id == attempt.leg).map(|leg| leg.actor.clone());
            let dest = actor
                .as_ref()
                .and_then(|actor| agents.get(actor))
                .map(|agent| agent.addr().to_string())
                .or_else(|| {
                    actor
                        .as_ref()
                        .and_then(|id| document.actors.iter().find(|a| &a.id == id))
                        .and_then(|actor| {
                            document.endpoints.iter().find(|e| e.id == actor.endpoint)
                        })
                        .map(|endpoint| endpoint.observed.clone())
                })
                .unwrap_or_else(|| format!("127.0.0.1:{BOB_PORT}"));
            out.push(Egress {
                call: call.id.clone(),
                identity: name.clone(),
                user,
                dest,
                no_answer_sec: attempt.no_answer_ms.map(|ms| no_answer_sec(name, ms, tolerance_ms)),
            });
        }
    }
    out
}

/// The ordered route list this lane hands the system so it HUNTS the document's
/// attempt chain — the ADR-0017 `X-Api-Call` plan the scripted decision walks,
/// one route per attempt of the call's FIRST branch, in `position` order.
///
/// Branch 0 is the initial INVITE's own chain, which is what a route list
/// decides. A later branch is a leg that JOINED the call — a transfer target, an
/// inserted media resource — dialled by the mechanism that joined it and not by
/// this plan, so it never becomes a route here.
///
/// A single-attempt branch states no hunt and compiles to `None`: the scene's
/// own destination already answers, and injecting a plan would rewrite what
/// every other rung dials.
fn hunt_plan(egress: &[Egress], call: &pivot_schema::call::Call) -> Option<serde_json::Value> {
    let mut chain: Vec<&pivot_schema::call::Attempt> =
        call.attempts.iter().filter(|a| a.branch == 0 && a.joined_by.is_none()).collect();
    chain.sort_by_key(|a| a.position);
    if chain.len() < 2 {
        return None;
    }
    let routes: Vec<serde_json::Value> = chain
        .iter()
        .filter_map(|attempt| {
            let hop = egress
                .iter()
                .find(|e| e.call == call.id && e.identity == attempt.callee.identity)?;
            let (host, port) = hop.dest.rsplit_once(':')?;
            let mut route = serde_json::json!({
                "destination": { "host": host, "port": port.parse::<u16>().ok()? },
                "new_ruri": format!("sip:{}@{}", hop.user, hop.dest),
            });
            if let Some(secs) = hop.no_answer_sec {
                route["no_answer_timeout_sec"] = serde_json::json!(secs);
            }
            Some(route)
        })
        .collect();
    (routes.len() == chain.len())
        .then(|| serde_json::json!({ "action": "route", "routes": routes }))
}

/// The admission cap this lane arms for one run: a limiter id and how many
/// concurrent calls it admits.
#[derive(Debug, Clone)]
struct Admission {
    id: String,
    limit: i64,
}

/// What this lane ARMS on its system for one run, beyond what the document
/// itself says: an admission cap, whether the platform processes a transfer
/// REFER locally instead of relaying it on, and how far this run's timers may
/// sit from the dwells the document declares.
///
/// The first two are properties of the SCENE, not of the case: `case.requires`
/// is an informative capability token the interpreter never reads (§3.2), and
/// neither `calls` nor `flow` has a field for either. The rung states them, the
/// driver lowers them into the decision backend's own vocabulary (§4.3), and the
/// interpreter learns of neither.
///
/// The tolerance is different in kind: it is a RUN-configuration knob (§9.2) the
/// interpreter reads directly, and the driver reads it too — a dwell this lane
/// cannot arm exactly is armed at the nearest value it can, and the difference
/// has to fit the same window the run will judge the arrival by.
#[derive(Debug, Clone, Default)]
struct LaneDirectives {
    admission: Option<Admission>,
    /// Activate the decision's REFER arm — the platform terminates a REFER
    /// (202 / 400 + `/call/refer`) rather than relaying it to the peer leg.
    local_refer: bool,
    /// The ± window this run states around a timer-anchored dwell (§9.2).
    timing_tolerance_ms: u64,
}

impl LaneDirectives {
    /// Whether a call needs an egress directive built for it at all. The
    /// tolerance is not one: it steers no egress.
    fn armed(&self) -> bool {
        self.admission.is_some() || self.local_refer
    }
}

/// The egress directive this lane hands the system for a call that states no
/// hunt: the ONE destination its first attempt is dialled at, Request-URI
/// userpart included (the ADR-0017 `X-Api-Call.destination` surface). Built
/// only when a rung armed something the directive has to carry.
///
/// A single-attempt call gets a destination and never a `routes` PLAN: a plan
/// opens a failover context, and a call refused by admission control would then
/// consult `/call/failure` for a next route instead of answering its caller.
fn single_destination(egress: &[Egress], call: &str) -> Option<serde_json::Value> {
    let hop = egress.iter().find(|e| e.call == call)?;
    let (host, port) = hop.dest.rsplit_once(':')?;
    Some(serde_json::json!({
        "action": "route",
        "destination": { "host": host, "port": port.parse::<u16>().ok()?, "user": hop.user },
    }))
}

/// The upstream B2BUA, as the settle contract's system under test.
struct B2buaUnderTest<'a>(&'a b2bua_harness::B2buaSut);

impl Sut for B2buaUnderTest<'_> {
    fn active_calls(&self) -> usize {
        self.0.active_calls()
    }

    fn cdr_records(&self) -> Vec<BTreeMap<String, String>> {
        self.0.cdr_records().iter().map(flatten).collect()
    }

    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        Err(format!("{name:?} is not an observable this lane publishes"))
    }
}

/// One CDR as a flat field map: `a_leg.call_id`, `b_legs.0.state`, … A check's
/// field selector is a path through the record the B2BUA serializes.
///
/// A LIST-valued field is published twice: once per element (`events.0`) and
/// once whole (`events`), because "some record whose events include a Bye" is a
/// regex over the list, and indexing it away would make that unaskable.
fn flatten<T: serde::Serialize>(record: &T) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let value = serde_json::to_value(record).expect("a CDR serializes");
    walk(String::new(), &value, &mut out);
    out
}

fn walk(prefix: String, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                walk(path, child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk(format!("{prefix}.{index}"), child, out);
            }
            let whole: Vec<String> = items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            out.insert(prefix, whole.join(", "));
        }
        serde_json::Value::Null => {}
        scalar => {
            let text = match scalar {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(prefix, text);
        }
    }
}

/// A document plus the directory its `resources/` refs resolve from.
struct Case {
    document: PivotV3,
    base_dir: PathBuf,
}

fn fixture(name: &str) -> Case {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    read_case(&base_dir.join(name), base_dir)
}

/// A case DIRECTORY (`<case-id>/scenario.json` plus `resources/`), which is the
/// layout `PCAP2TEST_PIVOT_V3.md` §1 states. `PIVOT_CASE_DIRS` points the
/// external-case rungs at a library outside this crate; the rung SKIPS LOUDLY
/// when it is unset rather than passing on an empty corpus.
fn case_dir(name: &str) -> Option<Case> {
    let roots = std::env::var("PIVOT_CASE_DIRS").ok()?;
    let found = roots
        .split(':')
        .filter(|root| !root.is_empty())
        .map(|root| PathBuf::from(root).join(name))
        .find(|dir| dir.join("scenario.json").is_file())?;
    Some(read_case(&found.join("scenario.json"), found))
}

/// A case from the regenerated v3 CORPUS. Two layouts exist and both are
/// served: the corpus tree's `<capture>/cases/<case-id>/<case-id>.v3.json`,
/// and the staged viewer tree's flat `<case-id>/scenario.json`.
/// `PIVOT_CORPUS_DIRS` points the corpus rungs at either; they SKIP LOUDLY
/// when it is unset, because an absent corpus must never read as a pass.
fn corpus_case(case_id: &str) -> Option<Case> {
    let roots = std::env::var("PIVOT_CORPUS_DIRS").ok()?;
    for root in roots.split(':').filter(|r| !r.is_empty()) {
        let capture = case_id.rsplit_once("-case").map(|(c, _)| c).unwrap_or(case_id);
        let capture = capture.strip_suffix("-auto").unwrap_or(capture);
        let dir = PathBuf::from(root).join(capture).join("cases").join(case_id);
        let document = dir.join(format!("{case_id}.v3.json"));
        if document.is_file() {
            return Some(read_case(&document, dir));
        }
        let flat = PathBuf::from(root).join(case_id);
        let document = flat.join("scenario.json");
        if document.is_file() {
            return Some(read_case(&document, flat));
        }
    }
    None
}

fn read_case(path: &std::path::Path, base_dir: PathBuf) -> Case {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let document =
        PivotV3::from_json(&text).unwrap_or_else(|e| panic!("{} parses: {e}", path.display()));
    Case { document, base_dir }
}

/// The scene the ladder runs against: a B2BUA that honours the deployment's own
/// egress control header, so the b-leg is dialled at the socket the document
/// names AND its Request-URI names the CALLEE.
///
/// `B2buaScene::new`'s decision (`route_all_to`) leaves `new_ruri` unset, so the
/// b-leg Request-URI copies the a-leg's — which, since the caller dials the
/// system's ingress, is the system's OWN address. That is a property of the
/// demo decision config, not of the interpreter and not of the production
/// composition path (`route_all_to_with_limiter` sets `new_ruri` explicitly and
/// its doc comment explains the anti-loop invariant behind it). The ladder uses
/// the honouring decision so its bundles carry a Request-URI a strict UAS would
/// accept.
async fn api_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_api_call("127.0.0.1", bob_port)
            // Short enough that a call held up for a couple of seconds really is
            // polled, so a `background` OPTIONS counter asserts something.
            .tune(|config| config.keepalive_interval_sec = 2)
    })
    .await
}

/// Where this rung writes its bundle: a per-test directory a human can open in
/// the viewer after the run.
fn bundle_dir(case: &str) -> PathBuf {
    let root = std::env::var("PIVOT_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("pivot-runs"));
    root.join(case)
}

/// Run one fixture on the default demo lane (alice → b2bua → bob), write its
/// bundle, and hand back what the run decided.
async fn replay(scene: &B2buaScene, fixture_name: &str) -> (Outcome, PathBuf) {
    replay_case(scene, fixture(fixture_name), BTreeMap::new()).await
}

/// [`replay`] with extra actors bound beyond the default alice/bob pair.
async fn replay_on(
    scene: &B2buaScene,
    fixture_name: &str,
    extra_agents: BTreeMap<String, scenario_harness::Agent>,
) -> (Outcome, PathBuf) {
    replay_case(scene, fixture(fixture_name), extra_agents).await
}

async fn replay_case(
    scene: &B2buaScene,
    input: Case,
    extra_agents: BTreeMap<String, scenario_harness::Agent>,
) -> (Outcome, PathBuf) {
    replay_directed(scene, input, extra_agents, LaneDirectives::default()).await
}

/// [`replay_case`] on a lane whose system the rung armed — an admission cap, a
/// locally processed REFER.
///
/// The whole run sequence — compile, arm the bundle writer, drive, commit — is
/// `pivot_interpreter::replay`'s. What this driver assembles is lane knowledge
/// (§4.3): numbers, bindings, egress directives, agents, the run config. The
/// plan compiled here serves only that assembly; the entry point compiles its
/// own from the same document.
async fn replay_directed(
    scene: &B2buaScene,
    input: Case,
    extra_agents: BTreeMap<String, scenario_harness::Agent>,
    directives: LaneDirectives,
) -> (Outcome, PathBuf) {
    let ingress = scene.b2bua.addr;
    let Case { document, base_dir } = input;
    let case = document.case.id.clone();
    let plan = Plan::compile(document.clone()).expect("the fixture compiles");

    // The lane's own allocation for whatever the document names, then the
    // ladder's fixed pins on top (a fixture states positions this lane binds to
    // the sockets it actually runs on).
    let (lane_positions, lane_bindings) = lane_numbers(&plan);
    let mut numbers = lane_positions.clone();
    numbers.extend(BTreeMap::from([
        ("caller".to_string(), "0009001".to_string()),
        ("called[0][0]".to_string(), format!("bob{BOB_PORT}")),
        ("c1.caller".to_string(), "0009001".to_string()),
        ("c1.called[0][0]".to_string(), format!("bob{BOB_PORT}")),
        ("c2.caller".to_string(), "0009002".to_string()),
        ("c2.called[0][0]".to_string(), format!("idle{IDLE_PORT}")),
    ]));
    let composer =
        DemoComposer { ingress, domain: "pivot.invalid".into(), numbers, trunk: "1999999".into() };
    let mut identities = lane_bindings;
    for (name, form, number) in [
        ("caller", "private", "0009001".to_string()),
        ("called-0-0", "e164", format!("bob{BOB_PORT}")),
        ("c1-caller", "private", "0009001".to_string()),
        ("c1-called-0-0", "e164", format!("bob{BOB_PORT}")),
        ("c2-caller", "private", "0009002".to_string()),
        ("c2-called-0-0", "e164", format!("idle{IDLE_PORT}")),
    ] {
        // A fixture's own pins win over the generic allocation.
        if plan.document().identities.iter().any(|i| i.name == name && i.observed.is_none()) {
            identities = identities.bind(name, form, number);
        }
    }
    let mut agents = BTreeMap::from([
        ("uac1".to_string(), scene.alice.clone()),
        ("uas1".to_string(), scene.bob.clone()),
    ]);
    agents.extend(extra_agents);
    // §4.3 is the DRIVER's: bind each callee identity to the Request-URI
    // userpart this lane's system will egress its leg under, and — where the
    // call states a hunt — hand the system the route list that walks it.
    let egress =
        lane_egress(&plan, &composer, &identities, directives.timing_tolerance_ms, &agents);
    // EACH call's own egress directive — a hunt where its chain states one, a
    // single destination otherwise — carrying whatever the rung armed. Nothing
    // armed and no hunt: the lane states nothing at all, so the system's own
    // defaults decide, which is what most documents replay against.
    //
    // Per CALL, never per run: a document with two calls dials two callees, and
    // one directive serving both would deliver the second call to the first
    // one's endpoint (§4.3).
    let mut per_call: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for call in &plan.document().calls {
        let directive = if directives.armed() {
            hunt_plan(&egress, call).or_else(|| single_destination(&egress, &call.id)).map(
                |mut value| {
                    if let Some(admission) = &directives.admission {
                        value["call_limiter"] =
                            serde_json::json!([{ "id": admission.id, "limit": admission.limit }]);
                    }
                    if directives.local_refer {
                        value["features"] = serde_json::json!({ "refer": {} });
                    }
                    value
                },
            )
        } else {
            hunt_plan(&egress, call)
        };
        if let Some(value) = directive {
            per_call.insert(call.id.clone(), value);
        }
    }
    let config = RunConfig::new(DEMO_LANE, ClockMode::Virtual, ingress.to_string())
        .with_timing_tolerance(directives.timing_tolerance_ms)
        .with_identities(identities)
        // The authored case LIBRARY dials a trunk-composed Request-URI that its
        // own deployment relays untouched, so this lane states the user part a
        // `ruri-pos` claim sees there. The ladder's own fixtures name it through
        // the egress header instead and need no override.
        .with_claim_number("called-0-0", format!("+1999999bob{BOB_PORT}"))
        .with_claim_number("called-0-0", format!("bob{BOB_PORT}"))
        .with_claim_number("c2-called-0-0", format!("idle{IDLE_PORT}"));
    let config = egress
        .iter()
        .fold(config, |config, hop| config.with_claim_number(&hop.identity, &hop.user));
    // A hunt is a lane DIRECTIVE on the call's OWN outbound INVITE (§4.3): the
    // ordered attempt chain, in the decision backend's own plan vocabulary,
    // stamped on the dial that opens that call and on nothing else.
    let config = per_call.iter().fold(config, |config, (call, value)| {
        config.with_call_header(call, "X-Api-Call", value.to_string())
    });
    let lane = Lane {
        agents,
        route_target: ingress,
        media: Booking::new("127.0.0.1", LANE_RTP_BASE),
        base_dir,
        composer: &composer,
    };

    let dir = bundle_dir(&case);
    let sut = B2buaUnderTest(&scene.b2bua);
    let outcome = pivot_interpreter::replay(document, config, lane, &sut, &dir)
        .await
        .expect("the run replays and leaves its bundle");
    (outcome, dir)
}

/// Every rung asserts the same floor: green verdict, a settled run, and a
/// bundle on disk holding both legs.
fn assert_bundle_is_complete(outcome: &Outcome, dir: &std::path::Path) {
    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::Ok,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    let legs = outcome.recording.legs();
    assert!(legs.contains_key("A") && legs.contains_key("B"), "both legs recorded: {legs:#?}");
    assert!(
        legs.values()
            .all(|messages| messages.iter().all(|m| !text(m).is_empty() || m.note.is_some())),
        "every recorded datagram carries its bytes"
    );
    for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
        assert!(dir.join(file).is_file(), "{file} missing from the bundle");
    }
    assert!(dir.join("recording/A.jsonl").is_file());
    assert!(dir.join("recording/B.jsonl").is_file());
}

#[tokio::test(start_paused = true)]
async fn rung_one_a_linear_answered_call_runs_green_and_writes_its_bundle() {
    let scene = api_scene("pivot-linear-attempt").await;
    let (outcome, dir) = replay(&scene, "linear-attempt.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");

    // s7's inline checks include one the CAPTURED platform's egress carries and
    // this system does not. It is classified, this lane is not the document's
    // origin lane, so it is evaluated, recorded, and the step still completes
    // (§9.1) — a downgrade is not a skip, and it is not a pass either.
    assert_eq!(outcome.verdict.informative.len(), 1, "{:#?}", outcome.verdict.informative);
    let note = &outcome.verdict.informative[0];
    assert_eq!(note.class, CheckClass::OriginPlatformHeader);
    assert!(
        matches!(&note.finding, Failure::CheckFailed { site, field, .. }
            if site == "step \"s7\"" && field == "header(P-Charging-Vector)"),
        "{note:#?}"
    );
    scene.finish().await;
}

/// The final a sent BYE is owed is RFC 3261 §15.1.2's, not the document's: a
/// capture whose vantage closed between the BYE and its answer scripts no
/// expect for it, and the answer still arrives. It is recorded as absorbed and
/// the run stays green.
#[tokio::test(start_paused = true)]
async fn the_final_owed_to_a_sent_bye_is_absorbed_where_no_expect_scripts_it() {
    let scene = api_scene("pivot-owed-bye-final").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-owed-bye-final".to_string();
    case.document.flow.retain(
        |node| !matches!(node, pivot_schema::flow::FlowNode::Message(step) if step.id == "s13"),
    );
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "every remaining step ran");
    let absorbed = outcome.recording.legs()["A"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 200") && text(m).contains("\r\nCSeq: 2 BYE\r\n"))
        .unwrap_or_else(|| panic!("the owed final was recorded: {:#?}", outcome.recording.legs()))
        .clone();
    assert!(absorbed.note.as_deref().is_some_and(|note| note.contains("15.1.2")), "{absorbed:#?}");
    scene.finish().await;
}

/// A document that scripts the final ITSELF keeps it, whatever status it
/// scripts. The absorption stands for a vantage that closed before the answer,
/// never for a document that disagrees with the answer: the captured platform
/// answered a crossing BYE `501 Not Implemented`, ours answers `200`, and that
/// substitution is the confrontation's to name, not the runner's to eat.
#[tokio::test(start_paused = true)]
async fn a_final_the_document_scripts_is_not_absorbed_when_its_status_differs() {
    let scene = api_scene("pivot-scripted-bye-final").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-scripted-bye-final".to_string();
    for node in &mut case.document.flow {
        if let pivot_schema::flow::FlowNode::Message(step) = node {
            if step.id == "s13" {
                step.msg.status = Some(501);
                step.msg.reason = Some("Not Implemented".to_string());
            }
        }
    }
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;
    let surfaced = outcome.recording.legs()["A"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 200") && text(m).contains("\r\nCSeq: 2 BYE\r\n"))
        .unwrap_or_else(|| panic!("the final was recorded: {:#?}", outcome.recording.legs()))
        .clone();
    assert!(
        !surfaced.note.as_deref().is_some_and(|note| note.contains("15.1.2")),
        "the scripted final is confronted, not absorbed: {surfaced:#?}"
    );
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(
            f,
            Failure::UnmatchedDatagram { step, .. } if step == "s13"
        )),
        "{:#?}",
        outcome.verdict.failures
    );
    scene.finish().await;
}

/// An auto ACK finds the final it acknowledges in LEG STATE, never in the
/// step's `cseq`.
///
/// The fixture is the shape the corpus is full of: leg B answers the call as a
/// UAS, then originates an in-dialog re-INVITE as a UAC, and every auto step
/// carries the CAPTURED CSeq — the authoring platform's numbers (458007/458009
/// on A, 385579/385581 on B), which no leg of this run can mint. The stack
/// numbers its own CSeqs 1 and 2, so a document number resolves to no
/// transaction; the ACK is still owed and still composable, because the leg
/// knows which INVITE it has outstanding.
#[tokio::test(start_paused = true)]
async fn an_auto_ack_acks_the_leg_s_own_invite_whatever_cseq_the_capture_carried() {
    let scene = api_scene("pivot-reinvite-captured-cseq").await;
    let (outcome, dir) = replay(&scene, "reinvite-captured-cseq.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.completed_steps.len(),
        20,
        "every step ran: {:#?}",
        outcome.verdict.failures
    );

    // The one ACK leg B puts on the wire is the stack's own number for the one
    // INVITE that leg opened — emitted by s15, the step whose `cseq` names a
    // transaction that exists nowhere in this run.
    let legs = outcome.recording.legs();
    let acks: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("ACK ") && m.step.as_deref() == Some("s15"))
        .collect();
    assert_eq!(acks.len(), 1, "the re-INVITE's ACK left leg B: {:#?}", legs["B"]);
    assert!(text(acks[0]).contains("CSeq: 1 ACK"), "{}", text(acks[0]));

    // And no captured token reached the wire at all, in either direction: §6.3
    // says `cseq` is never replayed, and this run has four chances to break it.
    for token in ["458007", "458009", "385579", "385581"] {
        for (leg, messages) in legs.iter() {
            assert!(
                messages.iter().all(|m| !text(m).contains(token)),
                "leg {leg} replayed the captured token {token}: {messages:#?}"
            );
        }
    }
    scene.finish().await;
}

/// A leg holding TWO un-ACKed INVITE transactions ACKs each of them, and each
/// ACK names its OWN.
///
/// RFC 3261 §14.1 leaves one INVITE outstanding per dialog, so leg state alone
/// names the transaction an auto ACK owes — until a capture records a peer that
/// broke §14.1. This fixture is that peer: leg B originates a re-INVITE, takes
/// its 200, then pipelines a SECOND re-INVITE before ACKing the first, draws the
/// §14.2 491 the crossing earns, ACKs that, and only then takes the deferred
/// 2xx ACK. Resolving either ACK by "the INVITE this leg sent last" hands both
/// to the newer transaction and strands the first 2xx un-ACKed for the life of
/// the dialog — the SUT then ladders it out and 491s everything that follows.
#[tokio::test(start_paused = true)]
async fn a_pipelined_re_invite_does_not_take_the_earlier_transaction_s_ack() {
    let scene = api_scene("pivot-reinvite-pipelined-unacked").await;
    // The §14.1 breach is the fixture's whole subject — the scripted callee is
    // the captured peer, and reproducing its non-compliance is what the case is
    // for. The SUT's own conduct stays gated.
    scene.h.allow_violation(
        "no-re-invite-while-invite-in-progress",
        "the scripted callee pipelines the second re-INVITE on purpose: it is the shape under test",
    );
    let (outcome, dir) = replay(&scene, "reinvite-pipelined-unacked.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    // Leg B minted CSeq 1 for its first re-INVITE and 2 for the crossing one.
    // s18 ACKs the 491 (CSeq 2, riding that INVITE's own branch, §17.1.1.3) and
    // s19 ACKs the deferred 200 (CSeq 1, a new transaction on the dialog).
    let legs = outcome.recording.legs();
    let ack_of = |step: &str| -> String {
        legs["B"]
            .iter()
            .filter(|m| text(m).starts_with("ACK ") && m.step.as_deref() == Some(step))
            .map(|m| text(m).clone())
            .next()
            .unwrap_or_else(|| panic!("{step} put no ACK on leg B: {:#?}", legs["B"]))
    };
    let negative = ack_of("s18");
    let deferred = ack_of("s19");
    assert!(negative.contains("CSeq: 2 ACK"), "s18 ACKs the 491's own transaction: {negative}");
    assert!(
        deferred.contains("CSeq: 1 ACK"),
        "s19 ACKs the transaction it is owed to, not the one s18 discharged: {deferred}"
    );

    // And the 2xx really was acknowledged: an ACK the SUT never matched leaves
    // its server transaction in RFC 6026 Accepted, laddering the 200 out.
    let repeats = legs["B"]
        .iter()
        .filter(|m| m.dir == Dir::In)
        .filter(|m| text(m).starts_with("SIP/2.0 200 OK") && text(m).contains("CSeq: 1 INVITE"))
        .count();
    assert_eq!(repeats, 1, "the answered re-INVITE was not laddered: {:#?}", legs["B"]);

    scene.finish().await;
}

/// A leg that ACKs ONE 2xx TWICE composes both, and the discharge the pipelined
/// case needs does not deny the second step its final.
///
/// RFC 3261 §13.2.2.4 has the UAC core generate one ACK per 2xx it RECEIVES, so
/// a second ACK on a fresh branch under one 2xx is the captured peer's own
/// non-compliance — and a `send` step MODELS a peer, so the document states it.
/// Resolving the final by "an INVITE not yet ACKed" alone strands that step: the
/// first ACK discharged the only transaction, and the leg then owns a 2xx it is
/// refused the right to acknowledge.
#[tokio::test(start_paused = true)]
async fn a_second_ack_for_one_2xx_still_finds_the_final_the_first_discharged() {
    let scene = api_scene("pivot-double-ack-one-2xx").await;
    let (outcome, dir) = replay(&scene, "double-ack-one-2xx.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.completed_steps.len(),
        14,
        "every step ran: {:#?}",
        outcome.verdict.failures
    );

    // Both ACKs left leg A under the one 2xx's CSeq, on DISTINCT branches: a
    // second transaction on the dialog, not a retransmission of the first.
    let legs = outcome.recording.legs();
    let acks: Vec<_> =
        legs["A"].iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 2, "both ACK steps reached the wire: {:#?}", legs["A"]);
    let branch = |raw: &str| {
        raw.lines()
            .find_map(|line| line.split_once("branch="))
            .map(|(_, b)| b.trim().to_string())
            .expect("every ACK carries a Via branch")
    };
    assert_ne!(
        branch(&text(acks[0])),
        branch(&text(acks[1])),
        "the re-ACK opens its own transaction: {:#?}",
        acks
    );
    for ack in &acks {
        assert!(text(ack).contains("CSeq: 1 ACK"), "both name the answered INVITE: {}", text(ack));
    }

    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn rung_two_a_cancelled_attempt_tolerates_the_ring_and_takes_the_teardown_pair_in_any_order()
{
    let scene = api_scene("pivot-cancel-race").await;
    let (outcome, dir) = replay(&scene, "cancel-race.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    // The order-free group's two members both arrived, whichever way round.
    for step in ["s11", "s12"] {
        assert!(
            outcome.verdict.completed_steps.contains(&step.to_string()),
            "{step} missing from {:?}",
            outcome.verdict.completed_steps
        );
    }
    scene.finish().await;
}

/// The transaction obligation a datagram the flow scripts NO step for leaves on
/// its leg (RFC 3261 §9.2). Leg B's document writes no step for the CANCEL the
/// system relays, nor for the pair it draws: the endpoint answers `200` to the
/// CANCEL and `487` to the INVITE it names while the arrival stays the refusal
/// it is, and the flow walks on to the ACK those answers draw.
#[tokio::test(start_paused = true)]
async fn an_unscripted_cancel_is_refused_and_answered_200_then_487() {
    let scene = api_scene("pivot-unscripted-cancel").await;
    let (outcome, _dir) = replay(&scene, "unscripted-cancel.v3.json").await;

    // The refusal STANDS: the CANCEL is a divergence the verdict names.
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { leg, .. } if leg == "B"))
        .unwrap_or_else(|| panic!("the CANCEL was not refused: {:#?}", outcome.verdict.failures));
    let Failure::UnmatchedDatagram { arrived, .. } = refused else { unreachable!("matched above") };
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "CANCEL"), "{arrived}");

    // And the endpoint answered it all the same, out of leg B's own stack.
    let legs = outcome.recording.legs();
    let answers: Vec<_> = legs["B"]
        .iter()
        .filter(|m| m.dir == Dir::Out && m.note.as_deref().is_some_and(is_unscripted_answer))
        .collect();
    assert_eq!(answers.len(), 2, "the §9.2 pair: {:#?}", legs["B"]);
    assert!(
        text(answers[0]).starts_with("SIP/2.0 200") && text(answers[0]).contains("CSeq: 1 CANCEL"),
        "{}",
        text(answers[0])
    );
    assert!(
        text(answers[1]).starts_with("SIP/2.0 487") && text(answers[1]).contains("CSeq: 1 INVITE"),
        "{}",
        text(answers[1])
    );

    // No cursor moved and no expect was satisfied: neither answer belongs to a
    // step, and the CANCEL that drew them belongs to none either.
    assert!(answers.iter().all(|m| m.step.is_none()), "an answer owns no step: {answers:#?}");
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::In && text(m).starts_with("CANCEL ") && m.step.is_some())),
        "the refused CANCEL satisfied an expect: {:#?}",
        legs["B"]
    );
    // The script was never abandoned: the flow took the ACK the 487 drew and ran
    // to its end.
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    for step in ["s10", "s11", "s12", "s13"] {
        assert!(
            outcome.verdict.completed_steps.contains(&step.to_string()),
            "{step} missing from {:?}",
            outcome.verdict.completed_steps
        );
    }
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The act RFC 3262 §3 makes the endpoint's own: a PRACK no step scripts is
/// answered `200` when its RAck names the reliable provisional this leg sent,
/// while the arrival stays the refusal it is. The document declares it — the
/// source never PRACKed its callee's reliable 180, and this platform relays the
/// caller's PRACK end to end — so the callee's `200` is what lets the relayed
/// `200 PRACK` reach the caller and the flow walk on to the 2xx, the ACK the
/// document also declares, and the scripted teardown.
#[tokio::test(start_paused = true)]
async fn an_unscripted_prack_is_refused_and_answered_200_so_the_relay_walks_on() {
    let scene = api_scene("pivot-unscripted-prack").await;
    let (outcome, _dir) = replay(&scene, "unscripted-prack-negative.v3.json").await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);

    // Both declarations were produced, the PRACK first: the refusal STANDS,
    // raised with its site, and the verdict inverts on it.
    let declared: Vec<DeclaredFailure> =
        outcome.verdict.must_fail.iter().map(|note| note.failure).collect();
    assert_eq!(declared, [DeclaredFailure::UnexpectedPrack, DeclaredFailure::UnexpectedAck]);
    for note in &outcome.verdict.must_fail {
        let observed = note.observed.as_ref().unwrap_or_else(|| panic!("{note:#?}"));
        assert_eq!(arrival_of(observed).0, "B", "the anchor's own leg: {note:#?}");
    }

    // And the endpoint answered the PRACK all the same, out of leg B's own
    // stack, right behind the arrival it refused.
    let legs = outcome.recording.legs();
    let prack = legs["B"]
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("PRACK "))
        .unwrap_or_else(|| panic!("no PRACK on leg B: {:#?}", legs["B"]));
    assert_eq!(legs["B"][prack].step, None, "no step claimed it — that is the failure");
    let answer = &legs["B"][prack + 1];
    assert!(
        answer.dir == Dir::Out
            && text(answer).starts_with("SIP/2.0 200")
            && text(answer).contains("CSeq: 2 PRACK")
            && answer.note.as_deref().is_some_and(is_unscripted_answer),
        "{answer:#?}"
    );
    assert_eq!(answer.step, None, "an answer owns no step");

    // The caller's own `200 PRACK` expect was satisfied by the relay, so the
    // script ran to its end and nothing was abandoned.
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    assert_eq!(outcome.verdict.completed_steps.len(), 14, "{:?}", outcome.verdict.completed_steps);

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The other act the RFC makes the endpoint's own (§17.1.1.3): a non-2xx final
/// no step scripts is ACKed on the INVITE's own branch, where the arrival is
/// refused — not left for the generic close to discover.
#[tokio::test(start_paused = true)]
async fn an_unscripted_non_2xx_final_is_refused_and_acked_on_the_invite_s_branch() {
    let scene = api_scene("pivot-unscripted-final").await;
    let (outcome, _dir) = replay(&scene, "unscripted-final.v3.json").await;

    // The refusal STANDS: the document waited for a ring and a final arrived.
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { leg, .. } if leg == "A"))
        .unwrap_or_else(|| panic!("the final was not refused: {:#?}", outcome.verdict.failures));
    let Failure::UnmatchedDatagram { arrived, .. } = refused else { unreachable!("matched above") };
    let Arrived::Response { status, .. } = arrived else { panic!("{arrived}") };
    assert!((300..700).contains(status), "a non-2xx final: {arrived}");

    // The ACK it owes went out where the refusal happened, on the transaction's
    // own branch, and it belongs to no step.
    let legs = outcome.recording.legs();
    let out = |prefix: &str| {
        legs["A"]
            .iter()
            .find(|m| m.dir == Dir::Out && text(m).starts_with(prefix))
            .unwrap_or_else(|| panic!("no outbound {prefix} on leg A: {:#?}", legs["A"]))
            .clone()
    };
    let ack = out("ACK ");
    assert!(ack.note.as_deref().is_some_and(is_unscripted_answer), "{:?}", ack.note);
    assert!(ack.step.is_none(), "the ACK owns no step: {ack:#?}");
    assert!(text(&ack).contains("CSeq: 1 ACK"), "{}", text(&ack));
    assert_eq!(
        via_branch(&text(&ack)),
        via_branch(&text(&out("INVITE "))),
        "§17.1.1.3: same branch"
    );

    // And ONLY the act the RFC names: the ACK the system sends leg B for its own
    // 486 is unscripted too, and no rule makes it anyone's to answer — it is
    // refused, and nothing is invented in reply.
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(
            f,
            Failure::UnexpectedDatagram { leg, arrived: Arrived::Request { method, .. }, .. }
                if leg == "B" && method == "ACK"
        )),
        "{:#?}",
        outcome.verdict.failures
    );
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::Out && m.note.as_deref().is_some_and(is_unscripted_answer))),
        "leg B answered something no rule makes its own: {:#?}",
        legs["B"]
    );

    // The close never had to compose it: the obligation was discharged at the
    // arrival, so what the abandoned script closed holds no ACK for leg A.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert!(
        !abandoned.closed.iter().any(|act| act.leg == "A" && act.owed == CloseOwed::Ack),
        "the close re-ACKed what the refusal had already answered: {:#?}",
        abandoned.closed
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// RFC 3261 §15.1.2 makes the answer to a BYE the endpoint's own whether or
/// not its flow is still running. The callee's script ends at the ACK, the
/// caller's BYE is the flow's last step, and the system relays that BYE onto
/// leg B once the flow has completed: the arrival is the late datagram it is —
/// the finding stands, the verdict is what it was — AND leg B answers it `200`
/// out of its own stack, so the system's BYE transaction ends there instead of
/// retransmitting to Timer F and the run settles as soon as the call is gone.
#[tokio::test(start_paused = true)]
async fn a_bye_taken_after_the_flow_completed_is_answered_200_and_still_a_late_arrival() {
    let scene = api_scene("pivot-late-bye").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-late-bye".to_string();
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(step)
            if ["s11", "s12", "s13"].contains(&step.id.as_str()))
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The finding stands, and it is the only one: the flow completed and a BYE
    // arrived on a leg that scripted nothing for it.
    assert_eq!(outcome.verdict.completed_steps.len(), 10, "{:?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    let late: Vec<&Failure> = outcome
        .verdict
        .failures
        .iter()
        .filter(|f| matches!(f, Failure::DatagramAfterFlow { .. }))
        .collect();
    assert!(
        matches!(
            late.as_slice(),
            [Failure::DatagramAfterFlow { leg, arrived: Arrived::Request { method, .. } }]
                if leg == "B" && method == "BYE"
        ),
        "{:#?}",
        outcome.verdict.failures
    );
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);

    // The endpoint answered it all the same, out of leg B's own stack, right
    // behind the arrival it recorded as late.
    let legs = outcome.recording.legs();
    let bye = legs["B"]
        .iter()
        .position(|m| m.dir == Dir::In && text(m).starts_with("BYE "))
        .unwrap_or_else(|| panic!("no BYE on leg B: {:#?}", legs["B"]));
    let taken = &legs["B"][bye];
    assert!(
        taken.note.as_deref().is_some_and(|note| note.contains("after the flow completed")),
        "{taken:#?}"
    );
    let taken_text = text(taken);
    let cseq = taken_text
        .lines()
        .find_map(|line| line.strip_prefix("CSeq: "))
        .expect("the BYE carries a CSeq");
    let answer =
        legs["B"].get(bye + 1).unwrap_or_else(|| panic!("nothing answered the BYE: {taken:#?}"));
    assert!(
        answer.dir == Dir::Out
            && text(answer).starts_with("SIP/2.0 200")
            && text(answer).contains(&format!("\r\nCSeq: {cseq}\r\n"))
            && answer.note.as_deref().is_some_and(is_unscripted_answer),
        "{answer:#?}"
    );
    assert_eq!(answer.step, None, "an answer owns no step");
    assert_eq!(
        via_branch(&text(answer)),
        via_branch(&text(taken)),
        "§17.2.2: the BYE's own branch"
    );

    // Answered once, the system's transaction never retransmitted, and the run
    // settled as soon as the call was gone — well inside the first rung of the
    // Timer E ladder a silent leg would have drawn.
    assert!(
        legs["B"]
            .iter()
            .all(|m| !(m.dir == Dir::In && text(m).starts_with("BYE ") && m.repeat_of.is_some())),
        "the BYE retransmitted: {:#?}",
        legs["B"]
    );
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    let taken_ms = taken.at_us / 1000;
    assert!(
        settled < taken_ms + 500,
        "settled at {settled} ms, the BYE arrived at {taken_ms} ms: the settle waited on a ladder"
    );
    // And the caller's own BYE drew the final §15.1.2 owes it, which the
    // document never scripted either.
    assert!(
        legs["A"].iter().any(|m| m.dir == Dir::In
            && text(m).starts_with("SIP/2.0 200")
            && m.note.as_deref().is_some_and(|note| note.contains("15.1.2"))),
        "{:#?}",
        legs["A"]
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// RFC 3261 §15.1.2, last paragraph: a UAS that answered a BYE still responds
/// to every request pending on that dialog, 487 recommended.
///
/// The caller re-INVITEs, then hangs up without waiting for the answer. The
/// system relays both onto a callee that scripts neither: the re-INVITE is
/// refused (nothing answers an unscripted INVITE — its answer is the
/// document's call decision, §14.2), then the BYE lands on the held dialog and
/// draws its 200 — and the re-INVITE its 487, out of leg B's own stack, so the
/// system's INVITE client transaction ends on a final instead of Timer B.
///
/// The 487 holds leg B's server transaction in Completed until the ACK the
/// system owes it on the INVITE's branch (§17.1.1.3, §17.2.1), so the settle
/// waits for that ACK — it is milliseconds behind — and records it as the
/// transaction's own closer, not as a datagram nothing scripted.
#[tokio::test(start_paused = true)]
async fn a_re_invite_pending_when_the_bye_is_answered_draws_487_behind_the_200() {
    let scene = api_scene("pivot-reinvite-under-bye").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "linear-attempt-reinvite-under-bye".to_string();
    // The callee's BYE steps go: leg B scripts nothing past the confirming ACK.
    case.document.flow.retain(|node| {
        !matches!(node, pivot_schema::flow::FlowNode::Message(step)
            if ["s11", "s12"].contains(&step.id.as_str()))
    });
    // The re-INVITE (the caller's offer again) and its 100, 50 ms after the ACK.
    let mut reinvite = step_of(&mut case, "s1").clone();
    reinvite.id = "s101".into();
    reinvite.in_dialog = true;
    reinvite.msg.ruri = None;
    reinvite.msg.from = None;
    reinvite.msg.to = None;
    reinvite.msg.headers.clear();
    reinvite.delay.from = "step:s9".parse().expect("s9 is a step id");
    reinvite.delay.ms = 50;
    let mut trying = step_of(&mut case, "s2").clone();
    trying.id = "s102".into();
    trying.in_dialog = true;
    trying.msg.cseq = Some(2);
    trying.delay.from = "step:s101".parse().expect("s101 is a step id");
    // The hang-up follows the 100 by 100 ms, well ahead of any answer.
    let bye = step_of(&mut case, "s10");
    bye.delay.from = "step:s102".parse().expect("s102 is a step id");
    bye.delay.ms = 100;
    step_of(&mut case, "s13").delay.from = "step:s10".parse().expect("s10 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [reinvite, trying].into_iter().enumerate() {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    let legs = outcome.recording.legs();
    let b = &legs["B"];
    let position =
        |predicate: &dyn Fn(&pivot_schema::bundle::recording::RecordedMessage) -> bool| {
            b.iter().position(predicate).unwrap_or_else(|| panic!("not on leg B: {b:#?}"))
        };
    let reinvite_at = position(&|m| {
        m.dir == Dir::In
            && text(m).starts_with("INVITE ")
            && text(m).contains("\r\nCSeq: 2 INVITE\r\n")
    });
    let bye_at = position(&|m| m.dir == Dir::In && text(m).starts_with("BYE "));
    assert!(reinvite_at < bye_at, "the re-INVITE landed before the BYE: {b:#?}");
    let bye_text = text(&b[bye_at]);
    let bye_cseq = bye_text
        .lines()
        .find_map(|line| line.strip_prefix("CSeq: "))
        .expect("the BYE carries a CSeq");
    // The 200 to the BYE, then the 487 to the pending INVITE, both the leg's own.
    let ok = b.get(bye_at + 1).unwrap_or_else(|| panic!("nothing answered the BYE: {b:#?}"));
    assert!(
        ok.dir == Dir::Out
            && text(ok).starts_with("SIP/2.0 200")
            && text(ok).contains(&format!("\r\nCSeq: {bye_cseq}\r\n"))
            && ok.note.as_deref().is_some_and(is_unscripted_answer),
        "{ok:#?}"
    );
    let terminated =
        b.get(bye_at + 2).unwrap_or_else(|| panic!("nothing answered the re-INVITE: {b:#?}"));
    assert!(
        terminated.dir == Dir::Out
            && text(terminated).starts_with("SIP/2.0 487")
            && text(terminated).contains("\r\nCSeq: 2 INVITE\r\n")
            && terminated.note.as_deref().is_some_and(is_unscripted_answer),
        "{terminated:#?}"
    );
    assert_eq!(
        via_branch(&text(terminated)),
        via_branch(&text(&b[reinvite_at])),
        "§17.2.3: the INVITE's own branch"
    );
    // Both answered in the instant the BYE landed.
    assert_eq!(ok.at_us, b[bye_at].at_us, "{ok:#?}");
    assert_eq!(terminated.at_us, b[bye_at].at_us, "{terminated:#?}");
    // The system ACKed the 487 on the INVITE's own branch (§17.1.1.3), and the
    // recording holds that ACK behind the 487 — past the INVITE repeat its
    // Timer A had already put on the wire — noted as the closer the settle
    // waited for. The run's one late-arrival finding is the caller's 487,
    // raised before this ACK lands, so the note is what pins the absorption.
    let ack = b[bye_at + 3..]
        .iter()
        .find(|m| m.dir == Dir::In && text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("no ACK followed the 487 on leg B: {b:#?}"));
    assert!(
        text(ack).contains("\r\nCSeq: 2 ACK\r\n")
            && ack.note.as_deref().is_some_and(|note| note.contains("17.1.1.3")),
        "{ack:#?}"
    );
    assert_eq!(
        via_branch(&text(ack)),
        via_branch(&text(terminated)),
        "§17.1.1.3: the INVITE's branch"
    );
    // And the run settled on the teardown at once: the ACK is milliseconds
    // behind the 487, and nothing else held the run open past it.
    let settled = outcome.timing.settled_at_ms.expect("the run settled");
    assert!(
        settled < b[bye_at].at_us / 1000 + 500,
        "settled at {settled} ms: the settle waited on a ladder"
    );
    assert!(
        settled >= ack.at_us / 1000,
        "settled at {settled} ms, before the ACK at {} us",
        ack.at_us
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The recording note an emission no step scripts carries.
fn is_unscripted_answer(note: &str) -> bool {
    note.starts_with("the transaction the flow never scripted")
}

/// A message's topmost Via branch.
fn via_branch(raw: &str) -> String {
    raw.lines()
        .find_map(|line| line.split_once("branch="))
        .map(|(_, branch)| branch.trim().to_string())
        .expect("every emission carries a Via branch")
}

#[tokio::test(start_paused = true)]
async fn rung_two_an_alt_commits_to_the_branch_that_arrived_and_discards_the_other() {
    let scene = api_scene("pivot-alt-answered").await;
    let (outcome, dir) = replay(&scene, "alt-answered.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        outcome.verdict.branches.get("a1").map(String::as_str),
        Some("answered"),
        "the run states which branch it took"
    );
    assert!(
        !outcome.verdict.completed_steps.contains(&"s12".to_string()),
        "the branch that did not run contributes no step"
    );
    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_claim_is_scoped_to_the_socket_the_invite_arrived_on() {
    // TWO calls, dialled at two different sockets, each callee leg claiming by
    // `ruri-pos` on its own endpoint. `ep-a-second` sorts FIRST, so offering an
    // INVITE to every endpoint instead of to the one it arrived on hands call
    // one to leg D and fails the run.
    let scene = api_scene("pivot-two-endpoint-claim").await;
    let second = scene.h.agent("second", &format!("127.0.0.1:{IDLE_PORT}")).await;
    let extra = BTreeMap::from([("uas2".to_string(), second)]);
    let (outcome, dir) = replay_on(&scene, "two-endpoint-claim.v3.json", extra).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 22, "both calls ran end to end");
    // Every leg heard something: the second socket is real traffic, not a
    // declaration.
    let legs = outcome.recording.legs();
    for leg in ["A", "B", "C", "D"] {
        assert!(!legs[leg].is_empty(), "leg {leg} heard nothing: {legs:#?}");
    }
    // And the claim landed on the socket that received it.
    let on_d = legs["D"].iter().any(|m| m.step.as_deref() == Some("s10"));
    assert!(on_d, "the second call's INVITE claimed leg D: {:#?}", legs["D"]);
    scene.finish().await;
}

/// The red path: the wire is RFC-complete on both sides, and the DOCUMENT is
/// what is wrong. The run must fail by name, at the right step, and still leave
/// its bundle behind.
///
/// The 200 that arrives is a FINAL on the very transaction `s5` waits for, so
/// the 486 it gates on can never come and the run CANNOT GO ON (§11.2). Polarity
/// has nothing to do with it: this document declares nothing, and its script
/// still ends, its call is still closed generically, and it still settles.
#[tokio::test(start_paused = true)]
async fn a_run_whose_document_asserts_the_wrong_final_fails_by_name_and_keeps_its_bundle() {
    let scene = api_scene("pivot-wrong-final-status").await;
    let (outcome, dir) = replay(&scene, "wrong-final-status.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    let unmatched = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { .. }))
        .unwrap_or_else(|| {
            panic!("no unmatched-datagram failure: {:#?}", outcome.verdict.failures)
        });
    let Failure::UnmatchedDatagram { step, leg, gated_on, arrived, .. } = unmatched else {
        unreachable!("matched above")
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s5", "A"));
    // A 486 to an INVITE is a final a compliant callee CAN send, so the gate is
    // rejecting a PLAUSIBLE assertion, not an impossible one.
    assert!(matches!(gated_on, GatedOn::Response { status: 486, .. }), "{gated_on}");
    assert!(matches!(arrived, Arrived::Response { status: 200, .. }), "{arrived}");
    // The document's own error is the WHOLE account: the close ended the call it
    // could no longer script, so nothing is piled on top of the real finding.
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some(), "a run whose call was closed settles");

    // A POSITIVE run that cannot go on abandons and closes exactly like any
    // other: leg A acknowledges the 2xx it took and BYEs the dialog it opened,
    // leg B answers the teardown that reaches the side which ANSWERED.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s5")));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(
        acts,
        [("A", CloseOwed::Ack), ("A", CloseOwed::Bye), ("B", CloseOwed::Answer)],
        "{:#?}",
        abandoned.closed
    );
    scene.b2bua.assert_fully_reaped();

    // The evidence survives the failure.
    for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
        assert!(dir.join(file).is_file(), "{file} missing from a failed run's bundle");
    }
    let written: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written.failed_step.as_deref(), Some("s5"));
    assert!(dir.join("recording/A.jsonl").is_file());
    assert!(!std::fs::read_to_string(dir.join("recording/A.jsonl")).unwrap().is_empty());

    scene.finish().await;
}

/// §14 item 12 is unconditional: EVERY message, in wire order. A retransmission
/// the receive view absorbs is still a message the run saw, so it is in the
/// bundle — noted as absorbed — and it still never reaches an expect.
#[tokio::test(start_paused = true)]
async fn an_absorbed_retransmission_is_recorded_and_never_reaches_an_expect() {
    let scene = api_scene("pivot-retransmit-absorbed").await;
    let (outcome, dir) = replay(&scene, "retransmit-absorbed.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    let legs = outcome.recording.legs();
    let absorbed: Vec<_> = legs
        .values()
        .flatten()
        .filter(|m| m.note.as_deref().is_some_and(|n| n.starts_with("absorbed")))
        .collect();
    assert!(
        !absorbed.is_empty(),
        "the held ring draws a Timer-A retransmission; none is recorded: {legs:#?}"
    );
    // It is a repeat of something already there, and it carries its bytes.
    for message in &absorbed {
        assert!(message.step.is_none(), "an absorbed repeat satisfies no step");
        assert!(!text(message).is_empty(), "an absorbed repeat is recorded verbatim");
    }
    // And the bundle on disk carries it, not just the in-memory handle.
    let written = std::fs::read_to_string(dir.join("recording/B.jsonl")).unwrap();
    assert!(written.contains("absorbed"), "{written}");

    // Issue 22: the interpreter agent mode exposes BOTH views over that one
    // stream — the retransmission is in the wire view and not in the TU view.
    let wire = &outcome.wire_view;
    let tu = outcome.tu_view();
    assert!(
        wire.len() > tu.len(),
        "the wire view is the wider one: {} vs {}",
        wire.len(),
        tu.len()
    );
    let repeats: Vec<_> = wire.iter().filter(|e| e.is_repeat()).collect();
    assert_eq!(repeats.len(), absorbed.len(), "one wire-view repeat per recorded absorption");
    assert!(
        repeats.iter().all(|e| e.start_line().starts_with("INVITE")),
        "the held ring's Timer-A retransmission is the repeat: {repeats:#?}",
    );
    assert!(!tu.iter().any(|e| e.is_repeat()), "no absorbed repeat reached the transaction user",);

    scene.finish().await;
}

/// The `retransmits` COUNT (§6.9), both directions of one call and both halves
/// of the issue-22 two-view table.
///
/// The caller's own ladder is EMITTED — three byte-identical INVITEs, paced by
/// RFC 3261 §17.1.1.2's Timer A, because the document states how many repeats
/// there were and never when. Each one draws the platform's replayed 100
/// (§17.2.1), which reaches the transaction user and is counted on the leg that
/// sees it; meanwhile the platform's own Timer A re-dials the callee, and THOSE
/// repeats are absorbed by the callee's transaction layer and exist in the wire
/// view alone.
#[tokio::test(start_paused = true)]
async fn a_declared_retransmit_ladder_is_emitted_paced_and_counted_in_both_views() {
    let Some(case) = case_dir("bc-rc-retransmit-ladder") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-retransmit-ladder").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");

    // The verdict states every declared ladder, and what the run actually saw.
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s1", (2, 2)), ("s2", (2, 2)), ("s3", (2, 2))]),
        "{:#?}",
        outcome.verdict.retransmits
    );

    let legs = outcome.recording.legs();
    // The emitted ladder: three identical INVITEs, the repeats pointing at the
    // first and paced by Timer A — T1, then 2·T1.
    let sent: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("INVITE ")).collect();
    assert_eq!(sent.len(), 3, "{:#?}", legs["A"]);
    assert!(sent.iter().all(|m| text(m) == text(sent[0])), "byte-identical repeats");
    assert_eq!((sent[1].repeat_of, sent[2].repeat_of), (Some(sent[0].seq), Some(sent[0].seq)));
    assert_eq!(
        (sent[1].at_us - sent[0].at_us, sent[2].at_us - sent[0].at_us),
        (500_000, 1_500_000),
        "RFC 3261 §17.1.1.2: T1, then 2·T1"
    );
    assert!(sent.iter().all(|m| m.step.as_deref() == Some("s1")), "the step emitted all three");

    // The platform's replay of its cached 100 (§17.2.1) reaches the transaction
    // user: three on the leg, one claimed by s2 and two counted against it.
    let trying: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("SIP/2.0 100")).collect();
    assert_eq!(trying.len(), 3, "{:#?}", legs["A"]);
    assert_eq!(trying[0].step.as_deref(), Some("s2"));
    for repeat in &trying[1..] {
        assert_eq!(repeat.step, None, "a repeat satisfies no second expect");
        assert_eq!(repeat.repeat_of, Some(trying[0].seq));
        assert!(repeat.note.as_deref().is_some_and(|n| n.contains("s2")), "{repeat:#?}");
    }

    // The platform's own Timer-A ladder toward the callee is ABSORBED: one
    // INVITE at the transaction user, two more in the wire view.
    let dialled: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("INVITE ")).collect();
    assert_eq!(dialled.len(), 3, "{:#?}", legs["B"]);
    assert_eq!(dialled[0].step.as_deref(), Some("s3"));
    for repeat in &dialled[1..] {
        assert!(repeat.note.as_deref().is_some_and(|n| n.starts_with("absorbed")), "{repeat:#?}");
        assert_eq!(repeat.repeat_of, Some(dialled[0].seq));
    }

    // And the two views agree with the two counts. The absorbed ladder is the
    // §17.2 repeat; the replayed provisional is not one — a provisional is never
    // deduped, deliberately — so the count on s2 is the STEP's ladder, not the
    // seam's.
    let wire = &outcome.wire_view;
    let tu = outcome.tu_view();
    let count = |view: &[scenario_harness::absorption::WireEntry], prefix: &str, repeat: bool| {
        view.iter()
            .filter(|e| e.start_line().starts_with(prefix) && e.is_repeat() == repeat)
            .count()
    };
    assert_eq!(count(wire, "INVITE", true), 2, "the absorbed ladder");
    assert_eq!(count(&tu, "INVITE", true), 0, "and none of it reached the transaction user");
    assert_eq!(count(wire, "SIP/2.0 100", false), 3, "every replayed provisional surfaces");

    // The bundle on disk carries the ladder, not just the handle.
    let written = std::fs::read_to_string(dir.join("recording/A.jsonl")).unwrap();
    assert!(written.contains("\"repeat_of\""), "{written}");
    let verdict: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(verdict.retransmits.len(), 3);

    scene.finish().await;
}

/// The other half of the two-view table: a retransmitted FINAL and the ACKs it
/// draws, neither of which the transaction layer absorbs.
///
/// The callee repeats its 200 on §13.3.1.4's own pacing and the caller HOLDS its
/// ACK — a held automatic is the step's `delay`, never a deviation (§11) — so
/// the platform runs its own 2xx ladder toward the caller, which the run counts
/// on the step that ladder repeats. Each 2xx the platform receives draws its own
/// ACK (§13.2.2.4) on the one client transaction the first 2xx armed: the copies
/// that land before the caller's ACK are answered as soon as that ACK supplies
/// the body, so three 200s leave three ACKs on the b-leg.
#[tokio::test(start_paused = true)]
async fn a_retransmitted_final_is_counted_and_each_2xx_draws_its_own_ack() {
    let Some(case) = case_dir("bc-rc-held-ack-final-ladder") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-held-ack-final-ladder").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The whole flow ran and the run settled: this is a counted ladder, not a
    // broken call.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some());

    // The declared ladders and what the lane produced, side by side.
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s6", (2, 2)), ("s7", (2, 2)), ("s9", (2, 2))]),
        "{:#?}",
        outcome.verdict.retransmits
    );
    // Every declared ladder holds, so the run is green.
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.failed_step, None);

    let legs = outcome.recording.legs();
    // The callee's own emitted ladder, paced by §13.3.1.4: T1, then 2·T1.
    let sent: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200") && m.step.as_deref() == Some("s6"))
        .collect();
    assert_eq!(sent.len(), 3, "{:#?}", legs["B"]);
    assert_eq!(
        (sent[1].at_us - sent[0].at_us, sent[2].at_us - sent[0].at_us),
        (500_000, 1_500_000)
    );
    // One ACK per 2xx received (§13.2.2.4), each answering the copy above it and
    // all three the same bytes — one client transaction, so the callee's INVITE
    // server transaction quiesces on whichever arrives.
    let acks: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 3, "{:#?}", legs["B"]);
    assert!(acks.iter().all(|a| text(a) == text(acks[0])), "{:#?}", acks);
    assert!(
        acks.iter().zip(&sent).all(|(a, s)| a.at_us > s.at_us),
        "each ACK follows the 2xx it answers"
    );

    // The platform's own ladder toward the held caller, counted where it lands.
    let to_invite: Vec<_> = legs["A"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(to_invite.len(), 3, "{:#?}", legs["A"]);
    assert!(to_invite[1..].iter().all(|m| m.repeat_of == Some(to_invite[0].seq)));
    assert!(to_invite[1..].iter().all(|m| m.step.is_none()), "a repeat satisfies no expect");

    // The trap row of the table: a 2xx retransmission is end-to-end, so every
    // repeat is in the wire view AND in the TU view.
    let tu = outcome.tu_view();
    let repeats = |view: &[scenario_harness::absorption::WireEntry], prefix: &str| {
        view.iter().filter(|e| e.start_line().starts_with(prefix) && e.is_repeat()).count()
    };
    assert_eq!(repeats(&outcome.wire_view, "SIP/2.0 200"), 2, "the platform's 2xx ladder");
    assert_eq!(repeats(&tu, "SIP/2.0 200"), 2, "2xx retransmission is never absorbed");
    assert!(dir.join("verdict.json").is_file(), "the run keeps its bundle");

    scene.finish().await;
}

/// The caller-side half of the same table: a count on an auto ACK step, which
/// the WIRE draws rather than a timer paces (§6.3).
///
/// The callee answers ONCE, so every repeat of the 200 on the caller's leg is
/// the platform's own un-ACKed-2xx watchdog and every ACK beyond the first is
/// one the caller's own core owed. Both copies arrive while the ACK is HELD:
/// RFC 3261 §13.2.2.4 owes one ACK per 2xx RECEIVED, so the hold releases all
/// three at once, and the count on the ACK step is what says so.
///
/// The callee's leg draws NO count from any of it: one final received there is
/// one ACK owed, and an ACK the far leg sends is that dialog's obligation, not
/// this one's — so `s9` declares no ladder and reports no note.
#[tokio::test(start_paused = true)]
async fn a_held_ack_s_count_is_drawn_by_the_repeats_of_the_final_its_transaction_drew() {
    let Some(case) = case_dir("bc-rc-drawn-ack-per-2xx") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-drawn-ack-per-2xx").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.failures);
    assert!(outcome.timing.settled_at_ms.is_some());
    let ladders: BTreeMap<&str, (u32, u32)> = outcome
        .verdict
        .retransmits
        .iter()
        .map(|note| (note.step.as_str(), (note.declared, note.observed)))
        .collect();
    assert_eq!(
        ladders,
        BTreeMap::from([("s7", (2, 2)), ("s8", (2, 2))]),
        "{:#?}",
        outcome.verdict.retransmits
    );
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);

    let legs = outcome.recording.legs();
    // The callee answered once: nothing on ITS leg repeats the final.
    let answered: Vec<_> = legs["B"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(answered.len(), 1, "{:#?}", legs["B"]);

    // Three 200s reach the held caller, and the three ACKs it drew all leave
    // AFTER the last of them: the hold is what makes the catch-up visible.
    let finals: Vec<_> = legs["A"]
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 200 ") && text(m).contains("CSeq: 1 INVITE"))
        .collect();
    assert_eq!(finals.len(), 3, "{:#?}", legs["A"]);
    assert!(finals[1..].iter().all(|m| m.repeat_of == Some(finals[0].seq)));
    let acks: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 3, "one ACK per 2xx received: {:#?}", legs["A"]);
    assert!(acks.iter().all(|a| a.step.as_deref() == Some("s8")), "{acks:#?}");
    assert!(acks.iter().all(|a| text(a) == text(acks[0])), "one client transaction, one ACK");
    assert!(acks[1..].iter().all(|a| a.repeat_of == Some(acks[0].seq)));
    assert!(acks[0].at_us > finals[2].at_us, "the ACK was held past the last copy");

    // The callee took ONE final, so it is owed ONE ACK: the caller's re-passes
    // discharge the caller dialog's obligation and never reach this leg.
    let relayed: Vec<_> = legs["B"].iter().filter(|m| text(m).starts_with("ACK ")).collect();
    assert_eq!(relayed.len(), 1, "one ACK per final received: {:#?}", legs["B"]);
    assert!(dir.join("verdict.json").is_file(), "the run keeps its bundle");

    scene.finish().await;
}

/// The false-success guard, end to end: a document whose defect IS the emission
/// must never run green while the interpreter emits a compliant message. The
/// fixture asks a `verbatim-emission` to preserve the ABSOLUTE header order —
/// the interleaving of the stored block with the tier-1 lines, which the
/// document does not store and this emission cannot restore — so the run stops
/// at that step, before anything reaches the wire.
#[tokio::test(start_paused = true)]
async fn a_deviation_this_interpreter_cannot_emit_stops_the_run_before_the_wire() {
    let scene = api_scene("pivot-verbatim-refused").await;
    let (outcome, dir) = replay(&scene, "verbatim-emission-refused.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s1"));
    let refusal = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::DeviationUnimplemented { .. }))
        .unwrap_or_else(|| panic!("no deviation refusal: {:#?}", outcome.verdict.failures));
    let Failure::DeviationUnimplemented { deviation, kind, step, reason } = refusal else {
        unreachable!("matched above")
    };
    assert_eq!((deviation.as_str(), kind.as_str()), ("d1", "verbatim-emission"));
    assert_eq!(step.as_deref(), Some("s1"));
    assert!(reason.contains("does not guarantee"), "{reason}");
    assert!(
        outcome.recording.is_empty(),
        "nothing reached the wire: {:#?}",
        outcome.recording.legs()
    );
    assert!(dir.join("verdict.json").is_file());

    scene.finish().await;
}

/// A LOOPBACK endpoint: one socket hosts both the caller and the callee, which
/// is the binding every captured document uses. The callee's `background`
/// policy has to be found on the ENDPOINT, since only one of the two actors
/// sharing the socket is pumped.
#[tokio::test(start_paused = true)]
async fn rung_two_a_loopback_endpoint_hosts_both_roles_on_one_socket() {
    // The document dials the b-leg back at the socket the a-leg came from, and
    // the scene honours it — same decision as every other rung.
    let scene = api_scene("pivot-loopback-attempt").await;
    let extra = BTreeMap::from([("uas1".to_string(), scene.alice.clone())]);
    let (outcome, dir) = replay_on(&scene, "loopback-attempt.v3.json", extra).await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    // Both legs rode ONE socket, and each kept its own ladder.
    let legs = outcome.recording.legs();
    assert!(!legs["A"].is_empty() && !legs["B"].is_empty(), "{legs:#?}");
    // The `background` counters are not vacuous: real OPTIONS crossed the wire
    // and were answered outside the flow.
    let answered = legs
        .values()
        .flatten()
        .filter(|m| m.note.as_deref().is_some_and(|n| n.starts_with("background")))
        .count();
    assert!(answered >= 2, "the system's keepalives were answered: {legs:#?}");
    scene.finish().await;
}

/// The block-settling rule on its OTHER path (§6.8): the member that completes
/// the block is a tolerated absence the BUDGET retired, not a message.
///
/// §6.5's budget-release is a completion like any other, so the alt it closes
/// settles with it and the send anchored on that alt still fires. A release
/// path that stamped the step but not the block would strand that send with no
/// deadline and end the run `flow-incomplete`.
#[tokio::test(start_paused = true)]
async fn an_alt_its_released_absence_completes_still_settles_for_what_waits_on_it() {
    let scene = api_scene("pivot-alt-released-settles").await;
    let mut case = fixture("alt-answered.v3.json");
    case.document.case.id = "alt-released-settles".into();
    // The committed branch now ENDS on a tolerated absence: a 481 to the BYE the
    // callee already answered 200, which only its budget can retire.
    let mut absent = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the answered branch ends on the 200 to the BYE")
        .clone();
    absent.id = "s98".into();
    absent.msg.status = Some(481);
    absent.msg.reason = None;
    absent.optional = true;
    absent.within_ms = Some(250);
    absent.delay.from = "step:s11".parse().expect("s11 is a step id");
    absent.delay.ms = 0;
    let pivot_schema::flow::FlowNode::Alt(alt) =
        case.document.flow.iter_mut().find(|node| node.id() == "a1").expect("the fixture's alt")
    else {
        panic!("a1 is the alt")
    };
    alt.branches[0].steps.push(absent);
    // And a step OUTSIDE the alt waits on the block itself.
    let mut after = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the 200 to the BYE")
        .clone();
    after.id = "s99".into();
    after.msg.status = Some(481);
    after.msg.reason = None;
    after.optional = true;
    after.within_ms = Some(250);
    after.delay.from = "step:a1".parse().expect("a1 is the alt's own id");
    after.delay.ms = 0;
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(after)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    for released in ["s98", "s99"] {
        assert!(
            outcome.verdict.released_optional.iter().any(|step| step == released),
            "{released} was released by its budget: {:#?}",
            outcome.verdict.released_optional
        );
    }
    scene.finish().await;
}

/// `calls[] > 1`: two calls in one document, ordered against each other by the
/// one cross-call device the format has.
#[tokio::test(start_paused = true)]
async fn rung_three_two_calls_interleave_through_after() {
    let scene = api_scene("pivot-two-calls").await;
    let (outcome, dir) = replay(&scene, "two-calls.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 18, "every step of both calls ran");

    // `after` is an ordering, and the run honoured it: the second call's INVITE
    // went out only once the first call's ACK had landed, and the first call's
    // BYE only once the second had been refused.
    let order = |id: &str| {
        outcome
            .verdict
            .completed_steps
            .iter()
            .position(|s| s == id)
            .unwrap_or_else(|| panic!("{id} never completed"))
    };
    assert!(order("s7") < order("s8"), "the second call waited on the first call's ACK");
    assert!(order("s14") < order("s15"), "the first call's teardown waited on the second");
    // Four legs, four ladders.
    let legs = outcome.recording.legs();
    for leg in ["A", "B", "C", "D"] {
        assert!(legs.contains_key(leg) && !legs[leg].is_empty(), "leg {leg} is empty: {legs:#?}");
    }
    scene.finish().await;
}

/// Rung three, over an authored case LIBRARY outside this crate: a draft, its
/// own `resources/`, and nothing about it rewritten. **The lane-scoping
/// exemplar** (§9.1) and the `rfc_violations` one (§11.1).
///
/// A draft's SIP choreography — fifteen steps over two legs, a CANCEL crossing
/// the answer, an order-free teardown pair — runs to completion against a real
/// B2BUA and the run is GREEN, carrying two facts it does not gate on:
///
/// - the draft asserts CDR events in the vocabulary of the deployment it was
///   written for (`InviteReceived`, `Bye`) while this system spells them
///   `invite_received` and `bye`. Both checks state `cdr-vocabulary`, this lane
///   is not the draft's `origin_lane`, so both are evaluated and RECORDED —
///   the document keeps the fact and the run does not turn red for replaying it
///   somewhere else;
/// - the RFC 3261 §9.2 violation the case exists for is emitted by a scripted
///   peer, so it is listed loudly and gates nothing.
///
/// It skips loudly when no library is configured — an empty corpus must never
/// read as a pass.
#[tokio::test(start_paused = true)]
async fn rung_three_a_draft_runs_green_with_its_origin_lane_s_vocabulary_recorded() {
    let Some(case) = case_dir("bc-rc-cancel-xing-200ok-invite") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-cancel-xing").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);

    // Every one of the draft's steps ran, in its own document's terms.
    for step in (1..=15).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The order-free pair on the b-leg: the platform's ACK and its teardown BYE
    // both arrived, in whichever order.
    assert!(outcome.verdict.completed_steps.contains(&"s13".to_string()));
    assert!(outcome.verdict.completed_steps.contains(&"s14".to_string()));
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");

    // The violation the case exists to reproduce is LISTED and gates nothing:
    // the callee that answers after taking the CANCEL is a scripted peer, and
    // reproducing a peer's non-compliance is what the document is for.
    assert_eq!(outcome.verdict.rfc_violations.len(), 1, "{:#?}", outcome.verdict.rfc_violations);
    let note = &outcome.verdict.rfc_violations[0];
    assert_eq!(note.rule.to_string(), "no-200-after-cancel");
    assert_eq!(note.step, "s11");
    assert!(!note.gating, "a scripted peer's violation never gates");

    // And the residue gates nothing: the draft's two CDR event regexes are the
    // authoring lane's PascalCase vocabulary read off a snake_case stream, both
    // classified, both evaluated, both recorded as informative.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 2, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );

    // The bundle on disk carries both sections, because reading them there is
    // the whole point of recording rather than gating.
    let written: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written["status"], "ok");
    assert_eq!(written["informative"].as_array().map(Vec::len), Some(2), "{written:#}");
    assert_eq!(written["rfc_violations"][0]["rule"], "no-200-after-cancel");

    scene.finish().await;
}

/// The scene a RELIABLE-PROVISIONAL document replays against: the system keeps
/// the callee on 100rel, PRACKs each reliable provisional on the caller's
/// behalf, and shows the caller one bare 180.
///
/// It relays the dialled Request-URI userpart onto the b-leg — which the 18x
/// route decision alone does not do — so a `ruri-pos` claim still matches the
/// callee it names.
async fn reliable_provisional_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::builder(std::sync::Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |request| {
                    let mut route = b2bua::decision::test_adapter::route_to_with_18x(
                        "127.0.0.1",
                        bob_port,
                        call::features::RelayFirst18xStrategy::FakePrack,
                    );
                    route.new_ruri = Some(callee_ruri(&request.ruri, bob_port));
                    b2bua::decision::NewCallResponse::Route(route)
                })
                .build(),
        ))
    })
    .await
}

/// The b-leg Request-URI: the userpart the caller dialled, at the callee's
/// socket. URI reading is `sip-message`'s.
fn callee_ruri(dialled: &str, bob_port: u16) -> String {
    match sip_message::header::Uri::parse(&sip_message::SipStr::owned(dialled))
        .ok()
        .and_then(|uri| uri.user().map(str::to_string))
        .filter(|user| !user.is_empty())
    {
        Some(user) => format!("sip:{user}@127.0.0.1:{bob_port}"),
        None => format!("sip:127.0.0.1:{bob_port}"),
    }
}

/// Rung three, the RELIABLE-PROVISIONAL rung: a draft whose callee rings two
/// forked early dialogs under RFC 3262, and whose system manages 100rel on the
/// callee's behalf.
///
/// The scripted UAS answers the INVITE twice over — a reliable 183 under early
/// dialog `f1`, a reliable 180 under `f2` — each with its own `RSeq` and its own
/// To-tag, and answers the PRACK the system originates for each. What the caller
/// sees is one bare 180 and then the answering fork's own SDP, so the rung
/// exercises `early` demux (§6.1), an RSeq the DOCUMENT states, and an RAck the
/// system composes.
///
/// Its residue is the lane-scoping one (§9.1): the draft states its CDR facts in
/// the vocabulary of the platform it was authored against — the event words and
/// the reason strings its 100rel service writes — and this lane spells all four
/// differently. Every one is classified, so all four are evaluated, recorded and
/// gate nothing.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_forked_reliable_provisional_draft_prack_s_each_early_dialog() {
    let Some(case) = case_dir("forked-100rel-prack-per-early-dialog") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = reliable_provisional_scene("pivot-forked-100rel").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=18).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // Two forks rang on one leg, and each PRACK reached its OWN early dialog:
    // the b-leg recording carries two reliable provisionals under two distinct
    // To-tags, and the two PRACKs that arrived acknowledge the RSeq each stated.
    let b_leg = &outcome.recording.legs()["B"];
    let rseqs: Vec<String> = b_leg
        .iter()
        .filter_map(|m| Some(text(m).split("RSeq: ").nth(1)?.split("\r\n").next()?.to_string()))
        .collect();
    assert_eq!(rseqs, ["1", "7001"], "each fork rings under its own RSeq: {rseqs:?}");
    let racks: Vec<String> = b_leg
        .iter()
        .filter_map(|m| Some(text(m).split("RAck: ").nth(1)?.split("\r\n").next()?.to_string()))
        .collect();
    assert_eq!(racks.len(), 2, "one PRACK per fork: {racks:?}");
    assert!(racks[0].starts_with("1 "), "fork 1's PRACK acknowledges RSeq 1: {racks:?}");
    assert!(racks[1].starts_with("7001 "), "fork 2's PRACK acknowledges RSeq 7001: {racks:?}");
    assert!(
        racks.iter().all(|rack| rack.ends_with(" INVITE")),
        "an RAck acknowledges the INVITE transaction: {racks:?}"
    );

    // The residue is the authoring platform's CDR vocabulary and nothing else:
    // two event words and two reason strings, all four classified, evaluated and
    // recorded rather than gating.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 4, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// The MINTED-FORK rung: leg B answers one INVITE twice over — a 180 under
/// early dialog `f1`, a 180 under `f2` — and rejects under `f1`'s tag, while
/// the caller's expects name no fork at all (§6.1, RFC 3261 §12.1.1).
///
/// The proof is on the b-leg wire: two provisionals of ONE transaction under
/// two distinct To-tags, and a 486 riding the first of them, all relayed and
/// the attempt properly rejected end to end.
#[tokio::test(start_paused = true)]
async fn a_callee_side_fork_rings_twice_under_two_minted_tags() {
    let scene = api_scene("pivot-forked-ring").await;
    let (outcome, dir) = replay(&scene, "forked-ring.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    // Two forks of one leg reached the wire as two To-tags, and the final rode
    // the first fork's.
    let b_leg = &outcome.recording.legs()["B"];
    let to_tags: Vec<String> = b_leg
        .iter()
        .map(text)
        .filter(|m| m.starts_with("SIP/2.0 180") || m.starts_with("SIP/2.0 486"))
        .filter_map(|m| {
            let to = m.lines().find(|line| line.starts_with("To:"))?;
            let tag = to.split("tag=").nth(1)?;
            Some(tag.split(';').next().unwrap_or(tag).trim().to_string())
        })
        .collect();
    assert_eq!(to_tags.len(), 3, "two rings and a final: {to_tags:?}");
    assert_ne!(to_tags[0], to_tags[1], "two forks are two dialogs: {to_tags:?}");
    assert!(to_tags[0].ends_with("-early-f1") && to_tags[1].ends_with("-early-f2"), "{to_tags:?}");
    assert_eq!(to_tags[2], to_tags[0], "the 486 answers under f1's tag: {to_tags:?}");

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The INTERLEAVED-FORK rung: both forks ring reliably BEFORE either is
/// PRACKed, so each PRACK must ride its own early dialog — the fork's To-tag
/// and the fork's RAck on ONE message, and a CSeq numbered in that fork's own
/// space (RFC 3261 §12.2.1.1, RFC 3262 §7.2). The INVITE was each dialog's
/// CSeq 1, so BOTH PRACKs are CSeq 2; a leg-wide counter would number the
/// second one 3 and hand it the wrong To-tag.
///
/// The proof is the a-leg wire: the caller composes both PRACKs against the
/// two OBSERVED forks r1/r2 it learned from the system's a-facing tags.
#[tokio::test(start_paused = true)]
async fn an_interleaved_fork_s_prack_rides_its_own_early_dialog() {
    let scene = api_scene("pivot-forked-ring-interleaved").await;
    let (outcome, dir) = replay(&scene, "forked-ring-interleaved.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=19).map(|n| format!("s{n}")) {
        assert!(
            outcome.verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    let a_leg = &outcome.recording.legs()["A"];
    let header = |raw: &str, name: &str| -> Option<String> {
        let wanted = format!("{}:", name.to_ascii_lowercase());
        raw.lines()
            .find(|line| line.to_ascii_lowercase().starts_with(&wanted))
            .and_then(|line| line.split_once(':').map(|(_, v)| v.trim().to_string()))
    };
    let to_tag = |raw: &str| {
        header(raw, "To").and_then(|to| {
            to.split_once("tag=").map(|(_, t)| t.split(';').next().unwrap_or(t).trim().to_string())
        })
    };
    // The two reliable provisionals, in ring order: each fork's a-facing
    // To-tag and the RSeq the system minted toward the caller.
    let rings: Vec<(String, String)> = a_leg
        .iter()
        .filter(|m| text(m).starts_with("SIP/2.0 183") || text(m).starts_with("SIP/2.0 180"))
        .filter_map(|m| Some((to_tag(&text(m))?, header(&text(m), "RSeq")?)))
        .collect();
    assert_eq!(rings.len(), 2, "two reliable forks rang: {rings:?}");
    assert_ne!(rings[0].0, rings[1].0, "two forks are two dialogs: {rings:?}");
    // The two PRACKs, in send order: the To-tag and the RAck name ONE dialog,
    // and each fork numbers from its own INVITE-seeded space.
    let pracks: Vec<(String, String, String)> = a_leg
        .iter()
        .filter(|m| text(m).starts_with("PRACK "))
        .filter_map(|m| {
            Some((to_tag(&text(m))?, header(&text(m), "RAck")?, header(&text(m), "CSeq")?))
        })
        .collect();
    assert_eq!(pracks.len(), 2, "one PRACK per fork: {pracks:?}");
    assert_eq!(pracks[0].0, rings[0].0, "fork 1's PRACK rides fork 1's dialog: {pracks:?}");
    assert_eq!(pracks[1].0, rings[1].0, "fork 2's PRACK rides fork 2's dialog: {pracks:?}");
    assert_eq!(pracks[0].1, format!("{} 1 INVITE", rings[0].1), "fork 1's own RSeq: {pracks:?}");
    assert_eq!(pracks[1].1, format!("{} 1 INVITE", rings[1].1), "fork 2's own RSeq: {pracks:?}");
    assert_eq!(pracks[0].2, "2 PRACK", "fork 1's space: the INVITE was its CSeq 1");
    assert_eq!(pracks[1].2, "2 PRACK", "fork 2's space, not fork 1's leavings");

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The OBSERVED-FORK rung — the deliverable that closes the collapse hole: the
/// caller's two provisional expects name two observed forks `r1` and `r2`,
/// each of which must be a dialog of its own, and the wire carries only ONE.
///
/// The callee rings one fork twice (180 then 183 under `f1`), so toward the
/// caller both provisionals ride the single To-tag the system minted — at the
/// caller, exactly the wire a B2BUA that collapses two early dialogs into one
/// produces (the demo B2BUA itself is fork-transparent: the minted-fork rung
/// above shows each b-leg fork reaching the caller under its own tag). The
/// second expect MUST be refused — the tag already rides `r1` — so a document
/// claiming two dialogs over one tag goes red instead of silently passing.
#[tokio::test(start_paused = true)]
async fn a_caller_side_collapse_of_two_observed_forks_fails_by_name() {
    let scene = api_scene("pivot-forked-ring-collapsed").await;
    let (outcome, dir) = replay(&scene, "forked-ring-collapsed.v3.json").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let unmatched = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnmatchedDatagram { .. }))
        .unwrap_or_else(|| {
            panic!("no unmatched-datagram failure: {:#?}", outcome.verdict.failures)
        });
    let Failure::UnmatchedDatagram { step, leg, reason, arrived, .. } = unmatched else {
        unreachable!("matched above")
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s7", "A"));
    assert!(matches!(arrived, Arrived::Response { status: 183, .. }), "{arrived}");
    // The refusal names the fork, the tag, and the id that already rides it —
    // the exact account the triage taxonomy quotes.
    assert!(
        reason.starts_with(
            "gated on early dialog \"r2\", which must be a dialog of its own; To-tag \""
        ),
        "{reason}"
    );
    assert!(reason.contains("\" already rides \"r1\""), "{reason}");

    // A red run still ends its call and keeps its evidence.
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    scene.b2bua.assert_fully_reaped();
    for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
        assert!(dir.join(file).is_file(), "{file} missing from a failed run's bundle");
    }

    scene.finish().await;
}

/// Rung three, the IN-DIALOG rung: a draft whose callee re-INVITEs an
/// established dialog, whose caller's BYE crosses that re-INVITE, and whose
/// 481 and teardown BYE reach the callee in either order.
///
/// It is the re-INVITE drive end to end — an in-dialog request composed on the
/// dialog the leg already owns, its 100 Trying, its relayed copy on the far leg,
/// an in-dialog NEGATIVE final and the hop ACK RFC 3261 §17.1.1.3 owes it — over
/// a real B2BUA, with the crossing teardown taken order-free.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_re_invite_draft_crosses_a_bye_and_takes_the_481_order_free() {
    let Some(case) = case_dir("bc-rc-reinvite-then-bye") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-reinvite-then-bye").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=20).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // The callee's re-INVITE and the caller's crossing BYE really crossed: the
    // b-leg carries an outbound in-dialog INVITE on the dialog it already owns,
    // and the ACK the 481 owes rides that same transaction (§17.1.1.3).
    let b_leg = &outcome.recording.legs()["B"];
    let re_invite = b_leg
        .iter()
        .find(|m| m.step.as_deref() == Some("s10"))
        .expect("the callee's re-INVITE is recorded");
    assert!(text(re_invite).starts_with("INVITE sip:"), "{}", text(re_invite));
    assert!(text(re_invite).contains("CSeq: 1 INVITE"), "{}", text(re_invite));
    let hop_ack = b_leg
        .iter()
        .find(|m| m.step.as_deref() == Some("s19"))
        .expect("the ACK to the 481 is recorded");
    assert!(text(hop_ack).contains("CSeq: 1 ACK"), "{}", text(hop_ack));
    let branch = |raw: &str| {
        raw.split("branch=").nth(1).and_then(|r| r.split(['\r', ';']).next()).map(str::to_string)
    };
    assert_eq!(
        branch(&text(hop_ack)),
        branch(&text(re_invite)),
        "the ACK to a non-2xx rides its own INVITE's branch"
    );

    // The residue is the authoring platform's CDR vocabulary and nothing else.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 2, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the TRANSFER rung, and a **SUT CAPABILITY FINDING**: the
/// INFO-intake transfer draft does not replay on the upstream demo B2BUA, and
/// the run says exactly where it stops.
///
/// The draft's transferor asks for the transfer with an in-dialog INFO
/// (`ct-04a` s8). This system has no INFO intake: `relay-info`
/// (`b2bua/src/rules/defaults/core_rules.rs`) forwards an in-dialog INFO to the
/// peer leg like any other transparent method, so the caller's leg receives it
/// and no transferee is ever dialled. The demo's only transfer intake is REFER
/// (`b2bua/src/rules/refer_transfer/`), and its failure treatment is the
/// release reroute, not a transfer-failure reroute.
///
/// The rung pins that outcome by NAME rather than skipping the case: the
/// document is not wrong, the lane cannot play it, and `case.lanes` records
/// `blocked:no-info-intake-transfer` for `upstream-demo`. Everything up to s8
/// DOES run — the dialog establishes and the in-dialog INFO reaches the wire
/// with its frozen body — so the bundle is real evidence of where the gap is.
#[tokio::test(start_paused = true)]
async fn rung_three_an_info_intake_transfer_finds_no_intake_on_this_lane() {
    let Some(case) = case_dir("ct-04a-crossing-caller-fail-reroute") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    // The frozen body's own content type, read from the document rather than
    // spelled here: a deployment's private vocabulary lives in its case library.
    let content_type = case
        .document
        .flow
        .iter()
        .find_map(|node| match node {
            pivot_schema::flow::FlowNode::Message(step) if step.id == "s8" => {
                match &step.msg.body {
                    Some(pivot_schema::body::Body::Resource(resource)) => {
                        resource.content_type.clone()
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("the transfer request states its frozen body's content type");
    let scene = B2buaScene::new("pivot-ct-04a").await;
    let transferees = scene.h.agent("transferees", "127.0.0.1:5071").await;
    let extra = BTreeMap::from([
        ("uas2".to_string(), transferees.clone()),
        ("uas3".to_string(), transferees),
    ]);
    let (outcome, dir) = replay_case(&scene, case, extra).await;

    // The establishment and the transfer request itself ran.
    for step in (1..=8).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    let info = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s8"))
            .expect("the transferor's INFO reached the wire"),
    );
    assert!(info.starts_with("INFO sip:"), "{info}");
    assert!(info.contains(&format!("Content-Type: {content_type}")), "{info}");

    // And then the lane's own answer: the INFO comes BACK on the caller's leg,
    // which is a relay, not an intake.
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let relayed = outcome
        .verdict
        .failures
        .iter()
        .find(|f| matches!(f, Failure::UnexpectedDatagram { .. }))
        .unwrap_or_else(|| panic!("no unexpected datagram: {:#?}", outcome.verdict.failures));
    let Failure::UnexpectedDatagram { leg, arrived, .. } = relayed else {
        unreachable!("matched above")
    };
    assert_eq!(leg, "A", "the INFO was relayed to the caller");
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "INFO"), "{arrived}");
    // The relay stopped nothing: the run went on recording what arrived until
    // the 200 to the INFO that never comes ran out its budget — which is what
    // ends the script (§11.2), so the finding is the WHOLE gap and not its first
    // packet.
    assert!(
        outcome
            .verdict
            .failures
            .iter()
            .any(|f| matches!(f, Failure::ExpectTimedOut { step, .. } if step == "s9")),
        "{:#?}",
        outcome.verdict.failures
    );
    // No transferee was ever dialled: legs C and D heard nothing at all.
    let legs = outcome.recording.legs();
    assert!(legs["C"].is_empty() && legs["D"].is_empty(), "{legs:#?}");
    assert!(dir.join("verdict.json").is_file(), "the finding keeps its bundle");

    // A lane that cannot play the case still owes it a teardown: the close ended
    // the established dialog the script walked away from.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert!(
        abandoned.closed.iter().any(|act| act.leg == "A" && act.owed == CloseOwed::Bye),
        "{:#?}",
        abandoned.closed
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// Rung three, the ACCESSOR rung: a header composed out of the lane's own
/// number allocation reaches the wire, and a platform that was told to process
/// this call's transfers itself refuses a Refer-To no reader accepts.
///
/// The draft's transferor REFERs with a deliberately unclosed name-addr
/// (`malformed-header` deviation d1) whose number is `${num:transferee:e164}` —
/// a registry identity NO attempt dials, so it exists only because §4.3 binds
/// every declared identity. The draft asserts 400, which is an answer only a
/// platform that terminates the REFER can give: the rung ARMS local REFER
/// processing on this lane (§4.3 — the document states the flow, the runner
/// states the scene), because a platform that relays REFER on would hand the
/// header to the caller instead of answering it.
#[tokio::test(start_paused = true)]
async fn rung_three_an_accessor_composed_refer_to_is_refused_by_name() {
    let Some(case) = case_dir("ct-01b-refer-malformed-referto") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = api_scene("pivot-ct-01b").await;
    let transferee = scene.h.agent("transferee", "127.0.0.1:5071").await;
    let extra = BTreeMap::from([("uas2".to_string(), transferee)]);
    let directives = LaneDirectives { local_refer: true, ..Default::default() };
    let (outcome, dir) = replay_directed(&scene, case, extra, directives).await;

    // The composed header is on the wire, verbatim as the deviation states it:
    // the lane's number for an identity no call dials, inside an unclosed
    // name-addr no reader accepts.
    let refer = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s8"))
            .expect("the REFER reached the wire"),
    );
    let refer_to = refer
        .split("Refer-To: ")
        .nth(1)
        .and_then(|rest| rest.split("\r\n").next())
        .expect("the REFER carries a Refer-To");
    assert!(
        refer_to.starts_with("<sip:transferee@"),
        "the lane's number for an identity no call dials: {refer_to}"
    );
    assert!(!refer_to.ends_with('>'), "the name-addr stays unclosed: {refer_to}");

    // And the answer the draft states is the answer on the wire: 400, on the
    // transferor's own leg, at the step that asserts it.
    assert_bundle_is_complete(&outcome, &dir);
    let refusal = text(
        outcome.recording.legs()["B"]
            .iter()
            .find(|m| m.step.as_deref() == Some("s9"))
            .expect("the REFER was answered"),
    );
    assert!(refusal.starts_with("SIP/2.0 400"), "{refusal}");

    // No transfer started: the transferee was never dialled and the transferor
    // heard no NOTIFY — the document's two `exactly: 0` counters, and legs C/D
    // stayed silent.
    let legs = outcome.recording.legs();
    for leg in ["C", "D"] {
        assert!(legs.get(leg).is_none_or(|messages| messages.is_empty()), "{legs:#?}");
    }
    assert!(
        !legs["B"].iter().any(|m| text(m).starts_with("NOTIFY")),
        "a refused REFER opens no subscription: {:#?}",
        legs["B"]
    );

    scene.finish().await;
}

/// The scene a REROUTE document replays against: a system whose decision backend
/// walks the ordered `X-Api-Call` route plan the lane injected (ADR-0017), so a
/// b-leg that fails — or that never answers inside its own ring timer — advances
/// the call to the chain's next route instead of ending it.
async fn hunting_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| B2buaSut::route_api_call("127.0.0.1", bob_port)).await
}

/// Rung three, the REROUTE rung: the first destination answers 486 and the
/// system hunts on to the alternate, which answers.
///
/// Two attempts on ONE branch and one socket, told apart by the Request-URI
/// user part each is dialled under — the relayed one for the attempt the caller
/// dialled, the registry's own number for the reroute the system composed — so
/// each INVITE reaches the leg the document names for it.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_reroute_draft_hunts_past_a_busy_destination() {
    let Some(case) = case_dir("bc-02-bl-reroutes-on-486") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = hunting_scene("pivot-bc-02-486").await;
    let bob = scene.bob.clone();
    let (outcome, dir) =
        replay_case(&scene, case, BTreeMap::from([("uas2".to_string(), bob)])).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=18).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The first attempt really was left behind and the second really was a
    // different destination: two b-leg INVITEs, each under its own callee's
    // Request-URI, and the hunt's own negative final in between.
    let legs = outcome.recording.legs();
    let dialled = |leg: &str| {
        legs[leg]
            .iter()
            .map(text)
            .find(|m| m.starts_with("INVITE sip:"))
            .and_then(|m| m.split_whitespace().nth(1).map(str::to_string))
            .unwrap_or_default()
    };
    assert_eq!(dialled("B"), "sip:+1999999bob5070@127.0.0.1:5070", "the dialled callee");
    assert_eq!(dialled("C"), "sip:+33000900005@127.0.0.1:5070", "the rerouted callee");

    // The residue is the authoring platform's CDR vocabulary and nothing else:
    // two event words it spells in PascalCase and one `b_legs.count` field this
    // lane does not publish at all.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 3, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the NO-ANSWER rung: the first destination rings and never
/// answers, the ring timer the lane armed from the document's own
/// `no_answer_ms` fires, the system CANCELs that attempt and hunts on.
///
/// The dwell is the DOCUMENT's (§4.1): the driver lowers `no_answer_ms` to the
/// route's whole-second `no_answer_timeout_sec` and nothing else picks it. On a
/// paused runtime the callee simply stays silent, so the only thing that can
/// end the ring is the system's own timer.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_three_a_no_answer_draft_rings_out_and_hunts_on() {
    let Some(case) = case_dir("bc-02-bl-reroutes-on-na") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = hunting_scene("pivot-bc-02-na").await;
    let bob = scene.bob.clone();
    let (outcome, dir) =
        replay_case(&scene, case, BTreeMap::from([("uas2".to_string(), bob)])).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=20).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The ring was the DOCUMENT's: the abandoned attempt's INVITE and the
    // CANCEL that ended it sit `no_answer_ms` apart on the b-leg's own ladder,
    // and nothing else on the wire could have ended it — the callee never
    // answered.
    let b_leg = &outcome.recording.legs()["B"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        b_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let invited = at(&|raw: &str| raw.starts_with("INVITE sip:+1999999bob5070"));
    let cancelled = at(&|raw: &str| raw.starts_with("CANCEL "));
    assert_eq!(
        cancelled - invited,
        1_000_000,
        "the system rang for exactly the `no_answer_ms` the document states"
    );

    // And the abandoned leg was closed the way RFC 3261 §17.1.1.3 owes it: the
    // 487 the CANCEL drew, ACKed on the INVITE's own branch.
    let ack =
        b_leg.iter().find(|m| text(m).starts_with("ACK sip:")).expect("the 487 draws its hop ACK");
    let branch = |raw: &str| {
        raw.split("branch=").nth(1).and_then(|r| r.split(['\r', ';']).next()).map(str::to_string)
    };
    let first_invite = b_leg
        .iter()
        .find(|m| text(m).starts_with("INVITE sip:+1999999bob5070"))
        .expect("the abandoned INVITE is recorded");
    assert_eq!(
        branch(&text(ack)),
        branch(&text(first_invite)),
        "the ACK to a non-2xx rides its own INVITE's branch"
    );

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 3, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );
    scene.finish().await;
}

/// Rung three, the SUB-SECOND ring: a `no_answer_ms` this lane's whole-second
/// route decision cannot arm exactly, replayed under a stated timing tolerance
/// (§9.2).
///
/// A captured platform measures its ring where its timer actually fired —
/// 15 139 ms is the corpus shape, not a round 15 000 — and no lane arms every
/// grain. The driver arms the NEAREST value it can, the run states the window it
/// will accept, and the difference is REPORTED rather than rounded away: the
/// verdict's timing note carries declared, observed and the window that covered
/// the gap, so a reader sees what the run absorbed.
///
/// The document is the no-answer draft with ONE number changed, built here
/// rather than authored beside it: a second twenty-step copy of one flow to vary
/// one field is a document nobody would maintain. Its own `case.id` keeps its
/// bundle its own.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn a_ring_this_lane_cannot_arm_exactly_rides_the_run_s_stated_tolerance() {
    /// What the capture measured, and what no whole-second lane arms.
    const RING_MS: u64 = 15_139;
    /// What this lane arms instead: the nearest second.
    const ARMED_MS: u64 = 15_000;

    let Some(mut case) = case_dir("bc-02-bl-reroutes-on-na") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    case.document.case.id = "bc-02-bl-reroutes-on-na-subsecond".to_string();
    for call in &mut case.document.calls {
        for attempt in &mut call.attempts {
            if attempt.no_answer_ms.is_some() {
                attempt.no_answer_ms = Some(RING_MS);
                if let Some(final_) = &mut attempt.r#final {
                    final_.at_ms = RING_MS + 80;
                }
            }
        }
    }
    for node in &mut case.document.flow {
        if let pivot_schema::flow::FlowNode::Message(step) = node {
            if step.delay.timer_linked {
                step.delay.ms = RING_MS;
            }
        }
    }

    let scene = hunting_scene("pivot-bc-02-subsecond").await;
    let bob = scene.bob.clone();
    let directives = LaneDirectives { timing_tolerance_ms: 200, ..Default::default() };
    let (outcome, dir) =
        replay_directed(&scene, case, BTreeMap::from([("uas2".to_string(), bob)]), directives)
            .await;
    assert_bundle_is_complete(&outcome, &dir);

    // The reading, in the verdict: what the document declares, what the system's
    // timer actually measured, and the window that covered the difference.
    assert_eq!(outcome.verdict.timings.len(), 1, "{:#?}", outcome.verdict.timings);
    let note = &outcome.verdict.timings[0];
    assert_eq!(note.step, "s6");
    assert_eq!(note.leg, "B");
    assert_eq!(note.declared_ms, RING_MS);
    assert_eq!(note.observed_ms, ARMED_MS);
    assert_eq!(note.delta_ms, -139, "the system rang 139 ms short of the document");
    assert_eq!(note.tolerance_ms, 200);

    // And the wire agrees: the ring really was the armed 15 s, not the declared
    // 15.139 s — the tolerance absorbs a difference, it does not invent one.
    let b_leg = &outcome.recording.legs()["B"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        b_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let invited = at(&|raw: &str| raw.starts_with("INVITE sip:+1999999bob5070"));
    let cancelled = at(&|raw: &str| raw.starts_with("CANCEL "));
    assert_eq!(cancelled - invited, ARMED_MS * 1000);

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    scene.finish().await;
}

/// The lane's own arming rule (§9.2), on the grain it actually has: a whole
/// second. What it cannot arm exactly it arms nearest, and what the window does
/// not cover it REFUSES rather than rounding.
#[test]
fn a_ring_between_two_seconds_arms_at_the_nearest_one_inside_the_stated_window() {
    assert_eq!(no_answer_sec("called-0-0", 15_000, 0), 15, "an armable ring needs no window");
    assert_eq!(no_answer_sec("called-0-0", 15_139, 200), 15, "139 ms below the nearest second");
    assert_eq!(no_answer_sec("called-0-0", 14_800, 200), 15, "200 ms above it, at the edge");
}

#[test]
#[should_panic(expected = "this lane arms whole seconds")]
fn a_ring_the_window_does_not_cover_is_refused_rather_than_rounded() {
    no_answer_sec("called-0-0", 15_139, 100);
}

/// The scene a CAPTURED document replays against: the system routes every call
/// to the callee socket and relays the Request-URI userpart the document dialled
/// onto the b-leg, which is what a captured `ruri-pos` claim matches.
async fn capture_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await
}

/// One auto-generated corpus case, replayed on its own scene.
///
/// Each case gets its OWN test — and therefore its own paused runtime and its
/// own simulated network. Several scenes inside one test share one clock, and
/// the previous scene's still-armed timers keep its auto-advance alive, so a
/// later scene's run grinds instead of converging.
///
/// The cases are picked for the property that makes a captured document
/// replayable on a FOREIGN system: `lanes["upstream-fake"] == "ok"`, one call, one attempt,
/// an answered linear flow, and every expect `check: "record"`, so nothing
/// asserts a header only the capture's own platform ever emitted.
async fn replay_corpus(case_id: &str, scene_name: &str) {
    let Some(case) = corpus_case(case_id) else {
        eprintln!("SKIPPED {case_id}: set PIVOT_CORPUS_DIRS to the regenerated v3 corpus");
        return;
    };
    let scene = capture_scene(scene_name).await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert!(dir.join("verdict.json").is_file(), "{case_id} left its bundle");
    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::Ok,
        "{case_id}\nfailures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert_eq!(outcome.verdict.completed_steps.len(), 14, "{case_id}: every step ran");
    assert!(outcome.timing.settled_at_ms.is_some(), "{case_id} settled");
    let legs = outcome.recording.legs();
    assert!(!legs["A"].is_empty() && !legs["B"].is_empty(), "{case_id}: {legs:#?}");
    scene.finish().await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_24590ae6() {
    replay_corpus(
        "capture_24590ae6-349b-4421-bed6-137a8121a9da.pcap.gz-case1",
        "pivot-corpus-24590ae6",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_d9ef02e4() {
    replay_corpus(
        "capture_d9ef02e4-785c-4906-a0ba-c15162e9392c.pcap.gz-case1",
        "pivot-corpus-d9ef02e4",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_54739770() {
    replay_corpus(
        "capture_54739770-1c3a-4661-bf88-52b1d3fb7fdf.pcap.gz-case1",
        "pivot-corpus-54739770",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_d7430352() {
    replay_corpus(
        "capture_d7430352-367e-4975-ba27-9e5ffc410730.pcap.gz-case1",
        "pivot-corpus-d7430352",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn rung_four_corpus_f4e5e2b8() {
    replay_corpus(
        "capture_f4e5e2b8-ca55-45f3-9ac3-94610c0b8af4.pcap.gz-case1",
        "pivot-corpus-f4e5e2b8",
    )
    .await;
}

/// Rung five, the EMISSION-PATH rung: the three deviation kinds §11 defines for
/// what an emission LOOKS like, in one call.
///
/// - `verbatim-emission` on the opening INVITE: the stored block rides as the
///   document holds it — two `Accept` rows in order, `user-agent` and
///   `supported` in the lower case the document spells them, and a trailing
///   header that stays trailing. The tier-1 lines around it are still the
///   stack's, because the document stores none of them (§8).
/// - `raw-order` on the re-INVITE: the same guarantee over the stored block,
///   asked for on its own, with a duplicate header opening and closing it.
/// - `cseq-override` on the BYE, in its relative form: the re-INVITE's CSeq
///   plus three. The leg's numbering CONTINUES from what went out (RFC 3261
///   §12.2.1.1), which is why the override sets it rather than decorating one
///   message.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_five_a_preserved_block_and_a_jumped_cseq_reach_the_wire() {
    let Some(case) = case_dir("bc-rc-cseq-jump-raw-order") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let scene = B2buaScene::new("pivot-cseq-jump-raw-order").await;
    // The `cseq-override` IS the non-compliance the case reproduces (RFC 3261
    // §12.2.1.1), and it is the scripted CALLER that emits it: the waiver is
    // scoped to that party so the same rule stays gated on the system's own
    // output.
    scene.h.waive(
        scenario_harness::WaiverScope::rule(
            "cseq-in-dialog-order",
            "the document's cseq-override deviation: the scripted caller jumps its dialog CSeq \
             by three, which is what this case exists to replay",
        )
        .on_party("alice"),
    );
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=20).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    let a_leg = &outcome.recording.legs()["A"];
    let sent = |step: &str| {
        text(
            a_leg
                .iter()
                .find(|m| m.step.as_deref() == Some(step))
                .unwrap_or_else(|| panic!("{step} is recorded")),
        )
    };

    // The stored block, in the document's own order and casing.
    let invite = sent("s1");
    let block = [
        "\r\nuser-agent: PivotReplay/1.0\r\n",
        "\r\nP-Asserted-Identity: <sip:0009001@pivot.invalid>\r\n",
        "\r\nAccept: application/sdp\r\n",
        "\r\nAccept: application/vnd.example.indata\r\n",
        "\r\nsupported: timer, replaces\r\n",
        "\r\nX-Pivot-Order: stored-last\r\n",
    ];
    let mut at = 0usize;
    for line in block {
        let found = invite[at..]
            .find(line)
            .unwrap_or_else(|| panic!("{line:?} out of order or absent in:\n{invite}"));
        // Stop on the CRLF this line ENDS with: it is the one the next line
        // begins with.
        at += found + line.len() - 2;
    }
    assert!(!invite.contains("User-Agent: PivotReplay"), "the document's casing survived");
    assert!(invite.contains("\r\nVia: SIP/2.0/UDP "), "tier-1 is still the stack's");

    // `raw-order` over a duplicate header: both rows ride, in their positions.
    let re_invite = sent("s10");
    assert!(re_invite.contains("CSeq: 2 INVITE"), "{re_invite}");
    let first = re_invite.find("X-Pivot-Order: first").expect("the opening row");
    let expires = re_invite.find("session-expires: 1800").expect("the stated spelling");
    let last = re_invite.find("X-Pivot-Order: last").expect("the closing row");
    assert!(first < expires && expires < last, "{re_invite}");

    // The override is the number that went out, and the leg keeps it.
    let bye = sent("s17");
    assert!(bye.starts_with("BYE sip:"), "{bye}");
    assert!(bye.contains("CSeq: 5 BYE"), "the BYE is numbered 2 + 3:\n{bye}");

    // The draft was authored FOR this lane, so its CDR vocabulary is this
    // system's own and nothing downgrades.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.informative.is_empty(), "{:#?}", outcome.verdict.informative);
    scene.finish().await;
}

/// Rung five, the MULTIPART rung: an initial INVITE offering SDP beside a
/// binary deployment payload, in the shape the corpus carries.
///
/// The parts are decomposed on disk and the FRAMING is regenerated — §8.3
/// stores the container type with its boundary stripped — so what the rung
/// proves is BINARY IDENTITY: the body the callee takes off the wire equals the
/// captured one byte for byte once the regenerated boundary and the rewritten
/// SDP address and port are substituted back, and nothing else is allowed to
/// differ.
/// It asserts on the datagram the CALLEE received, not on the one the caller
/// composed, so the whole path is covered.
#[tokio::test(start_paused = true)]
async fn rung_five_a_multipart_invite_reaches_the_callee_with_both_parts_byte_exact() {
    let Some(case) = case_dir("bc-rc-multipart-indata-invite") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let parts_dir = case.base_dir.join("resources");
    let offer = std::fs::read(parts_dir.join("uac1_0_0.sdp")).expect("the offer part");
    let payload = std::fs::read(parts_dir.join("uac1_0_1.bin")).expect("the binary part");
    let scene = B2buaScene::new("pivot-multipart-indata").await;
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=13).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // The bytes the CALLEE took off the wire.
    let arrived = scene
        .bob
        .wire_view()
        .into_iter()
        .find(|entry| entry.raw.starts_with(b"INVITE "))
        .expect("the callee received the INVITE");
    let raw = arrived.raw;
    let head = String::from_utf8_lossy(&raw[..raw.len().min(1200)]).into_owned();
    let boundary = head
        .split("boundary=")
        .nth(1)
        .and_then(|rest| rest.split(['\r', ';']).next())
        .expect("the relayed Content-Type names its boundary")
        .to_string();
    assert!(head.contains("multipart/mixed"), "{head}");
    assert!(
        find_bytes(&raw, format!("--{boundary}--\r\n").as_bytes()).is_some(),
        "the closing delimiter is present"
    );

    // Each part's payload, byte for byte: the offer with the lane's media
    // address and booked port written in — and nothing else about it touched —
    // and the binary part exactly as the capture held it.
    let offered = String::from_utf8_lossy(&offer)
        .replace("c=IN IP4 198.51.100.7", "c=IN IP4 127.0.0.1")
        .replace("m=audio 30000 ", &format!("m=audio {LANE_RTP_BASE} "));
    assert!(
        find_bytes(&raw, offered.as_bytes()).is_some(),
        "the SDP part rides byte-exact under the lane's media address and booked port:\n{}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        find_bytes(&raw, b"c=IN IP4 198.51.100.7").is_none(),
        "the connection address is the lane's"
    );
    assert!(
        find_bytes(&raw, b"m=audio 30000 ").is_none(),
        "and the media port is the one the lane BOOKED, not the one the capture stored"
    );
    assert!(
        find_bytes(&raw, b"o=alice 1 1 IN IP4 198.51.100.7").is_some(),
        "and only the connection line is rewritten"
    );
    assert!(
        find_bytes(&raw, &payload).is_some(),
        "the binary part rides byte-exact: {payload:02x?}"
    );

    // The container's own type, regenerated boundary and NOTHING else: the
    // document stores `multipart/mixed`, so that is what frames the body.
    assert!(
        head.contains(&format!("Content-Type: multipart/mixed;boundary={boundary}\r\n")),
        "the container type is the stored one plus its regenerated boundary:\n{head}"
    );

    // THE BINARY-IDENTITY PROOF (§8.3). The body that arrived is the captured
    // body byte for byte, save exactly the THREE things replay regenerates: the
    // container boundary, the SDP part's connection address, and the SDP part's
    // media port. All three are substituted back below — each one substitution,
    // named — and the equality then proves nothing ELSE moved: not a MIME
    // parameter, not an entity header or its order, not a delimiter's CRLF, not
    // the closing marker. A rewrite token this interpreter starts honouring
    // joins the named list here or fails this assertion, which is the point.
    let geoloc = std::fs::read(parts_dir.join("uac1_0_2.xml")).expect("the location part");
    let mut captured: Vec<u8> = Vec::new();
    captured.extend_from_slice(
        b"--unique-boundary-1\r\n\
          Content-Type: application/sdp\r\n\
          Content-ID: <offer@example.invalid>\r\n\
          Content-Disposition: session\r\n\r\n",
    );
    captured.extend_from_slice(&offer);
    captured.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
          Content-Type: application/vnd.example.indata\r\n\
          Content-ID: <indata@example.invalid>\r\n\
          Content-Transfer-Encoding: binary\r\n\
          Content-Disposition: signal;handling=optional\r\n\r\n",
    );
    captured.extend_from_slice(&payload);
    captured.extend_from_slice(
        b"\r\n--unique-boundary-1\r\n\
          Content-Type: application/pidf+xml;charset=utf-8\r\n\
          Content-ID: <geoloc@example.invalid>\r\n\
          Content-Disposition: render;handling=optional\r\n\r\n",
    );
    captured.extend_from_slice(&geoloc);
    captured.extend_from_slice(b"\r\n--unique-boundary-1--\r\n");

    let head_end = find_bytes(&raw, b"\r\n\r\n").expect("the datagram's head ends") + 4;
    let arrived_body = &raw[head_end..];
    let regenerated_boundary =
        replace_bytes(arrived_body, boundary.as_bytes(), b"unique-boundary-1");
    let rewritten_addr =
        replace_bytes(&regenerated_boundary, b"c=IN IP4 127.0.0.1", b"c=IN IP4 198.51.100.7");
    let rewritten_sdp = replace_bytes(
        &rewritten_addr,
        format!("m=audio {LANE_RTP_BASE} ").as_bytes(),
        b"m=audio 30000 ",
    );
    assert_eq!(
        String::from_utf8_lossy(&rewritten_sdp),
        String::from_utf8_lossy(&captured),
        "the emitted body differs from the captured one beyond its boundary, its SDP address \
         and its SDP media port"
    );
    assert_eq!(rewritten_sdp, captured, "the difference is not a UTF-8 artefact");

    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    scene.finish().await;
}

/// `haystack` with every occurrence of `from` replaced by `to`, over bytes a
/// string type cannot hold.
fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut at = 0usize;
    while at < haystack.len() {
        match find_bytes(&haystack[at..], from) {
            Some(i) => {
                out.extend_from_slice(&haystack[at..at + i]);
                out.extend_from_slice(to);
                at += i + from.len();
            }
            None => {
                out.extend_from_slice(&haystack[at..]);
                break;
            }
        }
    }
    out
}

/// Where `needle` sits inside `haystack`, for a payload no string comparison
/// can hold (a binary part is not UTF-8).
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The address the lane's limiter server answers at, on its own simulated HTTP
/// fabric — a private network the SIP transport never sees.
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// A real `LimiterServer` over `net`, plus the store its counters live in, so a
/// rung can read what the system actually held rather than inferring it from
/// the wire. The handle keeps the server task alive for the run.
async fn serve_limiter(
    net: &SimulatedHttpNetwork,
) -> (Arc<WindowStore>, Box<dyn http_net::HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net
        .serve(LIMITER_ADDR.parse().expect("the limiter address parses"), server)
        .await
        .expect("the limiter binds");
    (store, handle)
}

/// The scene a LIMITED document replays against: the deployed-engine decision —
/// which honours the lane's `X-Api-Call` egress directive, its admission entry
/// included — over the system's own `HttpCallLimiter` pointed at `net`.
async fn limited_scene(name: &str, net: &SimulatedHttpNetwork) -> B2buaScene {
    let client: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        LIMITER_ADDR.parse().expect("the limiter address parses"),
        std::time::Duration::from_millis(150),
    ));
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_api_call("127.0.0.1", bob_port).limiter(client)
    })
    .await
}

/// The LIMITER rung: a second call arrives while the first holds the only
/// admission slot, and the platform's own admission control refuses it.
///
/// The shape the draft exists for is a refusal that never becomes a call: the
/// caller of the second call gets its final (ADR-0022 — once 100 Trying is out,
/// a final is owed), no b-leg is ever dialled, both calls are billed, and the
/// admitted call's slot comes back. The document says all four — the 486 as a
/// step, the absent b-leg as a `background` counter bounded `exactly: 0`
/// (friction K5), two CDRs as a postcondition — and the rung reads the limiter's
/// own store beside them, so a green run cannot mean "the callee happened to be
/// busy".
///
/// The cap is what refuses: raise it to 2 and the second call is admitted and
/// dialled — at ITS OWN callee, because each call carries its own egress
/// directive (§4.3) — and the `exactly: 0` counter on that callee is what fails
/// the run. The counter counts because the lane could have reached the party it
/// counts (§5.1); on a lane that could not, it would be a check that cannot
/// fail.
///
/// It skips loudly when no case library is configured.
#[tokio::test(start_paused = true)]
async fn rung_six_a_limited_second_call_is_refused_before_any_b_leg() {
    let Some(case) = case_dir("bc-01-cac-exceeded-two-calls") else {
        eprintln!("SKIPPED: set PIVOT_CASE_DIRS to an authored case library");
        return;
    };
    let net = SimulatedHttpNetwork::new();
    let (store, _limiter) = serve_limiter(&net).await;
    let scene = limited_scene("pivot-bc-01", &net).await;
    let extra = BTreeMap::from([
        ("uac2".to_string(), scene.h.agent("uac2", "127.0.0.1:5061").await),
        ("uas2".to_string(), scene.h.agent("uas2", &format!("127.0.0.1:{IDLE_PORT}")).await),
    ]);
    let admission = Admission { id: "trunk-A".to_string(), limit: 1 };
    let directives = LaneDirectives { admission: Some(admission), ..Default::default() };
    let (outcome, dir) = replay_directed(&scene, case, extra, directives).await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=17).map(|n| format!("s{n}")) {
        let step = &step;
        assert!(
            outcome.verdict.completed_steps.contains(step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }

    // EACH call carries its own egress directive, naming its own callee's socket
    // (§4.3). Without that, the absence counter below would watch an endpoint
    // this lane could never dial, and a check that cannot fail is not evidence.
    let run_config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("run-config.json")).unwrap())
            .expect("the bundle's run configuration parses");
    let directive = |call: &str| {
        run_config["call_headers"][call]["X-Api-Call"].as_str().unwrap_or_default().to_string()
    };
    assert!(
        directive("c1").contains(&format!("\"port\":{BOB_PORT}")),
        "the first call is dialled at its own callee: {}",
        directive("c1")
    );
    assert!(
        directive("c2").contains(&format!("\"port\":{IDLE_PORT}")),
        "the second call is dialled at ITS own callee, not the first one's: {}",
        directive("c2")
    );

    let legs = outcome.recording.legs();
    // The refused caller got its final, and it is the document's own step.
    let refusal = legs["C"]
        .iter()
        .find(|m| text(m).starts_with("SIP/2.0 486"))
        .expect("the second caller was answered");
    assert_eq!(refusal.step.as_deref(), Some("s12"));
    // And it was refused BEFORE any leg: the second call's callee heard nothing
    // at all, which is the whole difference between admission control and a
    // busy destination. The document says the same thing as a `background`
    // counter bounded `exactly: 0`, and a green run is that counter holding.
    assert!(legs.get("D").is_none_or(|messages| messages.is_empty()), "{legs:#?}");
    assert!(
        !outcome.verdict.failures.iter().any(|f| matches!(f, Failure::BackgroundCount { .. })),
        "{:#?}",
        outcome.verdict.failures
    );

    // The limiter's own counter agrees: one hold taken, the refusal never
    // incremented, and the teardown gave the slot back.
    assert_eq!(store.stats().current_total, 0, "the admitted call released its slot");

    // Both calls were BILLED, and the document says so with the only per-record
    // scoping `{count, checks}` has: the count. Which record carries what is
    // unaskable there (friction K8), so the rung reads it off this lane's own
    // CDR — the draft's two event checks are the authoring platform's spelling
    // and are recorded rather than gating (§9.1).
    let records = scene.b2bua.cdr_records();
    assert_eq!(records.len(), 2, "{records:#?}");
    let refused = records
        .iter()
        .find(|record| record.b_legs.is_empty())
        .expect("the refused call was billed with no b-leg");
    assert!(
        refused.events.iter().any(|event| event.status_code == Some(486)),
        "the refusal is on the record: {:#?}",
        refused.events
    );
    let answered = records
        .iter()
        .find(|record| !record.b_legs.is_empty())
        .expect("the admitted call was billed");
    assert!(
        answered.events.iter().any(|event| event.event_type == CdrEventType::Answer),
        "{:#?}",
        answered.events
    );

    // And the residue is that authoring vocabulary and nothing else.
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.informative.len(), 2, "{:#?}", outcome.verdict.informative);
    assert!(
        outcome.verdict.informative.iter().all(|note| note.class == CheckClass::CdrVocabulary
            && matches!(&note.finding, Failure::CdrMismatch { .. })),
        "{:#?}",
        outcome.verdict.informative
    );

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **Rung seven, the negative case** (§11.2): a document that declares the
/// failure its run MUST produce, and passes only by producing exactly that one.
///
/// The source's peer never ACKed the callee's 200 — the capture holds no such
/// datagram, so the document holds no step for it — and this platform answers a
/// dialog-creating 2xx LOCALLY (RFC 3261 §13.2.2.4). So the run puts an ACK on
/// leg B where nothing expects it, and the document says so in advance. Nothing
/// is softened for it: the gate refuses the datagram, the failure is raised with
/// its site, and the recording holds it verbatim.
///
/// That delta stops NOTHING (§11.2): an ACK arriving where a BYE is expected
/// leaves the BYE as reachable as it was, so the failure is recorded and the run
/// GOES ON. The scripted teardown runs to completion, the call ends the way the
/// document says it ends, and no generic close is needed. The VERDICT is what
/// inverts.
#[tokio::test(start_paused = true)]
async fn rung_seven_a_negative_case_passes_by_failing_exactly_as_it_declared() {
    let scene = api_scene("pivot-unacked-2xx").await;
    let (outcome, dir) = replay(&scene, "unacked-2xx-negative.v3.json").await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.failed_step, None);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");

    // The declaration, against what the run produced for it.
    assert_eq!(outcome.verdict.must_fail.len(), 1, "{:#?}", outcome.verdict.must_fail);
    let note = &outcome.verdict.must_fail[0];
    assert_eq!(note.failure, DeclaredFailure::UnexpectedAck);
    assert_eq!(note.step, "s6", "the anchor is the 2xx the divergence turns on");
    assert_eq!(note.derived_from, RfcRule::NoAckToDialogCreating2xx);
    let observed = note.observed.as_ref().expect("the run produced the declared failure");
    let (leg, arrived) = arrival_of(observed);
    assert_eq!(leg, "B", "the anchor's own leg");
    assert!(matches!(arrived, Arrived::Request { method, .. } if method == "ACK"), "{arrived}");

    // The delta did not prevent the next message from being composed, so the
    // script ran to its end: every step, the scripted teardown included, and
    // nothing abandoned.
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "{:?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    for step in ["s10", "s11", "s12"] {
        assert!(
            outcome.verdict.completed_steps.iter().any(|ran| ran == step),
            "the teardown behind the delta still ran: {:?}",
            outcome.verdict.completed_steps
        );
    }

    // The recording is a faithful account either way: the ACK is in leg B's
    // ladder, unclaimed by any step, exactly where it landed — and the BYE
    // BEHIND it is claimed by `s10`, which the ACK never disarmed.
    let legs = outcome.recording.legs();
    let ack = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("ACK "))
        .unwrap_or_else(|| panic!("no ACK on leg B: {:#?}", legs["B"]));
    assert_eq!(ack.step, None, "no step claimed it — that is what made it a failure");
    let bye = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("BYE "))
        .unwrap_or_else(|| panic!("no BYE on leg B: {:#?}", legs["B"]));
    assert_eq!(bye.step.as_deref(), Some("s10"), "the expect the ACK was refused against");

    assert!(dir.join("verdict.json").is_file());
    let written: pivot_interpreter::RunVerdict =
        serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written.status, VerdictStatus::OkNegative);
    assert_eq!(written.must_fail, outcome.verdict.must_fail);
    assert_eq!(written.abandoned, None);

    // The postcondition the document states was evaluated: one CDR, billed for
    // a call that ended. Presence is what a negative case asserts; what is IN
    // the record is deliberately not this case's subject.
    assert_eq!(scene.b2bua.cdr_records().len(), 1);
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The first red proof, and the reason negative cases are worth generating: the
/// SAME document with the declaration removed is a plain failing run. The
/// detection the negative case rides on is alive, and a case that could go green
/// by behaving well would prove nothing.
#[tokio::test(start_paused = true)]
async fn the_same_document_without_its_declaration_fails_on_the_very_same_ack() {
    let scene = api_scene("pivot-unacked-2xx-undeclared").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.must_fail.clear();
    // Its own id, so this variant's bundle sits beside the negative case's
    // rather than overwriting the very run it is the counter-proof to.
    case.document.case.id = "unacked-2xx-undeclared".into();
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.must_fail.is_empty(), "nothing was declared");
    let refused = outcome
        .verdict
        .failures
        .iter()
        .find(|f| {
            matches!(f, Failure::UnexpectedDatagram { arrived, .. }
                | Failure::UnmatchedDatagram { arrived, .. }
                if matches!(arrived, Arrived::Request { method, .. } if method == "ACK"))
        })
        .unwrap_or_else(|| panic!("no ACK failure: {:#?}", outcome.verdict.failures));
    assert_eq!(arrival_of(refused).0, "B");
    // Polarity changes nothing about the RUN, only about the verdict: the ACK
    // stopped nothing here either, so the script ran to its end, tore its own
    // call down and needed no generic close.
    assert!(outcome.verdict.abandoned.is_none());
    assert_eq!(outcome.verdict.completed_steps.len(), 12, "{:?}", outcome.verdict.completed_steps);

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The second red proof, the other direction: reality stops failing and the
/// negative case is what turns red. The ordinary linear document — whose flow
/// DOES hold the platform's ACK — carrying the same declaration runs green on
/// the wire and fails on the declaration nothing produced.
#[tokio::test(start_paused = true)]
async fn a_declaration_whose_divergence_never_happened_fails_the_run() {
    let scene = api_scene("pivot-unacked-2xx-repaired").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "unacked-2xx-repaired".into();
    case.document.must_fail.push(MustFail {
        failure: DeclaredFailure::UnexpectedAck,
        step: "s6".into(),
        derived_from: RfcRule::NoAckToDialogCreating2xx,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // Every step ran and the wire was clean — and the run still fails, because
    // a negative case that does not fail as declared has not passed.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:?}", outcome.verdict.completed_steps);
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s6"));
    assert!(outcome.verdict.must_fail[0].observed.is_none());
    let Some(Failure::DeclaredFailureNotProduced { declared, detail, .. }) =
        outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!(*declared, DeclaredFailure::UnexpectedAck);
    assert!(detail.contains("no unclaimed ACK"), "{detail}");
    // Nothing aborted, so nothing was abandoned: the script itself tore the
    // call down, and the close had nothing to close.
    assert!(outcome.verdict.abandoned.is_none());

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The inclusion semantic, on a real run** (§11.2): a declaration says what
/// the run MUST produce, not everything it may produce. A wire divergence BESIDE
/// the declared one — here a ladder our platform paces differently, the very
/// thing a withheld ACK causes — is carried, listed under `tolerated`, and the
/// negative case is still green.
#[tokio::test(start_paused = true)]
async fn a_wire_divergence_past_the_anchor_is_carried_by_the_negative_case() {
    let scene = api_scene("pivot-unacked-2xx-carried").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-carried".into();
    // `s12` is leg A's expect of the 200 to the BYE, past the `s6` anchor. A
    // non-INVITE final rides no ladder of its own (§17.2.2), so the document's
    // count IS the assertion there — one repeat declared, and the run sees none.
    declare_retransmits(&mut case, "s12", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}",
        outcome.verdict.failures
    );
    assert!(outcome.verdict.failures.is_empty(), "{:#?}", outcome.verdict.failures);
    assert!(outcome.verdict.must_fail[0].observed.is_some());
    // Carried, and never invisible: the reader of the verdict sees it.
    let Some(Failure::RetransmitCountMismatch { step, declared, observed, .. }) =
        outcome.verdict.tolerated.first()
    else {
        panic!("{:#?}", outcome.verdict.tolerated)
    };
    assert_eq!((step.as_str(), *declared, *observed), ("s12", 1, 0));

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The counter-proof: the SAME divergence BEFORE the anchor fails the run. Up to
/// the declared divergence the replay was still following the capture, so a
/// failure there is a defect of its own and no consequence of the declaration.
#[tokio::test(start_paused = true)]
async fn a_wire_divergence_before_the_anchor_still_fails_the_negative_case() {
    let scene = api_scene("pivot-unacked-2xx-early").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-early".into();
    // `s5` is leg A's expect of the 180, one step AHEAD of the anchor. An
    // unreliable provisional rides no ladder of its own, so the document's count
    // is the assertion there and the run sees none.
    declare_retransmits(&mut case, "s5", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.tolerated.is_empty(), "{:#?}", outcome.verdict.tolerated);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    // The declared failure still matched: only the early divergence turned it red.
    assert!(outcome.verdict.must_fail[0].observed.is_some());

    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The generic close, doing real work** (§11.2): a negative case whose script
/// ends BEFORE the teardown it scripts, so the scripted ends terminate the call
/// themselves.
///
/// Gate `s7` on a 183 and the 200 that really arrives is a CONTRADICTING FINAL:
/// it ends the very transaction `s7` waits on, so the 183 can never come and the
/// run cannot go on. The delta is one step past the `s6` anchor and two steps
/// ahead of the ACK and the BYE the flow holds, so the close is what
/// acknowledges the 2xx (RFC 3261 §13.2.2.4), what closes the dialog leg A
/// opened (§15), and what answers the teardown on leg B, which ANSWERED its
/// dialog and therefore never starts one.
///
/// The declared ACK still arrives — the platform relays the close's own ACK —
/// and it arrives where no expect is armed, so no failure names it. §11.2
/// matches a declaration against the RECORDING, which holds it: the case is
/// green as a negative one, and the verdict says which of the two ways it was
/// observed.
#[tokio::test(start_paused = true)]
async fn a_script_that_ends_before_its_teardown_is_closed_by_the_scripted_ends() {
    let scene = api_scene("pivot-unacked-2xx-closed").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-closed".into();
    expect_status(&mut case, "s7", 183);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(
        outcome.verdict.status,
        VerdictStatus::OkNegative,
        "failures: {:#?}\nrecording: {:#?}",
        outcome.verdict.failures,
        outcome.recording.legs()
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    // Observed by the recording, not by a gate: the script had ended.
    let note = &outcome.verdict.must_fail[0];
    assert!(note.observed.is_none(), "no expect was armed to refuse it: {:#?}", note.observed);
    assert!(note.recorded.as_deref().is_some_and(|a| a.starts_with("ACK ")), "{note:#?}");
    // The delta itself is the carried one: a wire divergence past the anchor.
    assert_eq!(outcome.verdict.tolerated.len(), 1, "{:#?}", outcome.verdict.tolerated);

    // Three acts, and each is the RFC's own answer to what its leg held.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("s7"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(
        acts,
        [("A", CloseOwed::Ack), ("A", CloseOwed::Bye), ("B", CloseOwed::Answer)],
        "{:#?}",
        abandoned.closed
    );
    assert!(abandoned.closed[1].sent.starts_with("BYE "), "{:?}", abandoned.closed[1].sent);

    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The same close on a RED negative run: a case that fails for a reason of its
/// own must not leak a call either.
///
/// Two divergences, one on each side of the anchor. The `s7` delta ends the
/// script and the close tears the call down exactly as above; the `s3` ladder is
/// BEFORE the `s6` anchor, so it is a defect of its own and the run is red. The
/// close runs regardless — a run's calls end whatever its verdict says.
#[tokio::test(start_paused = true)]
async fn a_red_negative_run_is_still_closed_and_leaves_no_call_up() {
    let scene = api_scene("pivot-unacked-2xx-closed-red").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-closed-red".into();
    expect_status(&mut case, "s7", 183);
    // `s5` is leg A's expect of the 180, one step AHEAD of the anchor.
    declare_retransmits(&mut case, "s5", 1);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.tolerated.is_empty(), "carrying is all or nothing");
    // The whole account, not a filtered half: the delta that ended the script
    // AND the divergence before the anchor that made the run red.
    assert_eq!(outcome.verdict.failures.len(), 2, "{:#?}", outcome.verdict.failures);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::RetransmitCountMismatch { step, .. } if step == "s5")),
        "{:#?}",
        outcome.verdict.failures
    );
    // The declared divergence still happened, and is still stated.
    assert!(outcome.verdict.must_fail[0].recorded.is_some(), "{:#?}", outcome.verdict.must_fail);
    // And the call is over: the close ran, the CDR was written, nothing is up.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.closed.len(), 3, "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "a red run still settles");
    assert_eq!(scene.b2bua.cdr_records().len(), 1);
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The close's OTHER caller shape, on the other blocking trigger: a MISSING
/// MESSAGE. An INVITE with a provisional and no final is cancelled (RFC 3261
/// §9.1), and the callee end answers the INVITE it took.
///
/// Gate `s5` on a 183 and the 180 that really arrives is a delta the run walks
/// past: a provisional ends no transaction, so the 183 could still come. What
/// ends the script is `s5`'s own budget running out — and with `s6` dwelling on
/// `s5`, the call is still ringing when it does. Both failures are recorded, the
/// run is red (the delta is BEFORE the `s6` anchor and the declared ACK is never
/// produced), and the close is what ends the ringing call rather than a timer.
#[tokio::test(start_paused = true)]
async fn a_ringing_call_the_script_walked_away_from_is_cancelled_by_its_caller() {
    let scene = api_scene("pivot-unacked-2xx-cancelled").await;
    let mut case = fixture("unacked-2xx-negative.v3.json");
    case.document.case.id = "unacked-2xx-cancelled".into();
    expect_status(&mut case, "s5", 183);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s5"));
    // BOTH failures, in the order they happened: the 180 the run walked past
    // BECAUSE a provisional ends no transaction, and then the budget that ran out
    // on the message that never came, which is what ended the script.
    let Some(Failure::UnmatchedDatagram { step, arrived, .. }) = outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!(step.as_str(), "s5");
    assert!(matches!(arrived, Arrived::Response { status: 180, .. }), "{arrived}");
    assert!(
        matches!(outcome.verdict.failures.get(1),
            Some(Failure::ExpectTimedOut { step, .. }) if step == "s5"),
        "{:#?}",
        outcome.verdict.failures
    );
    // Nothing answered the INVITE, so the platform sent no ACK to declare.
    assert!(outcome.verdict.must_fail[0].observed.is_none());
    assert!(outcome.verdict.must_fail[0].recorded.is_none());
    assert!(
        outcome
            .verdict
            .failures
            .iter()
            .any(|f| matches!(f, Failure::DeclaredFailureNotProduced { .. })),
        "{:#?}",
        outcome.verdict.failures
    );

    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert!(acts.contains(&("A", CloseOwed::Cancel)), "{:#?}", abandoned.closed);
    assert!(
        acts.iter().any(|(leg, owed)| *leg == "B" && *owed == CloseOwed::Answer),
        "the callee answers the INVITE it took: {:#?}",
        abandoned.closed
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A talk phase longer than the expect budget** (§6.8): an expect's budget
/// opens at the later of its own dwell (`anchor + delay.ms`) and its leg
/// reaching it — never across the dwell.
///
/// Leg B is at `s11` — the BYE it is to receive — the instant `s9` completes,
/// but that BYE is `s10`'s, dwelling six times the document's 5 s budget away.
/// Arming on the frontier alone would spend the whole budget waiting for a
/// message the flow has not yet been asked to provoke, so EVERY captured call
/// whose talk phase outlasts `expect_budget_ms` would fail on a faithful
/// document.
#[tokio::test(start_paused = true)]
async fn an_expect_gated_on_a_dwelling_send_does_not_spend_its_budget_waiting_for_it() {
    let scene = api_scene("pivot-long-talk-phase").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "long-talk-phase".into();
    let budget = case.document.timing.expect_budget_ms;
    step_of(&mut case, "s10").delay.ms = budget * 6;
    // A call held up this long is POLLED: the system keepalives both endpoints
    // while it is up, which every captured document of a long call carries as
    // background traffic (§5.1) rather than as flow.
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    // And the dwell really was slept: the teardown sits a whole talk phase after
    // the answer, which is the fact the budget must not have been counting over.
    let a_leg = &outcome.recording.legs()["A"];
    let at = |predicate: &dyn Fn(&str) -> bool| {
        a_leg.iter().find(|m| predicate(&text(m))).map(|m| m.at_us).expect("recorded")
    };
    let acked = at(&|raw: &str| raw.starts_with("ACK "));
    let byed = at(&|raw: &str| raw.starts_with("BYE "));
    assert!(byed - acked >= budget * 6 * 1000, "the BYE dwelled: {acked} → {byed}");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The same rule on the OTHER shape (§6.8): the gap is the expect's OWN, not a
/// cross-leg send's.
///
/// `s11` waits for the BYE and declares the whole talk phase as its own dwell,
/// which is what the generator writes for a message the system originates. A
/// budget opening at the anchor's completion rather than at `anchor + ms` would
/// still expire a whole talk phase before the BYE is due — the same defect
/// wearing a different anchor.
#[tokio::test(start_paused = true)]
async fn an_expect_declaring_its_own_gap_opens_its_budget_at_the_end_of_it() {
    let scene = api_scene("pivot-long-gap-same-leg").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "long-gap-same-leg".into();
    let budget = case.document.timing.expect_budget_ms;
    // The caller dwells the talk phase before its BYE; the callee's expect
    // states the SAME gap against its own leg's previous step.
    step_of(&mut case, "s10").delay.ms = budget * 6;
    let s11 = step_of(&mut case, "s11");
    s11.delay.from = "step:s9".parse().expect("s9 is a step id");
    s11.delay.ms = budget * 6;
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A relayed exchange of a policy-answered method** (§5.1): a `background`
/// policy answers traffic OUTSIDE the flow, so it must not absorb the arrival a
/// scripted expect is open on.
///
/// The caller polls its own dialog with an in-dialog OPTIONS and the system
/// carries it end to end — call behaviour a step owns — while the system's own
/// audits of the SAME method on the SAME leg are the policy's. Nothing on the
/// wire separates them: what does is the expect's window, which opens at its
/// anchor (`s101`, the caller's send) and is shut over every audit before it.
#[tokio::test(start_paused = true)]
async fn a_background_policy_does_not_absorb_the_arrival_an_open_expect_waits_for() {
    let scene = api_scene("pivot-relayed-options").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "relayed-options".into();
    let budget = case.document.timing.expect_budget_ms;
    // The poll sits three budgets into the talk phase, so the system's own
    // audits provably fire on both legs first.
    let mut send_options = step_of(&mut case, "s10").clone();
    send_options.id = "s101".into();
    send_options.msg.method = Some("OPTIONS".into());
    send_options.delay.ms = budget * 3;
    let mut expect_options = step_of(&mut case, "s11").clone();
    expect_options.id = "s102".into();
    expect_options.msg.method = Some("OPTIONS".into());
    expect_options.delay.from = "step:s101".parse().expect("s101 is a step id");
    let mut send_ok = step_of(&mut case, "s12").clone();
    send_ok.id = "s103".into();
    send_ok.msg.cseq_method = Some("OPTIONS".into());
    send_ok.delay.from = "step:s102".parse().expect("s102 is a step id");
    let mut expect_ok = step_of(&mut case, "s13").clone();
    expect_ok.id = "s104".into();
    expect_ok.msg.cseq_method = Some("OPTIONS".into());
    expect_ok.delay.from = "step:s103".parse().expect("s103 is a step id");
    // The teardown follows the poll it now sits behind.
    step_of(&mut case, "s10").delay.from = "step:s104".parse().expect("s104 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [send_options, expect_options, send_ok, expect_ok].into_iter().enumerate()
    {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    // The two readings of one method really did both happen on leg B: the
    // relayed poll attributed to its step, the audits answered by the policy.
    let b_leg = &outcome.recording.legs()["B"];
    let options =
        |predicate: &dyn Fn(&pivot_schema::bundle::recording::RecordedMessage) -> bool| {
            b_leg.iter().filter(|m| text(m).starts_with("OPTIONS ") && predicate(m)).count()
        };
    assert_eq!(options(&|m| m.step.as_deref() == Some("s102")), 1, "the relay is the step's");
    assert!(
        options(&|m| m.note.as_deref().is_some_and(|n| n.starts_with("background"))) >= 1,
        "the audits are the policy's: {b_leg:#?}"
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **The same two readings, arriving in ONE instant** (§5.1): the system's own
/// audit and the relay of the caller's poll both land on the callee leg while
/// the scripted expect is open, and only the relay is the step's.
///
/// The window the sibling rung leans on is not enough here — both arrivals are
/// inside it — and arrival order is not evidence: the audit is a deployment
/// cadence that can fall anywhere, and here it falls first. What separates them
/// is the header the caller's poll carries and the audit does not, which the
/// expect freezes. Taking the audit costs the run twice over: the step answers
/// the wrong request, and the relayed answer then reaches the far leg before the
/// step gated on that answer is armed to receive it.
///
/// The poll is dwelled onto the audit cadence on purpose — just short of the
/// eighth 2 s interval — so the collision is the test and not an accident of
/// pacing.
#[tokio::test(start_paused = true)]
async fn an_open_expect_holds_out_for_the_relay_when_an_audit_lands_in_the_same_instant() {
    const POLL_MARK: &str = "P-Charging-Vector";
    let scene = api_scene("pivot-options-collision").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "options-collision".into();
    let mark = Header { name: POLL_MARK.into(), value: "icid-value=\"poll\"".into(), class: None };
    let mut send_options = step_of(&mut case, "s10").clone();
    send_options.id = "s101".into();
    send_options.msg.method = Some("OPTIONS".into());
    send_options.msg.headers = vec![mark.clone()];
    // Eight audit intervals into the talk phase, less the slack that puts the
    // poll's own send in front of the audit it collides with and the relay of
    // that poll behind it — the relay trails its send by the two transit hops it
    // crosses, so the audit has to fall inside those.
    send_options.delay.ms = 2000 * 8 - 300;
    let mut expect_options = step_of(&mut case, "s11").clone();
    expect_options.id = "s102".into();
    expect_options.msg.method = Some("OPTIONS".into());
    expect_options.msg.headers = vec![mark];
    expect_options.delay.from = "step:s101".parse().expect("s101 is a step id");
    let mut send_ok = step_of(&mut case, "s12").clone();
    send_ok.id = "s103".into();
    send_ok.msg.cseq_method = Some("OPTIONS".into());
    send_ok.delay.from = "step:s102".parse().expect("s102 is a step id");
    let mut expect_ok = step_of(&mut case, "s13").clone();
    expect_ok.id = "s104".into();
    expect_ok.msg.cseq_method = Some("OPTIONS".into());
    expect_ok.delay.from = "step:s103".parse().expect("s103 is a step id");
    step_of(&mut case, "s10").delay.from = "step:s104".parse().expect("s104 is a step id");
    let at = case
        .document
        .flow
        .iter()
        .position(|node| matches!(node, pivot_schema::flow::FlowNode::Message(s) if s.id == "s10"))
        .expect("the teardown is a message node");
    for (offset, step) in [send_options, expect_options, send_ok, expect_ok].into_iter().enumerate()
    {
        case.document
            .flow
            .insert(at + offset, pivot_schema::flow::FlowNode::Message(Box::new(step)));
    }
    for actor in &mut case.document.actors {
        actor.background.push(BackgroundPolicy {
            r#match: BackgroundMatch { method: "OPTIONS".into() },
            respond: BackgroundResponse { status: 200 },
            count: Some(CountBound { at_least: Some(1), ..Default::default() }),
        });
    }
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    let b_leg = &outcome.recording.legs()["B"];
    let relay =
        b_leg.iter().find(|m| m.step.as_deref() == Some("s102")).expect("the relay is the step's");
    assert!(text(relay).contains(POLL_MARK), "the step took the poll, not an audit: {relay:#?}");
    // The collision is the test, not a bonus: an audit has to have landed on
    // this leg INSIDE the step's open window — after the poll that opened it,
    // before the relay it was competing with — and to have gone to the policy.
    let polled_at = outcome.recording.legs()["A"]
        .iter()
        .find(|m| m.step.as_deref() == Some("s101"))
        .expect("the caller polled")
        .at_us;
    let collided = b_leg.iter().any(|m| {
        text(m).starts_with("OPTIONS ")
            && !text(m).contains(POLL_MARK)
            && (polled_at..relay.at_us).contains(&m.at_us)
            && m.note.as_deref().is_some_and(|n| n.starts_with("background"))
    });
    assert!(
        collided,
        "an audit landed inside the open window, before the relay, and was the policy's: \
         polled {polled_at}, relayed {}, leg {b_leg:#?}",
        relay.at_us
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A dwell anchored on a BLOCK (§6.8). Branch scoping refuses a reference into
/// an `alt` from outside, so the alt's own id is the ONLY legal way to anchor on
/// the branch that ran — and a block a run cannot time is a block nothing may be
/// anchored on.
///
/// The tolerated absence is what makes this observable: §6.5 releases it when
/// its own budget expires, and that is the only rule that keeps the steps behind
/// it from waiting forever. A block that never settles never opens the budget,
/// so the release never comes and the run dies `flow-incomplete` on a document
/// that should pass.
#[tokio::test(start_paused = true)]
async fn a_dwell_anchored_on_an_alt_settles_when_the_block_completes() {
    let scene = api_scene("pivot-alt-anchored-dwell").await;
    let mut case = fixture("alt-answered.v3.json");
    case.document.case.id = "alt-anchored-dwell".into();
    // A 481 to the BYE the callee already answered 200: nothing will ever match
    // it, so only the budget can retire it.
    let mut absent = case
        .document
        .flow
        .iter()
        .flat_map(pivot_schema::flow::FlowNode::steps)
        .find(|step| step.id == "s11")
        .expect("the answered branch ends on the 200 to the BYE")
        .clone();
    absent.id = "s99".into();
    absent.msg.status = Some(481);
    absent.msg.reason = None;
    absent.optional = true;
    absent.within_ms = Some(250);
    absent.delay.from = "step:a1".parse().expect("a1 is the alt's own id");
    absent.delay.ms = 0;
    case.document.flow.push(pivot_schema::flow::FlowNode::Message(Box::new(absent)));
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert!(
        outcome.verdict.released_optional.iter().any(|step| step == "s99"),
        "the tolerated absence was released by its budget: {:#?}",
        outcome.verdict.released_optional
    );
    scene.finish().await;
}

/// **The close on a POSITIVE run** (§11.2): a document that declares nothing,
/// whose expected answer is a rejection instead.
///
/// The script says the callee answers and the caller takes a 200; the callee
/// rejects with a 486 and the platform relays it. That final ENDS the very
/// transaction `s7` waits on, so the 200 can never come and the run cannot go
/// on — which has nothing to do with the document's polarity. The script ends at
/// `s7`, the caller acknowledges the reject it took (RFC 3261 §17.1.1.3), the
/// system settles and the attempt is billed. The VERDICT still fails: this run
/// declared nothing, so nothing inverts, and the abandonment is stated beside
/// the failure rather than hiding it.
#[tokio::test(start_paused = true)]
async fn a_positive_run_whose_answer_is_a_reject_ends_its_script_and_closes() {
    let scene = api_scene("pivot-answer-rejected").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "answer-rejected".into();
    reject_at(&mut case, "s6", 486);
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(outcome.verdict.must_fail.is_empty(), "nothing was declared");
    assert_eq!(outcome.verdict.failed_step.as_deref(), Some("s7"));
    let Some(Failure::UnmatchedDatagram { step, leg, gated_on, arrived, .. }) =
        outcome.verdict.failures.first()
    else {
        panic!("{:#?}", outcome.verdict.failures)
    };
    assert_eq!((step.as_str(), leg.as_str()), ("s7", "A"));
    assert!(
        matches!(gated_on, GatedOn::Response { status: 200, cseq_method: Some(m) } if m == "INVITE"),
        "{gated_on}"
    );
    assert!(matches!(arrived, Arrived::Response { status: 486, .. }), "{arrived}");
    // The close ended the call; nothing is stacked on top of the real finding.
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);

    // The abandonment is STATED on a positive run exactly as on a negative one.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s7")));
    for ran in ["s1", "s2", "s3", "s4", "s5", "s6"] {
        assert!(
            outcome.verdict.completed_steps.iter().any(|step| step == ran),
            "{ran} ran before the reject: {:?}",
            outcome.verdict.completed_steps
        );
    }
    // The refused answer's expect is RETIRED — the 486 ended its transaction
    // (§17.1.3) and was charged on it — and everything the caller's tail
    // scripted behind it is abandoned: the ACK that confirms the answer never
    // came, nor the BYE that ends it.
    assert_eq!(outcome.verdict.retired, ["s7"]);
    for never in ["s8", "s10", "s11", "s12", "s13"] {
        assert!(
            abandoned.pending.iter().any(|step| step == never),
            "{never} is abandoned: {:?}",
            abandoned.pending
        );
        assert!(
            outcome.verdict.completed_steps.iter().all(|step| step != never),
            "no step is both run and abandoned: {:?}",
            outcome.verdict.completed_steps
        );
    }
    // A rejected INVITE holds no dialog open, and the ACK §17.1.1.3 owes went out
    // where the reject was REFUSED — so the close is left with nothing, and the
    // leg that answered 486 holds nothing at all.
    assert!(abandoned.closed.is_empty(), "{:#?}", abandoned.closed);
    let ack = outcome.recording.legs()["A"]
        .iter()
        .find(|m| m.dir == Dir::Out && text(m).starts_with("ACK "))
        .cloned()
        .expect("the reject the flow never scripted was acknowledged");
    assert!(ack.note.as_deref().is_some_and(is_unscripted_answer), "{:?}", ack.note);
    assert!(ack.step.is_none(), "the ACK satisfied no expect: {ack:#?}");

    // And the run still owes its call an ending and its attempt a record.
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the attempt was billed");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A finding is not an abort** (§9.1, §11.2): a gating inline check that does
/// not hold fails the VERDICT and the flow keeps walking — the matched message
/// is still one the next step composes over, so the run ends by its own
/// scripted teardown and the close has nothing to do.
#[tokio::test(start_paused = true)]
async fn a_failed_inline_check_fails_the_verdict_and_the_flow_walks_to_its_own_teardown() {
    let scene = api_scene("pivot-check-continues").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "check-continues".into();
    step_of(&mut case, "s7").checks.push(pivot_schema::check::Check {
        field: "header(X-Never-Present)".into(),
        op: pivot_schema::check::CheckOp::Exists,
        value: None,
        class: None,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::CheckFailed { site, field, .. }
                if site == "step \"s7\"" && field == "header(X-Never-Present)")),
        "{:#?}",
        outcome.verdict.failures
    );
    // Every step ran, s7 and the whole teardown behind it included.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.completed_steps);
    assert!(
        outcome.verdict.abandoned.is_none(),
        "nothing was abandoned: {:#?}",
        outcome.verdict.abandoned
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A header set that differs names itself** (issue 255): an asserted `expect`
/// whose datagram is one header short still FAILS the run, and no longer
/// abandons the call to say so.
///
/// Gating the whole match on the header set refused the 2xx outright: the answer
/// went unanswered, the system retransmitted it to its ceiling and tore both
/// legs down, and the run died of abandonment several steps past the cause with
/// nothing anywhere naming the header. Taking the datagram is what ANSWERS it,
/// so the dialog walks to its own teardown; the header is named with the value
/// the document froze and the value that arrived; and the verdict fails on that
/// finding rather than on the ladder behind it.
#[tokio::test(start_paused = true)]
async fn a_header_the_answer_omits_names_itself_and_the_call_reaches_its_teardown() {
    const OMITTED: &str = "Content-Disposition";
    let scene = api_scene("pivot-header-divergent").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "header-divergent".into();
    // The answer this system relays carries no such header, so the asserted
    // expect for it is exactly one header short.
    step_of(&mut case, "s7").msg.headers.push(Header {
        name: OMITTED.into(),
        value: "session;handling=required".into(),
        class: None,
    });
    let (outcome, dir) = replay_case(&scene, case, BTreeMap::new()).await;

    // The verdict fails, and the ONE failure is the header, both sides stated.
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(outcome.verdict.failures.len(), 1, "{:#?}", outcome.verdict.failures);
    assert!(
        matches!(&outcome.verdict.failures[0],
            Failure::CheckFailed { site, field, op, expected, observed }
                if site == "step \"s7\""
                    && field == "header(Content-Disposition)"
                    && op == "eq"
                    && expected == "session;handling=required"
                    && observed == "absent"),
        "{:#?}",
        outcome.verdict.failures
    );

    // The call went on: the step took the answer, so the ACK behind it and the
    // whole scripted teardown ran, and nothing was abandoned.
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "{:#?}", outcome.verdict.completed_steps);
    assert!(outcome.verdict.abandoned.is_none(), "{:#?}", outcome.verdict.abandoned);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    scene.b2bua.assert_fully_reaped();

    // The reception is ATTRIBUTED to the step, which is what lets the post-run
    // confrontation pair it with its captured message and raise a per-header
    // probe of its own — the reporting the abandonment used to swallow.
    let leg_a = &outcome.recording.legs()["A"];
    let answer = leg_a
        .iter()
        .find(|m| m.step.as_deref() == Some("s7"))
        .unwrap_or_else(|| panic!("s7 took the answer: {leg_a:#?}"));
    assert!(text(answer).starts_with("SIP/2.0 200 "), "{answer:#?}");
    assert!(!text(answer).contains(OMITTED), "the answer really omits it: {answer:#?}");
    // Answered means not retransmitted: no datagram on the leg repeats another.
    assert!(
        leg_a.iter().all(|m| m.repeat_of.is_none()),
        "the answered 2xx drew no ladder: {leg_a:#?}"
    );

    // And the bundle on disk says so, which is where a triage reads it.
    let written: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(dir.join("verdict.json")).unwrap()).unwrap();
    assert_eq!(written["status"], "failed");
    assert_eq!(written["failures"][0]["field"], "header(Content-Disposition)", "{written:#}");
    assert_eq!(written["failures"][0]["observed"], "absent", "{written:#}");
    scene.finish().await;
}

/// **A refused send closes the call** (§11.2): a deviation this interpreter
/// cannot emit on a MID-DIALOG send stops the script there — and the close, not
/// a timeout, is what ends the established call the script could no longer
/// script.
#[tokio::test(start_paused = true)]
async fn a_send_refused_mid_dialog_ends_the_script_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-refused-send-closed").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "refused-send-closed".into();
    // The scripted BYE demands an emission fidelity no composition guarantees,
    // so the run refuses it AFTER the call is established.
    case.document.deviations.push(pivot_schema::deviation::Deviation {
        id: "d1".into(),
        kind: "verbatim-emission".into(),
        leg: Some("A".into()),
        step: Some("s10".into()),
        header: None,
        preserve: vec!["header-order".into(), "absolute-order".into()],
        retransmits: None,
        races: None,
        value: None,
    });
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::DeviationUnimplemented { step, .. } if step.as_deref() == Some("s10"))),
        "{:#?}",
        outcome.verdict.failures
    );
    // The refusal is stated as the abandonment it is, and the close ended the
    // established dialog: the caller BYEs, the callee answers it.
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!((abandoned.leg.as_deref(), abandoned.step.as_deref()), (Some("A"), Some("s10")));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **A missing injector closes the call** (§11.2): an `inject` no lane binding
/// performs aborts the run by name — and the call the script had already
/// established is still torn down rather than left to a settle timeout.
#[tokio::test(start_paused = true)]
async fn a_missing_injector_ends_the_script_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-injectorless-closed").await;
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = "injectorless-closed".into();
    // An external fault after the ACK lands, before the scripted teardown.
    let at = case
        .document
        .flow
        .iter()
        .position(|node| node.id() == "s10")
        .expect("the fixture has its BYE");
    case.document.flow.insert(
        at,
        pivot_schema::flow::FlowNode::Inject(pivot_schema::flow::Inject {
            id: "i1".into(),
            op: pivot_schema::flow::InjectOp::Inject,
            action: "node-kill".into(),
            target: None,
            after: vec!["s9".into()],
            delay: None,
        }),
    );
    let (outcome, _dir) = replay_case(&scene, case, BTreeMap::new()).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::InjectorMissing { node, action }
                if node == "i1" && action == "node-kill")),
        "{:#?}",
        outcome.verdict.failures
    );
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("i1"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// State a `retransmits` ladder on one step of a case, in memory.
fn declare_retransmits(case: &mut Case, step: &str, count: u32) {
    step_of(case, step).retransmits = Some(count);
}

/// Re-state a SEND as a rejection, in memory: the final it answers with, and no
/// body — a reject carries no answer to negotiate.
fn reject_at(case: &mut Case, step: &str, status: u16) {
    let held = step_of(case, step);
    held.msg.status = Some(status);
    held.msg.reason = None;
    held.msg.body = None;
}

/// Re-state the status an `expect` gates on, in memory: the status that then
/// arrives is a datagram the gate refuses.
fn expect_status(case: &mut Case, step: &str, status: u16) {
    let held = step_of(case, step);
    held.msg.status = Some(status);
    held.msg.reason = None;
}

/// One message step of a case, by id.
fn step_of<'a>(case: &'a mut Case, step: &str) -> &'a mut pivot_schema::flow::Step {
    case.document
        .flow
        .iter_mut()
        .find_map(|node| match node {
            pivot_schema::flow::FlowNode::Message(s) if s.id == step => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no step {step:?}"))
}

/// The leg and the identity a failure carrying an ARRIVAL states.
fn arrival_of(failure: &Failure) -> (&str, &Arrived) {
    match failure {
        Failure::UnexpectedDatagram { leg, arrived, .. }
        | Failure::DatagramAfterFlow { leg, arrived }
        | Failure::UnmatchedDatagram { leg, arrived, .. } => (leg, arrived),
        other => panic!("not a failure carrying an arrival: {other:?}"),
    }
}

/// A ladder paces ONE leg's transaction and gates nothing else.
///
/// The rungs wait BESIDE the loop, never inside it: sleeping them out in the
/// send path parks the whole run for the ladder's length, so every other leg's
/// dwell is pushed back by it and the peer this ladder is aimed at answers into
/// a loop that cannot hear it until the last rung is out.
#[tokio::test(start_paused = true)]
async fn a_ladder_paces_its_own_leg_and_parks_no_other() {
    let scene = api_scene("pivot-ladder-beside-the-loop").await;
    let (outcome, dir) = replay(&scene, "ladder-beside-the-loop.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);

    let legs = outcome.recording.legs();
    // The ladder still runs, and still on Timer E: two BYEs, T1 apart.
    let byes: Vec<_> = legs["A"].iter().filter(|m| text(m).starts_with("BYE ")).collect();
    assert_eq!(byes.len(), 2, "{:#?}", legs["A"]);
    assert_eq!(byes[1].repeat_of, Some(byes[0].seq));
    assert_eq!(
        byes[1].at_us - byes[0].at_us,
        500_000,
        "RFC 3261 §17.1.2.2: the rung is T1 past the first copy"
    );

    // And the teardown reached the callee — and was answered — WHILE the rung
    // was still owed, not queued behind it.
    let taken = legs["B"]
        .iter()
        .find(|m| text(m).starts_with("BYE "))
        .expect("the callee took the teardown");
    // Well inside the rung, not merely before it: a run parked by the ladder
    // relays the teardown only once the rung is out, so the midpoint separates
    // the two readings by more than this scene's own relay latency.
    let midpoint = byes[0].at_us + (byes[1].at_us - byes[0].at_us) / 2;
    assert!(
        taken.at_us < midpoint,
        "the teardown waited out the ladder: sent at {} us, relayed at {} us, rung at {} us",
        byes[0].at_us,
        taken.at_us,
        byes[1].at_us
    );

    scene.finish().await;
}

/// The recorded lines of one leg, read back off the bundle as raw JSON so the
/// on-disk encoding itself is what the rung asserts, not a decoder's reading
/// of it.
fn recorded_lines(dir: &std::path::Path, leg: &str) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join(format!("recording/{leg}.jsonl")))
        .unwrap_or_else(|e| panic!("recording/{leg}.jsonl: {e}"));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

/// The datagram a recorded line carries, reassembled from whichever of the
/// three arms the line wrote (`raw` | `head` + `body_b64` | `raw_b64`).
fn recorded_wire(line: &serde_json::Value) -> Vec<u8> {
    use base64::Engine as _;
    let b64 = |key: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(line[key].as_str().unwrap_or_else(|| panic!("{key} is a string: {line}")))
            .unwrap_or_else(|e| panic!("{key} decodes: {e}"))
    };
    if let Some(raw) = line["raw"].as_str() {
        return raw.as_bytes().to_vec();
    }
    if let Some(head) = line["head"].as_str() {
        let mut out = head.as_bytes().to_vec();
        out.extend(b64("body_b64"));
        return out;
    }
    b64("raw_b64")
}

/// The one recorded line on `leg`, in `dir`, whose datagram starts with `prefix`.
fn recorded_line<'a>(
    lines: &'a [serde_json::Value],
    dir: &str,
    prefix: &[u8],
) -> &'a serde_json::Value {
    lines
        .iter()
        .find(|line| line["dir"] == dir && recorded_wire(line).starts_with(prefix))
        .unwrap_or_else(|| {
            panic!("no {dir} line starting with {:?}: {lines:#?}", String::from_utf8_lossy(prefix))
        })
}

/// A recorded datagram whose body is not UTF-8 is written in the extractor's
/// `head` + `body_b64` form, and nowhere on the line is a replacement
/// character: the bytes that crossed the wire are the bytes on disk. Hands
/// back the reassembled datagram.
fn assert_recorded_as_head_and_bytes(line: &serde_json::Value) -> Vec<u8> {
    assert!(
        line.get("raw").is_none(),
        "a datagram that is not UTF-8 is never written as text: {line}"
    );
    assert!(
        line.get("raw_b64").is_none(),
        "a UTF-8 head is written as text, only the body as base64: {line}"
    );
    let head = line["head"].as_str().unwrap_or_else(|| panic!("head is a string: {line}"));
    assert!(head.ends_with("\r\n\r\n"), "the head runs through the blank line: {head:?}");
    // A heuristic over THESE fixtures, which carry no legitimate U+FFFD: a
    // replacement character on the line can only be a byte a lossy decode lost.
    let text = serde_json::to_string(line).unwrap();
    assert!(!text.contains('\u{FFFD}'), "no byte was replaced on the way to disk: {text}");
    recorded_wire(line)
}

/// The BINARY-BODY rung: a linear answered call carrying, mid-dialog, an INFO
/// whose body is bytes that are not UTF-8, sent by the caller and expected by
/// the callee under a `frozen` resource.
///
/// What the rung proves is that the recording keeps the datagram byte for
/// byte on BOTH legs — the caller's `out` line and the callee's `in` line are
/// written as a UTF-8 head plus the body's base64, never as text with the
/// bytes that would not decode replaced.
#[tokio::test(start_paused = true)]
async fn a_binary_body_is_recorded_byte_for_byte_on_both_legs() {
    let body = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/resources/binary-body_info.bin"),
    )
    .expect("the binary resource");
    assert!(std::str::from_utf8(&body).is_err(), "the resource is not UTF-8 by construction");

    let scene = api_scene("pivot-binary-body").await;
    let (outcome, dir) = replay(&scene, "binary-body.v3.json").await;

    // The bytes the CALLEE took off the wire are the resource's, so whatever
    // the recording holds is measured against a datagram that was right.
    let arrived = scene
        .bob
        .wire_view()
        .into_iter()
        .find(|entry| entry.raw.starts_with(b"INFO "))
        .expect("the callee received the INFO");
    assert!(arrived.raw.ends_with(&body), "the INFO reached the callee byte-exact");

    let callee = recorded_lines(&dir, "B");
    let taken = assert_recorded_as_head_and_bytes(recorded_line(&callee, "in", b"INFO "));
    assert!(taken.ends_with(&body), "the callee's line ends with the body bytes: {taken:02x?}");
    assert_eq!(
        taken, arrived.raw,
        "the callee's line IS the datagram its socket took, byte for byte"
    );
    let caller = recorded_lines(&dir, "A");
    let sent = assert_recorded_as_head_and_bytes(recorded_line(&caller, "out", b"INFO "));
    assert!(sent.ends_with(&body), "the caller's line ends with the body bytes: {sent:02x?}");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    scene.finish().await;
}

/// The same rung read from the CHECK side: the callee's expect asserts the
/// body's bytes as `body.b64`, the base64 of the resource file, and the run is
/// green — a binary body is confronted, not merely present.
#[tokio::test(start_paused = true)]
async fn a_binary_body_check_reads_its_bytes_as_base64_and_the_run_is_green() {
    let scene = api_scene("pivot-binary-body-check").await;
    let (outcome, dir) = replay(&scene, "binary-body.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 17, "every step ran");
    assert!(
        outcome.verdict.completed_steps.contains(&"s11".to_string()),
        "the callee's INFO expect completed with its body check: {:#?}",
        outcome.verdict.failures
    );
    assert!(
        outcome.verdict.failures.is_empty(),
        "the run is green: {:#?}",
        outcome.verdict.failures
    );
    scene.finish().await;
}

/// The MULTIPART variant: the initial INVITE carries an SDP part beside a
/// binary part, expected on the callee as a `multipart` body. The callee's
/// recorded line carries the body's LAYOUT — the extractor's `body.parts`,
/// offset and length per part — so a reader locates each part in the recorded
/// bytes without splitting on a boundary, and the binary part's bytes are the
/// resource's.
#[tokio::test(start_paused = true)]
async fn a_multipart_body_s_recording_locates_its_parts_and_keeps_the_binary_one() {
    let resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resources");
    let blob =
        std::fs::read(resources.join("binary-multipart_part1.bin")).expect("the binary part");
    assert!(std::str::from_utf8(&blob).is_err(), "the part is not UTF-8 by construction");

    let scene = api_scene("pivot-binary-multipart").await;
    let (outcome, dir) = replay(&scene, "binary-multipart.v3.json").await;

    let callee = recorded_lines(&dir, "B");
    let line = recorded_line(&callee, "in", b"INVITE ");
    let wire = assert_recorded_as_head_and_bytes(line);
    let head_len = wire.len() - line["body"]["len"].as_u64().expect("body.len") as usize;
    let body = &wire[head_len..];
    assert!(
        std::str::from_utf8(&wire[..head_len]).is_ok(),
        "the head runs to the blank line and is text"
    );
    let layout = &line["body"];
    assert_eq!(layout["content_type"], "multipart/mixed", "{layout}");
    let parts = layout["parts"].as_array().unwrap_or_else(|| panic!("body.parts: {line}"));
    assert_eq!(parts.len(), 2, "both parts located: {parts:#?}");
    let part = |n: usize| {
        let offset = parts[n]["offset"].as_u64().unwrap() as usize;
        let len = parts[n]["len"].as_u64().unwrap() as usize;
        &body[offset..offset + len]
    };
    assert_eq!(parts[0]["content_type"], "application/sdp");
    assert!(part(0).starts_with(b"v=0\r\n"), "the SDP part is located: {:?}", part(0));
    assert_eq!(parts[1]["content_type"], "application/vnd.example.blob");
    assert_eq!(part(1), &blob[..], "the binary part is the resource byte for byte");

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(outcome.verdict.completed_steps.len(), 13, "every step ran");
    scene.finish().await;
}

/// Two answers one leg is owed on two transactions carry no order between them
/// (RFC 3261 §17). The caller BYEs the early dialog its reliably-answered
/// INVITE opened (§15.1, §12.1.2), and the system owes it 200 to the BYE and
/// 487 to the INVITE (§15.1.2). The document lists the 487 first — the order
/// one implementation emitted them in — and this system answers the BYE first:
/// the run takes each where it lands, and the flow completes.
#[tokio::test(start_paused = true)]
async fn two_answers_on_two_transactions_are_taken_in_either_order() {
    let scene = api_scene("pivot-early-bye-answer-order").await;
    let (outcome, dir) = replay(&scene, "early-bye-answer-order.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    for step in (1..=17).map(|n| format!("s{n}")) {
        assert!(
            outcome.verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            outcome.verdict.completed_steps,
            outcome.verdict.failures
        );
    }
    // The rung proves the inversion only while the wire shows it: on leg A the
    // BYE's 200 landed BEFORE the INVITE's 487 the document lists first.
    let a_leg = &outcome.recording.legs()["A"];
    let position = |start: &str, method: &str| {
        a_leg
            .iter()
            .position(|m| {
                let raw = text(m);
                raw.starts_with(start)
                    && raw.lines().any(|l| l.starts_with("CSeq:") && l.ends_with(method))
            })
            .unwrap_or_else(|| panic!("{start} to {method} was recorded: {a_leg:#?}"))
    };
    let bye_ok = position("SIP/2.0 200", " BYE");
    let terminated = position("SIP/2.0 487", " INVITE");
    assert!(
        bye_ok < terminated,
        "the system answered the BYE first: 200 at {bye_ok}, 487 at {terminated}"
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// A response no armed expect matched is charged to the expect of ITS
/// transaction (RFC 3261 §17.1.3: CSeq method and number), whatever else is
/// armed on the leg; a final so charged retires that expect, and the flow runs
/// on. The callee rejects the INVITE (600) while the caller's PRACK is
/// unanswered; the system answers the PRACK 200 itself, where the document
/// names 481. One failure, on the PRACK step; the scripted ACK to the 600 goes
/// out; nothing is abandoned. The order the 600 and the 200 leave in is the
/// system's own, and the rung holds under either.
#[tokio::test(start_paused = true)]
async fn a_final_on_one_transaction_is_charged_to_that_transaction_s_expect_alone() {
    let scene = api_scene("pivot-answer-on-the-other-transaction").await;
    let (outcome, _dir) = replay(&scene, "answer-on-the-other-transaction.v3.json").await;
    assert_substituted_finals_run_on(&outcome, &[("s12", 481, "PRACK", 200)]);
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// Two finals on two transactions, both substituted: the callee's 600 where the
/// document names 486 to the INVITE, and the system's 200 where it names 481
/// to the PRACK. Each is charged to the expect of the transaction it rides,
/// never to the other's; both are retired; the ACK to the 600 is the scripted
/// one (a final of the same class leaves the dialog the tail was scripted for
/// where it was) and the flow completes.
#[tokio::test(start_paused = true)]
async fn two_substituted_finals_on_two_transactions_are_charged_one_to_each_expect() {
    let scene = api_scene("pivot-answers-on-two-transactions-substituted").await;
    let (outcome, _dir) = replay(&scene, "answers-on-two-transactions-substituted.v3.json").await;
    assert_substituted_finals_run_on(
        &outcome,
        &[("s11", 486, "INVITE", 600), ("s12", 481, "PRACK", 200)],
    );
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// The shape the two rungs above share: every listed `(step, named, method,
/// arrived)` is exactly one `UnmatchedDatagram` on that step and no other
/// failure exists; the listed steps are the retired ones; every other step
/// completed; leg A's ACK went out under its scripted step, not the generic
/// close's; the 600 was never repeated (the ACK landed inside T1); the run
/// settled.
fn assert_substituted_finals_run_on(outcome: &Outcome, substituted: &[(&str, u16, &str, u16)]) {
    let verdict = &outcome.verdict;
    assert!(verdict.abandoned.is_none(), "nothing abandoned: {:#?}", verdict.abandoned);
    assert_eq!(verdict.failures.len(), substituted.len(), "failures: {:#?}", verdict.failures);
    for (step_id, named, method, arrived_status) in substituted {
        let charged = verdict
            .failures
            .iter()
            .find(|f| matches!(f, Failure::UnmatchedDatagram { step, .. } if step == step_id))
            .unwrap_or_else(|| panic!("{step_id} is charged: {:#?}", verdict.failures));
        let Failure::UnmatchedDatagram { gated_on, arrived, .. } = charged else { unreachable!() };
        assert!(
            matches!(gated_on, GatedOn::Response { status, cseq_method: Some(m) }
                if status == named && m == method),
            "{step_id} gated on {gated_on}"
        );
        assert!(
            matches!(arrived, Arrived::Response { status, cseq_method, .. }
                if status == arrived_status && cseq_method == method),
            "{step_id} took {arrived}"
        );
    }
    let mut retired = verdict.retired.clone();
    retired.sort();
    let mut expected: Vec<String> = substituted.iter().map(|(s, ..)| s.to_string()).collect();
    expected.sort();
    assert_eq!(retired, expected, "the charged steps are the retired ones");
    for step in (1..=13).map(|n| format!("s{n}")) {
        if expected.contains(&step) {
            continue;
        }
        assert!(
            verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            verdict.completed_steps,
            verdict.failures
        );
    }
    let a_leg = &outcome.recording.legs()["A"];
    let ladder: Vec<String> = a_leg
        .iter()
        .map(|m| {
            format!(
                "{:?} {} step={:?} repeat_of={:?} note={:?}",
                m.dir,
                text(m).lines().next().unwrap_or_default(),
                m.step,
                m.repeat_of,
                m.note
            )
        })
        .collect();
    let acks: Vec<_> =
        a_leg.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "one ACK on A: {ladder:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s13"), "the scripted ACK, not the close's");
    assert!(
        !a_leg.iter().any(|m| m.dir == Dir::In
            && text(m).starts_with("SIP/2.0 600")
            && m.repeat_of.is_some()),
        "the 600 was never repeated: {ladder:#?}"
    );
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
}

/// A CANCEL sent after its INVITE's final reached the caller draws 200 from a
/// UAS whose INVITE server transaction still lingers in Completed (RFC 3261
/// §17.2.1, §9.2) and 481 from one that disposed of it. The document names the
/// 481; this system answers 200. The step takes it, the recording line says
/// why, and the call runs to its ACK with nothing retired and nothing
/// abandoned.
#[tokio::test(start_paused = true)]
async fn a_cancel_sent_after_its_invite_s_final_takes_200_or_481() {
    let scene = api_scene("pivot-late-cancel-final").await;
    let (outcome, dir) = replay(&scene, "late-cancel-final.v3.json").await;
    assert_bundle_is_complete(&outcome, &dir);
    let verdict = &outcome.verdict;
    for step in (1..=9).map(|n| format!("s{n}")) {
        assert!(
            verdict.completed_steps.contains(&step),
            "{step} never completed: {:#?}\nfailures: {:#?}",
            verdict.completed_steps,
            verdict.failures
        );
    }
    assert!(verdict.retired.is_empty(), "nothing retired: {:?}", verdict.retired);
    assert!(verdict.abandoned.is_none(), "nothing abandoned: {:#?}", verdict.abandoned);
    let a_leg = &outcome.recording.legs()["A"];
    let answer = a_leg
        .iter()
        .find(|m| {
            m.dir == Dir::In
                && text(m).lines().any(|l| l.starts_with("CSeq:") && l.ends_with(" CANCEL"))
        })
        .unwrap_or_else(|| panic!("the CANCEL was answered: {a_leg:#?}"));
    assert!(text(answer).starts_with("SIP/2.0 200"), "this system answers 200: {}", text(answer));
    assert_eq!(answer.step.as_deref(), Some("s8"), "the step naming 481 took the 200");
    assert_eq!(
        answer.note.as_deref(),
        Some(
            "tolerated: a final to a CANCEL sent after the INVITE's final arrived on this leg \
             draws 200 while the server transaction lives and 481 once it is gone \
             (RFC 3261 §9.2, §17.2.1); 200 arrived where the step names 481"
        ),
        "the recording says why"
    );
    let acks: Vec<_> =
        a_leg.iter().filter(|m| m.dir == Dir::Out && text(m).starts_with("ACK ")).collect();
    assert_eq!(acks.len(), 1, "one ACK on A: {a_leg:#?}");
    assert_eq!(acks[0].step.as_deref(), Some("s9"), "the scripted ACK, not the close's");
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}
