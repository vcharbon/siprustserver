//! A datagram cut into spans and the stretches between them, each stretch
//! rewritten by its pass and each span left as it is: the masked display
//! names the number pass leaves, the identifier words and SDP origin numbers
//! no name reaches, the protocol text nothing reaches.

use std::ops::Range;

/// `bytes` with `pass` applied to every stretch outside `spans` (sorted, not
/// overlapping), the spans as they are.
pub(super) fn outside(
    bytes: &[u8],
    spans: &[Range<usize>],
    pass: impl Fn(&[u8]) -> Vec<u8>,
) -> Vec<u8> {
    outside_tracked(bytes, spans, pass).0
}

/// [`outside`], with where each span stands in the output.
pub(super) fn outside_tracked(
    bytes: &[u8],
    spans: &[Range<usize>],
    pass: impl Fn(&[u8]) -> Vec<u8>,
) -> (Vec<u8>, Vec<Range<usize>>) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut moved = Vec::with_capacity(spans.len());
    let mut at = 0;
    for span in spans {
        out.extend(pass(&bytes[at..span.start]));
        moved.push(out.len()..out.len() + span.len());
        out.extend_from_slice(&bytes[span.clone()]);
        at = span.end;
    }
    out.extend(pass(&bytes[at..]));
    (out, moved)
}

/// `spans` sorted, overlapping ones merged, each with whether any span merged
/// into it was marked.
pub(super) fn merged(mut spans: Vec<(Range<usize>, bool)>) -> Vec<(Range<usize>, bool)> {
    spans.sort_by_key(|(span, _)| span.start);
    let mut out: Vec<(Range<usize>, bool)> = Vec::with_capacity(spans.len());
    for (span, marked) in spans {
        match out.last_mut() {
            Some((last, last_marked)) if span.start < last.end => {
                last.end = last.end.max(span.end);
                *last_marked |= marked;
            }
            _ => out.push((span, marked)),
        }
    }
    out
}
