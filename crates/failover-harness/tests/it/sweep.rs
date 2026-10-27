//! The failover sweep lane (ADR-0055 §11): the base catalogue cut at every
//! turn point by every incident at both sync levels, judged by the window
//! table at each cell's measured tier and held to `failover-sweep.ratchet`.
//! Run by `just sweep`; the default lane leaves it out.

use failover_harness::sweep::catalogue::*;

failover_harness::failover_sweep! {
    ratchet: concat!(env!("CARGO_MANIFEST_DIR"), "/failover-sweep.ratchet");
    basic => Basic,
    reinvite => Reinvite,
    update => Update,
    info => Info,
    prack => Prack,
    refer => Refer,
    reinvite_x_bye => CrossingBye(Crossed::Reinvite),
    update_x_bye => CrossingBye(Crossed::Update),
    info_x_bye => CrossingBye(Crossed::Info),
    refer_x_bye => CrossingBye(Crossed::Refer),
    keepalive => Keepalive,
    forked => Forked,
    media_leg => MediaServer,
}
