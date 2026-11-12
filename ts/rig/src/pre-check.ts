/**
 * The pre-checks of a scope (ADR-0060 appendix §3): its job- or batch-scope
 * collectors `at = "before"`, collected once before any call of the scope,
 * over no cell, then closed. A `fail` one answers of the scope refuses the
 * scope, its message the refusal's and an event `refused` of its instance. A
 * `not checkable`, a failed or timed-out `collect`'s included, refuses the
 * scope where the pre-check is `required` or the job asked for it by name (an
 * `on-demand` one, `collector-plan.ts`), so neither fails open; otherwise it
 * refuses nothing and stays the scope's line.
 *
 * A required pre-check that a real-clock lane does not wire, for lack of a
 * part it needs, refuses before any pre-check runs: leaving a part out never
 * skips what a required pre-check reads of the lab.
 */
import type { Verification } from "@sip/contracts"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Collector from "./collector.js"
import type * as CollectorRun from "./collector-run.js"
import { opened } from "./collector-run.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"

/** Whether `instance` is a pre-check: `at = "before"`. */
export const isPreCheck = (instance: CollectorRun.Instance): boolean => instance.collector.manifest.at === "before"

/** A required pre-check a lane does not wire, and why. */
export interface Unwired {
  readonly collector: string
  readonly lane: string
  readonly message: string
}

/** Each required pre-check of `rig`, of either scope, that a real-clock lane of `lanes` names no part of, by lane then name. */
export const unwiredOf = (rig: RigPath.RigPath, lanes: ReadonlyArray<Lanes.Loaded>): ReadonlyArray<Unwired> => {
  const required = [...rig.collectors.values()]
    .filter((defined) => defined.manifest.at === "before" && defined.manifest.required === true)
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
  return lanes.filter((lane) => lane.lane.clock === "real").flatMap((lane) =>
    required.flatMap((defined) => {
      const lacks = (defined.manifest.needs ?? []).filter((kind) => !lane.parts.has(kind))
      return lacks.length === 0 ? [] : [{
        collector: defined.name,
        lane: lane.id,
        message: `${lane.id} names no ${lacks.join(", ")} part, which the required pre-check ${defined.name} needs (${lane.file})`
      }]
    })
  )
}

/** What a scope's pre-checks came to. */
export interface Checked {
  readonly collected: ReadonlyArray<CollectorRun.Collected>
  /** The scope's own answers: each instance's in its order, or a `fail` per required pre-check unwired. */
  readonly answers: CollectorRun.Answers
  /** Why the scope is refused, each refusing line's reason joined; `undefined` where none refuses. */
  readonly refusal: string | undefined
  /** The labels of the instances whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
}

/** Nothing checked: no pre-check is wired. */
export const NOTHING: Checked = { collected: [], answers: { verifications: [], attachments: [] }, refusal: undefined, leftChanged: [] }

const refused = (sink: CollectorRun.Sink, collector: string, message: string) =>
  Effect.flatMap(Clock.currentTimeMillis, (clock_ms) => sink({ collector, phase: "refused", clock_ms, wall_ms: Date.now(), message }))

/** What a `fail` line says: its message, or that it failed. */
const saidOf = (line: Verification.Verification): string => line.message ?? `${line.name} failed`

/** How a refusal names the pre-check `instance` that could not check: required, or asked for; neither where no instance answered. */
const kindOf = (instance: CollectorRun.Instance | undefined): string | undefined =>
  instance === undefined
    ? undefined
    : Collector.isRequired(instance.collector.manifest)
    ? "required"
    : instance.asked === true
    ? "asked"
    : undefined

/**
 * Why `line`, answered by the entry labelled `label` (the pre-check
 * `instance` of that label, none for the base's own lines), refuses its
 * scope: a `fail`, and a `not checkable` of a required or asked pre-check;
 * `undefined` otherwise.
 */
const refusalOf = (label: string, instance: CollectorRun.Instance | undefined, line: Verification.Verification): string | undefined => {
  if (line.answer === "fail") return saidOf(line)
  const kind = kindOf(instance)
  if (line.answer !== "not-checkable" || kind === undefined) return undefined
  return `the ${kind} pre-check ${label} could not check ${line.name}` +
    (line.message === undefined ? "" : `: ${line.message}`)
}

/**
 * Runs the pre-checks `instances` of one scope, with `options`' sink and
 * locker (no judge of exports runs over a pre-check), where `unwired` is
 * empty; refuses with `unwired`'s messages, running none, otherwise. Each
 * answered entry is matched to its instance by label.
 */
export const run = (
  instances: ReadonlyArray<CollectorRun.Instance>,
  unwired: ReadonlyArray<Unwired>,
  options: Pick<CollectorRun.Options, "sink" | "locker">
): Effect.Effect<Checked> =>
  Effect.gen(function*() {
    if (unwired.length > 0) {
      for (const one of unwired) yield* refused(options.sink, one.collector, one.message)
      return {
        ...NOTHING,
        answers: {
          verifications: unwired.map((one) => ({ name: one.collector, source: one.collector, answer: "fail" as const, message: one.message })),
          attachments: []
        },
        refusal: unwired.map((one) => one.message).join("; ")
      }
    }
    if (instances.length === 0) return NOTHING
    let scope: CollectorRun.Opened | undefined
    const collected = yield* Effect.scoped(Effect.flatMap(opened(instances, { sink: options.sink, locker: options.locker }), (one) => {
      scope = one
      return one.collect({ cells: [], unread: [], failed: [] })
    }))
    const byLabel = new Map(instances.map((instance) => [instance.label, instance]))
    const failed: Array<string> = []
    for (const one of collected) {
      for (const line of one.scope.verifications) {
        const said = refusalOf(one.label, byLabel.get(one.label), line)
        if (said === undefined) continue
        failed.push(said)
        yield* refused(options.sink, one.label, said)
      }
    }
    return {
      collected,
      answers: {
        verifications: collected.flatMap((one) => one.scope.verifications),
        attachments: collected.flatMap((one) => one.scope.attachments)
      },
      refusal: failed.length === 0 ? undefined : failed.join("; "),
      leftChanged: scope?.leftChanged() ?? []
    }
  })
