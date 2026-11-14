/**
 * The situation reading's rules applied literally: each emitted message is
 * placed afresh by scanning back over the messages before it. Quadratic in a
 * call's messages, it is the reference `../src/situation.ts`'s single forward
 * pass is compared with, placement for placement.
 */
import type { Observation, Placed } from "../src/situation.js"

const statusClass = (status: number): string => `${Math.floor(status / 100)}xx`

const sameCseq = (a: number | undefined, b: number | undefined): boolean =>
  a === undefined || b === undefined || a === b

/** Every message's placement, index for index, as `place` defines it. */
export const placeReference = (
  observations: ReadonlyArray<Observation>,
  callerOf: (leg: string) => string | undefined
): ReadonlyArray<Placed | undefined> =>
  observations.map((o, at): Placed | undefined => {
    if (!o.emitted) return undefined
    if (o.request !== undefined) return placeRequest(observations, at, o.request, callerOf(o.leg))
    if (o.response !== undefined) return placeResponse(observations, at, o.response)
    return { situation: "UNKNOWN", pair: undefined }
  })

const opensDialog = (o: Observation): boolean => o.request?.method === "INVITE" && o.request.dialogCreating

const placeRequest = (
  observations: ReadonlyArray<Observation>,
  at: number,
  request: NonNullable<Observation["request"]>,
  callerLeg: string | undefined
): Placed => {
  const method = request.method
  if (method === "INVITE" && request.dialogCreating) {
    const callerInvite = observations.findIndex((o, i) => i < at && !o.emitted && o.leg === callerLeg && opensDialog(o))
    return { situation: "INV", pair: callerInvite >= 0 ? callerInvite : undefined }
  }
  if (method === "ACK" && answeredNon2xx(observations, at)) return { situation: "ACK-ERR", pair: undefined }
  const pair = relayedRequest(observations, at, method)
  if (method !== "CANCEL" && request.dialogCreating) return { situation: `OOD:${method}`, pair }
  return { situation: pair === undefined ? `SO:${method}` : `DQ:${method}`, pair }
}

/** Whether the last INVITE final the system received on this leg was not a 2xx. */
const answeredNon2xx = (observations: ReadonlyArray<Observation>, at: number): boolean => {
  const leg = observations[at]!.leg
  for (let i = at - 1; i >= 0; i--) {
    const r = observations[i]!
    if (r.emitted || r.leg !== leg || r.response === undefined) continue
    if (r.response.method === "INVITE" && r.response.status >= 200) return r.response.status >= 300
  }
  return false
}

/** The request this emitted request relays. */
const relayedRequest = (
  observations: ReadonlyArray<Observation>,
  at: number,
  method: string
): number | undefined => {
  const leg = observations[at]!.leg
  const start = observations.findIndex((o) => o.leg === leg)
  const opened = start >= 0 && opensDialog(observations[start]!) ? start : -1
  const floor = method === "ACK" ? Math.max(opened, lastInviteFinal(observations, at)) : opened
  for (let i = at - 1; i > floor; i--) {
    const r = observations[i]!
    if (r.request?.method !== method) continue
    if (r.emitted && r.leg === leg) return undefined
    if (!r.emitted && r.leg !== leg) return i
  }
  return undefined
}

/** The index of the last INVITE final the system received on this leg before `at`, or -1. */
const lastInviteFinal = (observations: ReadonlyArray<Observation>, at: number): number => {
  const leg = observations[at]!.leg
  for (let i = at - 1; i >= 0; i--) {
    const r = observations[i]!
    if (!r.emitted && r.leg === leg && r.response?.method === "INVITE" && r.response.status >= 200) return i
  }
  return -1
}

const placeResponse = (
  observations: ReadonlyArray<Observation>,
  at: number,
  response: NonNullable<Observation["response"]>
): Placed => {
  const { status, method } = response
  if (status === 100) return { situation: "T100", pair: undefined }
  const asked = answeredRequest(observations, at, response)
  const pair = relayedResponse(observations, at, response, asked ?? -1)
  const opened = asked !== undefined && observations[asked]!.request!.dialogCreating
  const local = pair === undefined ? ":local" : ""
  if (opened && method === "INVITE") {
    return { situation: `${status < 200 ? "P18" : `FIN:${statusClass(status)}`}${local}`, pair }
  }
  if (opened && method !== "CANCEL") return { situation: `OODS:${statusClass(status)}:${method}${local}`, pair }
  return { situation: `DS:${statusClass(status)}:${method}${local}`, pair }
}

/** The request this response answers: the latest one received on its leg with its CSeq. */
const answeredRequest = (
  observations: ReadonlyArray<Observation>,
  at: number,
  response: NonNullable<Observation["response"]>
): number | undefined => {
  const leg = observations[at]!.leg
  for (let i = at - 1; i >= 0; i--) {
    const r = observations[i]!
    if (r.emitted || r.leg !== leg || r.request === undefined) continue
    if (r.request.method === response.method && sameCseq(r.request.cseq, response.cseq)) return i
  }
  return undefined
}

/** The response this emitted response relays. */
const relayedResponse = (
  observations: ReadonlyArray<Observation>,
  at: number,
  response: NonNullable<Observation["response"]>,
  after: number
): number | undefined => {
  const leg = observations[at]!.leg
  const sameStatus = (o: Observation): boolean =>
    o.response !== undefined && o.response.status === response.status && o.response.method === response.method
  const run: Array<number> = []
  for (let i = at - 1; i > after; i--) {
    const r = observations[i]!
    if (r.emitted || r.leg === leg) continue
    if (!sameStatus(r) || (run.length > 0 && r.leg !== observations[run[0]!]!.leg)) break
    run.unshift(i)
  }
  if (run.length === 0) return undefined
  const sameBody = (o: Observation): boolean =>
    response.sdp === undefined || o.response?.sdp === undefined || o.response.sdp === response.sdp
  const alike = run.filter((i) => sameBody(observations[i]!))
  const pool = alike.length > 0 ? alike : run
  let earlier = 0
  for (let i = run[0]! + 1; i < at; i++) {
    const o = observations[i]!
    if (o.emitted && o.leg === leg && sameStatus(o) && (pool === run || sameBody(o))) earlier++
  }
  return pool[Math.min(earlier, pool.length - 1)]
}
