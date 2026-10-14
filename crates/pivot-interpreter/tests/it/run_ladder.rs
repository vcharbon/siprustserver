//! The run ladder: a compiled plan driven over a real B2BUA on a paused clock,
//! producing a complete run bundle.
//!
//! This module holds the fixtures every rung shares: the demo lane and its URI
//! composition, the scenes, the replay entry points, and the readers and
//! assertions over the bundle on disk. Each child module is one scenario family,
//! and each of its rungs proves the flow ran, the run SETTLED, and the bundle
//! holds every datagram.

mod background;
mod bodies;
mod budgets;
mod cancel;
mod close;
mod deviations;
mod drafts;
mod finals_and_acks;
mod flow_shapes;
mod forks;
mod hunting;
mod limiter;
mod matching;
mod negative;
mod retransmissions;
mod unscripted;

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

use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use b2bua_harness::{B2buaScene, B2buaSut, BOB_PORT};
use call::model::cdr::CdrEventType;
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpTransport, SimulatedHttpNetwork};
use pivot_interpreter::plan::Plan;
use pivot_interpreter::{
    Booking, ClockMode, CloseOwed, Failure, IdentityBindings, Lane, Outcome, RunConfig, Sut,
    UriComposer, VerdictStatus,
};
use pivot_schema::bundle::recording::{Dir, RecordedMessage};
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
    /// The ring timer the attempt's `no_answer_ms` states, where it states one.
    no_answer_ms: Option<i64>,
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
                no_answer_ms: attempt.no_answer_ms.map(|ms| ms as i64),
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
            if let Some(ms) = hop.no_answer_ms {
                route["no_answer_ms"] = serde_json::json!(ms);
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
/// external-case rungs at a library outside this crate, and each rung asks for
/// its case by a descriptive name the library provides (a directory, or a link
/// to one); the rung SKIPS LOUDLY when it is unset rather than passing on an
/// empty corpus.
fn case_dir(name: &str) -> Option<Case> {
    let roots = std::env::var("PIVOT_CASE_DIRS").ok()?;
    let found = roots
        .split(':')
        .filter(|root| !root.is_empty())
        .map(|root| PathBuf::from(root).join(name))
        .find(|dir| dir.join("scenario.json").is_file())?;
    Some(read_case(&found.join("scenario.json"), found))
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
pub(crate) async fn api_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_api_call("127.0.0.1", bob_port)
            // Short enough that a call held up for a couple of seconds really is
            // polled, so a `background` OPTIONS counter asserts something.
            .tune(|config| config.keepalive_interval_sec = 2)
    })
    .await
}

/// Where this rung writes its bundle: a per-test directory a human can open in
/// the viewer after the run, `<root>/<case>/<test>`. Several rungs replay one
/// case, possibly at once in separate processes, so the test's thread name (the
/// test's path under libtest and nextest alike) keeps their bundles apart.
fn bundle_dir(case: &str) -> PathBuf {
    let root = std::env::var("PIVOT_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("pivot-runs"));
    let test = std::thread::current().name().unwrap_or("unnamed").replace("::", "-");
    root.join(case).join(test)
}

/// Run one fixture on the default demo lane (alice → b2bua → bob), write its
/// bundle, and hand back what the run decided.
pub(crate) async fn replay(scene: &B2buaScene, fixture_name: &str) -> (Outcome, PathBuf) {
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
    let egress = lane_egress(&plan, &composer, &identities, &agents);
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
pub(crate) fn assert_bundle_is_complete(outcome: &Outcome, dir: &std::path::Path) {
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

/// The recording note an emission no step scripts carries.
fn is_unscripted_answer(note: &str) -> bool {
    note.starts_with("the transaction the flow never scripted")
}

/// State a `retransmits` ladder on one step of a case, in memory.
fn declare_retransmits(case: &mut Case, step: &str, count: u32) {
    step_of(case, step).retransmits = Some(count);
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

/// The recorded lines of one leg, read back off the bundle as raw JSON so the
/// on-disk encoding itself is what the rung asserts, not a decoder's reading
/// of it.
pub(crate) fn recorded_lines(dir: &std::path::Path, leg: &str) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join(format!("recording/{leg}.jsonl")))
        .unwrap_or_else(|e| panic!("recording/{leg}.jsonl: {e}"));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

/// The datagram a recorded line carries, reassembled from whichever of the
/// three arms the line wrote (`raw` | `head` + `body_b64` | `raw_b64`).
pub(crate) fn recorded_wire(line: &serde_json::Value) -> Vec<u8> {
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
