//! The recursion sites of a call graph (ADR-0057 §5): each strongly
//! connected component holding a cycle, direct or mutual, none of whose
//! functions takes a depth guard. A site sits on its first function by file
//! and line.

use std::collections::BTreeMap;

use crate::graph::Node;
use crate::scope::FnKey;
use crate::site::{Set, Site};

/// The unguarded cycles of `nodes`.
pub fn sites(nodes: &BTreeMap<FnKey, Node>) -> Vec<Site> {
    components(nodes)
        .into_iter()
        .filter(|c| match c.as_slice() {
            [one] => nodes.get(one).is_some_and(|n| n.calls.contains(one)),
            _ => true,
        })
        .filter(|c| !c.iter().any(|k| nodes.get(k).is_some_and(|n| n.guarded)))
        .filter_map(|c| {
            let mut names: Vec<String> = c.iter().map(name).collect();
            names.sort();
            let first = c
                .iter()
                .filter_map(|k| nodes.get(k))
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
                Some(tr) => format!("<{t} as {tr}>"),
                None => t.clone(),
            });
            (p, f.clone())
        }
    };
    parts.push(last);
    parts.join("::")
}

/// The strongly connected components of `nodes` (Tarjan's, iterative).
fn components(nodes: &BTreeMap<FnKey, Node>) -> Vec<Vec<FnKey>> {
    let keys: Vec<&FnKey> = nodes.keys().collect();
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
                    comp.push(keys[w].clone());
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
