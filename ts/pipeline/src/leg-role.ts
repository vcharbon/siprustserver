/**
 * Each leg's role in its call, read off a document's `calls`: the caller's
 * leg; a joined attempt by its join (a transferee joined on a REFER or on an
 * INFO, a media resource); on the serial hunt (branch 0) the first position is
 * the dialled leg and every later one a reroute; any other attempt is a
 * parallel fork. A leg no call names is `other`.
 *
 * Every message the system emits on a leg travels toward that leg's party, so
 * the direction follows from the role.
 */
import type { Call } from "@sip/contracts"

export type LegRole =
  | "caller"
  | "dialled"
  | "reroute"
  | "fork"
  | "transferee-refer"
  | "transferee-info"
  | "resource"
  | "other"

/** Which way a message the system emits on a leg travels. */
export type Direction = "to-caller" | "to-callee" | "other"

/** Where in the call a confronted message sits: its leg's role and its direction. */
export interface LegPlace {
  readonly role: LegRole
  readonly direction: Direction
}

/** The parts of a document's calls the reading uses. */
export type CallsView = ReadonlyArray<{
  readonly caller_leg: string
  readonly attempts: ReadonlyArray<{
    readonly leg: string
    readonly branch: number
    readonly position: number
    readonly joined_by?: { readonly kind: Call.JoinKind | string }
  }>
}>

const JOINED: Readonly<Record<string, LegRole>> = {
  mrf: "resource",
  refer: "transferee-refer",
  info: "transferee-info"
}

/** Every named leg's role. */
export const legRoles = (calls: CallsView): ReadonlyMap<string, LegRole> => {
  const roles = new Map<string, LegRole>()
  for (const call of calls) {
    roles.set(call.caller_leg, "caller")
    const hunted = call.attempts.filter((a) => a.joined_by === undefined)
    const serial = hunted.filter((a) => a.branch === 0).sort((a, b) => a.position - b.position)
    for (const a of hunted) if (a.branch !== 0) roles.set(a.leg, "fork")
    serial.forEach((a, n) => roles.set(a.leg, n === 0 ? "dialled" : "reroute"))
    for (const a of call.attempts) {
      const joined = a.joined_by === undefined ? undefined : JOINED[a.joined_by.kind]
      if (joined !== undefined) roles.set(a.leg, joined)
    }
  }
  return roles
}

/** The way a message emitted on a leg of `role` travels. */
export const directionOf = (role: LegRole): Direction =>
  role === "caller" ? "to-caller" : role === "other" ? "other" : "to-callee"

/** Each leg's place, a leg no call names placed `other` both ways. */
export const legPlaces = (calls: CallsView): ((leg: string) => LegPlace) => {
  const roles = legRoles(calls)
  return (leg) => {
    const role = roles.get(leg) ?? "other"
    return { role, direction: directionOf(role) }
  }
}
