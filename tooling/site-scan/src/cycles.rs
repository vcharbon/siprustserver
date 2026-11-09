//! The recursion sites of a call graph (ADR-0057 §5): a cycle, direct or
//! mutual, through no function that takes a depth guard. Each strongly
//! connected component of the graph without the guarded functions that
//! holds a cycle is one site, on its first function by file and line.

use std::collections::BTreeMap;

use crate::graph::Node;
use crate::scope::FnKey;
use crate::site::{Set, Site};

/// The unguarded cycles of `nodes`.
pub fn sites(nodes: &BTreeMap<FnKey, Node>) -> Vec<Site> {
    let unguarded: BTreeMap<&FnKey, &Node> = nodes.iter().filter(|(_, n)| !n.guarded).collect();
    components(&unguarded)
        .into_iter()
        .filter(|c| match c.as_slice() {
            [one] => unguarded.get(one).is_some_and(|n| n.calls.contains(*one)),
            _ => true,
        })
        .filter_map(|c| {
            let mut names: Vec<String> = c.iter().map(|k| name(k)).collect();
            names.sort();
            let first = c
                .iter()
                .filter_map(|k| unguarded.get(k))
                .min_by_key(|n| (n.file.clone(), n.span.start().line, n.span.start().column))?;
            let message = format!("recursion with no depth guard: {}", names.join(" -> "));
            Some(Site::at(&first.file, first.span, Set::Recursion, message))
        })
        .collect()
}

/// The function's path from the crate root.
fn name(key: &FnKey) -> String {
    let (mut parts, last) = match key {
        FnKey::Free(m, f) => (m.clone(), f.clone()),
        FnKey::Method((m, t), via, f) => {
            let mut p = m.clone();
            p.push(match via {
                Some((_, tr)) => format!("<{t} as {tr}>"),
                None => t.clone(),
            });
            (p, f.clone())
        }
    };
    parts.push(last);
    parts.join("::")
}

/// The strongly connected components of `nodes` (Tarjan's, iterative), an
/// edge to a function outside `nodes` left out.
fn components<'a>(nodes: &BTreeMap<&'a FnKey, &Node>) -> Vec<Vec<&'a FnKey>> {
    let keys: Vec<&FnKey> = nodes.keys().copied().collect();
    let index_of: BTreeMap<&FnKey, usize> = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let edges: Vec<Vec<usize>> = keys
        .iter()
        .map(|k| {
            nodes
                .get(*k)
                .map(|n| n.calls.iter().filter_map(|c| index_of.get(c).copied()).collect())
                .unwrap_or_default()
        })
        .collect();
    let n = keys.len();
    let (mut index, mut low, mut on_stack) = (vec![usize::MAX; n], vec![0; n], vec![false; n]);
    let (mut stack, mut out, mut next) = (Vec::new(), Vec::new(), 0);
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut frames = vec![(start, 0usize)];
        index[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;
        while let Some(&mut (v, ref mut at)) = frames.last_mut() {
            if let Some(&w) = edges[v].get(*at) {
                *at += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut comp = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    comp.push(keys[w]);
                    if w == v {
                        break;
                    }
                }
                out.push(comp);
            }
        }
    }
    out
}
