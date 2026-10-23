//! The base catalogue of the sweep (ADR-0055 §11): the shapes every change to
//! the HA layer is cut against — a plain call, a re-INVITE, an UPDATE, an INFO,
//! reliable provisionals and their PRACK, a REFER with its NOTIFYs, a BYE
//! crossing each in-dialog request, the B2BUA's keepalive, a forked answer and
//! a media-server leg.

mod plays;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{default_call_refer, route_to, route_to_processing_refer};
use b2bua::decision::{CallDecisionEngine, NewCallResponse, RouteDecision, ScriptedDecisionEngine};
use call::LimiterEntry;
use callshapes::shapes::default_binder;
use callshapes::{ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown};
use scenario_harness::actor::ActorScenario;
use scenario_harness::realcall::CallEnv;

pub use plays::{ByeCrossing, Crossed, InfoCall, MediaLeg, REFER_KEY};

use super::{SweepShape, BOB, MEDIA};
use crate::cluster::{ClusterRecipe, TRUNK};

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("a socket address")
}

/// `route` holding one set of the cluster's limiter.
fn limited(mut route: RouteDecision) -> NewCallResponse {
    route.call_limiter = vec![LimiterEntry { id: TRUNK.into(), limit: 8 }];
    NewCallResponse::Route(route)
}

/// Every call routed to bob, transfers authorized by the scripted REFER
/// decision.
fn transferring() -> ClusterRecipe {
    let bob = addr(BOB);
    ClusterRecipe::deciding(move |_| -> Arc<dyn CallDecisionEngine> {
        Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    limited(route_to_processing_refer(&bob.ip().to_string(), bob.port()))
                })
                .on_refer(default_call_refer)
                .build(),
        )
    })
}

/// A plain call: INVITE, ring, answer, ACK, the caller's BYE.
pub struct Basic;

impl SweepShape for Basic {
    fn id(&self) -> &'static str {
        "basic"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(callshapes::shapes::basic_call(default_binder()))
    }
}

/// The caller's delayed-offer re-INVITE on the established call.
pub struct Reinvite;

impl SweepShape for Reinvite {
    fn id(&self) -> &'static str {
        "reinvite"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(callshapes::shapes::reinvite(default_binder()))
    }
}

/// The caller's UPDATE on the established call (RFC 3311).
pub struct Update;

impl SweepShape for Update {
    fn id(&self) -> &'static str {
        "update"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(ShapePlan {
            id: "update",
            binder: default_binder(),
            establish: Establishment::Transparent,
            stages: vec![Stage::Script(Script::UpdatePostConnect)],
            teardown: Teardown::CallerBye { after: DwellKnob::None, feed: ByeFeed::NoFeed },
            ringing_gate: false,
            stamp_connected: false,
        })
    }
}

/// The caller's INFO on the established call.
pub struct Info;

impl SweepShape for Info {
    fn id(&self) -> &'static str {
        "info"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(InfoCall)
    }
}

/// A reliable provisional and its PRACK (RFC 3262) before the answer.
pub struct Prack;

impl SweepShape for Prack {
    fn id(&self) -> &'static str {
        "prack"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(ShapePlan {
            id: "prack",
            binder: default_binder(),
            establish: Establishment::Reliable,
            stages: vec![],
            teardown: Teardown::CallerBye { after: DwellKnob::TalkTime, feed: ByeFeed::NoFeed },
            ringing_gate: false,
            stamp_connected: false,
        })
    }
}

/// The callee's blind transfer: a REFER, its NOTIFYs, the transfer target
/// answering and the media realigned on both remaining legs.
pub struct Refer;

impl SweepShape for Refer {
    fn id(&self) -> &'static str {
        "refer"
    }

    fn recipe(&self) -> ClusterRecipe {
        transferring()
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(callshapes::shapes::refer(default_binder(), REFER_KEY))
    }
}

/// A BYE crossing an in-dialog request ([`ByeCrossing`]).
pub struct CrossingBye(pub Crossed);

impl SweepShape for CrossingBye {
    fn id(&self) -> &'static str {
        match self.0 {
            Crossed::Reinvite => "reinvite-x-bye",
            Crossed::Update => "update-x-bye",
            Crossed::Info => "info-x-bye",
            Crossed::Refer => "refer-x-bye",
        }
    }

    fn recipe(&self) -> ClusterRecipe {
        match self.0 {
            Crossed::Refer => transferring(),
            _ => ClusterRecipe::routing_to(addr(BOB)),
        }
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(ByeCrossing(self.0))
    }
}

/// A call held past one keepalive interval of the B2BUA: each leg answers the
/// B2BUA's own in-dialog OPTIONS.
pub struct Keepalive;

impl SweepShape for Keepalive {
    fn id(&self) -> &'static str {
        "keepalive"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(callshapes::shapes::long_call(default_binder()))
    }

    fn call_env(&self, env: &mut CallEnv<'_>) {
        // Past the 300 s keepalive interval the harness runs.
        env.long_hold = Duration::from_secs(330);
    }
}

/// A forked answer: three early dialogs ring under distinct tags, the middle
/// one answers (RFC 3261 §12.1.2).
pub struct Forked;

impl SweepShape for Forked {
    fn id(&self) -> &'static str {
        "forked"
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(callshapes::shapes::forked(default_binder()))
    }
}

/// A media-server leg: the announcement service plays a clip from a media
/// server as early media before it dials bob ([`MediaLeg`]).
pub struct MediaServer;

impl SweepShape for MediaServer {
    fn id(&self) -> &'static str {
        "media-leg"
    }

    fn recipe(&self) -> ClusterRecipe {
        let (bob, media) = (addr(BOB), addr(MEDIA));
        let mut recipe = ClusterRecipe::deciding(move |_| -> Arc<dyn CallDecisionEngine> {
            Arc::new(
                ScriptedDecisionEngine::builder()
                    .fallback(move |_| {
                        let mut r = route_to(&bob.ip().to_string(), bob.port());
                        r.service_ext.insert(
                            announcement::EXT_KEY.into(),
                            serde_json::json!({
                                "clip_id": "clip-1",
                                "mrf_host": media.ip().to_string(),
                                "mrf_port": media.port(),
                                "dest_host": bob.ip().to_string(),
                                "dest_port": bob.port(),
                                "defer_routing": true,
                            }),
                        );
                        limited(r)
                    })
                    .build(),
            )
        });
        recipe.services = || vec![announcement::service()];
        recipe
    }

    fn scenario(&self) -> Box<dyn ActorScenario> {
        Box::new(MediaLeg)
    }
}
