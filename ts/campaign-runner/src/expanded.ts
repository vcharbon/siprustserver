/**
 * A compiled cell's expectation spelled out for a reader (`validate --expand`):
 * read off the document it runs and the runner's cell, nothing re-derived.
 *
 * - `legs`: each leg's messages in flow order, each with what the party does
 *   (`send`) or expects (`expect`), the message (method, or status and reason,
 *   with its CSeq method), everything else the message spec states (R-URI,
 *   From, To, CSeq, Contact, headers with their values and classes, headers
 *   only present, body), its checks sorted by kind (`exact` for `eq`,
 *   `pattern` for `regex`, `present`, `absent`) with their classes, whether a
 *   check mode records rather than asserts, whether it is optional, which
 *   `unordered` group or `alt` branch holds it, its `after` edges and delay,
 *   and every other field the step states under its document name (`awaits`,
 *   `confirms_dialog`, `early`, `overlap`, `retransmits`), so none is dropped;
 * - `graph`: every flow node in order (message steps, injects with their
 *   action and target, `alt`, `unordered`), its `after` edges, the operations
 *   the cell injects as steps and whether a tag makes it run alone;
 * - `timers`: the lane timers the cell was compiled with (resolved at run),
 *   and every delay linked to a system or session timer;
 * - `claims`: the document's postconditions (its CDR expectation, its claims
 *   by collector, its checks), the claims nothing on a lane answers, and every
 *   check the cell runs in order, the pack's gates (the cleanup, the RFC audit)
 *   last.
 *
 * A pack adds what its compiler knows beside the document (where each value
 * came from) under `pack`.
 */
import { type Check, Flow, Pivot } from "@sip/contracts"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type { Cell } from "./cell.js"

/** A check as its sorted list holds it: every field of the check but its operator, which names the list. */
type Sorted = Omit<Check.Check, "op">

/** A check sorted by what it asserts. */
interface Checks {
  readonly exact: ReadonlyArray<Sorted>
  readonly pattern: ReadonlyArray<Sorted>
  readonly present: ReadonlyArray<Sorted>
  readonly absent: ReadonlyArray<Sorted>
}

/** The checks of `checks` asserting with `op`, each kept whole but its operator. */
const withOp = (checks: ReadonlyArray<Check.Check>, op: Check.CheckOp): ReadonlyArray<Sorted> =>
  checks.filter((c) => c.op === op).map(({ op: _op, ...sorted }) => sorted)

/** `checks` sorted by their operator, each check's other fields (its value, its class) kept. */
export const checksOf = (checks: ReadonlyArray<Check.Check>): Checks => ({
  exact: withOp(checks, "eq"),
  pattern: withOp(checks, "regex"),
  present: withOp(checks, "exists"),
  absent: withOp(checks, "absent")
})

/** Where a step sits in the flow: a node of its own, an `unordered` group or an `alt` branch. */
type Placed = { readonly node: string } | { readonly unordered: string } | { readonly alt: string; readonly branch: string }

/** A message as a reader names it: its method, or its status and reason. */
const messageOf = (msg: Flow.Step["msg"]): string =>
  msg.method !== undefined ? msg.method.toUpperCase() : `${msg.status ?? "?"}${msg.reason === undefined ? "" : ` ${msg.reason}`}`

/**
 * One message step as a leg's reader sees it. The fields restated here are
 * named for the reader; every other field the step states (`awaits`,
 * `confirms_dialog`, `early`, `overlap`, `retransmits`, any field the format
 * gains) is kept under its document name, and `stated` is the message spec
 * whole but the method, status, reason and CSeq method `message` names.
 */
const stepOf = (step: Flow.Step, placed: Placed) => {
  const { id, op, msg, checks, after, delay, within_ms, optional, auto, in_dialog, check, ...unrestated } = step
  const { method: _method, status: _status, reason: _reason, "cseq-method": cseqMethod, ...stated } = msg
  return {
    step: id,
    op,
    message: messageOf(msg),
    ...(cseqMethod === undefined ? {} : { cseqMethod: cseqMethod.toUpperCase() }),
    ...placed,
    ...(optional === true ? { optional: true } : {}),
    ...(auto === true ? { auto: true } : {}),
    ...(in_dialog === true ? { inDialog: true } : {}),
    ...(check === "record" ? { checkMode: "record" } : {}),
    ...unrestated,
    stated,
    checks: checksOf(checks ?? []),
    after: after ?? [],
    delay,
    ...(within_ms === undefined ? {} : { withinMs: within_ms })
  }
}

/** Every message step of `flow` in order, with where it sits. */
const placedSteps = (flow: ReadonlyArray<Flow.FlowNode>): ReadonlyArray<{ readonly step: Flow.Step; readonly placed: Placed }> =>
  flow.flatMap((node): ReadonlyArray<{ readonly step: Flow.Step; readonly placed: Placed }> => {
    if (Flow.isStepNode(node)) return [{ step: node, placed: { node: node.id } }]
    if (Flow.isUnorderedNode(node)) return node.steps.map((step) => ({ step, placed: { unordered: node.id } }))
    if (Flow.isAltNode(node)) {
      return node.branches.flatMap((branch) => branch.steps.map((step) => ({ step, placed: { alt: node.id, branch: branch.name } })))
    }
    return []
  })

/** One flow node of the step graph. */
const nodeOf = (node: Flow.FlowNode) => {
  const after = node.after ?? []
  if (Flow.isStepNode(node)) return { id: node.id, kind: node.op, leg: node.leg, after }
  if (Flow.isInjectNode(node)) {
    return { id: node.id, kind: "inject", action: node.action, ...(node.target === undefined ? {} : { target: node.target }), after }
  }
  if (Flow.isAltNode(node)) {
    return { id: node.id, kind: "alt", branches: node.branches.map((b) => ({ name: b.name, steps: b.steps.map((s) => s.id) })), after }
  }
  return { id: node.id, kind: "unordered", steps: node.steps.map((s) => s.id), after }
}

/** The expanded expectation of `cell`, whose document is `document`, with what its pack adds under `pack`. */
export const expandedOf = (cell: Cell, document: Pivot.PivotV3, pack?: unknown) => {
  const steps = placedSteps(document.flow)
  const actorOf = (leg: string) => document.legs.find((l) => l.id === leg)?.actor
  return {
    id: cell.id,
    ref: cell.ref,
    subcase: cell.subcase,
    file: cell.source.file,
    legs: Object.fromEntries(document.legs.map((leg) => [leg.id, {
      actor: actorOf(leg.id),
      endpoint: document.actors.find((a) => a.id === leg.actor)?.endpoint,
      messages: steps.filter(({ step }) => step.leg === leg.id).map(({ placed, step }) => stepOf(step, placed))
    }])),
    graph: {
      nodes: document.flow.map(nodeOf),
      operations: cell.operations ?? [],
      alone: cell.alone,
      tags: cell.tags
    },
    timers: {
      lane: Object.entries(cell.laneTimers ?? {}).map(([name, ms]) => ({ name, ms, source: "resolved at run over the run's lanes" })),
      linkedDelays: steps.filter(({ step }) => step.delay.timer_linked).map(({ step }) => ({ step: step.id, ...step.delay }))
    },
    claims: {
      postconditions: document.postconditions ?? {},
      notRequested: cell.notRequested ?? [],
      checked: cell.checked
    },
    ...(pack === undefined ? {} : { pack })
  }
}

/**
 * The expanded expectation of `cell`, its document read from its directory's
 * `scenario.json`; one that does not read or decode is named so, with why.
 */
export const readExpanded = (cell: Cell, pack?: unknown) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const text = yield* fs.readFileString(path.join(cell.dir, "scenario.json"))
    const document = yield* Pivot.parsePivot(text)
    return expandedOf(cell, document, pack)
  }).pipe(Effect.catchCause((cause) => Effect.succeed({ id: cell.id, unreadable: whyOf(cause) })))

/** A cause's lines before its stack trace, on one line. */
const whyOf = (cause: Cause.Cause<unknown>): string =>
  Cause.pretty(cause).split("\n").filter((line) => !/^\s+at .*:\d+:\d+\)?$/.test(line)).map((line) => line.trim()).join(" ")
