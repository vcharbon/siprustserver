/**
 * The case-id shapes come from the grammar alone: a deployment's own suffix
 * (here a frozen `k<hex>` name) reads as a full case id with its capture and
 * as a bare suffix, and the bare suffixes a text cites alone default to
 * `caseN`.
 */
import { describe, expect, it } from "vitest"
import * as Resolver from "../src/resolver.js"

const GRAMMAR: Resolver.Grammar = {
  isCapture: (input) => input.includes(".pcap"),
  suffixes: `${Resolver.DEFAULT_SUFFIXES}|k[0-9a-f]{12}`
}

describe("Resolver.caseShapes", () => {
  it("reads a deployment's suffix in a full case id, capturing the capture, and as a bare suffix", () => {
    const shapes = Resolver.caseShapes(GRAMMAR)
    expect(shapes.caseId.exec("trace.pcap.gz-k0123456789ab")?.[1]).toBe("trace.pcap.gz")
    expect(shapes.caseId.exec("trace.pcap.gz-case2")?.[1]).toBe("trace.pcap.gz")
    expect(shapes.bare.test("-k0123456789ab")).toBe(true)
    expect(shapes.caseId.test("trace.pcap.gz-kzz")).toBe(false)
  })

  it("cites a case alone by caseN where the grammar states no bare suffixes", () => {
    expect(Resolver.caseShapes(GRAMMAR).bareSuffixes).toBe(Resolver.DEFAULT_BARE_SUFFIXES)
    expect(Resolver.caseShapes({ ...GRAMMAR, bareSuffixes: "x\\d+" }).bareSuffixes).toBe("x\\d+")
  })
})
