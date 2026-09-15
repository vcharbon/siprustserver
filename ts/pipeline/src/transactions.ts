/**
 * INVITE transactions as a leg's STATE holds them: which INVITE a final lands
 * on and which final an ACK discharges, read from step order alone.
 *
 * Every INVITE request on a leg opens a transaction in the direction it runs.
 * A final to an INVITE lands on the newest transaction running the other way —
 * a response travels opposite to its request, and a leg runs one CSeq space
 * per direction (RFC 3261 §12.2) — and REPLACES the final it held: a second
 * final on one INVITE (a fork's 2xx, a re-emission) is its own final owed its
 * own ACK (§13.2.2.4), while a repeat the document folded is no step. The
 * newest transaction takes it whatever it holds, so a 2xx re-emitted after a
 * newer INVITE of its direction was answered reads as that INVITE's: an
 * authored shape, since the cut folds a repeat onto the step it repeats. A
 * final arriving before any INVITE of its direction opens the transaction
 * itself: the INVITE ran before the flow starts.
 *
 * An ACK discharges the newest transaction running its own way that holds a
 * final and no ACK yet. A non-2xx final consumes an ACK the same way
 * (§17.1.1.3), so an ACK sent while an older 2xx still waits — the 491 round of
 * a re-INVITE sent over an un-ACKed 2xx (§14.1) — discharges the newer
 * transaction and leaves the older one owed. The reading is by position: the
 * captured `cseq` is never read, and a leg whose ACKs run in the other order is
 * read the other way round. A fork's tag (`early`) is not read either — the
 * cut stamps before it names forks — so two forks' 2xx outstanding at once
 * share the one slot: the walk pairs each ACK with the fork's 2xx it follows,
 * and the lint pairs an ACK that names its fork by the tag instead.
 */
import { Flow } from "@sip/contracts"

/** One INVITE transaction a leg holds. */
export interface InviteTransaction<S extends Flow.Step> {
  /** The direction the INVITE ran: the op of its request step. */
  readonly op: Flow.Op
  /** The INVITE step; absent where the flow starts after it. */
  readonly invite?: S
  /** The final the leg holds for it, and where in the steps; absent while open. */
  final?: { readonly step: S; readonly at: number }
  /** Whether an ACK discharged that final. */
  acked: boolean
}

/** What one ACK step discharged: the transaction, and the final it held then. */
export interface Discharge<S extends Flow.Step> {
  readonly transaction: InviteTransaction<S>
  readonly final: S
}

export interface InviteTransactions<S extends Flow.Step> {
  /** Per leg, every transaction in the order it opened. */
  readonly byLeg: ReadonlyMap<string, ReadonlyArray<InviteTransaction<S>>>
  /** By final step id, the transaction it landed on. */
  readonly landings: ReadonlyMap<string, InviteTransaction<S>>
  /** By ACK step id, what it discharged; absent for an ACK nothing awaited. */
  readonly discharges: ReadonlyMap<string, Discharge<S>>
}

/** Replay the leg state of every leg over `steps`, in document order. */
export const inviteTransactions = <S extends Flow.Step>(
  steps: ReadonlyArray<S>
): InviteTransactions<S> => {
  const byLeg = new Map<string, Array<InviteTransaction<S>>>()
  const landings = new Map<string, InviteTransaction<S>>()
  const discharges = new Map<string, Discharge<S>>()
  const on = (leg: string): Array<InviteTransaction<S>> => {
    const held = byLeg.get(leg)
    if (held !== undefined) return held
    const fresh: Array<InviteTransaction<S>> = []
    byLeg.set(leg, fresh)
    return fresh
  }
  const other = (op: Flow.Op): Flow.Op => (op === "send" ? "expect" : "send")
  steps.forEach((step, at) => {
    if (Flow.isRequest(step, "INVITE")) {
      on(step.leg).push({ op: step.op, invite: step, acked: false })
    } else if (Flow.isFinalToInvite(step)) {
      const held = on(step.leg)
      const op = other(step.op)
      let landing = held.findLast((t) => t.op === op)
      if (landing === undefined) {
        landing = { op, acked: false }
        held.push(landing)
      }
      landing.final = { step, at }
      landing.acked = false
      landings.set(step.id, landing)
    } else if (Flow.isRequest(step, "ACK")) {
      const awaiting = on(step.leg).findLast(
        (t) => t.op === step.op && t.final !== undefined && !t.acked
      )
      if (awaiting !== undefined) {
        awaiting.acked = true
        discharges.set(step.id, { transaction: awaiting, final: awaiting.final!.step })
      }
    }
  })
  return { byLeg, landings, discharges }
}
