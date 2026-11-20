/**
 * What a delta rule is and how one is built (ADR-0062): its words, its match
 * key, and a predicate over the probe kind the key names and the deployment's
 * case facts `F`. A built rule reads its predicate only on a probe its key
 * admits, so it matches no probe outside its key. The deployment names its
 * sections `S` and lists its rules in match order; matching reads that list
 * order alone, and {@link shapeIssues} holds the list to its sections.
 */
import type { Probe } from "@sip/pipeline"
import { keyTest, type RuleOn } from "./rule-on.js"

/** The longest `why` a rule states (ADR-0062). */
export const WHY_CAP = 300

/** The longest lane note a rule states. */
export const LANE_NOTE_CAP = 120

/** Why a lane's ruling differs, by lane list name. */
export type LaneNotes = Readonly<Record<string, string>>

/**
 * One named difference: what it names, why the difference is not evidence about
 * the system under test, and where a lane's own ruling differs. The lane lists
 * carry a class and a ticket and nothing else.
 */
export interface DeltaRule<F, S extends string = string> {
  /**
   * The rule's name: lane lists, docs and stored verdicts read it, so it never
   * changes, save once for a row that moves to the generic rows under a name in
   * one deployment's vocabulary (ADR-0062 item 7).
   */
  readonly id: string
  /** The section the deployment placed the rule in: sections run in the deployment's order. */
  readonly section: S
  /** What the predicate can match: it holds on no probe outside this key. */
  readonly on: RuleOn
  /** The case-wide fact the rule keys on, where it keys on one. */
  readonly gate?: Extract<keyof F, string>
  /**
   * The clause the ruling rests on, where it rests on one; for the documented
   * behaviour of this repository's B2BUA or replay rig, the ADR that states it.
   */
  readonly rfc?: string
  /**
   * The ruling in one or two sentences, at most {@link WHY_CAP} characters:
   * what is named, why, what stays visible. Never the rule's position.
   */
  readonly why: string
  /** Why a lane's ruling differs, at most {@link LANE_NOTE_CAP} characters per lane, never the class its list states. */
  readonly lanes?: LaneNotes
  readonly matches: (probe: Probe.Probe, facts: F) => boolean
}

/** A rule's words: everything but its section, its key and its predicate. */
export type RuleText<F> = Omit<DeltaRule<F>, "section" | "on" | "matches">

/** A predicate over one kind of probe and the case facts. */
export type Predicate<P, F> = (probe: P, facts: F) => boolean

/**
 * A rule as a deployment writes it: its words, its key, and a predicate over
 * the probe kind the key names (`header`, `shape`, `body`), or over any probe
 * (`matches`) where the key names several kinds.
 */
export type RuleSpec<F> =
  & RuleText<F>
  & (
    | { readonly on: RuleOn & { readonly kind: "header" }; readonly header: Predicate<Probe.HeaderProbe, F> }
    | { readonly on: RuleOn & { readonly kind: "shape" }; readonly shape: Predicate<Probe.ShapeProbe, F> }
    | { readonly on: RuleOn & { readonly kind: "body" }; readonly body: Predicate<Probe.BodyProbe, F> }
    | { readonly on: RuleOn; readonly matches: Predicate<Probe.Probe, F> }
  )

const predicateOf = <F>(spec: RuleSpec<F>): Predicate<Probe.Probe, F> => {
  if ("header" in spec) return spec.header as Predicate<Probe.Probe, F>
  if ("shape" in spec) return spec.shape as Predicate<Probe.Probe, F>
  if ("body" in spec) return spec.body as Predicate<Probe.Probe, F>
  return spec.matches
}

const built = <F, S extends string>(section: S, spec: RuleSpec<F>): DeltaRule<F, S> => {
  const admitted = keyTest(spec.on)
  const read = predicateOf(spec)
  const { id, on, gate, rfc, why, lanes } = spec
  return {
    id,
    section,
    on,
    ...(gate === undefined ? {} : { gate }),
    ...(rfc === undefined ? {} : { rfc }),
    why,
    ...(lanes === undefined ? {} : { lanes }),
    matches: (probe, facts) => admitted(probe) && read(probe, facts)
  }
}

/** The rules of one section, in match order. */
export const section = <F, S extends string>(name: S, specs: ReadonlyArray<RuleSpec<F>>): ReadonlyArray<DeltaRule<F, S>> =>
  specs.map((spec) => built(name, spec))

/** The first rule of `rules` that names `probe`, in order, or `undefined`: the one classification reads. */
export const matchRule = <F, S extends string>(
  rules: ReadonlyArray<DeltaRule<F, S>>,
  probe: Probe.Probe,
  facts: F
): DeltaRule<F, S> | undefined => rules.find((rule) => rule.matches(probe, facts))

/**
 * Every rule of `rules` that names `probe`, in order: the rule {@link matchRule}
 * takes first, then every rule it shadows. Read-only; classification never
 * reads past the first.
 */
export const matchingRules = <F, S extends string>(
  rules: ReadonlyArray<DeltaRule<F, S>>,
  probe: Probe.Probe,
  facts: F
): ReadonlyArray<DeltaRule<F, S>> => rules.filter((rule) => rule.matches(probe, facts))

/** The sections of `rules` in list order, each named once per contiguous run. */
const sectionRuns = <S extends string>(rules: ReadonlyArray<{ readonly section: S }>): ReadonlyArray<S> =>
  rules.flatMap((rule, i) => (i > 0 && rules[i - 1]!.section === rule.section ? [] : [rule.section]))

/** Each declared run of `runs` that follows a later-declared one. */
const outOfOrder = <S extends string>(runs: ReadonlyArray<S>, sections: ReadonlyArray<S>): ReadonlyArray<string> => {
  const declared = runs.filter((name) => sections.includes(name))
  return declared.flatMap((name, i) =>
    i > 0 && sections.indexOf(name) < sections.indexOf(declared[i - 1]!) ? [`section ${name} runs after ${declared[i - 1]}`] : []
  )
}

/**
 * What breaks the shape every deployment's rules hold, one line each; none for
 * a sound list. Each section is declared, contiguous, and runs in the order
 * `sections` states, so the rules page, which groups by section, prints the
 * match order. Every id is minted once, every `why` fits {@link WHY_CAP} and
 * every lane note {@link LANE_NOTE_CAP}. A deployment's test asserts it empty.
 */
export const shapeIssues = <F, S extends string>(
  rules: ReadonlyArray<DeltaRule<F, S>>,
  sections: ReadonlyArray<S>
): ReadonlyArray<string> => {
  const runs = sectionRuns(rules)
  const ids = rules.map((rule) => rule.id)
  return [
    ...runs.filter((name) => !sections.includes(name)).map((name) => `section ${name} is not declared`),
    ...runs.filter((name, i) => runs.indexOf(name) !== i).map((name) => `section ${name} is split`),
    ...outOfOrder(runs, sections),
    ...ids.filter((id, i) => ids.indexOf(id) !== i).map((id) => `id ${id} is minted twice`),
    ...rules.filter((rule) => rule.why.length > WHY_CAP).map((rule) => `${rule.id}: why of ${rule.why.length} characters`),
    ...rules.flatMap((rule) =>
      Object.entries(rule.lanes ?? {})
        .filter(([, note]) => note.length > LANE_NOTE_CAP)
        .map(([lane, note]) => `${rule.id}: ${lane} note of ${note.length} characters`)
    )
  ]
}
