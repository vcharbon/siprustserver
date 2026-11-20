/**
 * Where a check of a compiled document was made, independent of the step ids
 * a sub-case numbers: a leg's message (`B1 INVITE` for a leg's initial INVITE,
 * `B1 re-INVITE` for a later one, `A final` for a final response to the
 * caller's INVITE, `A 180`), read off the document's flow, a group's members
 * in its place.
 */

type Json = Readonly<Record<string, unknown>>

const object = (value: unknown): Json =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Json) : {}

const array = (value: unknown): ReadonlyArray<unknown> => (Array.isArray(value) ? value : [])

/** A step's anchor, its leg, and whether it is the leg's initial INVITE. */
export interface Anchored {
  readonly anchor: string
  readonly leg: string
  /** Whether the step is the leg's initial INVITE. */
  readonly initial: boolean
}

/** Every step of `document`'s flow in order, a group's members in its place. */
export const stepsOf = (document: unknown): ReadonlyArray<Json> =>
  array(object(document).flow).map(object).flatMap((node) =>
    node.op === "unordered" ? array(node.steps).map(object) : [node]
  )

/** The anchor of step `id` of `document`'s flow, a group's members included; undefined where the flow has no such step. */
export const anchorOf = (document: unknown, id: string): Anchored | undefined => {
  const flow = stepsOf(document)
  const step = flow.find((s) => s.id === id)
  if (step === undefined || typeof step.leg !== "string") return undefined
  const leg = step.leg
  const msg = object(step.msg)
  if (msg.method === "INVITE") {
    const first = flow.find((s) => s.leg === leg && object(s.msg).method === "INVITE" && s.in_dialog !== true)
    const initial = first?.id === id
    return { anchor: `${leg} ${initial ? "INVITE" : "re-INVITE"}`, leg, initial }
  }
  if (typeof msg.method === "string") return { anchor: `${leg} ${msg.method}`, leg, initial: false }
  const status = Number(msg.status)
  const method = msg["cseq-method"]
  const final = status >= 200 && (method === undefined || method === "INVITE")
  return {
    anchor: `${leg} ${final ? "final" : Number.isFinite(status) ? status : "response"}`,
    leg,
    initial: false
  }
}
