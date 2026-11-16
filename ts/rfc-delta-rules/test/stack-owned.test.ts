/**
 * The RFC rows: each names the difference its clause makes the stack's own and
 * leaves the same-shape defect visible, and no two of them match one probe.
 */
import { headerProbe } from "@sip/delta-rules/probes"
import { matchingRules, section, WHY_CAP } from "@sip/delta-rules/rule"
import { kindsOf } from "@sip/delta-rules/rule-on"
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import type { RfcFacts } from "../src/facts.js"
import { RFC_ROWS } from "../src/stack-owned.js"

const NO_FACTS: RfcFacts = { acknowledgedRSeqAtStep: new Map() }

const RULES = section<RfcFacts, "rfc">("rfc", RFC_ROWS)

interface Seen {
  readonly probe: Probe.Probe
  readonly facts?: RfcFacts
}

interface Fixture {
  readonly id: string
  /** Probes the row names. */
  readonly named: ReadonlyArray<Seen>
  /** Same-shape defects the row leaves visible, each with what it is. */
  readonly visible: ReadonlyArray<Seen & { readonly defect: string }>
}

const onBye: Probe.MsgScope = { kind: "request", method: "BYE", inDialog: true }
const on180: Probe.MsgScope = { kind: "response", status: 180, cseqMethod: "INVITE" }
const on200: Probe.MsgScope = { kind: "response", status: 200, cseqMethod: "INVITE" }
const onPrack: Probe.MsgScope = { kind: "request", method: "PRACK", inDialog: true }

const ALICE = "<sip:+15555550100@192.0.2.10>"

const FIXTURES: ReadonlyArray<Fixture> = [
  {
    id: "via-stack-owned",
    named: [{
      probe: headerProbe({
        name: "Via",
        captured: ["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK776asdhds"],
        replayed: ["SIP/2.0/UDP 198.51.100.20:5060;branch=z9hG4bKnashds8"]
      })
    }],
    visible: [{
      defect: "a BYE the replay sent with no Via",
      probe: headerProbe({ name: "Via", scope: onBye, captured: ["SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK776asdhds"] })
    }]
  },
  {
    id: "call-id-stack-owned",
    named: [{ probe: headerProbe({ name: "Call-ID", captured: ["a84b4c76e66710@192.0.2.10"], replayed: ["f81d4fae7dec@198.51.100.20"] }) }],
    visible: [{
      defect: "a BYE the replay sent with no Call-ID",
      probe: headerProbe({ name: "Call-ID", scope: onBye, captured: ["a84b4c76e66710@192.0.2.10"] })
    }]
  },
  {
    id: "content-length-stack-owned",
    named: [
      { probe: headerProbe({ name: "Content-Length", captured: ["142"], replayed: ["158"] }) },
      { probe: headerProbe({ name: "Content-Length", scope: onBye, captured: [], replayed: ["0"] }) }
    ],
    visible: [
      { defect: "a body the replay carries where the capture carried none", probe: headerProbe({ name: "Content-Length", captured: ["0"], replayed: ["158"] }) },
      { defect: "a body the replay dropped", probe: headerProbe({ name: "Content-Length", captured: ["142"], replayed: [] }) }
    ]
  },
  {
    id: "multipart-boundary-stack-owned",
    named: [{
      probe: headerProbe({
        name: "Content-Type",
        captured: ["multipart/mixed;boundary=unique-boundary-1"],
        replayed: ["multipart/mixed;boundary=boundary42"]
      })
    }],
    visible: [
      {
        defect: "a parameter other than the boundary dropped",
        probe: headerProbe({
          name: "Content-Type",
          captured: ["multipart/mixed;boundary=a;type=application/sdp"],
          replayed: ["multipart/mixed;boundary=b"]
        })
      },
      {
        defect: "another multipart subtype",
        probe: headerProbe({ name: "Content-Type", captured: ["multipart/mixed;boundary=a"], replayed: ["multipart/alternative;boundary=b"] })
      }
    ]
  },
  {
    id: "max-forwards-stack-owned",
    named: [
      { probe: headerProbe({ name: "Max-Forwards", captured: ["70"], replayed: ["69"] }) },
      { probe: headerProbe({ name: "Max-Forwards", scope: on200, captured: ["70"], replayed: [] }) }
    ],
    visible: [{
      defect: "a BYE the replay sent with no Max-Forwards",
      probe: headerProbe({ name: "Max-Forwards", scope: onBye, captured: ["70"] })
    }]
  },
  {
    id: "cseq-number-stack-owned",
    named: [{ probe: headerProbe({ name: "CSeq", captured: ["314159 INVITE"], replayed: ["1 INVITE"] }) }],
    visible: [
      { defect: "another method", probe: headerProbe({ name: "CSeq", captured: ["314159 INVITE"], replayed: ["314159 BYE"] }) },
      { defect: "a CSeq the replay dropped", probe: headerProbe({ name: "CSeq", scope: onBye, captured: ["2 BYE"], replayed: [] }) }
    ]
  },
  {
    id: "rseq-stack-owned",
    named: [{ probe: headerProbe({ name: "RSeq", scope: on180, captured: ["988789"], replayed: ["1"] }) }],
    visible: [
      { defect: "an RSeq the replay omits", probe: headerProbe({ name: "RSeq", scope: on180, captured: ["988789"], replayed: [] }) },
      { defect: "an RSeq the capture never carried", probe: headerProbe({ name: "RSeq", scope: on180, captured: [], replayed: ["1"] }) }
    ]
  },
  {
    id: "rack-cseq-stack-owned",
    named: [
      { probe: headerProbe({ name: "RAck", scope: onPrack, captured: ["988789 314159 INVITE"], replayed: ["988789 1 INVITE"] }) },
      {
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4711 1 INVITE"] }),
        facts: { acknowledgedRSeqAtStep: new Map([["s7", 4711]]) }
      }
    ],
    visible: [
      {
        defect: "a PRACK of a provisional neither side sent",
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4711 1 INVITE"] })
      },
      {
        defect: "a PRACK of another provisional than the actor's",
        probe: headerProbe({ name: "RAck", scope: onPrack, step: "s7", captured: ["988789 314159 INVITE"], replayed: ["4712 1 INVITE"] }),
        facts: { acknowledgedRSeqAtStep: new Map([["s7", 4711]]) }
      },
      {
        defect: "another method",
        probe: headerProbe({ name: "RAck", scope: onPrack, captured: ["988789 314159 INVITE"], replayed: ["988789 1 UPDATE"] })
      }
    ]
  },
  {
    id: "from-to-tag-stack-owned",
    named: [
      { probe: headerProbe({ name: "From", captured: [`${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=a6c85cf`] }) },
      { probe: headerProbe({ name: "To", scope: on200, captured: [`${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=314159`] }) }
    ],
    visible: [
      {
        defect: "a display name dropped beside the tag",
        probe: headerProbe({ name: "From", captured: [`"Alice" ${ALICE};tag=1928301774`], replayed: [`${ALICE};tag=a6c85cf`] })
      },
      {
        defect: "a user=phone dropped beside the tag",
        probe: headerProbe({
          name: "From",
          captured: ["<sip:+15555550100@192.0.2.10;user=phone>;tag=1928301774"],
          replayed: [`${ALICE};tag=a6c85cf`]
        })
      },
      {
        defect: "a ?headers component added beside the tag",
        probe: headerProbe({
          name: "From",
          captured: [`${ALICE};tag=1928301774`],
          replayed: ["<sip:+15555550100@192.0.2.10?Subject=x>;tag=a6c85cf"]
        })
      },
      {
        defect: "another user beside the tag",
        probe: headerProbe({ name: "From", captured: [`${ALICE};tag=1928301774`], replayed: ["<sip:+15555550101@192.0.2.10>;tag=a6c85cf"] })
      }
    ]
  }
]

const namers = (seen: Seen): ReadonlyArray<string> => matchingRules(RULES, seen.probe, seen.facts ?? NO_FACTS).map((rule) => rule.id)

describe("the RFC rows", () => {
  it("each have a fixture, one per row", () => {
    expect(FIXTURES.map((f) => f.id)).toEqual(RFC_ROWS.map((row) => row.id))
  })

  it("each name their own difference, and no other row names it", () => {
    const named = FIXTURES.flatMap((f) => f.named.map((seen) => ({ id: f.id, by: namers(seen) })))
    expect(named.filter(({ id, by }) => by.length !== 1 || by[0] !== id)).toEqual([])
  })

  it("leave every same-shape defect visible", () => {
    const taken = FIXTURES.flatMap((f) => f.visible.map((seen) => ({ id: f.id, defect: seen.defect, by: namers(seen) })))
    expect(taken.filter(({ by }) => by.length > 0)).toEqual([])
  })

  it("key on disjoint header families, so no two rows match one probe", () => {
    const families = RFC_ROWS.map((row) => {
      expect(kindsOf(row.on), row.id).toEqual(["header"])
      expect(row.on.family?.every((name) => !name.endsWith("-")), row.id).toBe(true)
      return row.on.family ?? []
    })
    const all = families.flat()
    expect(new Set(all).size).toBe(all.length)
  })

  it("each cite their clause in `rfc` and in `why`, within the why cap, with no section and no lane note", () => {
    const uncited = RFC_ROWS.filter((row) =>
      row.rfc === undefined || !/RFC \d+ §/.test(row.rfc) || !/RFC \d+ §/.test(row.why) || row.why.length > WHY_CAP ||
      row.lanes !== undefined || "section" in row
    )
    expect(uncited.map((row) => row.id)).toEqual([])
  })
})
