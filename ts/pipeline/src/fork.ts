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
 * dialog-creating final: past it `in_dialog` says the step rides the dialog
 * the leg confirmed, and the fork that answered needs no name. Two steps past
 * it still name theirs, because they open and confirm a FURTHER dialog on the
 * leg: a later fork's own 2xx to the forked INVITE — every 2xx to an INVITE
 * is a dialog of its own that the UAC ACKs (RFC 3261 §13.2.2.4) — and the ACK
 * the leg EXPECTS for it, whose To-tag says which dialog it answers. An ACK
 * the leg SENDS is composed from the final it discharges and names none, like
 * every other request send that rides no early dialog.
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

/** Whether the captured message is a 2xx to an INVITE: a dialog-creating final. */
const createsDialog = (msg: Flows.Msg): boolean =>
  msg.summary.kind === "response" &&
  answersInvite(msg) &&
  msg.summary.status >= 200 &&
  msg.summary.status < 300

/**
 * The INVITE transaction of the leg a message belongs to, as {@link txnKey}
 * names it: a response travels opposite to the INVITE it answers, an ACK
 * travels with it.
 */
const transactionOf = (step: StepDraft, msg: Flows.Msg): string => {
  const sentByAgent = msg.summary.kind === "response" ? step.op === "expect" : step.op === "send"
  return txnKey(msg.summary.cseq.seq, sentByAgent)
}

/**
 * Whether a step may name the fork its captured message rode.
 *
 * A request the leg SENDS names one only where the method rides an early
 * dialog and the leg's is not yet confirmed: a plan refuses `early` on any
 * other request send, because such a send consumes a fork's tag without a
 * dialog to consume it from, and a session-refresh UPDATE inside the confirmed
 * dialog rides that dialog. Any other step inside a confirmed dialog names
 * none, but for the two that open and confirm a FURTHER dialog on the leg: a
 * 2xx to the forked INVITE under a tag other than the first 2xx's — a later
 * fork answering, its own dialog (RFC 3261 §13.2.2.4) — and the ACK the leg
 * expects for it, which the To-tag pairs with its dialog. `furtherDialog` says
 * whether the message is one of those: of the forked INVITE transaction, under
 * a tag the leg was not first answered under.
 */
const nameable = (step: StepDraft, msg: Flows.Msg, furtherDialog: boolean): boolean => {
  if (msg.summary.kind === "request") {
    const method = msg.summary.method.toUpperCase()
    if (step.op === "send") return step.in_dialog !== true && RIDES_EARLY.has(method)
    return step.in_dialog !== true || (method === "ACK" && furtherDialog)
  }
  return step.in_dialog !== true || (createsDialog(msg) && furtherDialog)
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
  /** `<leg> <transaction>` of every FORKED transaction. */
  const forked = new Set<string>()
  const forks: Array<Fork> = []
  for (const [leg, perTransaction] of rang) {
    const ordered = [...perTransaction.values()]
      .sort((a, b) => a.seq - b.seq || Number(a.sentByAgent) - Number(b.sentByAgent))
    for (const { seq, sentByAgent, tags } of ordered) {
      if (tags.length < 2) continue
      forked.add(`${leg} ${txnKey(seq, sentByAgent)}`)
      for (const tag of tags) {
        const early = `f${forks.length + 1}`
        named.set(`${leg} ${tag}`, early)
        forks.push({ leg, early, tag, steps: [] })
      }
    }
  }
  if (forks.length === 0) return []

  const rode = new Map(forks.map((f) => [f.early, [] as Array<string>]))
  /** Per leg, the tag its first dialog-creating 2xx answered under. */
  const answered = new Map<string, string>()
  for (const [i, step] of steps.entries()) {
    const msg = msgOf(flows, sources[i])
    const tag = toTag(msg)
    if (msg === undefined || tag === undefined) continue
    if (createsDialog(msg) && !answered.has(step.leg)) answered.set(step.leg, tag)
    const early = named.get(`${step.leg} ${tag}`)
    if (early === undefined) continue
    const furtherDialog =
      forked.has(`${step.leg} ${transactionOf(step, msg)}`) && answered.get(step.leg) !== tag
    if (!nameable(step, msg, furtherDialog)) continue
    step.early = early
    rode.get(early)!.push(step.id)
  }
  // A name no step ended up carrying is no dialog the document opens: the one
  // response that rang it sits inside a confirmed dialog, where §6.1 has the
  // `in_dialog` marker say which dialog it is.
  return forks.filter((f) => rode.get(f.early)!.length > 0)
    .map((f) => ({ ...f, steps: rode.get(f.early)! }))
}
