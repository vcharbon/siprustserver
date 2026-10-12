import { describe, expect, it } from "vitest"
import { exclusionCallIds, exclusionReasons, parseExclusions } from "../src/exclusion/parse.ts"

/** A census-driven refusal, in the shape the generator writes it. */
const censusRefusal = {
  caseId: "trace_204.pcap.gz-auto",
  reason: "sut-violates:no-ack-to-dialog-creating-2xx",
  callIds: ["1-137264948_132375232@172.20.245.4"],
  evidence: [
    {
      rule: "no-ack-to-dialog-creating-2xx",
      callId: "1-137264948_132375232@172.20.245.4",
      emitter: "172.22.245.246:5072",
      leg: 12,
      detail: "took the 200 confirming dialog tag 'SDvacs699' on CSeq 605367 and never ACKed it"
    }
  ],
  line: "trace_204.pcap.gz: case 'trace_204.pcap.gz-auto' EXCLUDED …"
}

/** A mechanism refusal: group/leg/msg coordinates, no call-ids. */
const mechanismRefusal = {
  caseId: "trace_9.pcap.gz-auto",
  reason: "refer-replaces-out-of-scope",
  groups: [2],
  evidence: [{ group: 2, leg: 4, msg: 7, detail: "REFER carries a Replaces header" }],
  line: "trace_9.pcap.gz: case 'trace_9.pcap.gz-auto' EXCLUDED refer-replaces-out-of-scope — …"
}

/** A cut artifact: legs and call-ids, no evidence list. */
const artifactRefusal = {
  caseId: "trace_9.pcap.gz-artifact-3",
  reason: "missing-upstream-leg",
  legs: [3],
  callIds: ["SDpbch601@172.20.245.145"],
  line: "trace_9.pcap.gz: case 'trace_9.pcap.gz-artifact-3' EXCLUDED missing-upstream-leg — …"
}

describe("parseExclusions", () => {
  it("reads a census refusal down to the charged socket and its coordinates", () => {
    const read = parseExclusions([censusRefusal])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(read.records).toHaveLength(1)
    const entry = read.records[0]!
    expect(entry.caseId).toBe("trace_204.pcap.gz-auto")
    expect(entry.reason).toBe("sut-violates:no-ack-to-dialog-creating-2xx")
    expect(entry.callIds).toEqual(["1-137264948_132375232@172.20.245.4"])
    expect(entry.evidence[0]?.emitter).toBe("172.22.245.246:5072")
    expect(entry.evidence[0]?.leg).toBe(12)
    expect(entry.evidence[0]?.detail).toContain("never ACKed it")
    expect(entry.line).toContain("EXCLUDED")
    expect(read.warnings).toEqual([])
  })

  it("reads the mechanism and artifact shapes with the same reader", () => {
    const read = parseExclusions([mechanismRefusal, artifactRefusal])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(read.records.map((entry) => entry.reason)).toEqual([
      "refer-replaces-out-of-scope",
      "missing-upstream-leg"
    ])
    expect(read.records[0]?.groups).toEqual([2])
    expect(read.records[0]?.evidence[0]?.msg).toBe(7)
    expect(read.records[1]?.legs).toEqual([3])
    expect(read.records[1]?.evidence).toEqual([])
  })

  it("refuses anything that is not an array of refusals", () => {
    expect(parseExclusions({ caseId: "x", reason: "y" })).toEqual({
      ok: false,
      reason: "an exclusion file is a JSON array of refusals"
    })
    expect(parseExclusions(null).ok).toBe(false)
    expect(parseExclusions("[]").ok).toBe(false)
  })

  it("warns about a record it cannot read instead of dropping it silently", () => {
    const read = parseExclusions([censusRefusal, { line: "no case, no reason" }, 7])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(read.records).toHaveLength(1)
    expect(read.warnings).toEqual([
      "entry 1 names no case and no reason; it is not drawn",
      "entry 2 names no case and no reason; it is not drawn"
    ])
  })

  it("says an empty file holds no refusal rather than reading as a clean case", () => {
    const read = parseExclusions([])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(read.records).toEqual([])
    expect(read.warnings).toEqual(["the file holds no refusal"])
  })

  it("keeps an unknown reason token: the categories are the generator's", () => {
    const read = parseExclusions([{ caseId: "c", reason: "invented-tomorrow" }])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(read.records[0]?.reason).toBe("invented-tomorrow")
    expect(read.records[0]?.line).toBe("")
  })

  it("lists the reasons and the calls a listing badges the entry with", () => {
    const read = parseExclusions([
      censusRefusal,
      { ...censusRefusal, reason: "sut-violates:unacked-reliable-provisional" }
    ])
    expect(read.ok).toBe(true)
    if (!read.ok) return
    expect(exclusionReasons(read.records)).toEqual([
      "sut-violates:no-ack-to-dialog-creating-2xx",
      "sut-violates:unacked-reliable-provisional"
    ])
    // The same call charged under two rules is named once.
    expect(exclusionCallIds(read.records)).toEqual(["1-137264948_132375232@172.20.245.4"])
  })
})
