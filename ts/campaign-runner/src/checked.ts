/**
 * What one cell checks, passing or not, read off its compiled document, each
 * check at the flow step it is made at and anchored as a failed check is
 * (`anchors.ts`):
 *
 * - every message the ladder waits for: that it arrives as stated (`*`: any
 *   failure at the step no other check of the step names);
 * - its body type, its frozen headers and the headers it requires present;
 * - every field check, numbers bound as the lane binds them and a leg's
 *   address named `{B1 host}`;
 *
 * then the gates every cell runs, as the pack's case compiler states them
 * (`cases.ts` `CaseCompiler.gates`): the post-run cleanup and the RFC audit
 * (`*`: any field at the anchor) this module states, and any its SUT adds.
 */
import { anchorOf, stepsOf } from "./anchors.js"
import type { Checked } from "./cell.js"

export type { Checked }

type Json = Readonly<Record<string, unknown>>

const object = (value: unknown): Json =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Json) : {}

const array = (value: unknown): ReadonlyArray<unknown> => (Array.isArray(value) ? value : [])

/** The post-run cleanup: the SUT holds no call once the cell ended. */
export const CLEANUP_GATE: Checked = {
  step: null,
  anchor: "post-run cleanup",
  field: "sut.reaped",
  op: "reaped",
  expected: "every call removed"
}

/** The RFC audit: no finding of the cell's wire gates it. */
export const RFC_GATE: Checked = { step: null, anchor: "rfc", field: "*", op: "audit", expected: "no gating finding" }

/** `text` with each `${num:<identity>:<form>}` bound to the identity's number and each leg address named. */
const bound = (document: unknown, text: string): string => {
  const identities = new Map(
    array(object(document).identities).map(object).map((i) => [String(i.name), String(i.observed)] as const)
  )
  return text
    .replace(/\$\{num:([^:}]+):[^}]+\}/g, (whole, name: string) => identities.get(name) ?? whole)
    .replace(/\$\{leg:([^.}]+)\.local-hostport\}/g, (_whole, leg: string) => `{${leg} host}`)
}

/** What a waited-for message is: its method, else its status and reason. */
const messageOf = (msg: Json): string =>
  typeof msg.method === "string"
    ? msg.method
    : [msg.status, msg.reason].filter((part) => part !== undefined).join(" ") || "response"

/** The checks of one waited-for step. */
const stepChecks = (document: unknown, step: Json, id: string, anchor: string): ReadonlyArray<Checked> => {
  const msg = object(step.msg)
  const at = (field: string, op: string, expected: string | null): Checked => ({ step: id, anchor, field, op, expected })
  const asserts = step.check === "assert"
  const body = object(msg.body)
  return [
    at("*", "arrives", messageOf(msg)),
    ...(typeof body.mode === "string" ? [at("body", "type", body.mode)] : []),
    ...(asserts
      ? array(msg.headers).map(object).map((h) => at(`header(${String(h.name)})`, "eq", bound(document, String(h.value))))
      : []),
    ...(asserts ? array(msg.headers_present).map((name) => at(`header(${String(name)})`, "exists", null)) : []),
    ...array(step.checks).map(object).map((check) => {
      const op = String(check.op)
      const value = check.value
      return at(
        String(check.field),
        op,
        op === "absent" || op === "exists" || value === undefined ? null : bound(document, String(value))
      )
    })
  ]
}

/** The checks of `document`, in flow order, then `gates`, the gates every cell of the pack runs. */
export const checkedOf = (document: unknown, gates: ReadonlyArray<Checked>): ReadonlyArray<Checked> => [
  ...stepsOf(document).flatMap((step) => {
    if (step.op !== "expect" || step.check === "record" || typeof step.id !== "string") return []
    const anchor = anchorOf(document, step.id)?.anchor
    return anchor === undefined ? [] : stepChecks(document, step, step.id, anchor)
  }),
  ...gates
]
