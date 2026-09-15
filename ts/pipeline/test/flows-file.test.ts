/**
 * The flows document read per leg: the segments are the file, and no reader
 * ever holds the file as one value.
 *
 * A document of a few thousand calls is larger than V8 will hold as one string,
 * so the claim under test is a MEMORY claim as much as a parsing one: the file
 * is never read whole, and the largest piece any reader sees is one leg.
 */
import { FlowsFile, FlowsSegments } from "@sip/pipeline"
import { Buffer } from "node:buffer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it, vi } from "vitest"

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "flows-file-test-"))

afterAll(() => fs.rmSync(scratch, { recursive: true, force: true }))

const legOf = (index: number) => ({
  call_id: `call-${index}@10.0.0.1`,
  hops: [{ a: "10.0.0.1:5060", b: "10.0.0.2:5060" }],
  invite: null,
  final_status: 200,
  saw_180: false,
  terminated_by: "BYE",
  tokens: [`token-${index}`],
  msgs: [{
    ts_us: 1_000_000 + index,
    src: "10.0.0.1:5060",
    dst: "10.0.0.2:5060",
    hop: 0,
    // Wide enough that a document of a few thousand legs is megabytes, and
    // carrying the characters a scanner must not read as structure.
    raw: `INVITE sip:${index} SIP/2.0\r\nSubject: {"[,]"} \\ ${"x".repeat(2000)}\r\n\r\n`
  }]
})

/** A document of `legs` legs, written to a file the way `sipflow --enrich` writes one. */
const documentOf = (name: string, legs: number): string => {
  const file = path.join(scratch, name)
  const document = {
    schema: 5,
    emit_headers: [],
    decode_stats: {
      records: legs,
      non_ip: 0,
      non_udp: 0,
      snap_truncated: 0,
      datagrams: legs,
      fragments: 0,
      reassembled: 0,
      frag_dropped: 0,
      tail_truncated: 0
    },
    flow_stats: { sip_messages: legs, capture_dups: 0, parse_failed: 0, non_sip: 0 },
    legs: Array.from({ length: legs }, (_, index) => legOf(index)),
    groups: Array.from({ length: legs }, (_, index) => ({
      legs: [index],
      t0_us: 1_000_000 + index,
      initial_invite: { leg: index, msg: 0 },
      methods: { INVITE: { requests: 1, content_types: [] } }
    }))
  }
  fs.writeFileSync(file, JSON.stringify(document, null, 2))
  return file
}

const LEGS = 3_000
const big = documentOf("big.flows.json", LEGS)

describe("FlowsSegments.segments", () => {
  it("is the file, byte for byte, concatenated in order", () => {
    const parts = [...FlowsSegments.segments(big)].map((segment) => Buffer.from(segment.bytes))
    expect(Buffer.concat(parts).equals(fs.readFileSync(big))).toBe(true)
  })

  it("names one segment per leg, between the envelope's two halves", () => {
    const kinds = [...FlowsSegments.segments(big)].map((segment) => segment.kind)
    expect(kinds[0]).toBe("head")
    expect(kinds[kinds.length - 1]).toBe("tail")
    expect(kinds.filter((kind) => kind === "leg")).toHaveLength(LEGS)
  })

  it("holds one leg at a time, never a share of the file", () => {
    const size = fs.statSync(big).size
    const widest = Math.max(
      ...[...FlowsSegments.segments(big)]
        .filter((segment) => segment.kind === "leg")
        .map((segment) => segment.bytes.length)
    )
    expect(widest).toBeLessThan(size / 100)
  })

  it("carries each leg's own JSON text apart from the separator behind it", () => {
    for (const segment of FlowsSegments.segments(big)) {
      if (segment.kind !== "leg") continue
      const text = Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")
      expect(JSON.parse(text)).toEqual(legOf(segment.index))
    }
  })

  it("reads a document whose legs array is empty", () => {
    const file = documentOf("empty.flows.json", 0)
    const parts = [...FlowsSegments.segments(file)]
    expect(parts.map((segment) => segment.kind)).toEqual(["head", "tail"])
    expect(Buffer.concat(parts.map((p) => Buffer.from(p.bytes))).equals(fs.readFileSync(file)))
      .toBe(true)
  })

  it("refuses a document with no top-level legs array", () => {
    const file = path.join(scratch, "no-legs.flows.json")
    fs.writeFileSync(file, JSON.stringify({ schema: 5, groups: [] }))
    expect(() => [...FlowsSegments.segments(file)]).toThrow(/legs/)
  })
})

describe("FlowsFile.readFlowsFile", () => {
  it("assembles the document JSON.parse would have made of the whole file", () => {
    expect(FlowsFile.readFlowsFile(big)).toEqual(JSON.parse(fs.readFileSync(big, "utf8")))
  })

  it("never holds the document as one string", () => {
    const size = fs.statSync(big).size
    const parse = vi.spyOn(JSON, "parse")
    try {
      FlowsFile.readFlowsFile(big)
      const widest = Math.max(
        ...parse.mock.calls.map(([text]) => (typeof text === "string" ? text.length : 0))
      )
      expect(widest).toBeLessThan(size / 2)
    } finally {
      parse.mockRestore()
    }
  })
})
