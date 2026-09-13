//! RFC 3261 §17 transaction timer constants: they live beside the schedule
//! they pace (ADR-0032) and are re-exported here, so every consumer keeps its
//! path.

pub use sip_retransmit::timers::*;
