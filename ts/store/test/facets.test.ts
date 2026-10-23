/**
 * The queue's chip counts, on their own, over the GROUPS the statement
 * aggregates: one per distinct answer of the pool (the scope's and the
 * capture's rows), with how many calls give it.
 *
 * The one rule a reader relies on: a chip's count answers "in this capture and
 * scope, with the excluded values hidden, how many calls died on X" — so the
 * counts follow the excludes and ignore the include chips themselves, or
 * clicking one chip would zero every other. An excluded chip itself answers
 * how many calls it hides. The scope and the capture are the statement's:
 * `calls.test.ts` states them against the store.
 */
import { describe, expect, it } from "vitest"
import type { CallRow } from "../src/calls.js"
import { facets, type Group, type Query } from "../src/facets.js"
import type { MainError } from "../src/model.js"
import { groupsOf, pool, queueOf } from "../src/memory-queue.js"

const rfc = (rule: string, cite: string | null): MainError => ({
  kind: "rfc",
  rule,
  rfc: cite,
  step: null,
  expected: null,
  detail: `${rule} — ${cite ?? "uncited"}`
})

const died = (step: string, expected: string): MainError => ({
  kind: "died",
  rule: null,
  rfc: null,
  step,
  expected,
  detail: `no ${expected}`
})

const failure = (tag: string): MainError => ({
  kind: "failure",
  rule: tag,
  rfc: null,
  step: null,
  expected: null,
  detail: tag
})

const refusal = (reason: string): MainError => ({
  kind: "refused",
  rule: reason,
  rfc: null,
  step: null,
  expected: null,
  detail: reason
})

/** A refused row's fields: the first reason leads, every reason is carried. */
const refused = (...reasons: ReadonlyArray<string>): Partial<CallRow> => ({
  kind: "refused",
  runId: null,
  setKind: null,
  prepId: 7,
  bundlePath: null,
  reasons,
  mainError: refusal(reasons[0]!)
})

const row = (
  capture: string,
  caseId: string,
  rest: Partial<CallRow> = {}
): CallRow => ({
  capture,
  caseId: `${capture}-${caseId}`,
  kind: "case",
  callKey: `${caseId}@key`,
  runId: 1,
  setKind: "full",
  prepId: null,
  bundlePath: `/w/${capture}/run1`,
  documentHash: null,
  passed: false,
  skipped: null,
  failedStep: null,
  rfcGating: null,
  reasons: [],
  features: [],
  mainError: null,
  finishedAt: "2026-09-15T00:00:00.000Z",
  ...rest
})

const rows: ReadonlyArray<CallRow> = [
  row("capture_a.pcap.gz", "case1", { mainError: rfc("rport-echo", "RFC 3581 §4") }),
  row("capture_a.pcap.gz", "case2", { mainError: died("s14", "200 to INVITE"), failedStep: "s14" }),
  row("capture_a.pcap.gz", "case3", { passed: true }),
  row("capture_b.pcap.gz", "case1", { mainError: rfc("rport-echo", null) }),
  row("capture_b.pcap.gz", "case2", { mainError: died("s3", "BYE"), failedStep: "s3" }),
  row("capture_b.pcap.gz", "case3", { skipped: "blocked:no-mrf" }),
  row("capture_b.pcap.gz", "cut4", refused("egress-answer-not-captured")),
  row("B231e.01_222706_sip_vHKH", "case1", { mainError: died("s14", "200 to INVITE"), failedStep: "s14" }),
  row("B231e.01_222706_sip_vHKH", "case2", { mainError: rfc("no-200-after-cancel", "RFC 3261 §9.1") }),
  row("capture_c.pcap.gz", "case1", { mainError: failure("check-failed") }),
  row("capture_c.pcap.gz", "case2", { mainError: refusal("interpreter-refused") }),
  row("capture_c.pcap.gz", "auto", refused("source-call-incomplete", "egress-answer-not-captured"))
]

/** The chips over the pool of a query: the groups the statement would hand back for it. */
const chips = (query: Query, over: ReadonlyArray<CallRow> = rows) => facets(groupsOf(pool(over, query)), query)

describe("facets", () => {
  it("counts the RFC rules with a citation, the failures, the expected messages and the refusals, most first", () => {
    expect(chips({ scope: "red" })).toEqual({
      rules: [
        { value: "rport-echo", count: 2, rfc: "RFC 3581 §4" },
        { value: "no-200-after-cancel", count: 1, rfc: "RFC 3261 §9.1" }
      ],
      failures: [
        { value: "check-failed", count: 1 },
        { value: "interpreter-refused", count: 1 }
      ],
      expected: [
        { value: "200 to INVITE", count: 2 },
        { value: "BYE", count: 1 }
      ],
      // A call refused under two reasons counts once under each.
      refusals: [
        { value: "egress-answer-not-captured", count: 2 },
        { value: "source-call-incomplete", count: 1 }
      ],
      // The outcomes in their own order, only those the population holds.
      features: [],
      sources: [],
      outcomes:[
        { value: "refused", count: 2 },
        { value: "failed", count: 8 }
      ]
    })
    expect(chips({ scope: "all" }).outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 8 },
      { value: "passed", count: 1 },
      { value: "skipped", count: 1 }
    ])
    // The default population holds no refusal, so no refusal chip is offered on it.
    const failed = chips({})
    expect(failed.refusals).toEqual([])
    expect(failed.outcomes).toEqual([{ value: "failed", count: 8 }])
  })

  it("counts the pool it is handed — the scope's and the capture's — and ignores the chip filters", () => {
    const narrowed = chips({ capture: "capture_a", rule: "no-200-after-cancel", expected: "BYE" })
    expect(narrowed.rules).toEqual([{ value: "rport-echo", count: 1, rfc: "RFC 3581 §4" }])
    expect(narrowed.failures).toEqual([])
    expect(narrowed.expected).toEqual([{ value: "200 to INVITE", count: 1 }])
    expect(narrowed.refusals).toEqual([])

    expect(chips({ scope: "passed" })).toEqual({
      rules: [],
      failures: [],
      expected: [],
      refusals: [],
      features: [],
      sources: [],
      outcomes:[{ value: "passed", count: 1 }]
    })
    expect(chips({ scope: "refused" }).refusals).toEqual([
      { value: "egress-answer-not-captured", count: 2 },
      { value: "source-call-incomplete", count: 1 }
    ])
    // The outcome include narrows the rows, not the counts, like every include.
    expect(chips({ outcome: "refused" }).rules).toHaveLength(2)
  })

  it("counts over the population with the excluded values hidden", () => {
    // Hiding one reason takes the two-reason call out of the other reason's
    // count too; the excluded chip itself counts the one call it hides.
    const hidden = chips({ scope: "red", exclude: { refusal: ["source-call-incomplete"] } })
    expect(hidden.refusals).toEqual([
      { value: "egress-answer-not-captured", count: 1 },
      { value: "source-call-incomplete", count: 1 }
    ])
    expect(hidden.outcomes).toEqual([
      { value: "refused", count: 1 },
      { value: "failed", count: 8 }
    ])
    // An excluded rule counts the calls it hides, with its citation; those
    // calls leave every other count.
    const rules = chips({ scope: "red", exclude: { rule: ["rport-echo"] } })
    expect(rules.rules).toEqual([
      { value: "rport-echo", count: 2, rfc: "RFC 3581 §4" },
      { value: "no-200-after-cancel", count: 1, rfc: "RFC 3261 §9.1" }
    ])
    expect(rules.failures).toEqual([
      { value: "check-failed", count: 1 },
      { value: "interpreter-refused", count: 1 }
    ])
    expect(rules.outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 6 }
    ])
    // An excluded outcome: the refused calls leave, and with them their
    // reasons; the outcome chip says how many it hides.
    const runs = chips({ scope: "red", exclude: { outcome: ["refused"] } })
    expect(runs.refusals).toEqual([])
    expect(runs.outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 8 }
    ])
    // Every capture still holds a failing case.
    // An include and an exclude at once: the exclude counts, the include does not.
    const both = chips({ scope: "red", outcome: "refused", exclude: { expected: ["200 to INVITE"] } })
    expect(both.expected).toEqual([
      { value: "200 to INVITE", count: 2 },
      { value: "BYE", count: 1 }
    ])
    expect(both.outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 6 }
    ])
  })

  it("counts an excluded chip as the calls it hides with the other excludes still applied", () => {
    // Both reasons excluded: `egress-answer-not-captured` hides the one call
    // not also hiding on `source-call-incomplete`; the two-reason call still
    // hides on the other, so `source-call-incomplete` lifts nothing and is not
    // listed (the page pins it at zero).
    const both = chips({ scope: "red", exclude: { refusal: ["source-call-incomplete", "egress-answer-not-captured"] } })
    expect(both.refusals).toEqual([{ value: "egress-answer-not-captured", count: 1 }])
    expect(both.outcomes).toEqual([{ value: "failed", count: 8 }])
    // An excluded failure tag is listed among the failures, never the rules.
    const tag = chips({ exclude: { rule: ["check-failed"] } })
    expect(tag.rules.map((one) => one.value)).toEqual(["rport-echo", "no-200-after-cancel"])
    expect(tag.failures).toEqual([
      { value: "check-failed", count: 1 },
      { value: "interpreter-refused", count: 1 }
    ])
    // Out of scope, an exclusion hides nothing and is not listed.
    expect(chips({ scope: "passed", exclude: { rule: ["rport-echo"] } }).rules).toEqual([])
  })

  it("cites a rule cited two ways by the lexically first citation, whatever order the groups come in", () => {
    const cited = (rfcs: ReadonlyArray<string>): ReadonlyArray<Group> =>
      rfcs.map((cite) => ({
        kind: "case",
        outcome: "failed",
        errorKind: "rfc",
        rule: "rport-echo",
        rfc: cite,
        expected: null,
        reasons: [],
        features: [],
        count: 1
      }))
    expect(facets(cited(["RFC B", "RFC A"]), {}).rules).toEqual([{ value: "rport-echo", count: 2, rfc: "RFC A" }])
    expect(facets(cited(["RFC A", "RFC B"]), {}).rules).toEqual([{ value: "rport-echo", count: 2, rfc: "RFC A" }])
  })

  it("counts a feature under each call carrying it, whatever the call's outcome, and hides it on exclusion", () => {
    const featured: ReadonlyArray<CallRow> = [
      row("capture_f.pcap.gz", "case1", { passed: true, features: ["reroute", "18x:transparent"] }),
      row("capture_f.pcap.gz", "case2", { mainError: died("s3", "BYE"), features: ["reroute"] })
    ]
    expect(chips({ scope: "all" }, featured).features).toEqual([
      { value: "reroute", count: 2 },
      { value: "18x:transparent", count: 1 }
    ])
    expect(chips({ scope: "all", exclude: { feature: ["18x:transparent"] } }, featured).features).toEqual([
      { value: "18x:transparent", count: 1 },
      { value: "reroute", count: 1 }
    ])
  })

  it("counts a source fault under its rule, passed or failed, as information, and narrows and hides by it", () => {
    const sourceFault = { rule: "prack-2xx-or-481", rfc: "RFC 3262 §3", detail: "uas1: answered 481" }
    const sourced: ReadonlyArray<CallRow> = [
      row("capture_s.pcap.gz", "case1", { passed: true, sourceFault }),
      row("capture_s.pcap.gz", "case2", { mainError: { ...died("s3", "BYE"), source: sourceFault }, sourceFault }),
      row("capture_s.pcap.gz", "case3", { mainError: died("s3", "BYE") })
    ]
    expect(chips({ scope: "all" }, sourced).sources).toEqual([{ value: "prack-2xx-or-481", count: 2 }])
    // Information: the failed count is the SUT's, whatever source fault a call carries.
    expect(chips({ scope: "all" }, sourced).outcomes).toEqual([
      { value: "failed", count: 2 },
      { value: "passed", count: 1 }
    ])
    const narrowed = queueOf(sourced, { scope: "all", source: "prack-2xx-or-481" }, 0)
    expect(narrowed.rows.map((one) => one.caseId)).toEqual(["capture_s.pcap.gz-case1", "capture_s.pcap.gz-case2"])
    const hidden = queueOf(sourced, { scope: "all", exclude: { source: ["prack-2xx-or-481"] } }, 0)
    expect(hidden.rows.map((one) => one.caseId)).toEqual(["capture_s.pcap.gz-case3"])
    expect(chips({ scope: "all", exclude: { source: ["prack-2xx-or-481"] } }, sourced).sources)
      .toEqual([{ value: "prack-2xx-or-481", count: 2 }])
  })

  it("breaks a tie on the value, so the chip order is stable", () => {
    const tied = chips({ capture: "B231e" })
    expect(tied.rules).toEqual([{ value: "no-200-after-cancel", count: 1, rfc: "RFC 3261 §9.1" }])
    const both = chips({}, [
      row("c", "1", { mainError: rfc("zeta", null) }),
      row("c", "2", { mainError: rfc("alpha", null) })
    ])
    expect(both.rules.map((one) => one.value)).toEqual(["alpha", "zeta"])
  })

  it("weights every chip by its group's count, an excluded chip included", () => {
    const groups: ReadonlyArray<Group> = [
      { kind: "case", outcome: "failed", errorKind: "rfc", rule: "rport-echo", rfc: "RFC 3581 §4", expected: null, reasons: [], features: [], count: 5 },
      { kind: "case", outcome: "failed", errorKind: "died", rule: null, rfc: null, expected: "BYE", reasons: [], features: [], count: 3 },
      { kind: "refused", outcome: "refused", errorKind: "refused", rule: null, rfc: null, expected: null, reasons: ["source-call-incomplete", "egress-answer-not-captured"], features: [], count: 4 },
      { kind: "refused", outcome: "refused", errorKind: "refused", rule: null, rfc: null, expected: null, reasons: ["egress-answer-not-captured"], features: [], count: 2 }
    ]
    expect(facets(groups, { scope: "red" })).toEqual({
      rules: [{ value: "rport-echo", count: 5, rfc: "RFC 3581 §4" }],
      failures: [],
      expected: [{ value: "BYE", count: 3 }],
      refusals: [
        { value: "egress-answer-not-captured", count: 6 },
        { value: "source-call-incomplete", count: 4 }
      ],
      features: [],
      sources: [],
      outcomes:[
        { value: "refused", count: 6 },
        { value: "failed", count: 8 }
      ]
    })
    const hidden = facets(groups, { scope: "red", exclude: { refusal: ["source-call-incomplete"] } })
    expect(hidden.refusals).toEqual([
      { value: "source-call-incomplete", count: 4 },
      { value: "egress-answer-not-captured", count: 2 }
    ])
    expect(hidden.outcomes).toEqual([
      { value: "refused", count: 2 },
      { value: "failed", count: 8 }
    ])
  })
})
