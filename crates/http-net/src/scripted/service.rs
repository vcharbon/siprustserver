//! [`ScriptedHttpService`]: serves added [`HttpScript`]s under the
//! hard-failure rule.
//!
//! Position within an instance rides the token only; per instance the service
//! keeps two monotone facts that matching never reads: `opened`, and how far
//! it got (the highest position reached, or `done` for a code step), from
//! which the verdict derives `unserved`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::error::HttpScriptError;
use super::matcher;
use super::open::{self, Candidate, Pick};
use super::program::{HttpBindings, HttpReply, HttpScript, HttpScriptStep, HttpState};
use super::template::{self, Captures};
use super::token::{self, HttpContinuation, TokenAt};
use super::validate;
use super::verdict::{HttpFinding, HttpFindingKind, HttpScriptHandle, HttpVerdict};
use crate::{HttpAnswer, HttpRequest, HttpResponse, HttpService};

/// How many near-miss opens an unmatched opening request's diagnostic lists.
const NEAR_MISSES_SHOWN: usize = 3;

/// The state every clone of a service and every handle share.
pub(super) struct Shared {
    nonce: u64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Indexed by instance id.
    instances: Vec<Instance>,
    findings: Vec<HttpFinding>,
}

struct Instance {
    script: Arc<HttpScript>,
    bindings: Arc<HttpBindings>,
    fragments: Vec<super::open::Fragment>,
    opened: bool,
    progress: Progress,
}

enum Progress {
    /// The highest position a served step pointed past, of `len` steps.
    Reified { reached: usize, len: usize },
    /// Whether the code step returned no next state.
    Code { done: bool },
}

impl Progress {
    fn complete(&self) -> bool {
        match self {
            Self::Reified { reached, len } => reached >= len,
            Self::Code { done } => *done,
        }
    }

    fn describe(&self, opened: bool) -> String {
        match (opened, self) {
            (false, _) => "never opened".to_string(),
            (true, Self::Reified { reached, len }) => {
                format!("stopped after step {reached} of {len}")
            }
            (true, Self::Code { .. }) => "the code step never returned a final state".to_string(),
        }
    }
}

impl Shared {
    pub(super) fn verdict(&self, instance: u64) -> HttpVerdict {
        let state = self.state.lock().unwrap();
        let Some(inst) = usize::try_from(instance).ok().and_then(|i| state.instances.get(i)) else {
            return HttpVerdict { instance, opened: false, complete: false, findings: Vec::new() };
        };
        let mut findings: Vec<HttpFinding> =
            state.findings.iter().filter(|f| f.instances.contains(&instance)).cloned().collect();
        let complete = inst.progress.complete();
        if !complete {
            findings.push(unserved(instance, inst));
        }
        HttpVerdict { instance, opened: inst.opened, complete, findings }
    }
}

fn unserved(instance: u64, inst: &Instance) -> HttpFinding {
    HttpFinding {
        kind: HttpFindingKind::Unserved,
        instances: vec![instance],
        method: String::new(),
        path: String::new(),
        body: String::new(),
        detail: format!(
            "{} {}: {}",
            inst.script.open.method,
            inst.script.open.path,
            inst.progress.describe(inst.opened)
        ),
    }
}

/// An [`HttpService`] serving the scripts added to it. Clones share the
/// scripts, their progress and the findings.
#[derive(Clone)]
pub struct ScriptedHttpService {
    shared: Arc<Shared>,
}

impl Default for ScriptedHttpService {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedHttpService {
    /// A service with a random token nonce, so a token minted by another
    /// service (another run against the same peer) is told apart.
    pub fn new() -> Self {
        Self::with_nonce(token::random_nonce())
    }

    /// A service whose tokens travel under `codec`.
    pub fn with_codec(_codec: Arc<dyn super::HttpContinuationCodec>) -> Self {
        Self::new()
    }

    /// A service whose tokens carry `nonce` and travel under `codec`.
    pub fn with_nonce_and_codec(nonce: u64, _codec: Arc<dyn super::HttpContinuationCodec>) -> Self {
        Self::with_nonce(nonce)
    }

    /// A service whose tokens carry `nonce`.
    pub fn with_nonce(nonce: u64) -> Self {
        Self { shared: Arc::new(Shared { nonce, state: Mutex::new(State::default()) }) }
    }

    /// Add one instance of `script`, its `${bind:…}` resolved from `bindings`.
    /// Refused when it could never be served as written.
    pub fn add(
        &self,
        script: HttpScript,
        bindings: HttpBindings,
    ) -> Result<HttpScriptHandle, HttpScriptError> {
        let checked = validate::check(&script, &bindings)?;
        let progress = match &script.step {
            HttpScriptStep::Reified(steps) => Progress::Reified { reached: 0, len: steps.len() },
            HttpScriptStep::Code(_) => Progress::Code { done: false },
        };
        let mut state = self.shared.state.lock().unwrap();
        let instance = state.instances.len() as u64;
        state.instances.push(Instance {
            script: Arc::new(script),
            bindings: Arc::new(bindings),
            fragments: checked.fragments,
            opened: false,
            progress,
        });
        Ok(HttpScriptHandle {
            shared: self.shared.clone(),
            instance,
            attributable: checked.attributable,
        })
    }

    /// Every finding so far, plus one [`HttpFindingKind::Unserved`] per
    /// instance not complete: read at run end, the service-level verdict.
    pub fn findings(&self) -> Vec<HttpFinding> {
        let state = self.shared.state.lock().unwrap();
        let mut findings = state.findings.clone();
        for (i, inst) in state.instances.iter().enumerate() {
            if !inst.progress.complete() {
                findings.push(unserved(i as u64, inst));
            }
        }
        findings
    }

    /// Decide the reply to `req`: a step to serve, or a finding.
    fn decide(&self, req: &HttpRequest) -> Result<Serve, Refusal> {
        let body = matcher::normalize(&req.body);
        // The same token echoed in two places is one position, not two.
        let mut tokens: Vec<HttpContinuation> = Vec::new();
        for token in HttpContinuation::scan(&String::from_utf8_lossy(&req.body)) {
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }
        let own = tokens.iter().filter(|t| t.nonce == self.shared.nonce).count();
        match (tokens.as_slice(), own) {
            ([], _) => self.open(req, &body),
            ([token], 1) => self.follow(req, &body, token),
            (foreign, 0) => Err(finding(
                HttpFindingKind::ForeignToken,
                Vec::new(),
                req,
                format!(
                    "tokens of another service only (nonces {:016x?})",
                    foreign.iter().map(|t| t.nonce).collect::<Vec<_>>()
                ),
            )),
            (several, _) => Err(finding(
                HttpFindingKind::Ambiguous,
                several
                    .iter()
                    .filter(|t| t.nonce == self.shared.nonce)
                    .map(|t| t.instance)
                    .collect(),
                req,
                format!("{} tokens in one body, {own} of this service", several.len()),
            )),
        }
    }

    /// A tokenless request: the open-match rule picks the instance it opens,
    /// among those whose open and step 0 both match.
    fn open(&self, req: &HttpRequest, body: &str) -> Result<Serve, Refusal> {
        let mut state = self.shared.state.lock().unwrap();
        let mut candidates = Vec::new();
        let mut captured = Vec::new();
        let mut near = Vec::new();
        for (i, inst) in state.instances.iter().enumerate() {
            if inst.opened || !matcher::same_target(&inst.script.open, req) {
                continue;
            }
            let mut captures = Captures::new();
            let step_zero = match &inst.script.step {
                HttpScriptStep::Reified(steps) => Some(&steps[0].expect),
                HttpScriptStep::Code(_) => None,
            };
            let opens =
                matcher::matches(&inst.script.open, &inst.bindings, req, body, &mut captures)
                    .and_then(|()| match step_zero {
                        Some(expect) => {
                            matcher::matches(expect, &inst.bindings, req, body, &mut captures)
                        }
                        None => Ok(()),
                    });
            match opens {
                Ok(()) => {
                    candidates
                        .push(Candidate { instance: i as u64, fragments: inst.fragments.clone() });
                    captured.push(captures);
                }
                Err(why) => near.push(why),
            }
        }
        let instance = match open::pick(&candidates) {
            Pick::One(instance) => instance,
            Pick::None => {
                let detail = if near.is_empty() {
                    format!("no unopened script opens {} {}", req.method, req.path)
                } else {
                    let shown = near.len().min(NEAR_MISSES_SHOWN);
                    let more = near.len() - shown;
                    let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
                    format!(
                        "no unopened script opens this request: {}{more}",
                        near[..shown].join("; ")
                    )
                };
                return Err(finding(HttpFindingKind::Unmatched, Vec::new(), req, detail));
            }
            Pick::Ambiguous(instances) => {
                let detail = format!("instances {instances:?} all open this request");
                return Err(finding(HttpFindingKind::Ambiguous, instances, req, detail));
            }
        };
        let captures = candidates
            .iter()
            .position(|c| c.instance == instance)
            .map(|at| std::mem::take(&mut captured[at]))
            .unwrap_or_default();
        let inst = &mut state.instances[instance as usize];
        inst.opened = true;
        let (script, bindings) = (inst.script.clone(), inst.bindings.clone());
        drop(state);
        match &script.step {
            HttpScriptStep::Reified(_) => self.reified(req, body, instance, 0, captures),
            HttpScriptStep::Code(_) => self.code(req, instance, &script, bindings, None),
        }
    }

    /// A request carrying one of this service's tokens.
    fn follow(
        &self,
        req: &HttpRequest,
        body: &str,
        token: &HttpContinuation,
    ) -> Result<Serve, Refusal> {
        let (script, bindings) = {
            let state = self.shared.state.lock().unwrap();
            let inst = usize::try_from(token.instance).ok().and_then(|i| state.instances.get(i));
            let Some(inst) = inst else {
                let detail = format!("the token names no instance ({})", token.instance);
                return Err(finding(HttpFindingKind::Unmatched, Vec::new(), req, detail));
            };
            (inst.script.clone(), inst.bindings.clone())
        };
        match (&script.step, &token.at) {
            (HttpScriptStep::Reified(_), TokenAt::Reified { position, captures }) => {
                self.reified(req, body, token.instance, *position, captures.clone())
            }
            (HttpScriptStep::Code(_), TokenAt::Code { state: Some(s) }) => {
                self.code(req, token.instance, &script, bindings, Some(HttpState(s.clone())))
            }
            (HttpScriptStep::Code(_), TokenAt::Code { state: None }) => Err(finding(
                HttpFindingKind::Unmatched,
                vec![token.instance],
                req,
                "a request past the last step: the code step had completed".to_string(),
            )),
            _ => Err(finding(
                HttpFindingKind::Unmatched,
                vec![token.instance],
                req,
                "the token does not fit its script's step kind".to_string(),
            )),
        }
    }

    /// Serve reified step `position` of `instance`, or refuse the request.
    fn reified(
        &self,
        req: &HttpRequest,
        body: &str,
        instance: u64,
        position: usize,
        mut captures: Captures,
    ) -> Result<Serve, Refusal> {
        let mut state = self.shared.state.lock().unwrap();
        let inst = &mut state.instances[instance as usize];
        let HttpScriptStep::Reified(steps) = &inst.script.step else {
            unreachable!("reified() is called for reified scripts only");
        };
        let Some(step) = steps.get(position) else {
            let detail = format!("a request past the last step ({} steps)", steps.len());
            return Err(finding(HttpFindingKind::Unmatched, vec![instance], req, detail));
        };
        if let Err(why) = matcher::matches(&step.expect, &inst.bindings, req, body, &mut captures) {
            let detail = format!("step {position}: {why}");
            return Err(finding(HttpFindingKind::Unmatched, vec![instance], req, detail));
        }
        let reply = step.reply.clone();
        if let Progress::Reified { reached, .. } = &mut inst.progress {
            *reached = (*reached).max(position + 1);
        }
        let token = HttpContinuation {
            nonce: self.shared.nonce,
            instance,
            at: TokenAt::Reified { position: position + 1, captures: captures.clone() },
        };
        Ok(Serve { instance, reply, bindings: inst.bindings.clone(), captures, token })
    }

    /// Run the code step of `instance` on `req`, outside the lock.
    fn code(
        &self,
        req: &HttpRequest,
        instance: u64,
        script: &HttpScript,
        bindings: Arc<HttpBindings>,
        state: Option<HttpState>,
    ) -> Result<Serve, Refusal> {
        let HttpScriptStep::Code(code) = &script.step else {
            unreachable!("code() is called for code scripts only");
        };
        let (reply, next) = code(req, &bindings, state.as_ref()).map_err(|refusal| {
            finding(HttpFindingKind::Unmatched, vec![instance], req, refusal.detail)
        })?;
        if next.is_none() {
            let mut shared = self.shared.state.lock().unwrap();
            if let Progress::Code { done } = &mut shared.instances[instance as usize].progress {
                *done = true;
            }
        }
        let token = HttpContinuation {
            nonce: self.shared.nonce,
            instance,
            at: TokenAt::Code { state: next.map(|s| s.0) },
        };
        Ok(Serve { instance, reply, bindings, captures: Captures::new(), token })
    }

    /// Record `finding` and build the `500` that answers it.
    fn record(&self, finding: HttpFinding) -> HttpResponse {
        let body = format!("{:?}: {}\n", finding.kind, finding.detail);
        self.shared.state.lock().unwrap().findings.push(finding);
        HttpResponse::status(500)
            .with_body(body.into_bytes())
            .header("content-type", "text/plain; charset=utf-8")
    }

    /// Answer `req`, naming the instance that served it when one did.
    async fn serve(&self, req: &HttpRequest) -> (HttpAnswer, Option<u64>) {
        let serve = match self.decide(req) {
            Ok(serve) => serve,
            Err(finding) => return (HttpAnswer::Response(self.record(*finding)), None),
        };
        let token = serve.token.render();
        let answer = match render(&serve.reply, &serve, &token) {
            Ok(rendered) => deliver(rendered).await,
            Err(why) => HttpAnswer::Response(self.record(*finding(
                HttpFindingKind::Unmatched,
                vec![serve.instance],
                req,
                format!("the script's reply does not render: {why}"),
            ))),
        };
        (answer, Some(serve.instance))
    }
}

/// A step to serve: its reply and what its templates resolve against.
struct Serve {
    instance: u64,
    reply: HttpReply,
    bindings: Arc<HttpBindings>,
    captures: Captures,
    token: HttpContinuation,
}

/// A reply with its templates rendered.
enum Rendered {
    Respond(HttpResponse),
    Silence,
    Late(u64, Box<Rendered>),
    Reset,
}

fn render(reply: &HttpReply, serve: &Serve, token: &str) -> Result<Rendered, String> {
    let text = |t: &str| {
        let pieces = template::parse(t).map_err(|bad| format!("placeholder {bad}"))?;
        template::render(&pieces, &serve.bindings, &serve.captures, token)
    };
    Ok(match reply {
        HttpReply::Respond { status, headers, body } => {
            if !validate::final_status(*status) {
                return Err(format!("status {status} is not a final response status"));
            }
            let mut resp = HttpResponse::status(*status).with_body(text(body)?.into_bytes());
            for (name, value) in headers {
                let value = text(value)?;
                validate::header_name(name)
                    .and_then(|()| validate::header_value(&value))
                    .map_err(|why| format!("header {name:?}: {why}"))?;
                resp = resp.header(name.clone(), value);
            }
            Rendered::Respond(resp)
        }
        HttpReply::Silence => Rendered::Silence,
        HttpReply::Late { ms, then } => Rendered::Late(*ms, Box::new(render(then, serve, token)?)),
        HttpReply::Reset => Rendered::Reset,
    })
}

async fn deliver(rendered: Rendered) -> HttpAnswer {
    let mut rendered = rendered;
    loop {
        match rendered {
            Rendered::Respond(resp) => return HttpAnswer::Response(resp),
            Rendered::Reset => return HttpAnswer::Abort,
            Rendered::Silence => std::future::pending::<()>().await,
            Rendered::Late(ms, then) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                rendered = *then;
            }
        }
    }
}

/// A request the scripts refuse, answered `500` and recorded.
type Refusal = Box<HttpFinding>;

fn finding(
    kind: HttpFindingKind,
    instances: Vec<u64>,
    req: &HttpRequest,
    detail: String,
) -> Refusal {
    Box::new(HttpFinding {
        kind,
        instances,
        method: req.method.clone(),
        path: req.path.clone(),
        body: String::from_utf8_lossy(&req.body).into_owned(),
        detail,
    })
}

#[async_trait]
impl HttpService for ScriptedHttpService {
    /// For a caller that cannot close a connection: a [`HttpReply::Reset`]
    /// is answered `500` and recorded as [`HttpFindingKind::ResetNotForwarded`],
    /// so a caller that does not forward `answer` fails the run instead of
    /// passing for a reset.
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match self.serve(&req).await {
            (HttpAnswer::Response(resp), _) => resp,
            (HttpAnswer::Abort, instance) => self.record(*finding(
                HttpFindingKind::ResetNotForwarded,
                instance.into_iter().collect(),
                &req,
                "a Reset reached HttpService::handle: the caller does not forward \
                 HttpService::answer"
                    .to_string(),
            )),
        }
    }

    async fn answer(&self, req: HttpRequest) -> HttpAnswer {
        self.serve(&req).await.0
    }
}
