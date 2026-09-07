/**
 * Early dialogs: which fork of a forking leg each step belongs to (§6.1
 * `early`).
 *
 * A forking peer rings several times on ONE INVITE transaction, and RFC 3261
 * §12.1.1 tells the dialogs apart by the To-tag the UAS minted per fork. A leg
 * that rang under one tag has no fork to name; a leg that rang under two has
 * two dialogs, and every step riding one says which.
 *
 * The field reads two ways, and which one is decided by the side that minted
 * the tag. On the leg our UAS ANSWERS the run mints a To-tag per fork and
 * answers under it. On the leg it RECEIVES the run learns the tag from the
 * arrival that opens the dialog, and two steps naming two forks can no longer
 * be satisfied by one tag — which is what makes a peer collapsing two early
 * dialogs into one a failure rather than a pass. The cut writes the same field
 * for both: the leg's role decides the reading, and this pass only states what
 * the capture showed.
 *
 * **The transaction is a CSeq number AND the side that sent the request.** A
 * leg runs one CSeq space per direction (RFC 3261 §12.2), and the two numbers
 * are not the same kind of thing: a CSeq our actor generates is replayed as it
 * was captured, while a CSeq the SUT manages is whatever arrives at run time
 * and our answer adapts to. Keyed on the number alone the two spaces collide,
 * bucketing the responses to the INVITE the leg TOOK with the responses to one
 * it later SENT. Within one leg and one direction a UAC's sequence is
 * monotonic, so an authentication retry and a re-INVITE each carry a number of
 * their own. A serial reroute reuses a CSeq across the attempts it makes, which
 * is why a corpus-wide survey has to key on the Via branch — but those attempts
 * are separate ORIGINAL legs and a pivot leg is one of them.
 *
 * **A confirmed dialog is not an early one.** Stamping stops at the leg's
 * dialog-creating final: past it the leg has adopted the fork's tag as its own
 * (RFC 3261 §12.2.1.1) and `in_dialog` is what says so. The 2xx itself is the
 * fork answering and carries the name.
 */
import { Flows } from "@sip/contracts"
import type { StepDraft } from "./draft.js"
import type { StepSource } from "./flowsteps.js"

/**
 * One INVITE transaction of one leg, and the To-tags it rang under in order.
 * `sentByAgent` says which of the leg's two CSeq spaces `seq` belongs to.
 */
interface Transaction {
  readonly seq: number
  readonly sentByAgent: boolean
  readonly tags: Array<string>
}

const txnKey = (seq: number, sentByAgent: boolean): string => `${seq} ${sentByAgent}`

/** One early dialog the cut named, for the flag. */
export interface Fork {
  readonly leg: string
  readonly early: string
  /** The To-tag the CAPTURE rang it under — never the tag a replay uses. */
  readonly tag: string
  readonly steps: ReadonlyArray<string>
}

/**
 * The methods that run INSIDE an early dialog rather than waiting for a
 * confirmed one: PRACK (RFC 3262 §7.2) and UPDATE (RFC 3311 §5.1). Any other
 * request the leg SENDS names no fork — it either belongs to the INVITE
 * transaction, which needs no dialog, or to a dialog that has confirmed.
 */
const RIDES_EARLY = new Set(["PRACK", "UPDATE"])

const msgOf = (flows: Flows.FlowsDoc, src: StepSource | undefined): Flows.Msg | undefined =>
  src === undefined ? undefined : flows.legs[src.origLeg]?.msgs[src.msgIdx]

/** The To-tag the captured message carries, where it carries one. */
const toTag = (msg: Flows.Msg | undefined): string | undefined => msg?.summary.to.tag ?? undefined

/** Whether the captured message answers an INVITE transaction. */
const answersInvite = (msg: Flows.Msg): boolean => Flows.isResponseTo(msg, "INVITE")

/**
 * Whether a step may name the fork its captured message rode.
 *
 * A step already inside a confirmed dialog names none, and a request the leg
 * SENDS names one only where the method rides an early dialog: a plan refuses
 * `early` on any other request send, because such a send consumes a fork's tag
 * without a dialog to consume it from.
 */
const nameable = (step: StepDraft, msg: Flows.Msg): boolean => {
  if (step.in_dialog === true) return false
  if (msg.summary.kind !== "request") return true
  return step.op !== "send" || RIDES_EARLY.has(msg.summary.method.toUpperCase())
}

/**
 * Name every early dialog the capture rang and stamp `early` on the steps that
 * ride it. Mutates `steps`; `sources` is parallel to it.
 *
 * Ids are unique across the DOCUMENT, not per leg: a `${early:<id>.…}`
 * accessor names one dialog, and an id two legs declare names two.
 */
export const stampEarlyDialogs = (
  flows: Flows.FlowsDoc,
  steps: Array<StepDraft>,
  sources: ReadonlyArray<StepSource>
): Array<Fork> => {
  /** leg -> transaction -> the To-tags it rang under, in order. */
  const rang = new Map<string, Map<string, Transaction>>()
  steps.forEach((step, i) => {
    const msg = msgOf(flows, sources[i])
    const tag = toTag(msg)
    if (msg === undefined || tag === undefined || !answersInvite(msg)) return
    const perTransaction = rang.get(step.leg) ?? new Map<string, Transaction>()
    const seq = msg.summary.cseq.seq
    // A response travels opposite to the request it answers.
    const sentByAgent = step.op === "expect"
    const txn = perTransaction.get(txnKey(seq, sentByAgent)) ?? { seq, sentByAgent, tags: [] }
    if (!txn.tags.includes(tag)) txn.tags.push(tag)
    perTransaction.set(txnKey(seq, sentByAgent), txn)
    rang.set(step.leg, perTransaction)
  })

  /** `<leg> <tag>` -> the id it took, for every tag of a FORKED transaction. */
  const named = new Map<string, string>()
  const forks: Array<Fork> = []
  for (const [leg, perTransaction] of rang) {
    const ordered = [...perTransaction.values()]
      .sort((a, b) => a.seq - b.seq || Number(a.sentByAgent) - Number(b.sentByAgent))
    for (const { tags } of ordered) {
      if (tags.length < 2) continue
      for (const tag of tags) {
        const early = `f${forks.length + 1}`
        named.set(`${leg} ${tag}`, early)
        forks.push({ leg, early, tag, steps: [] })
      }
    }
  }
  if (forks.length === 0) return []

  const rode = new Map(forks.map((f) => [f.early, [] as Array<string>]))
  for (const [i, step] of steps.entries()) {
    const msg = msgOf(flows, sources[i])
    const tag = toTag(msg)
    if (msg === undefined || tag === undefined) continue
    const early = named.get(`${step.leg} ${tag}`)
    if (early === undefined || !nameable(step, msg)) continue
    step.early = early
    rode.get(early)!.push(step.id)
  }
  // A name no step ended up carrying is no dialog the document opens: the one
  // response that rang it sits inside a confirmed dialog, where §6.1 has the
  // `in_dialog` marker say which dialog it is.
  return forks.filter((f) => rode.get(f.early)!.length > 0)
    .map((f) => ({ ...f, steps: rode.get(f.early)! }))
}
