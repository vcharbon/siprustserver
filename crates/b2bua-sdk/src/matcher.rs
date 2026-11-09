//! The declarative [`Match`] a rule selects its events by: the event kind,
//! method, status, direction, leg and call state, and a free filter over the
//! [`RuleContext`].

use call::{CallModelState, Direction, LegDisposition, LegState, MachineId, TimerType};

use crate::event::CallEvent;
use crate::rule_context::RuleContext;

/// Status matcher for a response rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusMatch {
    Any,
    /// Exact status code.
    Code(u16),
    /// Status class (1 = 1xx, 2 = 2xx, …).
    Class(u8),
}

impl StatusMatch {
    fn accepts(self, status: u16) -> bool {
        match self {
            StatusMatch::Any => true,
            StatusMatch::Code(c) => c == status,
            StatusMatch::Class(c) => status / 100 == u16::from(c),
        }
    }
}

/// Which event family a rule intercepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Request,
    Response,
    Timer,
    Timeout,
    Cancelled,
    InternalEvent,
    /// A lifecycle event the engine delivers to the active services after a
    /// turn's claim ([`crate::lifecycle`]); only a machine-bound rule
    /// receives one.
    Lifecycle,
}

/// A declarative match descriptor — every omitted column matches any value.
#[derive(Clone)]
pub struct Match {
    pub kind: MatchKind,
    /// Request method / response cseq-method / timeout method, as spelled (RFC 3261 §7.1).
    pub methods: Option<Vec<&'static str>>,
    pub status: StatusMatch,
    /// Exact timer-type matching — for a [`TimerType::Service`] entry this is
    /// exact `(service_id, key)` equality (the leg, if any, is not part of the
    /// type). Use [`Self::service_timers`] for a per-service any-key wildcard.
    pub timer_types: Option<Vec<TimerType>>,
    /// Per-service timer wildcard: accept any [`TimerType::Service`] firing
    /// whose `service_id` matches, regardless of `key`. A service can therefore
    /// funnel all its watchdogs through one rule and branch on
    /// [`RuleContext::service_timer_key`]. Never matches a core timer, and a
    /// rule must name a `MachineId` to receive service fires at all — so no
    /// rule catches another service's (or core's) timers by accident.
    pub timer_service: Option<MachineId>,
    pub call_state: Option<Vec<CallModelState>>,
    pub leg_state: Option<Vec<LegState>>,
    pub leg_disposition: Option<Vec<LegDisposition>>,
    pub direction: Option<Direction>,
    pub topic: Option<&'static str>,
    pub outcome: Option<&'static str>,
    /// For a lifecycle rule: the one event kind it takes
    /// ([`LifecycleEvent::kind`](crate::lifecycle::LifecycleEvent::kind)); `None` takes every kind.
    pub lifecycle: Option<&'static str>,
    /// A corner-case predicate (pure, sync) over the resolved context.
    pub filter: Option<fn(&RuleContext) -> bool>,
}

impl Match {
    fn base(kind: MatchKind) -> Self {
        Self {
            kind,
            methods: None,
            status: StatusMatch::Any,
            timer_types: None,
            timer_service: None,
            call_state: None,
            leg_state: None,
            leg_disposition: None,
            direction: None,
            topic: None,
            outcome: None,
            lifecycle: None,
            filter: None,
        }
    }
    pub fn request() -> Self {
        Self::base(MatchKind::Request)
    }
    pub fn response() -> Self {
        Self::base(MatchKind::Response)
    }
    pub fn timer() -> Self {
        Self::base(MatchKind::Timer)
    }
    pub fn timeout() -> Self {
        Self::base(MatchKind::Timeout)
    }
    pub fn cancelled() -> Self {
        Self::base(MatchKind::Cancelled)
    }
    pub fn internal_event() -> Self {
        Self::base(MatchKind::InternalEvent)
    }
    /// An attempt opened ([`LifecycleEvent::AttemptStarted`](crate::lifecycle::LifecycleEvent::AttemptStarted)).
    pub fn attempt_started() -> Self {
        Self { lifecycle: Some("attempt-started"), ..Self::base(MatchKind::Lifecycle) }
    }
    /// An attempt ended ([`LifecycleEvent::AttemptEnded`](crate::lifecycle::LifecycleEvent::AttemptEnded)).
    pub fn attempt_ended() -> Self {
        Self { lifecycle: Some("attempt-ended"), ..Self::base(MatchKind::Lifecycle) }
    }
    /// A leg ended ([`LifecycleEvent::LegEnded`](crate::lifecycle::LifecycleEvent::LegEnded)).
    pub fn leg_ended() -> Self {
        Self { lifecycle: Some("leg-ended"), ..Self::base(MatchKind::Lifecycle) }
    }
    pub fn topic(mut self, t: &'static str) -> Self {
        self.topic = Some(t);
        self
    }
    pub fn outcome(mut self, o: &'static str) -> Self {
        self.outcome = Some(o);
        self
    }
    pub fn method(mut self, m: &'static str) -> Self {
        self.methods.get_or_insert_with(Vec::new).push(m);
        self
    }
    pub fn methods(mut self, ms: &[&'static str]) -> Self {
        self.methods = Some(ms.to_vec());
        self
    }
    pub fn status_code(mut self, c: u16) -> Self {
        self.status = StatusMatch::Code(c);
        self
    }
    pub fn status_class(mut self, c: u8) -> Self {
        self.status = StatusMatch::Class(c);
        self
    }
    pub fn timer_type(mut self, t: TimerType) -> Self {
        self.timer_types.get_or_insert_with(Vec::new).push(t);
        self
    }
    /// Match any [`TimerType::Service`] firing owned by `service_id` (any key).
    /// See [`Self::timer_service`].
    pub fn service_timers(mut self, service_id: MachineId) -> Self {
        self.timer_service = Some(service_id);
        self
    }
    pub fn call_state(mut self, s: CallModelState) -> Self {
        self.call_state.get_or_insert_with(Vec::new).push(s);
        self
    }
    /// The source leg's state ([`call::Leg::state`]), read off its facts as
    /// the event finds them, is one of `s`.
    pub fn leg_states(mut self, s: &[LegState]) -> Self {
        self.leg_state = Some(s.to_vec());
        self
    }
    /// The source leg's INVITE outcome ([`call::Leg::disposition`]) is `d`
    /// (cumulative: one of every `d` given).
    pub fn leg_disposition(mut self, d: LegDisposition) -> Self {
        self.leg_disposition.get_or_insert_with(Vec::new).push(d);
        self
    }
    pub fn direction(mut self, d: Direction) -> Self {
        self.direction = Some(d);
        self
    }
    pub fn filter(mut self, f: fn(&RuleContext) -> bool) -> Self {
        self.filter = Some(f);
        self
    }

    /// Does `ctx`'s event match this descriptor (columns only; the `filter` is
    /// applied by the matcher after column acceptance)?
    pub fn accepts_columns(&self, ctx: &RuleContext) -> bool {
        // Event-kind gate.
        let kind_ok = matches!(
            (self.kind, ctx.event),
            (MatchKind::Request, CallEvent::Sip { .. })
                | (MatchKind::Response, CallEvent::Sip { .. })
                | (MatchKind::Timer, CallEvent::Timer { .. })
                | (MatchKind::Timeout, CallEvent::Timeout { .. })
                | (MatchKind::Cancelled, CallEvent::Cancelled { .. })
                | (MatchKind::InternalEvent, CallEvent::InternalEvent { .. })
                | (MatchKind::Lifecycle, CallEvent::Lifecycle(_))
        );
        if !kind_ok {
            return false;
        }
        // For Sip events, request-rules only match requests and response-rules
        // only responses.
        match (self.kind, ctx.request(), ctx.response()) {
            (MatchKind::Request, Some(_), _) => {}
            (MatchKind::Request, None, _) => return false,
            (MatchKind::Response, _, Some(_)) => {}
            (MatchKind::Response, _, None) => return false,
            _ => {}
        }

        if let Some(methods) = &self.methods {
            let m = match self.kind {
                MatchKind::Request => ctx.request().map(|r| r.method().to_string()),
                MatchKind::Response => ctx.response().map(|r| r.cseq().method().to_string()),
                MatchKind::Timeout => ctx.timeout_method().map(str::to_string),
                _ => None,
            };
            match m {
                Some(m) if methods.iter().any(|x| *x == m) => {}
                _ => return false,
            }
        }

        if self.kind == MatchKind::Response {
            if let Some(r) = ctx.response() {
                if !self.status.accepts(r.status()) {
                    return false;
                }
            }
        }

        if let Some(tts) = &self.timer_types {
            match ctx.timer_type() {
                Some(t) if tts.contains(t) => {}
                _ => return false,
            }
        }
        if let Some(sid) = &self.timer_service {
            match ctx.timer_type() {
                Some(TimerType::Service { service_id, .. }) if service_id == sid => {}
                _ => return false,
            }
        }

        if let Some(states) = &self.call_state {
            if !states.contains(&ctx.call.state()) {
                return false;
            }
        }
        if let Some(states) = &self.leg_state {
            match ctx.source_leg() {
                Some(leg) if states.contains(&leg.state()) => {}
                _ => return false,
            }
        }
        if let Some(disps) = &self.leg_disposition {
            match ctx.source_leg() {
                Some(leg) if disps.contains(&leg.disposition()) => {}
                _ => return false,
            }
        }
        if let Some(dir) = self.direction {
            if ctx.direction != dir {
                return false;
            }
        }
        if let Some(topic) = self.topic {
            if !matches!(ctx.event, CallEvent::InternalEvent { topic: t, .. } if t == topic) {
                return false;
            }
        }
        if let Some(lifecycle) = self.lifecycle {
            if !matches!(ctx.event, CallEvent::Lifecycle(e) if e.kind() == lifecycle) {
                return false;
            }
        }
        if let Some(outcome) = self.outcome {
            if !matches!(ctx.event, CallEvent::InternalEvent { outcome: o, .. } if o == outcome) {
                return false;
            }
        }
        true
    }
}
