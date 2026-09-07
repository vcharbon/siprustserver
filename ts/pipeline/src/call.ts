/**
 * The correlation engine's input unit: one flows-document call group, flattened
 * to the facts the five rule kinds anchor on.
 *
 * Built once per document; `./engine.ts` reads this and nothing else, so no rule
 * kind can reach back into the raw flows shape. Every SIP fact below is read
 * from the extractor's parsed projections — no header text is parsed on this
 * side of the seam.
 */
import { Flows } from "@sip/contracts"

export interface Referral {
  readonly ts_ms: number
  /** `Refer-To` user part, digits-normalized. */
  readonly target?: string
  /** `Replaces` carried inside `Refer-To`, if any (attended transfer). */
  readonly replaces?: DialogRef
}

export interface DialogRef {
  readonly callId: string
  readonly toTag?: string
  readonly fromTag?: string
}

export type IdentityKind = "from-user" | "to-user" | "ruri-user"

export const IDENTITY_KINDS: ReadonlyArray<IdentityKind> = ["from-user", "to-user", "ruri-user"]

/** One digits-normalized identity value, with the INVITE it was read from. */
export interface IdentityValue {
  readonly value: string
  /** Call-ID of the leg carrying that INVITE. */
  readonly leg: string
  readonly ts_ms: number
  /** True on the call's initial INVITE, false on a later leg's INVITE. */
  readonly initial: boolean
}

/** The initial INVITE of a call group, as the extractor identifies it. */
export interface InitialInvite {
  readonly ts_ms: number
  readonly ruriUser?: string
  readonly fromUser?: string
  readonly toUser?: string
  readonly replaces?: DialogRef
  /** Any projected header of the initial INVITE, for `header-key` rules. */
  readonly headers: (name: string) => Array<string>
  readonly msg: Flows.Msg
}

export interface Call {
  readonly id: string
  readonly callIds: ReadonlyArray<string>
  /** Dialog identities this call OWNS, for `Replaces` / `refer` targeting. */
  readonly dialogs: ReadonlyArray<DialogRef>
  readonly t0_ms: number
  readonly invite?: InitialInvite
  /**
   * A call's identities are the SET of values on ANY dialog-forming INVITE it
   * sends, across all its legs — not the initial INVITE's alone. Two calls share
   * an identity when the sets intersect; the matched value keeps the leg and
   * timestamp it came from, so a join through a b-leg INVITE reads differently
   * from one through the initial INVITE.
   */
  readonly identities: Readonly<Record<IdentityKind, ReadonlyArray<IdentityValue>>>
  /** Terminal status of the group and when it landed, as the extractor states it. */
  readonly final?: { readonly status: number; readonly ts_ms: number }
  readonly referrals: ReadonlyArray<Referral>
  /** Human-facing only: never read by a matcher. */
  readonly describe: string
}

const dialogRef = (r: {
  readonly call_id: string
  readonly to_tag: string | null
  readonly from_tag: string | null
}): DialogRef => ({
  callId: r.call_id,
  ...(r.to_tag === null ? {} : { toTag: r.to_tag }),
  ...(r.from_tag === null ? {} : { fromTag: r.from_tag })
})

const ms = (us: number): number => us / 1000

export const callsOf = (doc: Flows.FlowsDoc): Array<Call> => {
  const calls = doc.groups.map((group, gi): Call => {
    const legs = group.legs.map((i) => doc.legs[i]).filter((l) => l !== undefined)

    // Which message is the CALL's initial INVITE is the extractor's verdict, not
    // a scan for the earliest INVITE repeated by every consumer.
    const at = group.initial_invite
    const inviteMsg = at === undefined ? undefined : doc.legs[at.leg]?.msgs[at.msg]
    const invite: InitialInvite | undefined = inviteMsg
      ? {
          ts_ms: ms(inviteMsg.ts_us),
          ...user("ruriUser", Flows.userOf(inviteMsg.identities.ruri)),
          ...user("fromUser", Flows.userOf(inviteMsg.identities.from)),
          ...user("toUser", Flows.userOf(inviteMsg.identities.to)),
          ...(inviteMsg.replaces ? { replaces: dialogRef(inviteMsg.replaces) } : {}),
          headers: (name: string) => Flows.headerValues(inviteMsg, name),
          msg: inviteMsg
        }
      : undefined

    // Identity sets. Every dialog-forming INVITE the call sends contributes,
    // whichever leg it belongs to; re-INVITEs are excluded because a callee-side
    // re-INVITE reverses From/To and would poison the sets. Retransmissions add
    // nothing.
    const identities: Record<IdentityKind, Array<IdentityValue>> = {
      "from-user": [],
      "to-user": [],
      "ruri-user": []
    }
    const add = (
      kind: IdentityKind,
      value: string | undefined,
      v: Omit<IdentityValue, "value">
    ): void => {
      if (value === undefined) return
      const bucket = identities[kind]
      if (bucket.some((x) => x.value === value && x.leg === v.leg)) return
      bucket.push({ value, ...v })
    }
    for (const leg of legs) {
      for (const m of leg.msgs) {
        if (m.summary.kind !== "request" || m.summary.method !== "INVITE" || m.retx) continue
        if (m.summary.to.tag !== null && m !== inviteMsg) continue
        const where = { leg: leg.call_id, ts_ms: ms(m.ts_us), initial: m === inviteMsg }
        add("from-user", Flows.userOf(m.identities.from), where)
        add("to-user", Flows.userOf(m.identities.to), where)
        add("ruri-user", Flows.userOf(m.identities.ruri), where)
      }
    }
    for (const bucket of Object.values(identities)) bucket.sort((a, b) => a.ts_ms - b.ts_ms)

    // The terminal status of the call is the extractor's stated rule — the last
    // response >= 200 to an INVITE across the group — not a rule each consumer
    // reinvents.
    const final =
      group.final_status === undefined || group.final_us === undefined
        ? undefined
        : { status: group.final_status, ts_ms: ms(group.final_us) }

    const referrals = legs
      .flatMap((l) => l.msgs)
      .sort((a, b) => a.ts_us - b.ts_us)
      .filter((m) => m.summary.kind === "request" && m.summary.method === "REFER")
      .map(
        (m): Referral => ({
          ts_ms: ms(m.ts_us),
          ...user("target", Flows.userOf(m.refer_to?.target)),
          ...(m.refer_to?.replaces ? { replaces: dialogRef(m.refer_to.replaces) } : {})
        })
      )

    // A dialog the call owns: its leg's Call-ID with the tags one of its
    // messages carried.
    const dialogs = legs.flatMap((l) =>
      l.msgs.map(
        (m): DialogRef => ({
          callId: l.call_id,
          ...(m.summary.to.tag === null ? {} : { toTag: m.summary.to.tag }),
          ...(m.summary.from.tag === null ? {} : { fromTag: m.summary.from.tag })
        })
      )
    )

    return {
      id: `g${gi}`,
      callIds: [...new Set(legs.map((l) => l.call_id))],
      dialogs,
      t0_ms: ms(group.t0_us),
      ...(invite === undefined ? {} : { invite }),
      identities,
      ...(final === undefined ? {} : { final }),
      referrals,
      describe:
        `${legs.length} leg(s) ${invite?.fromUser ?? "?"}->${invite?.toUser ?? "?"} ` +
        `final=${final?.status ?? "-"}`
    }
  })
  return calls.filter((c) => c.invite !== undefined || c.referrals.length > 0)
}

/** A named optional field, present exactly when the projection resolved one. */
const user = <K extends string>(key: K, value: string | undefined): { [P in K]?: string } =>
  (value === undefined ? {} : { [key]: value }) as { [P in K]?: string }
