//! The scan the one-pass rewrite must reproduce byte for byte, written
//! naively: every key tried at every byte, the occurrences outside a
//! protocol counter kept longest first when nothing chosen overlaps them,
//! then the per-position display loop. Test-only, never called by the
//! crate.

/// Every `(start, end, fake, weak)` of every strong and weak key in
/// `input`, one probe per key per byte, minus those partially
/// overlapping a CC anchor behind a `+`.
fn occurrences<'a>(
    input: &[u8],
    strong: &'a [(Vec<u8>, Vec<u8>)],
    weak: &'a [(Vec<u8>, Vec<u8>)],
    anchors: &[Vec<u8>],
) -> Vec<(usize, usize, &'a [u8], bool)> {
    let mut found = Vec::new();
    for i in 0..input.len() {
        for (subs, is_weak) in [(strong, false), (weak, true)] {
            for (from, to) in subs {
                if input[i..].starts_with(from) {
                    found.push((i, i + from.len(), to.as_slice(), is_weak));
                }
            }
        }
    }
    let mut anchored: Vec<(usize, usize)> = Vec::new();
    for i in 1..input.len() {
        for cc in anchors {
            if input[i - 1] == b'+' && input[i..].starts_with(cc) {
                anchored.push((i, i + cc.len()));
            }
        }
    }
    found.retain(|&(start, end, _, _)| {
        !anchored.iter().any(|&(s, e)| s < end && start < e && !(start <= s && e <= end))
    });
    found
}

/// A weak occurrence partially overlapping a strong one is dropped;
/// then longest first, then leftmost; an occurrence overlapping a
/// chosen one is dropped.
fn choose(found: Vec<(usize, usize, &[u8], bool)>) -> Vec<(usize, usize, &[u8])> {
    let strong: Vec<(usize, usize)> = found.iter().filter(|f| !f.3).map(|f| (f.0, f.1)).collect();
    let mut found: Vec<(usize, usize, &[u8])> = found
        .into_iter()
        .filter(|&(start, end, _, weak)| {
            !weak || !strong.iter().any(|&(s, e)| s < end && start < e && !(start <= s && e <= end))
        })
        .map(|(s, e, to, _)| (s, e, to))
        .collect();
    found.sort_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)).then(a.0.cmp(&b.0)));
    let mut chosen: Vec<(usize, usize, &[u8])> = Vec::new();
    for (start, end, to) in found {
        if chosen.iter().all(|(s, e, _)| end <= *s || start >= *e) {
            chosen.push((start, end, to));
        }
    }
    chosen.sort_by_key(|(s, _, _)| *s);
    chosen
}

fn substitute(input: &[u8], chosen: &[(usize, usize, &[u8])], gap: Option<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut last = 0;
    for (start, end, to) in chosen {
        match gap {
            Some(zero) => out.resize(out.len() + start - last, zero),
            None => out.extend_from_slice(&input[last..*start]),
        }
        out.extend_from_slice(to);
        last = *end;
    }
    match gap {
        Some(zero) => out.resize(out.len() + input.len() - last, zero),
        None => out.extend_from_slice(&input[last..]),
    }
    out
}

pub fn substitute_digits(
    input: &[u8],
    strong: &[(Vec<u8>, Vec<u8>)],
    weak: &[(Vec<u8>, Vec<u8>)],
    anchors: &[Vec<u8>],
    keep: Option<(&[u8], &[u8])>,
) -> Vec<u8> {
    let found = occurrences(input, strong, weak, anchors)
        .into_iter()
        .filter(|(i, _, _, _)| !is_protocol_counter_at(input, *i, keep))
        .collect();
    substitute_chosen(input, found)
}

/// [`substitute_digits`] with no counter exempt: what the exemption keeps
/// from happening.
pub fn substitute_digits_unexempt(
    input: &[u8],
    strong: &[(Vec<u8>, Vec<u8>)],
    weak: &[(Vec<u8>, Vec<u8>)],
    anchors: &[Vec<u8>],
) -> Vec<u8> {
    substitute_chosen(input, occurrences(input, strong, weak, anchors))
}

fn substitute_chosen(input: &[u8], found: Vec<(usize, usize, &[u8], bool)>) -> Vec<u8> {
    // An identifier's digits are the identifier pass's, an escape's hex
    // digits a byte: dropped after choosing, by the scan's own classifiers.
    let chosen: Vec<_> = choose(found)
        .into_iter()
        .filter(|&(start, end, _)| {
            !crate::rewrite::identifiers::is_identifier(input, start, end)
                && !crate::rewrite::fields::in_escape(input, start)
                && !crate::rewrite::fields::in_escape(input, end - 1)
        })
        .collect();
    substitute(input, &chosen, None)
}

fn digit_run_at(input: &[u8], i: usize) -> (usize, usize) {
    let start = input[..i].iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |p| p + 1);
    let end = input[i..].iter().position(|b| !b.is_ascii_digit()).map_or(input.len(), |p| i + p);
    (start, end)
}

/// Whether the digit run around `i` is the stated CSeq value or a counter
/// by its field, judged by the scan's own field classifier: the reference
/// checks the scan's mechanics, not which fields count.
fn is_protocol_counter_at(input: &[u8], i: usize, cseq: Option<(&[u8], &[u8])>) -> bool {
    let (start, end) = digit_run_at(input, i);
    cseq.is_some_and(|cseq| is_cseq_value_at(input, i, cseq))
        || crate::rewrite::fields::is_counter(input, start, end)
}

fn is_cseq_value_at(input: &[u8], i: usize, (seq, method): (&[u8], &[u8])) -> bool {
    let (start, end) = digit_run_at(input, i);
    if &input[start..end] != seq {
        return false;
    }
    let mut j = end;
    while input.get(j).is_some_and(|b| *b == b' ' || *b == b'\t') {
        j += 1;
    }
    input[j..].len() >= method.len()
        && input[j..j + method.len()].eq_ignore_ascii_case(method)
        && !input.get(j + method.len()).is_some_and(|b| b.is_ascii_alphanumeric())
}

pub fn substitute_digits_zeroing(
    input: &[u8],
    strong: &[(Vec<u8>, Vec<u8>)],
    weak: &[(Vec<u8>, Vec<u8>)],
    anchors: &[Vec<u8>],
) -> Vec<u8> {
    substitute(input, &choose(occurrences(input, strong, weak, anchors)), Some(b'0'))
}

pub fn substitute_displays(input: &[u8], subs: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    'scan: while i < input.len() {
        for (from, to) in subs {
            let f = from.as_bytes();
            if input[i] == b'"'
                && input[i + 1..].starts_with(f)
                && input.get(i + 1 + f.len()) == Some(&b'"')
            {
                out.push(b'"');
                out.extend_from_slice(to.as_bytes());
                out.push(b'"');
                i += f.len() + 2;
                continue 'scan;
            }
            if input[i..].starts_with(f) {
                let mut j = i + f.len();
                while input.get(j).is_some_and(|b| *b == b' ' || *b == b'\t') {
                    j += 1;
                }
                if input.get(j) == Some(&b'<') {
                    out.extend_from_slice(to.as_bytes());
                    i += f.len();
                    continue 'scan;
                }
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}
