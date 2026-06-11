//! State-machine documentation generation (ADR-0016 X5). Walks the composed
//! service registry and derives each **service machine's** transition graph
//! *entirely* from its rules' declared `active_states` / `transitions` / `effects`
//! / matchers — a diagram is only worth generating if it is fully generated, so
//! nothing here is hand-authored. (The always-on `global-call` machine is a
//! projection of `CallModelState` with no declarative transition data, so it has
//! no generated diagram — only the real runtime cursor in `sm_cursors` / the
//! `b2bua_sm_cursors` gauge.) Both the `xtask state-machine-docs` subcommand and
//! the CI freshness test call [`render_registry`], so the committed diagrams
//! cannot silently drift.

use std::collections::BTreeSet;

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
/// message / timer / internal event that triggers the rule, followed by its
/// **guard** conditions (the leg/call state the rule peeks at, and `[guard]` when
/// a corner-case `filter` predicate gates it — opaque, so shown only as a flag).
fn summarize_matcher(m: &Match) -> String {
    let dir = match m.direction {
        Some(call::Direction::FromA) => " (A)",
        Some(call::Direction::FromB) => " (B)",
        None => "",
    };
    let methods = m.methods.as_ref().map(|v| v.join("/"));
    let base = match m.kind {
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
    };
    fn dbg_list<T: std::fmt::Debug>(v: &[T]) -> String {
        v.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join("/")
    }
    let mut guards = String::new();
    if let Some(ls) = &m.leg_state {
        guards.push_str(&format!(" [leg {}]", dbg_list(ls)));
    }
    if let Some(cs) = &m.call_state {
        guards.push_str(&format!(" [call {}]", dbg_list(cs)));
    }
    if let Some(ds) = &m.leg_disposition {
        guards.push_str(&format!(" [disp {}]", dbg_list(ds)));
    }
    if m.filter.is_some() {
        guards.push_str(" [guard]");
    }
    format!("{base}{guards}")
}

/// Summarise a rule's declared effects as the **output** half of an edge label —
/// the tracked side effects (each effect's free label), joined.
fn summarize_effects(effects: &[Effect]) -> String {
    effects.iter().map(Effect::label).collect::<Vec<_>>().join(" · ")
}

/// Neutralise characters that corrupt a Mermaid `stateDiagram-v2` transition
/// label: `;` is a statement separator (a SIP `Subscription-State:
/// terminated;reason=timeout` would split the diagram), and a raw newline ends
/// the statement. Applied at render time so no effect/matcher label — present or
/// future — can break the generated diagram.
fn sanitize_label(s: &str) -> String {
    s.replace(['\n', '\r'], " ").replace(';', ",")
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

/// Prepend `[*] --> entry` start edges (ADR-0016 X8 activation): a real state
/// with no **cross-state** inbound edge is an entry point — the cursor the
/// service's `init` (or its seed rules) writes when the machine activates. Self-
/// loops don't count as inbound, so an entry that also handles in-state events is
/// still detected.
fn with_start_edges(states: &[String], mut edges: Vec<Edge>) -> Vec<Edge> {
    let incoming: BTreeSet<&str> = edges
        .iter()
        .filter(|e| e.from != e.to)
        .map(|e| e.to.as_str())
        .collect();
    let terminal = call::StateLabel::terminal();
    let entries: BTreeSet<&str> = states
        .iter()
        .map(String::as_str)
        .filter(|s| *s != terminal.as_str() && !incoming.contains(s))
        .collect();
    for s in entries {
        edges.push(Edge { from: terminal.as_str().into(), to: s.into(), label: String::new() });
    }
    edges
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
    let edges = with_start_edges(&states, edges);
    graph_from(def.id, states, edges)
}

/// Render a machine graph as a committed-diagram markdown document.
pub fn render_mermaid(g: &MachineGraph) -> String {
    format!(
        "# State machine: `{}`\n\n\
         <!-- GENERATED by `cargo run -p xtask -- state-machine-docs` from the composed\n     \
         service registry (ADR-0016). Do not edit by hand; regenerate instead. -->\n\n\
         ```mermaid\n{}```\n",
        g.id,
        diagram_body(g),
    )
}

/// The raw Mermaid `stateDiagram-v2` source for a machine (no markdown wrapper) —
/// shared by the committed `.md` ([`render_mermaid`]) and the rendered HTML
/// ([`render_registry_html`]).
fn diagram_body(g: &MachineGraph) -> String {
    let mut out = String::from("stateDiagram-v2\n");
    for s in &g.states {
        out.push_str(&format!("    {s}\n"));
    }
    for e in &g.edges {
        if e.label.is_empty() {
            out.push_str(&format!("    {} --> {}\n", e.from, e.to));
        } else {
            out.push_str(&format!("    {} --> {} : {}\n", e.from, e.to, sanitize_label(&e.label)));
        }
    }
    out
}

/// Every fully-generated service machine's graph, in registration order. (The
/// `global-call` projection has no declarative graph and is intentionally absent.)
fn registry_graphs(services: &[ServiceDef]) -> Vec<MachineGraph> {
    services.iter().map(service_graph).collect()
}

/// Render the whole registry: one document per registered service machine,
/// derived entirely from its rules. Returns `(machine_id, markdown)` pairs — the
/// doc file is `docs/sm/<machine_id>.md`.
pub fn render_registry(services: &[ServiceDef]) -> Vec<(String, String)> {
    registry_graphs(services)
        .into_iter()
        .map(|g| (g.id.clone(), render_mermaid(&g)))
        .collect()
}

/// Render the whole registry as a single **self-contained HTML page** that draws
/// every machine as an SVG in the browser (ADR-0016 X5). Each diagram is a
/// `<pre class="mermaid">` block; a pinned Mermaid ESM module renders them on
/// load — so there is no build-time browser dependency, the SVG is produced when
/// the file is opened. Written to `docs/sm/index.html` by `xtask
/// state-machine-docs` and guarded by the same freshness test as the `.md`s.
pub fn render_registry_html(services: &[ServiceDef]) -> String {
    let mut sections = String::new();
    for g in registry_graphs(services) {
        sections.push_str(&format!(
            "  <section>\n    <h2><code>{}</code></h2>\n    <pre class=\"mermaid\">\n{}    </pre>\n  </section>\n",
            g.id,
            diagram_body(&g),
        ));
    }
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>Callflow state machines (ADR-0016)</title>\n\
         <!-- GENERATED by `cargo run -p xtask -- state-machine-docs`. Do not edit by hand. -->\n\
         <style>\n\
         body{{font-family:system-ui,sans-serif;max-width:1200px;margin:2rem auto;padding:0 1rem;color:#222}}\n\
         h1{{margin-bottom:.25rem}}\n\
         section{{margin:2.5rem 0}}\n\
         h2{{border-bottom:1px solid #ddd;padding-bottom:.3rem}}\n\
         .mermaid{{overflow-x:auto;border:1px solid #eee;padding:.5rem}}\n\
         .mermaid svg{{height:auto}}\n\
         p.sub{{color:#666;margin-top:0}}\n\
         </style>\n</head>\n<body>\n\
         <h1>Callflow state machines</h1>\n\
         <p class=\"sub\">ADR-0016 \u{2014} generated from the composed service registry. \
         Edge labels read <em>input message [guards] \u{21d2} output side effects</em>; \
         <code>[*]</code> is machine activation / deactivation.</p>\n\
         {sections}\
         <script type=\"module\">\n\
         import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.esm.min.mjs';\n\
         // useMaxWidth:false renders each diagram at its natural size (the wide\n\
         // transfer machine then scrolls in its box instead of being shrunk to fit).\n\
         mermaid.initialize({{ startOnLoad: true, theme: 'neutral', state: {{ useMaxWidth: false }} }});\n\
         </script>\n</body>\n</html>\n",
    )
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
    fn stub_init(_: &crate::rules::RuleCall) -> Option<ServiceSeed> {
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
    fn label_semicolon_is_neutralised() {
        // `;` is a Mermaid stateDiagram-v2 statement separator: a raw one in a
        // label (e.g. SIP `Subscription-State: terminated;reason=timeout`) splits
        // the diagram and trips a parse error. It must render as `,`, not `;`.
        assert_eq!(sanitize_label("terminated;timeout"), "terminated,timeout");
        assert_eq!(sanitize_label("a\nb\rc"), "a b c");
    }

    #[test]
    fn stub_service_diagram_is_derived_from_rules() {
        let g = service_graph(&stub_def());
        assert_eq!(g.id, "stub");
        assert_eq!(g.states, vec!["S0", "S1"]);
        // The move edge is labelled with the matcher summary (no effects declared);
        // S0 has no cross-state inbound edge, so it gets a `[*] --> S0` start edge.
        assert_eq!(
            g.edges,
            vec![
                Edge { from: "S0".into(), to: "S1".into(), label: "INFO".into() },
                Edge { from: "[*]".into(), to: "S0".into(), label: String::new() },
            ]
        );
        let md = render_mermaid(&g);
        assert_well_formed_mermaid(&md, "stub");
        assert!(md.contains("S0 --> S1 : INFO"));
        assert!(md.contains("[*] --> S0"));
    }

    #[test]
    fn render_registry_is_service_machines_only() {
        // Only fully-generated service machines — no fabricated `global-call`.
        let rendered = render_registry(&[stub_def()]);
        let ids: Vec<&str> = rendered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["stub"]);
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
