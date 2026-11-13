/**
 * An operation as a step of a case's call (ADR-0060 appendix §7): an `inject`
 * node of the cell's pivot document (`PCAP2TEST_PIVOT_V3.md` §6.6) whose
 * action is `operation:<name>`, the capability a lane binding `<name>` in its
 * `[operations]` gives. Its `after` names the point of the call it injects
 * at; a step that waits on the node waits for the operation's `inject` to
 * have answered. The rig's injector performs operations alone: a document
 * injecting another action, an operation step naming no point, and one
 * stating a `target` (which no operation reads) do not hold.
 */
import { Flow, type Pivot } from "@sip/contracts"
import { OPERATION_PREFIX } from "./capability.js"

/** One operation step: its inject node's id, the operation it asks for, and the steps it injects after. */
export interface Step {
  readonly node: string
  readonly operation: string
  readonly after: ReadonlyArray<string>
}

/** An operation's name as the rig path and a lane's `[operations]` state it. */
const NAME = /^[a-z0-9][a-z0-9-]*$/

/** The action an inject node states to ask for the operation `name`: its capability. */
export const actionOf = (name: string): string => `${OPERATION_PREFIX}${name}`

/** The operation `action` asks for, or `undefined` where it asks for none. */
export const operationOf = (action: string): string | undefined => {
  if (!action.startsWith(OPERATION_PREFIX)) return undefined
  const name = action.slice(OPERATION_PREFIX.length)
  return NAME.test(name) ? name : undefined
}

const injectsOf = (document: Pivot.PivotV3): ReadonlyArray<Flow.Inject> => document.flow.filter(Flow.isInjectNode)

/** The operation steps of `document`, in its order; an inject asking for no operation is none. */
export const stepsOf = (document: Pivot.PivotV3): ReadonlyArray<Step> =>
  injectsOf(document).flatMap((inject) => {
    const operation = operationOf(inject.action)
    return operation === undefined ? [] : [{ node: inject.id, operation, after: inject.after ?? [] }]
  })

/** The operations `document`'s steps ask for, each once, in its order. */
export const operationsOf = (document: Pivot.PivotV3): ReadonlyArray<string> => [
  ...new Set(stepsOf(document).map((step) => step.operation))
]

/** Why `document`'s injects do not hold as operation steps, or `undefined`. */
export const refusalOf = (document: Pivot.PivotV3): string | undefined => {
  for (const inject of injectsOf(document)) {
    if (operationOf(inject.action) === undefined) {
      return `its inject ${inject.id} asks for ${inject.action}, and the rig injects only an operation (${OPERATION_PREFIX}<name>)`
    }
    if ((inject.after ?? []).length === 0) return `its operation step ${inject.id} names no point of its call to inject at (after)`
    if (inject.target !== undefined) return `its operation step ${inject.id} states a target, which an operation does not read`
  }
  return undefined
}
