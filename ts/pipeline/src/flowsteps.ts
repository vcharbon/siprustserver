/**
 * Flow synthesis: EVERY captured message becomes a step. Transaction-derived
 * messages (100 Trying, ACK-to-final, PRACK and its 2xx) are marked `auto`
 * instead of being dropped, so an elided automatic and a message the capture
 * never carried stop looking alike; a retransmission collapses onto the step it
 * repeats as a count instead of vanishing — except where nothing paces the
 * copies, which is the unreliable provisional: that repeat stays its own step
 * (§6.9, §13.2).
 *
 * `auto` is a COMPOSITION marker, never a storage one (§6.3): such a step goes
 * through the same three-tier build as any other, so an ACK's frozen headers
 * and the delayed-offer answer riding it have the home the closed field list
 * denied them. The one thing the marker withholds is a body the stack could not
 * place — see {@link bodyStorable}.
 *
 * WHICH step a repeat collapses onto is the flows document's `repeat_of`, not a
 * search of its own: the extractor states the earliest message each one repeats,
 * under a request criterion that ignores the Via branch, so a peer re-ACKing
 * each retransmitted final with a fresh branch collapses like any other ladder.
 * That relation is BOUNDED by the transaction envelope (64·T1 = 32 s), so bytes
 * matching an earlier message past it arrive here carrying no `repeat_of` and
 * become their own step — a re-emission, never a count (§6.9).
 * A document whose producer did not compute the field falls back to `retx` alone
 * and SAYS so — never silently.
 *
 * Assert-vs-record is decided HERE and stated in the document (§6.4): relayed
 * content is asserted, a SUT-MINTED message is recorded. Authorship, not delay
 * causality: every message the SUT sends on a leg IT initiated is minted — a
 * B2BUA's egress INVITE propagates content AND mints the message — so every
 * expect at a UAS actor records; on the caller's leg an expect whose content no
 * captured cross-leg message relayed was minted too. No lane re-reads this.
 *
 * `in_dialog` is stamped here too, TOTALLY (§6.1): the method never says where a
 * transaction sits — an OPTIONS rides a dialog or does not — so every step after
 * its leg's dialog-creating final states the marker and `pivot-schema lint`
 * refuses a leg marked on only some of them. The ACK that ANSWERS that final
 * takes `confirms_dialog` in the same pass, which is what tells it from a
 * re-INVITE's ACK — both are in-dialog ACKs.
 */
import { Flows, Tokens, type Case, type Check } from "@sip/contracts"
import { claimedByBackground, type BackgroundMap } from "./background.js"
import type { ResourceFile } from "./bodies.js"
import { classify as classifyDelays, type DelayCausality, type StepTiming } from "./delay.js"
import type { MsgSpecDraft, StepDraft } from "./draft.js"
import { stampDrawnAckCounts } from "./drawn-ack.js"
import { deriveFarSideReinvites } from "./far-side-reinvite.js"
import { stampEarlyDialogs } from "./fork.js"
import { mirrorRelayedProvisionals } from "./mirrored-provisional.js"
import { stampSpareProvisionals } from "./spare-provisional.js"
import { buildMsg, isAutomatic, typeKey } from "./msgspec.js"
import type { PartsIndex } from "./parts.js"
import { stampOverlaps } from "./race.js"
import type { Plan } from "./plan.js"
import { peerSide, type ActorObs, type Layout } from "./topology.js"
import { inviteTransactions, type InviteTransaction } from "./transactions.js"
import { hasHeader } from "./wire.js"

/** The step id a generated step takes: dense, 1-based, `s<n>`. */
export const stepId = (n: number): string => `s${n}`

/**
 * The §9.1 class an asserted frozen header of this NAME carries, where the
 * deployment owns its vocabulary. What an unconfigured pipeline runs:
 * {@link NO_HEADER_CLASS}, which classifies nothing.
 */
export type HeaderClassifier = (name: string) => Check.CheckClass | undefined

export const NO_HEADER_CLASS: HeaderClassifier = () => undefined

/** Provenance of one step, back to the captured message. */
export interface StepSource {
  readonly id: string
  readonly pivotLeg: string
  readonly origLeg: number
  readonly msgIdx: number
  readonly emits: boolean
  readonly auto: boolean
  /**
   * The step was derived on ANOTHER leg from the message this coordinate names
   * (`far-side-reinvite.ts`): the coordinate is what the step is compared
   * against, and the message it names was captured on the other leg.
   */
  readonly mirrored?: true
}

export interface FlowOut {
  readonly steps: Array<StepDraft>
  readonly sources: Array<StepSource>
  /**
   * Each step's delay CAUSALITY, parallel to `steps`. The emitted `delay` keeps
   * the anchor and drops the reason it was chosen, and a lane adaptation has to
   * know the difference: `propagated` says the capture showed the platform
   * RELAYING this message across legs, `sut-originated` that the platform minted
   * it locally.
   */
  readonly derived: Array<DelayCausality>
  readonly resources: Array<ResourceFile>
  readonly flags: Array<Case.Flag>
  readonly t0_us: number
}

export const synthesize = (
  flows: Flows.FlowsDoc,
  layout: Layout,
  plan: Plan,
  headerClass: HeaderClassifier = NO_HEADER_CLASS,
  parts: PartsIndex = new Map(),
  background: BackgroundMap = new Map(),
  /**
   * Whether the captured platform relayed its provisionals AS THEY ARRIVED. A
   * platform running a rewrite mode emits one 18x by design, so its legs are
   * not one for one and the capture is missing nothing (§6.9).
   */
  transparent18x = true,
  /**
   * Whether the replaying platform relays an in-dialog INVITE end to end, so a
   * leg the vantage lost past its 2xx is owed the far side of the exchange the
   * other leg holds (`far-side-reinvite.ts`). A platform that answers one
   * itself derives nothing, and the run shows what it did.
   */
  relaysReinvite = false
): FlowOut => {
  interface Obs {
    actor: ActorObs
    origLeg: number
    msgIdx: number
    ts_us: number
  }
  const obs: Array<Obs> = []
  for (const a of layout.actorsObs) {
    for (const i of a.msgIdxs) {
      obs.push({
        actor: a,
        origLeg: a.origLeg,
        msgIdx: i,
        ts_us: flows.legs[a.origLeg]!.msgs[i]!.ts_us
      })
    }
  }
  obs.sort((x, y) => x.ts_us - y.ts_us || x.origLeg - y.origLeg || x.msgIdx - y.msgIdx)
  const t0 = obs.length > 0 ? obs[0]!.ts_us : 0

  // Exchanges a background policy claims (§5.1) collapse into the policy
  // instead of becoming steps — before ids, delays, and repeat collapse exist.
  const claimed = claimedByBackground(
    flows,
    obs.map((o) => ({
      actorId: o.actor.actorId,
      pivotLeg: o.actor.pivotLeg,
      emits: peerSide(o.actor, flows.legs[o.origLeg]!.msgs[o.msgIdx]!.src),
      origLeg: o.origLeg,
      msgIdx: o.msgIdx,
      ts_us: o.ts_us
    })),
    background
  )

  const steps: Array<StepDraft> = []
  const sources: Array<StepSource> = []
  /** Coordinates kept as their own step because nothing paces their copies. */
  const expanded: Array<string> = []
  const timings: Array<StepTiming> = []
  const resources: Array<ResourceFile> = []
  const flags: Array<Case.Flag> = []
  /**
   * Resource names are step-index-free: actor plus an ordinal over the messages
   * that actor sends, or over those it expects, so renumbering the flow never
   * renames a file on disk.
   */
  const ordinals = new Map<string, number>()
  /** Captured coordinate -> the step index it became, for `repeat_of`. */
  const stepOfMsg = new Map<string, number>()
  /** Step index -> capture time of the LAST copy on its ladder, for the gaps. */
  const rungAt = new Map<number, number>()
  const coord = (origLeg: number, msgIdx: number): string => `${origLeg}:${msgIdx}`
  const collapseOnRepeatOf = carriesRepeatOf(flows)
  if (!collapseOnRepeatOf) {
    flags.push({
      kind: "retransmit-collapse-legacy",
      detail:
        `flows schema ${flows.schema} states no repeat_of, so retransmissions collapse on ` +
        `retx alone: a peer re-answering with a fresh branch stays a separate step`
    })
  }

  if (claimed.size > 0) {
    flags.push({
      kind: "background-claimed",
      detail:
        `${claimed.size} captured message(s) collapsed into actor background ` +
        `policies (the captured platform's own keepalive exchanges): ` +
        [...claimed].sort().join(", ")
    })
  }

  for (const o of obs) {
    if (claimed.has(coord(o.origLeg, o.msgIdx))) continue
    const msg = flows.legs[o.origLeg]!.msgs[o.msgIdx]!
    const emits = peerSide(o.actor, msg.src)
    const auto = isAutomatic(msg)

    const repeated = collapseOnRepeatOf
      ? msg.repeat_of === undefined
        ? -1
        : stepOfMsg.get(coord(o.origLeg, msg.repeat_of)) ?? -1
      : msg.retx
        ? lastMatching(timings, o.actor.pivotLeg, emits, typeKey(msg), msg.summary.cseq.seq)
        : -1
    if (repeated >= 0 && !unpaced(msg)) {
      // The count says how many rungs; the gap says when this one went out
      // (§6.9). Measured from the emission BEFORE it — the ladder's head on the
      // first rung, the previous rung after that — so a peer whose ladder is
      // not the RFC's is replayed as it ran, and the two 18x of a capture stay
      // on the side of the PRACK the capture put them.
      const head = steps[repeated]!
      const previous = rungAt.get(repeated) ?? o.ts_us
      head.retransmits = (head.retransmits ?? 0) + 1
      head.retransmit_intervals_ms = [
        ...(head.retransmit_intervals_ms ?? []),
        Math.max(0, Math.round((o.ts_us - previous) / 1000))
      ]
      rungAt.set(repeated, o.ts_us)
      continue
    }
    if (repeated >= 0) expanded.push(`${o.actor.pivotLeg} leg${o.origLeg}/msg${o.msgIdx}`)

    const id = stepId(steps.length + 1)
    const spec: MsgSpecDraft = built(
      flows,
      layout,
      plan,
      o.actor,
      msg,
      emits,
      slug(o.actor, emits, ordinals),
      resources,
      flags,
      parts,
      !auto || bodyStorable(flows, o.origLeg, o.msgIdx, msg)
    )
    // The captured CSeq NUMBER, on a transaction-derived step only (§6.3): the
    // label of the transaction that obliged the message, which pairs it for
    // confrontation and scopes an auto ACK's DRAWN `retransmits`. Never
    // replayed — the stack numbers its own.
    if (auto) spec.cseq = msg.summary.cseq.seq

    timings.push({
      leg: o.actor.pivotLeg,
      emits,
      ts_us: o.ts_us,
      typeKey: typeKey(msg),
      cseq: msg.summary.cseq.seq,
      timerLinked: hasHeader(msg, "Session-Expires")
    })
    stepOfMsg.set(coord(o.origLeg, o.msgIdx), steps.length)
    rungAt.set(steps.length, o.ts_us)
    sources.push({
      id,
      pivotLeg: o.actor.pivotLeg,
      origLeg: o.origLeg,
      msgIdx: o.msgIdx,
      emits,
      auto
    })
    steps.push({
      id,
      leg: o.actor.pivotLeg,
      op: emits ? "send" : "expect",
      ...(auto ? { auto: true } : {}),
      msg: spec,
      delay: { ms: 0, from: TRIGGER, compressible: true, timer_linked: false },
      observed: { leg: o.origLeg, msg: o.msgIdx, at_us: o.ts_us - t0 }
    })
  }

  if (expanded.length > 0) {
    flags.push({
      kind: "unreliable-provisional-repeat-expanded",
      detail:
        `${expanded.length} repeat(s) of an unreliable provisional kept as their own step ` +
        `instead of a count, each with its own dwell and observed coordinate: ` +
        expanded.join(", ")
    })
  }

  // Before the delays, so a derived step is classified like any other — the
  // relay it is, the answer it emits (§6.9). Every array below indexes the
  // others, and the records hold step OBJECTS: the provisional pass renumbers
  // behind this one, and the flag is written once every id has settled.
  const farSide = relaysReinvite
    ? deriveFarSideReinvites({ steps, timings, sources })
    : { derived: [], left: [] }

  // Before the delays, so a derived arrival is classified like any other: the
  // relay it is (§6.9, issue 116). Every array below indexes the others.
  const derived = transparent18x
    ? mirrorRelayedProvisionals({ resources, sources, steps, timings })
    : []
  if (derived.length > 0) {
    flags.push({
      kind: "relayed-provisional-expect-derived",
      detail:
        `${derived.length} caller-facing provisional expectation(s) derived from the emission ` +
        `that causes them: an unreliable provisional does not retransmit, so each emission is ` +
        `its own message and a relaying B2BUA passes each one on, where this capture holds the ` +
        `arrival on one leg only (§6.9). Each copies a captured arrival, its coordinate ` +
        `included: ` +
        derived
          .map((d) => `${d.step} (leg ${d.leg}, ${d.status}, relays ${d.relays}, copies ${d.copies})`)
          .join("; ")
    })
  }

  if (farSide.derived.length > 0) {
    flags.push({
      kind: "far-side-reinvite-derived",
      detail:
        `${farSide.derived.length} in-dialog INVITE exchange(s) the capture holds on one leg only ` +
        `transcribed onto the far leg: that leg's record ends at the 2xx its peer sent, so the ` +
        `exchange the near leg carries has no counterpart there, and the replaying platform ` +
        `relays it (§6.9). Each derived step copies the near-leg message it mirrors, coordinate ` +
        `included: ` +
        farSide.derived
          .map(
            (d) =>
              `leg ${d.farLeg} (record ends at ${d.ended.id}): ${d.invite.id} expect INVITE ` +
              `mirrors ${d.nearInvite.id}, ${d.answer.id} send ${d.answer.msg.status} mirrors ` +
              `${d.nearAnswer.id}` +
              (d.ack === undefined || d.nearAck === undefined
                ? ""
                : `, ${d.ack.id} expect ACK mirrors ${d.nearAck.id}`)
          )
          .join("; ")
    })
  }
  if (farSide.left.length > 0) {
    flags.push({
      kind: "far-side-reinvite-not-derived",
      detail:
        `${farSide.left.length} in-dialog INVITE exchange(s) onto a leg whose record ends at its ` +
        `2xx were NOT transcribed, and the far leg scripts nothing for the relayed INVITE: ` +
        farSide.left.map((l) => `${l.nearInvite.id} onto leg ${l.farLeg} (${l.detail})`).join("; ")
    })
  }

  const delays = classifyDelays(timings)
  // Legs the SUT initiated: every message it sends there is a fresh transaction
  // it authored, however much of the content it propagated.
  const mintedLegs = new Set(
    layout.actorsObs.filter((a) => a.kind === "uas").map((a) => a.pivotLeg)
  )
  steps.forEach((step, i) => {
    const d = delays[i]!
    step.delay = {
      ms: d.ms,
      from: anchorId(d.from),
      compressible: !d.timer_linked,
      timer_linked: d.timer_linked
    }
    // An automatic is the stack's business either way, so its content is only
    // ever recorded; a message on a SUT-initiated leg is SUT-minted and
    // recorded (§6.4); otherwise a relayed expect is matched and a
    // SUT-originated one is not. An asserted message's origin-platform headers
    // are scoped per header (§9.1), never by weakening the whole check.
    if (step.op === "expect") {
      step.check =
        step.auto || mintedLegs.has(step.leg) || d.derived === "sut-originated"
          ? "record"
          : "assert"
      if (step.check === "assert" && step.msg.headers !== undefined) {
        step.msg = {
          ...step.msg,
          headers: step.msg.headers.map((h) => {
            const cls = headerClass(h.name)
            return cls === undefined ? h : { ...h, class: cls }
          })
        }
      }
    }
  })
  stampInDialog(steps)
  stampOverlaps(steps, delays.map((d) => d.derived))

  // After `in_dialog`, which is what says where an early dialog stops.
  const forks = stampEarlyDialogs(flows, steps, sources)
  if (forks.length > 0) {
    flags.push({
      kind: "early-dialogs-named",
      detail:
        `${forks.length} early dialog(s) named across ` +
        `${new Set(forks.map((f) => f.leg)).size} forking leg(s): a peer ringing one INVITE ` +
        `transaction under several To-tags rang several dialogs (RFC 3261 §12.1.1), and each ` +
        `step says which it rides. The captured tag is stated for provenance only — the run ` +
        `mints one where the leg answers and learns one where it receives: ` +
        forks
          .map((f) => `${f.early} (leg ${f.leg}, captured tag ${f.tag}, ${f.steps.join("/")})`)
          .join("; ")
    })
  }

  const drawn = stampDrawnAckCounts(flows, steps, sources)
  if (drawn.length > 0) {
    flags.push({
      kind: "ack-count-drawn-from-final",
      detail:
        `${drawn.length} ACK expectation(s) carry the count their answered final DRAWS ` +
        `rather than the ACKs the capture held (§6.3): ` +
        drawn
          .map((d) =>
            `${d.step} (leg ${d.leg}, ${d.drawn} drawn against ${d.final}, ` +
            `capture held ${d.captured})`
          )
          .join("; ")
    })
  }

  const spared = stampSpareProvisionals(steps)
  if (spared.length > 0) {
    flags.push({
      kind: "provisional-expect-surplus-tolerated",
      detail:
        `${spared.length} caller-facing provisional expectation(s) marked \`optional\`: the capture ` +
        `holds more relayed provisionals on the leg than peer emissions anchoring them, and a ` +
        `platform's spare copy is not a datagram a relay is caused to send (§6.9). Stamped at the ` +
        `END of each run, where a step of another status can release it: ` +
        spared
          .map((s) => `${s.step} (leg ${s.leg}, ${s.status}, ${s.surplus} of a ${s.run}-step run)`)
          .join("; ")
    })
  }

  return { steps, sources, derived: delays.map((d) => d.derived), resources, flags, t0_us: t0 }
}

const TRIGGER = Tokens.anchorToken({ _tag: "trigger" })

/**
 * Mark every step that runs after its leg's DIALOG-CREATING FINAL — the first
 * 2xx to an INVITE on that leg (§6.1). Strictly after: the dialog exists once
 * that final's To-tag arrives (RFC 3261 §13.2.2.4), so the ACK answering it is
 * already a request within the dialog, while §17.1.1.3's transaction-owned ACK
 * is the one to a non-2xx final — and a leg that never took a 2xx has no dialog
 * to be inside.
 *
 * A CANCEL, and any response to one, is excluded wherever it sits: it is scoped
 * to the INVITE transaction it cancels (RFC 3261 §9.1), never sent within a
 * dialog (§12.2), so a 200 to a CANCEL that crossed the answer is a race and not
 * a renegotiation.
 *
 * The ACK that ANSWERS the dialog-creating final takes `confirms_dialog` beside
 * its `in_dialog`: the ACK that DISCHARGES that final as the leg's state holds
 * it ({@link inviteTransactions}). A re-INVITE sent over the un-ACKed 2xx is
 * answered 491 (RFC 3261 §14.1) and its ACK is that transaction's own
 * (§17.1.1.3): it runs first and confirms nothing, and the 2xx's ACK behind it
 * is the confirming one. A re-INVITE's ACK stays plain `in_dialog`; an ACK to
 * a non-2xx final takes neither marker.
 * FIXME(fork): the spec wants one confirming ACK per answered fork; the cut
 * names no fork on an in-dialog step, so it stamps once per leg.
 *
 * A generated flow is FLAT — `alt` is authored-only — so document order is run
 * order and one forward pass states the whole rule. A lane delta may reorder
 * later, and never across this boundary: it only moves an ACK back onto the 2xx
 * it answers, which is at or after its own leg's dialog-creating final, and it
 * moves the step OBJECT, so both markers ride along.
 */
export const stampInDialog = (steps: Array<StepDraft>): void => {
  const { landings, discharges } = inviteTransactions(steps)
  /** Per leg, the transaction whose 2xx created the dialog. */
  const creating = new Map<string, InviteTransaction<StepDraft>>()
  const confirmed = new Set<string>()
  for (const step of steps) {
    if (!cancelScoped(step) && creating.has(step.leg)) step.in_dialog = true
    const discharged = discharges.get(step.id)
    if (
      discharged !== undefined &&
      discharged.transaction === creating.get(step.leg) &&
      isDialogCreatingFinal(discharged.final) &&
      !confirmed.has(step.leg)
    ) {
      step.confirms_dialog = true
      confirmed.add(step.leg)
    }
    if (isDialogCreatingFinal(step) && !creating.has(step.leg)) {
      creating.set(step.leg, landings.get(step.id)!)
    }
  }
}

const isDialogCreatingFinal = (step: StepDraft): boolean =>
  step.msg.status !== undefined &&
  step.msg.status >= 200 &&
  step.msg.status < 300 &&
  (step.msg["cseq-method"] ?? "").toUpperCase() === "INVITE"

/** Whether the message belongs to a CANCEL transaction rather than to a dialog. */
const cancelScoped = (step: StepDraft): boolean => {
  const method = step.msg.status === undefined ? step.msg.method : step.msg["cseq-method"]
  return (method ?? "").toUpperCase() === "CANCEL"
}

/**
 * The delay classifier anchors by step POSITION; a document anchors by step id.
 * The one place the two numberings meet, and it holds because generated ids are
 * dense and 1-based over the same list.
 */
const anchorId = (from: string): Tokens.Anchor => {
  const n = from.startsWith("step:") ? Number(from.slice(5)) : NaN
  return Number.isFinite(n)
    ? Tokens.anchorToken({ _tag: "step", step: stepId(n) })
    : Tokens.anchorToken({ _tag: "trigger" })
}

/**
 * A message's resource slug: the actor plus an ordinal over the scripted
 * messages it sends (`uas1_3`) or, in its own counter, over the messages it
 * expects (`uas1_r3`, §8.4). Every message takes an ordinal, whether or not it
 * turns out to carry a body — the counter is the message's identity, not its
 * payload's, so adding a body to a message never renumbers its neighbours'
 * files, and the two counters never name one file.
 */
const slug = (actor: ActorObs, emits: boolean, ordinals: Map<string, number>): string => {
  const key = emits ? actor.actorId : `${actor.actorId} expects`
  const n = ordinals.get(key) ?? 0
  ordinals.set(key, n + 1)
  return emits ? `${actor.actorId}_${n}` : `${actor.actorId}_r${n}`
}

/**
 * Whether the stack has anywhere to put this transaction-derived message's body
 * (§6.3). Three classes carry one: the ACK to a 2xx, which is where a delayed
 * offer's ANSWER rides (RFC 3261 §13.2.1), and PRACK with its 2xx (RFC 3262
 * §5). A 100 Trying negotiates nothing, and an ACK to a non-2xx is absorbed by
 * the INVITE transaction (RFC 3261 §17.1.1.3) and reaches no TU that could read
 * one — so a payload stored there would be emitted by nobody.
 */
const bodyStorable = (
  flows: Flows.FlowsDoc,
  origLeg: number,
  msgIdx: number,
  msg: Flows.Msg
): boolean => {
  if (msg.summary.kind === "response") {
    return msg.summary.status >= 200 &&
      msg.summary.status < 300 &&
      msg.summary.cseq.method.toUpperCase() === "PRACK"
  }
  const method = msg.summary.method.toUpperCase()
  if (method === "PRACK") return true
  return method === "ACK" && ackedFinalIs2xx(flows, origLeg, msgIdx, msg.summary.cseq.seq)
}

/**
 * Whether the final this ACK answers was a 2xx, read off the ACK's own captured
 * leg: the nearest earlier INVITE final sharing its CSeq number. A leg holding
 * no such final reads as non-2xx — a body the stack cannot place is worse than
 * a body the capture never had, and a capture missing its own final is issue 71.
 */
const ackedFinalIs2xx = (
  flows: Flows.FlowsDoc,
  origLeg: number,
  msgIdx: number,
  cseq: number
): boolean => {
  const msgs = flows.legs[origLeg]!.msgs
  for (let i = msgIdx - 1; i >= 0; i--) {
    const m = msgs[i]!
    if (m.summary.kind !== "response" || m.summary.status < 200) continue
    if (m.summary.cseq.seq !== cseq) continue
    if (m.summary.cseq.method.toUpperCase() !== "INVITE") continue
    return m.summary.status < 300
  }
  return false
}

const built = (
  flows: Flows.FlowsDoc,
  layout: Layout,
  plan: Plan,
  actor: ActorObs,
  msg: Flows.Msg,
  emits: boolean,
  slugText: string,
  resources: Array<ResourceFile>,
  flags: Array<Case.Flag>,
  parts: PartsIndex,
  bodyIsStorable: boolean
): MsgSpecDraft => {
  const out = buildMsg(flows, layout, plan, actor, msg, emits, slugText, parts, bodyIsStorable)
  resources.push(...out.resources)
  flags.push(...out.flags)
  return out.spec
}

/**
 * Whether a repeat of this message would name a ladder nothing paces.
 *
 * An unreliable provisional — a 1xx carrying no `RSeq` — is re-sent at the
 * transaction user's discretion: RFC 3261 states no timer for it and RFC 3262 §3
 * paces only the RELIABLE one. A `retransmits` count carries the number and
 * borrows the PACING from the message class (§6.9), so on this class the count
 * asks for an interval that does not exist and the interpreter refuses it. The
 * repeat therefore stays its own step, keeping the measured dwell and the
 * `observed` coordinate a count cannot hold.
 *
 * Both directions, not just the `send` the interpreter refuses: one propagated
 * repeat is one event seen at two vantages, and a count on the `expect` half
 * would encode the two legs of it two different ways.
 */
const unpaced = (msg: Flows.Msg): boolean =>
  msg.summary.kind === "response" &&
  msg.summary.status >= 100 &&
  msg.summary.status < 200 &&
  !hasHeader(msg, "RSeq")

/**
 * Whether this document's producer computed `repeat_of` at all.
 *
 * The extractor decides `retx` and `repeat_of` together, so `retx` IMPLIES the
 * field by construction: a `retx` message carrying none is a document from a
 * producer that did not compute it. Absence is not "no repeats": it is a reason
 * to keep the old collapse and warn.
 */
const carriesRepeatOf = (flows: Flows.FlowsDoc): boolean =>
  flows.schema >= Flows.EMIT_SCHEMA_VERSION &&
  flows.legs.every((l) => l.msgs.every((m) => !m.retx || m.repeat_of !== undefined))

/**
 * The step a retransmission repeats: same leg, direction, message type AND
 * transaction. Type alone collapses a re-INVITE's retransmitted final onto the
 * initial INVITE's step, which is how a hold reads as longer than the call.
 *
 * The LEGACY path, for a document carrying no `repeat_of`.
 */
const lastMatching = (
  timings: ReadonlyArray<StepTiming>,
  leg: string,
  emits: boolean,
  key: string,
  cseq: number
): number => {
  for (let i = timings.length - 1; i >= 0; i--) {
    const t = timings[i]!
    if (t.leg === leg && t.emits === emits && t.typeKey === key && t.cseq === cseq) return i
  }
  return -1
}

/** Whether this step's message is the first INVITE the named UAS receives. */
export const isClaimStep = (
  steps: ReadonlyArray<StepDraft>,
  index: number,
  uasLegs: ReadonlySet<string>
): boolean => {
  const s = steps[index]!
  if (s.op !== "expect" || !uasLegs.has(s.leg)) return false
  if ((s.msg.method ?? "").toUpperCase() !== "INVITE") return false
  for (let i = 0; i < index; i++) {
    const p = steps[i]!
    if (p.op === "expect" && p.leg === s.leg && (p.msg.method ?? "").toUpperCase() === "INVITE") {
      return false
    }
  }
  return true
}
