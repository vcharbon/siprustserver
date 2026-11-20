/**
 * A header probe's two sides as items: the header's own grammar splits them,
 * a set reads as a set, and a Content-Length and a session description's
 * descriptors state whether a body rides.
 */
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  bothSides,
  canon,
  delta,
  is,
  itemsOf,
  itemsRemoved,
  lengthStatesBody,
  onlyCaptured,
  onlyReplayed,
  removedAlone,
  sdpDescriptorDropped,
  single,
  singles,
  valueDiffers
} from "../src/header-items.js"
import { headerProbe, shape } from "./probes.js"

const header = (name: string, captured: ReadonlyArray<string>, replayed: ReadonlyArray<string>): Probe.HeaderProbe =>
  headerProbe({ name, captured, replayed }) as Probe.HeaderProbe

describe("a header's name", () => {
  it("reads in its canonical form, a compact form spelt out (RFC 3261 §7.3.3)", () => {
    expect(canon("CALL-ID")).toBe("call-id")
    expect(canon("i")).toBe("call-id")
    expect(canon("v")).toBe("via")
  })

  it("names a header probe of one of the canonical names given, never a shape probe", () => {
    expect(is(header("Call-ID", ["a"], ["b"]), "via", "call-id")).toBe(true)
    expect(is(header("i", ["a"], ["b"]), "call-id")).toBe(true)
    expect(is(header("To", ["a"], ["b"]), "from")).toBe(false)
    expect(is(shape({ shape: "extra-message", method: "BYE" }), "call-id")).toBe(false)
  })
})

describe("a side's items", () => {
  it("splits a comma list as its grammar does, and keeps a single-valued header whole", () => {
    expect(itemsOf(header("Allow", ["INVITE, ACK", "BYE,"], []), "captured")).toEqual(["INVITE", "ACK", "BYE"])
    expect(itemsOf(header("From", ['"Doe, John" <sip:jdoe@example.com>'], []), "captured")).toEqual([
      '"Doe, John" <sip:jdoe@example.com>'
    ])
  })

  it("names a side's one item, and both sides' where each carries exactly one", () => {
    const h = header("Supported", ["timer"], ["100rel, timer"])
    expect(single(h, "captured")).toBe("timer")
    expect(single(h, "replayed")).toBeUndefined()
    expect(singles(h)).toBeUndefined()
    expect(singles(header("Max-Forwards", ["70"], ["69"]))).toEqual(["70", "69"])
  })

  it("tells which sides carry items", () => {
    expect(onlyCaptured(header("Subject", ["lunch"], []))).toBe(true)
    expect(onlyReplayed(header("Subject", [], ["lunch"]))).toBe(true)
    expect(bothSides(header("Subject", ["lunch"], ["dinner"]))).toBe(true)
    expect(onlyCaptured(header("Allow", [" , "], []))).toBe(false)
  })

  it("reads a rendering where both sides carry as many items, at least one", () => {
    expect(valueDiffers(header("Subject", ["lunch"], ["dinner"]))).toBe(true)
    expect(valueDiffers(header("Allow", ["INVITE, ACK"], ["INVITE"]))).toBe(false)
    expect(valueDiffers(header("Subject", [], []))).toBe(false)
  })
})

describe("a set's delta", () => {
  it("states what the replay added and removed for a set-folded header, nothing for another", () => {
    expect(delta(header("Allow", ["INVITE, ACK, BYE"], ["BYE, INVITE, UPDATE"]))).toEqual({ added: ["UPDATE"], removed: ["ACK"] })
    expect(delta(header("Via", ["a"], ["b"]))).toBeUndefined()
  })

  it("reads a set narrowed, and the one item it lost alone", () => {
    expect(itemsRemoved(header("Supported", ["100rel, timer"], ["timer"]))).toBe(true)
    expect(removedAlone(header("Supported", ["100rel, timer"], ["timer"]))).toBe("100rel")
    expect(removedAlone(header("Supported", ["100rel, timer"], ["timer, path"]))).toBeUndefined()
    expect(removedAlone(header("Supported", ["100rel, timer"], []))).toBeUndefined()
    expect(itemsRemoved(header("Supported", ["timer"], ["timer, path"]))).toBe(false)
  })
})

describe("a Content-Length (RFC 3261 §20.14)", () => {
  const states = (values: ReadonlyArray<string>) => lengthStatesBody(header("Content-Length", values, []), "captured")

  it("states a body when non-zero, none when zero or absent", () => {
    expect(states(["142"])).toBe(true)
    expect(states(["0"])).toBe(false)
    expect(states([])).toBe(false)
  })

  it("states nothing either way when it is no single number", () => {
    expect(states(["14x"])).toBeUndefined()
    expect(states(["142", "142"])).toBeUndefined()
  })
})

describe("a dropped session-description descriptor", () => {
  it("reads a Content-Length the replay zeroed or dropped", () => {
    expect(sdpDescriptorDropped(header("Content-Length", ["142"], ["0"]))).toBe(true)
    expect(sdpDescriptorDropped(header("Content-Length", ["142"], []))).toBe(true)
    expect(sdpDescriptorDropped(header("Content-Length", ["0"], ["0"]))).toBe(false)
    expect(sdpDescriptorDropped(header("Content-Length", ["142"], ["158"]))).toBe(false)
  })

  it("reads an application/sdp Content-Type the replay dropped, never another type", () => {
    expect(sdpDescriptorDropped(header("Content-Type", ["Application/SDP"], []))).toBe(true)
    expect(sdpDescriptorDropped(header("Content-Type", ["multipart/mixed;boundary=x"], []))).toBe(false)
    expect(sdpDescriptorDropped(header("Content-Type", ["application/sdp"], ["application/sdp"]))).toBe(false)
  })

  it("reads a session or early-session Content-Disposition the replay dropped (RFC 3959)", () => {
    expect(sdpDescriptorDropped(header("Content-Disposition", ["session;handling=required"], []))).toBe(true)
    expect(sdpDescriptorDropped(header("Content-Disposition", ["early-session"], []))).toBe(true)
    expect(sdpDescriptorDropped(header("Content-Disposition", ["render"], []))).toBe(false)
  })
})
