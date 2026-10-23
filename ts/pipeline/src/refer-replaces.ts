/**
 * REFER with `Replaces` is out of the pivot format's scope
 * (`PCAP2TEST_PIVOT_V3.md` §0.1): the format carries no attended-transfer
 * construct (RFC 3891), and a UAS has no discriminator for an INVITE that
 * names another dialog. A group carrying that mechanism is refused whole — a
 * case cut from half of an attended transfer would replay a transfer the
 * format cannot state.
 */
import { Flows } from "@sip/contracts"
import type { CaptureInput, CaptureRule } from "./capture-rules.js"
import type { RefusalEvidence } from "./policy.js"
import type { Finding } from "./refusal-rule.js"
import * as Wire from "./wire.js"

/** The reason token a refused REFER-with-Replaces case is greppable by. */
export const REFER_REPLACES = "refer-replaces-out-of-scope"

/**
 * The out-of-scope mechanism as it appears on the wire: a REFER naming another
 * dialog, either by a `Replaces` header of its own or by the `Replaces=`
 * parameter embedded in its `Refer-To` URI, and the transfer target's INVITE
 * carrying the same header. `Supported: replaces` is an option tag — the
 * mechanism advertised, not used — and never matches.
 */
const replacesIn = (m: Flows.Msg): string | undefined => {
  const refer = Flows.isMethod(m, "REFER")
  if (!refer && !Flows.isMethod(m, "INVITE")) return undefined
  if (Wire.hasHeader(m, "Replaces")) {
    return refer ? "REFER carries a Replaces header" : "INVITE carries a Replaces header"
  }
  if (!refer) return undefined
  const referTo = Wire.headerValue(m, "Refer-To")
  return referTo !== undefined && /[?&]\s*replaces\s*=/i.test(referTo)
    ? "REFER's Refer-To embeds a Replaces= parameter"
    : undefined
}

/** Every datagram in `groups` that carries the out-of-scope mechanism. */
export const findReferReplaces = (
  flows: Flows.FlowsDoc,
  groups: ReadonlyArray<number>
): Array<RefusalEvidence> => {
  const out: Array<RefusalEvidence> = []
  for (const group of groups) {
    for (const leg of flows.groups[group]?.legs ?? []) {
      const msgs = flows.legs[leg]?.msgs ?? []
      msgs.forEach((m, msg) => {
        const detail = replacesIn(m)
        if (detail !== undefined) out.push({ group, leg, msg, detail })
      })
    }
  }
  return out
}

/** Whether the case cut from `legs` carries the out-of-scope mechanism. */
const mechanismFinding = (input: CaptureInput): Finding | undefined => {
  const groups = Flows.groupsForLegs(input.flows, input.legs)
  const evidence = findReferReplaces(input.flows, groups)
  if (evidence.length === 0) return undefined
  const where = evidence
    .map((e) => `group ${e.group} leg ${e.leg} msg ${e.msg} (${e.detail})`)
    .join("; ")
  return {
    groups,
    evidence,
    line: `${input.capture}: case '${input.caseId}' EXCLUDED ${REFER_REPLACES} — ${where}`
  }
}

/** The rule: a group carrying REFER with `Replaces` yields no case. */
export const REFER_REPLACES_RULE: CaptureRule = {
  id: REFER_REPLACES,
  subject: "scope",
  disposition: "refuses",
  why: "the call carries an attended transfer (REFER with Replaces, RFC 3891), which the pivot format cannot state",
  refuses: mechanismFinding
}
