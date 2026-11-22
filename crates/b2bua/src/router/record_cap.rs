//! The record-size cap (ADR-0057): the record a turn lands, measured as its
//! write encodes it ([`crate::store::Measured`]), past
//! `B2BUA_CAP_RECORD_BYTES` trips the turn before the write, so a record past
//! the cap is never stored, replicated or followed by the turn's effects.
//! Only a write of a live call is held: a quiet turn writes nothing, and a
//! reaper verdict's turn and the turns of a call going away run with the cap
//! off, as with the collection caps. A held write's size is read against the
//! cap (`b2bua_cap_peak`), counted near past half of it
//! (`b2bua_cap_near_total`) and observed in `b2bua_call_record_bytes`.

use b2bua_sdk::event::CallEvent;
use call::Call;

use super::RouterCtx;
use crate::caps::Tripped;
use crate::config::Cap;
use crate::store::Measured;

/// `call`, the record a turn on `trigger` lands, held to the record-size cap
/// when the turn writes it as a live call (`writes_live`): `Err` past the
/// cap; the measured record when held, for the write to carry; `None` when
/// the cap is off for the turn.
pub(super) fn hold(
    ctx: &RouterCtx,
    call: &Call,
    writes_live: bool,
    trigger: Option<&CallEvent>,
) -> Result<Option<Measured>, Tripped> {
    if !writes_live || is_verdict(trigger) {
        return Ok(None);
    }
    let measured = ctx.state.measure(call);
    let (bytes, limit) = (measured.bytes(), ctx.config.caps.record_bytes);
    if bytes > limit {
        return Err(Tripped { cap: Cap::RecordBytes, verdict: false });
    }
    if bytes > limit / 2 {
        ctx.metrics.record_cap_near(Cap::RecordBytes);
    }
    ctx.metrics.record_cap_reading(Cap::RecordBytes, bytes, 0);
    ctx.metrics.observe_call_record_bytes(bytes);
    Ok(Some(measured))
}

/// Whether `trigger` is a reaper verdict.
fn is_verdict(trigger: Option<&CallEvent>) -> bool {
    matches!(trigger, Some(CallEvent::InternalEvent { topic, .. })
        if topic == crate::reaper_verdict::REAPER_TOPIC)
}
