/**
 * The differential mutants of one probe: deterministic one-step variations of a
 * probe and its case facts along every axis a rule reads, so a reordered or
 * re-cut set of rules that keeps the base ruling but moves a boundary is caught
 * on the probe next to it. The overlap graph counts their match sets as the
 * `fixture-mutants` and `archive-mutants` sources (`./overlap.ts`).
 *
 * The axes, each mutant changing one thing:
 *
 * - `leg=<role>` / `leg=none`: the leg's place in the call;
 * - `scope=<scope>`: the message scope, one of method, status, CSeq method or
 *   in-dialog moved at a time (a shape probe also loses or gains a scope);
 * - `inbound=`, `driven=`, `bodiless=`, `carries-body=`: a header probe's flags;
 * - `inboundValues=`, `inAnsweredRequest=`, `inReplayedParts=`: item provenance,
 *   set to none, the captured items or the replayed items;
 * - `gate:<name>`: one boolean case fact flipped, or the probe's step put in or
 *   taken out of a step-keyed fact; `<name>:leg` the same for the step's leg in
 *   a {@link RunFacts} fact;
 * - `captured+`, `captured-`, `replayed+`, `replayed-`: one item added or removed.
 *
 * A mutant identical to the probe or to an earlier mutant is dropped. Every
 * mutant carries the facts it differs by, so a digest of the probe and that
 * delta names it without hashing a run's facts.
 */
import type { Probe } from "@sip/pipeline"
import { LegRole } from "@sip/pipeline"
import { canonicalText, digestOf } from "./canonical.js"
import type { RunFacts } from "./run-facts.js"

/** One mutant: what changed, the mutated probe and case facts, and the facts it changed. */
export interface Mutant<F> {
  readonly label: string
  readonly probe: Probe.Probe
  readonly facts: F
  readonly delta: Readonly<Record<string, unknown>>
}

const ROLES: ReadonlyArray<LegRole.LegRole> = [
  "caller",
  "dialled",
  "reroute",
  "fork",
  "transferee-refer",
  "transferee-info",
  "resource",
  "other"
]

const METHODS = ["INVITE", "ACK", "BYE", "CANCEL", "PRACK", "UPDATE", "INFO", "REFER", "NOTIFY", "OPTIONS"]
const STATUSES = [100, 180, 183, 200, 202, 481, 486, 487, 500]
const CSEQ_METHODS = ["INVITE", "BYE", "CANCEL", "PRACK", "UPDATE", "REFER", "NOTIFY"]
const MUTANT_ITEM = "mutant-item"

/** The scopes one step away from `scope`. */
const scopesNear = (scope: Probe.MsgScope | undefined): ReadonlyArray<Probe.MsgScope | undefined> => {
  if (scope === undefined) {
    return [{ kind: "response", status: 200, cseqMethod: "INVITE" }, { kind: "request", method: "BYE", inDialog: true }]
  }
  switch (scope.kind) {
    case "initial-invite":
      return [
        { kind: "request", method: "INVITE", inDialog: true },
        { kind: "request", method: "INVITE", inDialog: false },
        { kind: "response", status: 183, cseqMethod: "INVITE" },
        { kind: "response", status: 200, cseqMethod: "INVITE" }
      ]
    case "request":
      return [
        { ...scope, inDialog: !scope.inDialog },
        ...METHODS.filter((m) => m !== scope.method).map((method) => ({ ...scope, method })),
        { kind: "initial-invite" },
        { kind: "response", status: 200, cseqMethod: scope.method }
      ]
    case "response":
      return [
        ...STATUSES.filter((s) => s !== scope.status).map((status) => ({ ...scope, status })),
        ...CSEQ_METHODS.filter((m) => m !== scope.cseqMethod).map((cseqMethod) => ({ ...scope, cseqMethod })),
        { kind: "request", method: scope.cseqMethod, inDialog: true }
      ]
  }
}

const scopeLabel = (scope: Probe.MsgScope | undefined): string => {
  if (scope === undefined) return "none"
  switch (scope.kind) {
    case "initial-invite":
      return "initial-invite"
    case "request":
      return `request:${scope.method}${scope.inDialog ? ":in-dialog" : ""}`
    case "response":
      return `response:${scope.status}:${scope.cseqMethod}`
  }
}

/** The probe-only mutants of `probe`, labelled. */
const probeMutants = (probe: Probe.Probe): ReadonlyArray<readonly [string, Probe.Probe]> => {
  const out: Array<readonly [string, Probe.Probe]> = []
  for (const role of ROLES) {
    if (probe.leg?.role !== role) out.push([`leg=${role}`, { ...probe, leg: { role, direction: LegRole.directionOf(role) } }])
  }
  if (probe.leg !== undefined) {
    const { leg: _leg, ...rest } = probe
    out.push(["leg=none", rest])
  }
  for (const scope of scopesNear(probe.scope)) {
    const label = `scope=${scopeLabel(scope)}`
    if (probe.kind === "shape") out.push([label, { ...probe, scope }])
    else if (scope !== undefined) out.push([label, { ...probe, scope }])
  }
  if (probe.kind === "header") {
    out.push([`inbound=${!probe.inbound}`, { ...probe, inbound: !probe.inbound }])
    for (const driven of [true, false, undefined]) {
      if (driven !== probe.driven) out.push([`driven=${driven}`, { ...probe, driven }])
    }
    out.push([`bodiless=${!probe.bodiless}`, { ...probe, bodiless: !probe.bodiless }])
    out.push([`carries-body=${!probe.replayedCarriesBody}`, { ...probe, replayedCarriesBody: !probe.replayedCarriesBody }])
    const sources: ReadonlyArray<readonly [string, ReadonlyArray<string>]> = [
      ["none", []],
      ["captured", probe.captured],
      ["replayed", probe.replayed]
    ]
    for (const [name, values] of sources) {
      out.push([`inboundValues=${name}`, { ...probe, inboundValues: values }])
      out.push([`inAnsweredRequest=${name}`, { ...probe, inAnsweredRequest: values }])
      out.push([`inReplayedParts=${name}`, { ...probe, inReplayedParts: values }])
    }
  }
  if (probe.kind === "header" || probe.kind === "body") {
    const added = (side: ReadonlyArray<string>, other: ReadonlyArray<string>): ReadonlyArray<string> => [
      ...side,
      other.find((item) => !side.includes(item)) ?? MUTANT_ITEM
    ]
    out.push(["captured+", { ...probe, captured: added(probe.captured, probe.replayed) }])
    out.push(["replayed+", { ...probe, replayed: added(probe.replayed, probe.captured) }])
    if (probe.captured.length > 0) out.push(["captured-", { ...probe, captured: probe.captured.slice(0, -1) }])
    if (probe.replayed.length > 0) out.push(["replayed-", { ...probe, replayed: probe.replayed.slice(0, -1) }])
  }
  return out
}

/** The value a step-keyed map fact holds for a step put into it: the probe's status where it states one. */
const mapValueFor = (probe: Probe.Probe): number =>
  probe.scope?.kind === "response" ? probe.scope.status : 1

/** One fact's value with the probe's step toggled, or flipped; `undefined` for a value of no known shape. */
const toggled = (value: unknown, probe: Probe.Probe): unknown => {
  if (typeof value === "boolean") return !value
  if (value instanceof Set) {
    const next = new Set(value)
    if (next.has(probe.step)) next.delete(probe.step)
    else next.add(probe.step)
    return next
  }
  if (value instanceof Map) {
    const next = new Map(value)
    if (next.has(probe.step)) next.delete(probe.step)
    else next.set(probe.step, mapValueFor(probe))
    return next
  }
  return undefined
}

/** Whether a case fact is the run's per-leg facts, joined to probes by the step's leg. */
const isRunFacts = (value: unknown): value is RunFacts =>
  typeof value === "object" && value !== null && "run" in value && "legOfStep" in value &&
  (value as RunFacts).legOfStep instanceof Map

type FactMutant<F> = readonly [string, F, Readonly<Record<string, unknown>>]

/** The fact-only mutants of `facts` for `probe`, labelled, each with the facts it changed. */
const factMutants = <F extends object>(probe: Probe.Probe, facts: F): ReadonlyArray<FactMutant<F>> => {
  const out: Array<FactMutant<F>> = []
  const entries = Object.entries(facts).sort(([a], [b]) => (a < b ? -1 : 1))
  for (const [name, value] of entries) {
    const next = toggled(value, probe)
    if (next === undefined) continue
    out.push([`gate:${name}`, { ...facts, [name]: next }, { [name]: next }])
  }
  for (const [name, value] of entries) {
    if (!isRunFacts(value)) continue
    const legOfStep = new Map(value.legOfStep)
    if (legOfStep.has(probe.step)) legOfStep.delete(probe.step)
    else legOfStep.set(probe.step, [...legOfStep.values()][0] ?? "A")
    out.push([`${name}:leg`, { ...facts, [name]: { ...value, legOfStep } }, { [`${name}.legOfStep`]: legOfStep }])
  }
  return out
}

/** Every mutant of `probe` under `facts`, in a fixed order, duplicates dropped. */
export const mutantsOf = <F extends object>(probe: Probe.Probe, facts: F): ReadonlyArray<Mutant<F>> => {
  const seen = new Set<string>([canonicalText({ probe, delta: {} })])
  const out: Array<Mutant<F>> = []
  const keep = (mutant: Mutant<F>) => {
    const key = canonicalText({ probe: mutant.probe, delta: mutant.delta })
    if (seen.has(key)) return
    seen.add(key)
    out.push(mutant)
  }
  for (const [label, mutated] of probeMutants(probe)) keep({ label, probe: mutated, facts, delta: {} })
  for (const [label, mutated, delta] of factMutants(probe, facts)) keep({ label, probe, facts: mutated, delta })
  return out
}

/** The digest naming a mutant set: every mutant's label, probe and changed facts, in order. */
export const mutantsDigest = <F>(mutants: ReadonlyArray<Mutant<F>>): string =>
  digestOf(mutants.map((m) => [m.label, m.probe, m.delta]))
