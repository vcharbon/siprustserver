//! Where two copies of one message first disagree, the rows and stamps their
//! emitter re-renders per copy left out of the comparison.

use std::ops::Range;

use sip_message::sniff;

use crate::wire::endpoint_addr;

/// Where two copies' bytes first disagree, the emitter's own `Record-Route`
/// rows left out of the comparison.
pub(super) struct Divergence {
    /// `"head"` or `"body"`.
    pub(super) region: &'static str,
    /// Byte offset into that region of the FIRST copy.
    pub(super) offset: usize,
    /// The line spanning the disagreement in each copy; `None` where that copy
    /// ends before it.
    pub(super) first_line: Option<String>,
    pub(super) rung_line: Option<String>,
    pub(super) first_len: usize,
    pub(super) rung_len: usize,
}

impl Divergence {
    /// The first disagreement between two copies, or `None` where they are one
    /// datagram byte for byte outside the rows `emitter` owns per forward.
    ///
    /// A vantage may store the whole datagram as the head (the live recording
    /// does) or split the body off it (a capture with a binary part does); a
    /// disagreement past a blank line both copies carry is located in the body
    /// either way, so the two vantages report one region.
    pub(super) fn between(
        first_head: &[u8],
        first_body: Option<&[u8]>,
        copy_head: &[u8],
        copy_body: Option<&[u8]>,
        emitter: &str,
    ) -> Option<Divergence> {
        let first_mask = own_record_route_spans(first_head, emitter);
        let copy_mask = own_record_route_spans(copy_head, emitter);
        let stamps = |raw: &[u8]| {
            if sniff::is_response(raw) {
                Vec::new()
            } else {
                sniff::relay_stamp_spans(raw)
            }
        };
        let free = (stamps(first_head), stamps(copy_head));
        if let Some((fi, ci)) =
            first_difference(first_head, &first_mask, copy_head, &copy_mask, free)
        {
            let body_start = |raw: &[u8]| sniff::body(raw).map(|b| raw.len() - b.len());
            return Some(match (body_start(first_head), body_start(copy_head)) {
                (Some(fs), Some(cs)) if fi >= fs && ci >= cs => Divergence::in_region(
                    "body",
                    fi - fs,
                    ci - cs,
                    &first_head[fs..],
                    &copy_head[cs..],
                ),
                _ => Divergence::in_region("head", fi, ci, first_head, copy_head),
            });
        }
        let (first_body, copy_body) = (first_body?, copy_body?);
        let (fi, ci) = first_difference(first_body, &[], copy_body, &[], (Vec::new(), Vec::new()))?;
        Some(Divergence::in_region("body", fi, ci, first_body, copy_body))
    }

    fn in_region(
        region: &'static str,
        first_offset: usize,
        rung_offset: usize,
        first: &[u8],
        copy: &[u8],
    ) -> Divergence {
        Divergence {
            region,
            offset: first_offset,
            first_line: sniff::line_at(first, first_offset),
            rung_line: sniff::line_at(copy, rung_offset),
            first_len: first.len(),
            rung_len: copy.len(),
        }
    }
}

/// The spans of `raw`'s `Record-Route` rows every hop of which names `emitter`
/// — the emitter's own §16.6 stamp, the one row class a rung may re-render. A
/// row no reader accepts, or one that folds a foreign hop beside the emitter's
/// own, keeps its bytes in the comparison. Empty where `emitter` is no socket
/// address: nothing then names it.
fn own_record_route_spans(raw: &[u8], emitter: &str) -> Vec<Range<usize>> {
    let Some(addr) = endpoint_addr(emitter) else { return Vec::new() };
    let names_emitter = |hop: &sniff::UriFacts| {
        hop.port == addr.port()
            && sip_message::ip_literal::ip_host(&hop.host).is_some_and(|ip| ip == addr.ip())
    };
    sniff::route_rows(raw, "Record-Route")
        .into_iter()
        .filter(|row| {
            row.hops.as_ref().is_some_and(|hops| !hops.is_empty() && hops.iter().all(names_emitter))
        })
        .map(|row| row.span)
        .collect()
}

/// The first byte at which `a` and `b` disagree once the spans `a_mask` /
/// `b_mask` (ascending, disjoint) are left out of each — as an offset into
/// each ORIGINAL, the shorter's end where what remains of one is a strict
/// prefix of the other — or `None` where the remainders are equal. The spans
/// pair in lockstep: a masked row one copy carries where the other carries
/// none is a disagreement at that row, so a rung that drops, adds or moves
/// the emitter's own row is charged like any other re-render. `free` holds
/// spans per side, outside both masks, each skipped on its own side alone.
fn first_difference(
    a: &[u8],
    a_mask: &[Range<usize>],
    b: &[u8],
    b_mask: &[Range<usize>],
    free: (Vec<Range<usize>>, Vec<Range<usize>>),
) -> Option<(usize, usize)> {
    let (mut i, mut j) = (0, 0);
    let (mut am, mut bm) = (a_mask.iter().peekable(), b_mask.iter().peekable());
    let (a_free, b_free) = free;
    loop {
        if let Some(r) = a_free.iter().find(|r| r.start == i) {
            i = r.end;
            continue;
        }
        if let Some(r) = b_free.iter().find(|r| r.start == j) {
            j = r.end;
            continue;
        }
        let a_masked = am.peek().is_some_and(|r| r.start <= i);
        let b_masked = bm.peek().is_some_and(|r| r.start <= j);
        match (a_masked, b_masked) {
            (true, true) => {
                i = i.max(am.next().unwrap().end);
                j = j.max(bm.next().unwrap().end);
                continue;
            }
            (false, false) => {}
            _ => return Some((i, j)),
        }
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
            }
            (None, None) => return None,
            _ => return Some((i, j)),
        }
    }
}
