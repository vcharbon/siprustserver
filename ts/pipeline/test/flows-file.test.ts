/**
 * The flows document read per leg: the segments are the file, and no reader
 * ever holds the file as one value.
 *
 * A document of a few thousand calls is larger than V8 will hold as one string,
 * so the claim under test is a MEMORY claim as much as a parsing one: the file
 * is never read whole, and the largest piece any reader sees is one leg.
 */
import { Flows } from "@sip/contracts"
import { FlowsFile, FlowsIndex, FlowsSegments } from "@sip/pipeline"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
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
    retx: false,
    summary: {
      kind: "request",
      method: "INVITE",
      uri: `sip:${index}@10.0.0.2`,
      cseq: { seq: 1, method: "INVITE" },
      from: { uri: `sip:caller-${index}@10.0.0.1`, tag: "f1" },
      to: { uri: `sip:${index}@10.0.0.2`, tag: null }
    },
    // Wide enough that a document of a few thousand legs is megabytes, and
    // carrying the characters a scanner must not read as structure.
    raw: `INVITE sip:${index} SIP/2.0\r\nSubject: {"[,]"} \\ ${"x".repeat(2000)}\r\n\r\n`
  }]
})

/** A document of `legs` legs, indented the way `sipflow --enrich` writes one. */
const documentOf = (name: string, legs: number, indent = 2): string => {
  const file = path.join(scratch, name)
  const document = documentValue(legs)
  fs.writeFileSync(file, JSON.stringify(document, null, indent))
  return file
}

/** The value `documentOf` writes: a whole flows document the contract decodes. */
const documentValue = (legs: number) => ({
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
    evidence: [],
    t0_us: 1_000_000 + index,
    initial_invite: { leg: index, msg: 0 },
    methods: { INVITE: { requests: 1, content_types: [] } }
  }))
})

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

  it("names each leg's own text inside its segment, on a document with no indentation", () => {
    // Nothing promises a pretty-printed emitter, and a separator of one byte is
    // where a `json` length read off the wrong end stops working.
    const file = path.join(scratch, "compact.flows.json")
    const legs = [legOf(0), legOf(1), legOf(2)]
    fs.writeFileSync(file, JSON.stringify({ schema: 5, legs, groups: [] }))
    const read: Array<unknown> = []
    for (const segment of FlowsSegments.segments(file)) {
      if (segment.kind !== "leg") continue
      expect(segment.json).toBeGreaterThan(0)
      expect(segment.json).toBeLessThanOrEqual(segment.bytes.length)
      read.push(JSON.parse(Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")))
    }
    expect(read).toEqual(legs)
  })

  it("reads a legs array of bare values, the last one included", () => {
    const file = path.join(scratch, "bare.flows.json")
    fs.writeFileSync(file, `{"schema":5,"legs":["a",12,null],"groups":[]}`)
    const parts = [...FlowsSegments.segments(file)]
    expect(parts.map((segment) => segment.kind)).toEqual(["head", "leg", "leg", "leg", "tail"])
    expect(
      parts.filter((segment) => segment.kind === "leg")
        .map((segment) => JSON.parse(Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")))
    ).toEqual(["a", 12, null])
    expect(Buffer.concat(parts.map((p) => Buffer.from(p.bytes))).equals(fs.readFileSync(file)))
      .toBe(true)
  })

  it("reads a document whose legs array is empty", () => {
    const file = documentOf("empty.flows.json", 0)
    const parts = [...FlowsSegments.segments(file)]
    expect(parts.map((segment) => segment.kind)).toEqual(["head", "tail"])
    expect(Buffer.concat(parts.map((p) => Buffer.from(p.bytes))).equals(fs.readFileSync(file)))
      .toBe(true)
  })

  it("states each segment's offset in the file", () => {
    const bytes = fs.readFileSync(big)
    for (const segment of FlowsSegments.segments(big)) {
      expect(bytes.subarray(segment.at, segment.at + segment.bytes.length).equals(Buffer.from(segment.bytes)))
        .toBe(true)
    }
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

  it("reads a document with no indentation", () => {
    const file = path.join(scratch, "compact-read.flows.json")
    const document = { schema: 5, legs: [legOf(0), legOf(1)], groups: [{ legs: [0, 1] }] }
    fs.writeFileSync(file, JSON.stringify(document))
    expect(FlowsFile.readFlowsFile(file)).toEqual(document)
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

describe("FlowsFile.readFlowsFileDecoded", () => {
  const read = (file: string) => Effect.runSyncExit(FlowsFile.readFlowsFileDecoded(file))

  const decoded = (file: string): Flows.FlowsDoc => {
    const exit = read(file)
    if (Exit.isFailure(exit)) throw new Error(Cause.pretty(exit.cause))
    return exit.value
  }

  it("decodes the document decoding the whole file would have decoded", () => {
    expect(decoded(big)).toEqual(Flows.decodeFlowsSync(JSON.parse(fs.readFileSync(big, "utf8"))))
  })

  it("decodes a document with no indentation", () => {
    const file = documentOf("compact-decoded.flows.json", 4, 0)
    expect(decoded(file)).toEqual(Flows.decodeFlowsSync(JSON.parse(fs.readFileSync(file, "utf8"))))
  })

  it("applies the contract's defaults to a leg as the whole-document decode does", () => {
    // `probe` and `identities` are defaulted fields of a message: a leg decoded
    // apart carries them exactly as one decoded inside the document does.
    const msg = decoded(big).legs[7]!.msgs[0]!
    expect(msg.probe).toBe(0)
    expect(msg.identities).toEqual({ from: { uri: "", user: null, digits: null }, to: { uri: "", user: null, digits: null } })
  })

  it("names the index of the leg the contract refuses", () => {
    const file = path.join(scratch, "bad-leg.flows.json")
    const document = documentValue(3) as { legs: Array<unknown> }
    document.legs[1] = { ...legOf(1), final_status: "200" }
    fs.writeFileSync(file, JSON.stringify(document, null, 2))
    const exit = read(file)
    expect(Exit.isFailure(exit)).toBe(true)
    if (Exit.isFailure(exit)) {
      const error = Cause.squash(exit.cause)
      expect(error).toBeInstanceOf(FlowsFile.FlowsLegRefused)
      expect((error as FlowsFile.FlowsLegRefused).index).toBe(1)
      expect((error as FlowsFile.FlowsLegRefused).file).toBe(file)
    }
  })

  it("refuses an envelope the contract refuses, naming no leg", () => {
    const file = path.join(scratch, "bad-envelope.flows.json")
    const { decode_stats: _dropped, ...document } = documentValue(2)
    fs.writeFileSync(file, JSON.stringify(document, null, 2))
    const exit = read(file)
    expect(Exit.isFailure(exit)).toBe(true)
    if (Exit.isFailure(exit)) {
      expect(Cause.squash(exit.cause)).not.toBeInstanceOf(FlowsFile.FlowsLegRefused)
    }
  })

  it("never holds the document as one string", () => {
    const size = fs.statSync(big).size
    const parse = vi.spyOn(JSON, "parse")
    try {
      decoded(big)
      const widest = Math.max(
        ...parse.mock.calls.map(([text]) => (typeof text === "string" ? text.length : 0))
      )
      expect(widest).toBeLessThan(size / 2)
    } finally {
      parse.mockRestore()
    }
  })
})

describe("FlowsIndex", () => {
  it("names every leg's own JSON text by offset and length", () => {
    const index = FlowsIndex.indexOf(big)
    expect(FlowsIndex.legCount(index)).toBe(LEGS)
    const texts = FlowsIndex.legTexts(index, [0, 7, LEGS - 1])
    expect([...texts.keys()]).toEqual([0, 7, LEGS - 1])
    for (const [leg, text] of texts) expect(JSON.parse(text)).toEqual(legOf(leg))
  })

  it("holds the envelope's halves, which parse as the document with its legs taken out", () => {
    const index = FlowsIndex.indexOf(big)
    expect(JSON.parse(index.head + index.tail)).toEqual({ ...documentValue(LEGS), legs: [] })
  })

  it("leaves out a leg the document has no entry for", () => {
    const index = FlowsIndex.indexOf(big)
    expect([...FlowsIndex.legTexts(index, [LEGS, 1]).keys()]).toEqual([1])
  })

  it("answers each named leg once, in index order, however it was asked", () => {
    const index = FlowsIndex.indexOf(big)
    expect([...FlowsIndex.legTexts(index, [5, 5, 2]).keys()]).toEqual([2, 5])
  })

  it("re-scans a file rewritten under the same name", () => {
    const file = documentOf("rewritten.flows.json", 2)
    expect(FlowsIndex.legCount(FlowsIndex.indexOf(file))).toBe(2)
    // A later mtime for certain: a rewrite inside the clock's resolution would
    // otherwise be the same file to the cache.
    const later = new Date(fs.statSync(file).mtimeMs + 2000)
    fs.writeFileSync(file, JSON.stringify(documentValue(3), null, 2))
    fs.utimesSync(file, later, later)
    expect(FlowsIndex.legCount(FlowsIndex.indexOf(file))).toBe(3)
  })
})

describe("FlowsFile.readFlowsLegsDecoded", () => {
  const excerpt = (file: string, legs: ReadonlyArray<number>): FlowsFile.FlowsExcerpt => {
    const exit = Effect.runSyncExit(FlowsFile.readFlowsLegsDecoded(file, legs))
    if (Exit.isFailure(exit)) throw new Error(Cause.pretty(exit.cause))
    return exit.value
  }

  it("decodes the named legs as the whole-document decode does, and no other", () => {
    const whole = Flows.decodeFlowsSync(JSON.parse(fs.readFileSync(big, "utf8")))
    const read = excerpt(big, [7, 1234])
    expect([...read.legs.keys()]).toEqual([7, 1234])
    expect(read.legs.get(7)).toEqual(whole.legs[7])
    expect(read.legs.get(1234)).toEqual(whole.legs[1234])
    expect(read.envelope).toEqual({ ...whole, legs: [] })
  })

  it("parses the named leg and the envelope, nothing else of the document", () => {
    const size = fs.statSync(big).size
    const parse = vi.spyOn(JSON, "parse")
    try {
      excerpt(big, [42])
      const widths = parse.mock.calls.map(([text]) => (typeof text === "string" ? text.length : 0))
      expect(widths).toHaveLength(2)
      expect(Math.max(...widths)).toBeLessThan(size / 2)
    } finally {
      parse.mockRestore()
    }
  })

  it("names the index of the leg the contract refuses", () => {
    const file = path.join(scratch, "bad-leg-excerpt.flows.json")
    const document = documentValue(3) as { legs: Array<unknown> }
    document.legs[2] = { ...legOf(2), final_status: "200" }
    fs.writeFileSync(file, JSON.stringify(document, null, 2))
    const exit = Effect.runSyncExit(FlowsFile.readFlowsLegsDecoded(file, [0, 2]))
    expect(Exit.isFailure(exit)).toBe(true)
    if (Exit.isFailure(exit)) {
      const error = Cause.squash(exit.cause)
      expect(error).toBeInstanceOf(FlowsFile.FlowsLegRefused)
      expect((error as FlowsFile.FlowsLegRefused).index).toBe(2)
    }
  })
})
