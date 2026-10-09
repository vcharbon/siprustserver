/**
 * The captured ACKs to a non-2xx final that ride no transaction the replay's
 * stack composes.
 *
 * The ACK to a non-2xx final belongs to the INVITE client transaction (RFC 3261
 * §17.1.1.3): one per final, on the INVITE's own branch, and the stack composes
 * every auto ACK step there. A peer that answers one final with several ACKs on
 * fresh branches sends requests no server transaction matches (§17.2.3); as
 * auto steps they would all leave on the INVITE's branch with different bytes.
 * So each final, its retransmissions read as the final they repeat, keeps the
 * ACKs of ONE branch per hop (the final's own, else the first ACK's), and the
 * ACKs on other branches, with their repeats, are dropped.
 */
import { Flows } from "@sip/contracts"

/** The `<leg>:<msg>` coordinates of every dropped ACK. */
export const offTransactionAcks = (flows: Flows.FlowsDoc): ReadonlySet<string> => {
  const dropped = new Set<string>()
  flows.legs.forEach((leg, legIdx) => {
    const byFinal = new Map<number, Array<number>>()
    leg.msgs.forEach((msg, i) => {
      if (msg.repeat_of !== undefined || !Flows.isMethod(msg, "ACK")) return
      const copy = ackedFinal(leg.msgs, i)
      if (copy === undefined) return
      const final = originalOf(leg.msgs, copy)
      if (finalStatus(leg.msgs[final]!) < 300) return
      byFinal.set(final, [...(byFinal.get(final) ?? []), i])
    })
    for (const [final, acks] of byFinal) {
      if (acks.length < 2) continue
      const own = topBranch(leg.msgs[final]!)
      const kept = acks.find((i) => own !== undefined && topBranch(leg.msgs[i]!) === own) ?? acks[0]!
      const branch = topBranch(leg.msgs[kept]!)
      for (const i of acks) if (topBranch(leg.msgs[i]!) !== branch) dropped.add(`${legIdx}:${i}`)
    }
    leg.msgs.forEach((msg, i) => {
      if (msg.repeat_of !== undefined && dropped.has(`${legIdx}:${msg.repeat_of}`)) dropped.add(`${legIdx}:${i}`)
    })
  })
  return dropped
}

/**
 * The index of the INVITE final the ACK at `ack` answers: the nearest earlier
 * one on the same hop with the ACK's CSeq number and To tag.
 */
const ackedFinal = (msgs: ReadonlyArray<Flows.Msg>, ack: number): number | undefined => {
  const a = msgs[ack]!
  for (let i = ack - 1; i >= 0; i--) {
    const m = msgs[i]!
    if (m.hop !== a.hop || m.summary.kind !== "response" || m.summary.status < 200) continue
    if (m.summary.cseq.method.toUpperCase() !== "INVITE" || m.summary.cseq.seq !== a.summary.cseq.seq) continue
    if (m.summary.to.tag !== a.summary.to.tag) continue
    return i
  }
  return undefined
}

/** The message a retransmission at `index` repeats, followed to the first copy. */
const originalOf = (msgs: ReadonlyArray<Flows.Msg>, index: number): number => {
  let at = index
  for (let seen = 0; seen < msgs.length; seen++) {
    const before = msgs[at]!.repeat_of
    if (before === undefined || before >= at) return at
    at = before
  }
  return at
}

const finalStatus = (msg: Flows.Msg): number => (msg.summary.kind === "response" ? msg.summary.status : 0)

const topBranch = (msg: Flows.Msg): string | undefined => msg.via?.[0]?.branch ?? undefined
