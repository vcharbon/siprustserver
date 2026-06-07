//! State-machine documentation generation (ADR-0016 X5). Walks the composed
//! service registry, derives each machine's transition graph from its rules'
//! declared `active_states` / `transitions` (plus the framework `global-call`
//! machine), and renders one Mermaid `stateDiagram-v2` per machine. Both the
//! `xtask state-machine-docs` subcommand and the CI freshness test call
//! [`render_registry`], so the committed diagrams cannot silently drift.

use std::collections::BTreeSet;

use super::invariants::GLOBAL_CALL_MACHINE;
use super::model::{Effect, Match, MatchKind, RuleDefinition, StatusMatch};
use super::service::ServiceDef;

/// A labelled transition edge (ADR-0016 X9). The `label` reads
/// `<input message> ⇒ <output side effects>` — the matcher summary that *triggers*
/// the rule, then the rule's declared tracked effects (leg messages / lifecycle
/// commands / guard timers). Empty for the framework `global-call` lifecycle edges.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub label: String,
}

/// A machine's transition graph: a deterministic (sorted, deduped) set of state
/// labels and labelled edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineGraph {
    pub id: String,
    pub states: Vec<String>,
    pub edges: Vec<Edge>,
}

fn graph_from(
    id: &str,
    states: impl IntoIterator<Item = String>,
    edges: Vec<Edge>,
) -> MachineGraph {
    let mut state_set: BTreeSet<String> = states.into_iter().collect();
    for e in &edges {
        state_set.insert(e.from.clone());
        state_set.insert(e.to.clone());
    }
    // The terminal sentinel `[*]` (ADR-0016 X9 deactivation) is Mermaid's built-in
    // sink — it appears only as an edge target (`S --> [*]`), never as a declared
    // state node.
    state_set.remove(call::StateLabel::terminal().as_str());
    let edge_set: BTreeSet<Edge> = edges.into_iter().collect();
    MachineGraph {
        id: id.to_string(),
        states: state_set.into_iter().collect(),
        edges: edge_set.into_iter().collect(),
    }
}

/// Summarise a rule's matcher as the **input** half of an edge label — the SIP
/// message / timer / internal event that triggers the rule.
fn summarize_matcher(m: &Match) -> String {
    let dir = match m.direction {
        Some(call::Direction::FromA) => " (A)",
        Some(call::Direction::FromB) => " (B)",
        None => "",
    };
    let methods = m.methods.as_ref().map(|v| v.join("/"));
    match m.kind {
        MatchKind::Request => format!("{}{dir}", methods.unwrap_or_else(|| "request".into())),
        MatchKind::Response => {
            let status = match m.status {
                StatusMatch::Any => String::new(),
                StatusMatch::Code(c) => format!(" {c}"),
                StatusMatch::Class(c) => format!(" {c}xx"),
            };
            format!("{}{status}{dir}", methods.unwrap_or_else(|| "response".into()))
        }
        MatchKind::Timer => {
            let ts = m
                .timer_types
                .as_ref()
                .map(|v| v.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join("/"))
                .unwrap_or_default();
            format!("timer {ts}")
        }
        MatchKind::Timeout => format!("timeout {}", methods.unwrap_or_default()),
        MatchKind::Cancelled => "CANCEL".into(),
        MatchKind::InternalEvent => match (m.topic, m.outcome) {
            (Some(t), Some(o)) => format!("{t}/{o}"),
            (Some(t), None) => t.to_string(),
            _ => "event".into(),
        },
    }
}

/// Summarise a rule's declared effects as the **output** half of an edge label —
/// the tracked side effects (each effect's free label), joined.
fn summarize_effects(effects: &[Effect]) -> String {
    effects.iter().map(Effect::label).collect::<Vec<_>>().join(" · ")
}

/// The full `<input> ⇒ <output>` edge label for a rule.
fn edge_label(r: &RuleDefinition) -> String {
    let input = summarize_matcher(&r.matcher);
    let output = summarize_effects(r.effects);
    if output.is_empty() {
        input
    } else {
        format!("{input} ⇒ {output}")
    }
}

/// The framework global call machine — projected from `CallModelState`
/// (ADR-0016 X2), so its edges are the lifecycle, not rule-declared (unlabelled).
pub fn global_call_graph() -> MachineGraph {
    let edges = [
        ("Active", "Terminating"),
        ("Active", "Terminated"),
        ("Terminating", "Terminated"),
    ]
    .into_iter()
    .map(|(a, b)| Edge { from: a.into(), to: b.into(), label: String::new() })
    .collect();
    graph_from(
        GLOBAL_CALL_MACHINE.as_str(),
        ["Active", "Terminating", "Terminated"].into_iter().map(str::to_string),
        edges,
    )
}

/// Derive a service machine's graph from its rules. A rule with a transition
/// yields a labelled edge per `(from, to)`; a rule with **no** transition is an
/// in-state handler (a guard / response / timeout that fires without moving the
/// cursor) and yields a labelled **self-loop** on each of its active states — so
/// the diagram shows the looping in-state behaviour, not just the moves.
pub fn service_graph(def: &ServiceDef) -> MachineGraph {
    let rules = (def.rules)();
    let mut states = Vec::new();
    let mut edges = Vec::new();
    for r in &rules {
        if r.machine.as_ref().map(|m| m.as_str()) != Some(def.id) {
            continue; // not this machine's rule (defensive; checked elsewhere).
        }
        for s in r.active_states {
            states.push(s.as_str().to_string());
        }
        let label = edge_label(r);
        if r.transitions.is_empty() {
            for s in r.active_states {
                edges.push(Edge { from: s.as_str().into(), to: s.as_str().into(), label: label.clone() });
            }
        } else {
            for (from, to) in r.transitions {
                states.push(from.as_str().to_string());
                states.push(to.as_str().to_string());
                edges.push(Edge { from: from.as_str().into(), to: to.as_str().into(), label: label.clone() });
            }
        }
    }
    graph_from(def.id, states, edges)
}

/// Render a machine graph as a committed-diagram markdown document.
pub fn render_mermaid(g: &MachineGraph) -> String {
    let mut out = String::new();
    out.push_str(&format!("# State machine: `{}`\n\n", g.id));
    out.push_str(
        "<!-- GENERATED by `cargo run -p xtask -- state-machine-docs` from the composed\n     \
         service registry (ADR-0016). Do not edit by hand; regenerate instead. -->\n\n",
    );
    out.push_str("```mermaid\nstateDiagram-v2\n");
    for s in &g.states {
        out.push_str(&format!("    {s}\n"));
    }
    for e in &g.edges {
        if e.label.is_empty() {
            out.push_str(&format!("    {} --> {}\n", e.from, e.to));
        } else {
            out.push_str(&format!("    {} --> {} : {}\n", e.from, e.to, e.label));
        }
    }
    out.push_str("```\n");
    out
}

/// Render the whole registry: the framework `global-call` machine first, then
/// one document per registered service. Returns `(machine_id, markdown)` pairs —
/// the doc file is `docs/sm/<machine_id>.md`.
pub fn render_registry(services: &[ServiceDef]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let g = global_call_graph();
    out.push((g.id.clone(), render_mermaid(&g)));
    for def in services {
        let g = service_graph(def);
        out.push((g.id.clone(), render_mermaid(&g)));
    }
    out
}

/// Static validation (ADR-0016 X5): every rule a service contributes must belong
/// to that service's own machine. (State-label membership in the machine's enum
/// is already a compile-time guarantee — `sm_rule!` references enum variants.)
/// Returns the list of violations, empty when the registry is well-formed.
pub fn check_registry(services: &[ServiceDef]) -> Vec<String> {
    let mut errs = Vec::new();
    for def in services {
        for r in (def.rules)() {
            match r.machine.as_ref().map(|m| m.as_str()) {
                Some(m) if m == def.id => {}
                Some(m) => errs.push(format!(
                    "service '{}' contributes rule '{}' bound to a foreign machine '{}'",
                    def.id, r.id, m
                )),
                None => errs.push(format!(
                    "service '{}' contributes machine-less rule '{}' (use a core rule instead)",
                    def.id, r.id
                )),
            }
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::model::{Match, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
    use crate::rules::ServiceSeed;
    use call::{MachineId, StateLabel};

    static STUB_ACTIVE: [StateLabel; 1] = [StateLabel::new("S0")];
    static STUB_TRANS: [(StateLabel, StateLabel); 1] =
        [(StateLabel::new("S0"), StateLabel::new("S1"))];

    fn stub_handle(_: &crate::rules::RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![]))
    }
    fn stub_rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "stub-advance",
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::request().method("INFO"),
            handle: stub_handle,
            machine: Some(MachineId::new("stub")),
            active_states: &STUB_ACTIVE,
            transitions: &STUB_TRANS,
            effects: &[],
        }]
    }
    fn stub_init(_: &call::Call) -> Option<ServiceSeed> {
        None
    }
    fn stub_def() -> ServiceDef {
        ServiceDef { id: "stub", init: stub_init, rules: stub_rules }
    }

    fn assert_well_formed_mermaid(md: &str, machine: &str) {
        assert!(md.starts_with(&format!("# State machine: `{machine}`")), "title");
        assert!(md.contains("```mermaid\nstateDiagram-v2\n"), "mermaid fence + diagram type");
        assert!(md.trim_end().ends_with("```"), "closing fence");
    }

    #[test]
    fn global_call_diagram_is_non_empty_and_well_formed() {
        let g = global_call_graph();
        assert_eq!(g.id, "global-call");
        assert_eq!(g.states, vec!["Active", "Terminated", "Terminating"]);
        assert!(g.edges.contains(&Edge {
            from: "Active".into(),
            to: "Terminating".into(),
            label: String::new(),
        }));
        let md = render_mermaid(&g);
        assert_well_formed_mermaid(&md, "global-call");
        // Lifecycle edges are unlabelled (no trailing " : ...").
        assert!(md.contains("Active --> Terminating\n"));
    }

    #[test]
    fn stub_service_diagram_is_derived_from_rules() {
        let g = service_graph(&stub_def());
        assert_eq!(g.id, "stub");
        assert_eq!(g.states, vec!["S0", "S1"]);
        // The edge is labelled with the matcher summary (no effects declared).
        assert_eq!(
            g.edges,
            vec![Edge { from: "S0".into(), to: "S1".into(), label: "INFO".into() }]
        );
        let md = render_mermaid(&g);
        assert_well_formed_mermaid(&md, "stub");
        assert!(md.contains("S0 --> S1 : INFO"));
    }

    #[test]
    fn render_registry_prepends_global_call() {
        let rendered = render_registry(&[stub_def()]);
        let ids: Vec<&str> = rendered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["global-call", "stub"]);
    }

    #[test]
    fn check_registry_flags_foreign_machine() {
        // A service whose rules belong to a different machine is a violation.
        fn foreign_rules() -> Vec<RuleDefinition> {
            stub_rules() // machine == "stub"
        }
        let bad = ServiceDef { id: "other", init: stub_init, rules: foreign_rules };
        let errs = check_registry(&[bad]);
        assert_eq!(errs.len(), 1, "foreign-machine rule flagged");
        // A self-consistent service passes.
        assert!(check_registry(&[stub_def()]).is_empty());
    }
}
