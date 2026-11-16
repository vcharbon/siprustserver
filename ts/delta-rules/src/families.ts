/**
 * The rule families: rules that differ only by a parameter, written as one
 * row each (ADR-0062). A row names each of its rules' ids and words; the
 * family states the key and the predicate once. A row expands where it stands
 * in the deployment's rules, so every id keeps its place in the match order.
 */
import type { Probe } from "@sip/pipeline"
import { itemsOf } from "./header-items.js"
import { notGenerated, notRelayed } from "./provenance.js"
import type { Predicate, RuleSpec, RuleText } from "./rule.js"
import { onAnInvite, onKeepalive } from "./scope.js"
import { stray, substituted, type Substitution } from "./shapes.js"

/** The keys of the case facts `F` that are step sets. */
export type StepGate<F> = { [K in keyof F]: F[K] extends ReadonlySet<string> ? K : never }[keyof F] & string

/**
 * A header family the relay carries (`<family>-not-relayed`) and the system
 * may mint (`<family>-not-generated`): its captured-only loss, driven in by the
 * run where handed in, or minted where never handed. `request` holds both to
 * that method's requests.
 */
export const carried = <F>(
  family: ReadonlyArray<string>,
  rows: { readonly relayed?: RuleText<F>; readonly generated?: RuleText<F> },
  request?: "INVITE"
): ReadonlyArray<RuleSpec<F>> => {
  const on = request === undefined ? { kind: "header" as const, family } : { kind: "header" as const, family, scope: [request] }
  const held = (h: Probe.HeaderProbe): boolean => request === undefined || onAnInvite(h.scope)
  return [
    ...(rows.relayed === undefined ? [] : [{ ...rows.relayed, on, header: (h: Probe.HeaderProbe) => held(h) && notRelayed(h) }]),
    ...(rows.generated === undefined ? [] : [{ ...rows.generated, on, header: (h: Probe.HeaderProbe) => held(h) && notGenerated(h) }])
  ]
}

/** One rule of the keepalive family: its family of headers, any where absent, and what it holds beyond. */
export interface KeepaliveRow<F> extends RuleText<F> {
  readonly family?: ReadonlyArray<string>
  readonly also?: (h: Probe.HeaderProbe) => boolean
}

/**
 * A header on the in-dialog OPTIONS keepalive exchange, request or answer, an
 * exchange the answering stack terminates (RFC 3261 §11.2): one rule per row.
 */
export const keepalive = <F>(rows: ReadonlyArray<KeepaliveRow<F>>): ReadonlyArray<RuleSpec<F>> =>
  rows.map(({ family, also, ...text }) => ({
    ...text,
    rfc: "RFC 3261 §11.2",
    on: family === undefined ? { kind: "header", scope: ["OPTIONS"] } : { kind: "header", family, scope: ["OPTIONS"] },
    header: (h) => onKeepalive(h.scope) && (also === undefined || also(h))
  }))

/** A header the replay stack allocates whole on every message it sends: one rule per header name. */
export const stackAllocated = <F>(rows: Readonly<Record<string, RuleText<F>>>): ReadonlyArray<RuleSpec<F>> =>
  Object.entries(rows).map(([name, text]) => ({
    ...text,
    on: { kind: "header", family: [name] },
    header: (h) => itemsOf(h, "replayed").length > 0
  }))

/** A request the stack sends or answers on its own, a stray the flow never scripted: one rule per method. */
export const strays = <F>(rows: Readonly<Record<string, RuleText<F>>>): ReadonlyArray<RuleSpec<F>> =>
  Object.entries(rows).map(([method, text]) => ({
    ...text,
    on: { kind: "shape", family: ["serviced-stray", "extra-message"] },
    shape: (s) => stray(s, method)
  }))

/**
 * A status substitution named by its statuses alone, at the steps of the
 * rule's own `gate` where it states one.
 */
export const substitution = <F>(text: RuleText<F> & { readonly gate?: StepGate<F> }, row: Substitution): RuleSpec<F> => {
  const method = row.cseq ?? (row.at === "initial-invite" ? "INVITE" : undefined)
  const gated: Predicate<Probe.ShapeProbe, F> = text.gate === undefined
    ? (s) => substituted(s, row)
    : (s, facts) => substituted(s, row) && (facts[text.gate!] as ReadonlySet<string>).has(s.step)
  return {
    ...text,
    on: method === undefined
      ? { kind: "shape", family: ["status-substitution"] }
      : { kind: "shape", family: ["status-substitution"], scope: [method] },
    shape: gated
  }
}
