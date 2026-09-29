/**
 * The origin ledger: an origin the replayed endpoint mints is read as the
 * captured one only where both sides minted it, the pairing of origins holds
 * one-to-one across the cell, a session opens in the same form on both sides,
 * and each session steps its version alike on both sides. A relayed origin, a session continued under another sess-id and
 * a version step of another size stay differences.
 */
import { describe, expect, it } from "vitest"
import { identitiesIn, identityOf, ledger, originOf, parseOrigin, withOriginOf } from "../src/sdporigin.js"

const sdp = (origin: string, port = 6000): string =>
  `v=0\r\n${origin}\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio ${port} RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\n`
const o = (id: number | string, version: number | string, user = "-", address = "192.0.2.10"): string =>
  `o=${user} ${id} ${version} IN IP4 ${address}`

const PEER = o(555, 555, "peer", "198.51.100.7")
const nothingDriven = { captured: new Set<string>(), replayed: new Set<string>() }
const peerDriven = {
  captured: identitiesIn([sdp(PEER)]),
  replayed: identitiesIn([sdp(PEER)])
}

describe("parseOrigin", () => {
  it("reads six fields with numeric sess-id and sess-version", () => {
    expect(parseOrigin(o(100, 101))).toEqual({
      username: "-",
      sessId: 100n,
      sessVersion: 101n,
      network: "IN IP4 192.0.2.10"
    })
    expect(parseOrigin("o=- 18446744073709551615 1 IN IP4 h")?.sessId).toBe(18446744073709551615n)
    expect(parseOrigin("o=- abc 1 IN IP4 h")).toBeUndefined()
    expect(parseOrigin("o=- 1 1 IN IP4")).toBeUndefined()
  })

  it("an origin is the first `o=` line of a text; the identity leaves the version out", () => {
    const origin = originOf(sdp(o(7, 9)))!
    expect(identityOf(origin)).toBe("- 7 IN IP4 192.0.2.10")
    expect(originOf("v=0\r\ns=-\r\n")).toBeUndefined()
  })

  it("withOriginOf reads one text's origin line into another, and nothing else", () => {
    expect(withOriginOf(sdp(o(9, 9)), sdp(o(1, 2)))).toBe(sdp(o(1, 2)))
    expect(withOriginOf(sdp(o(9, 9), 7000), sdp(o(1, 2)))).toBe(sdp(o(1, 2), 7000))
    expect(withOriginOf("v=0\r\n", sdp(o(1, 2)))).toBe("v=0\r\n")
  })
})

describe("ledger", () => {
  it("admits an opened session and its +1 continuation, numbers apart", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(leg.read(sdp(o(1000, 1001)), sdp(o(7, 8)))).toBe(true)
  })

  it("the same session continued under another sess-id at the right +1 step is refused", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(leg.read(sdp(o(1000, 1001)), sdp(o(9, 8)))).toBe(false)
  })

  it("a new captured session the replay continues under its old sess-id is refused", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(leg.read(sdp(o(2000, 2000)), sdp(o(7, 8)))).toBe(false)
  })

  it("a version step of another size is refused", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(leg.read(sdp(o(1000, 1001)), sdp(o(7, 9)))).toBe(false)
    expect(leg.read(sdp(o(1000, 1001)), sdp(o(7, 7)))).toBe(false)
  })

  it("an unchanged version on both sides is the same step", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
  })

  it("a pair recorded as equal still binds the pairing", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(1000, 1000)))).toBe(true)
    expect(leg.read(sdp(o(1000, 1001)), sdp(o(7, 1001)))).toBe(false)
  })

  it("a relayed peer origin with shifted numbers is refused on either side", () => {
    expect(ledger(peerDriven).read(sdp(PEER), sdp(o(556, 556, "peer", "198.51.100.7")))).toBe(false)
    // The capture minted it; the replay relayed a peer's.
    expect(ledger(peerDriven).read(sdp(o(1000, 1000)), sdp(PEER))).toBe(false)
    // The capture relayed a peer's; the replay minted one.
    expect(ledger(peerDriven).read(sdp(PEER), sdp(o(7, 7)))).toBe(false)
  })

  it("a username or network that differs is no numbers-only difference", () => {
    expect(ledger(nothingDriven).read(sdp(o(1000, 1000)), sdp(o(7, 7, "other")))).toBe(false)
    expect(ledger(nothingDriven).read(sdp(o(1000, 1000)), sdp(o(7, 7, "-", "192.0.2.11")))).toBe(false)
  })

  it("a description with no readable origin is no pair and records nothing", () => {
    const leg = ledger(nothingDriven)
    expect(leg.read("v=0\r\n", sdp(o(7, 7)))).toBe(false)
    expect(leg.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
  })

  it("two captured sessions the replay folds into one are refused, whatever the step", () => {
    const cell = ledger(nothingDriven)
    expect(cell.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(cell.read(sdp(o(2000, 2000)), sdp(o(7, 8)))).toBe(false)
  })

  it("one captured session the replay splits into two is refused", () => {
    const cell = ledger(nothingDriven)
    expect(cell.read(sdp(o(1000, 1000)), sdp(o(7, 7)))).toBe(true)
    expect(cell.read(sdp(o(1000, 1001)), sdp(o(8, 8)))).toBe(false)
  })

  it("a pairing broken on the third description is refused although its step matches", () => {
    const cell = ledger(nothingDriven)
    expect(cell.read(sdp(o(100, 100)), sdp(o(7, 7)))).toBe(true)
    expect(cell.read(sdp(o(200, 200)), sdp(o(9, 9)))).toBe(true)
    expect(cell.read(sdp(o(100, 101)), sdp(o(9, 10)))).toBe(false)
  })

  it("a session opened in another form is refused: sess-version equal to sess-id on one side only", () => {
    expect(ledger(nothingDriven).read(sdp(o(1000, 1000)), sdp(o(7, 1)))).toBe(false)
    expect(ledger(nothingDriven).read(sdp(o(1000, 1)), sdp(o(7, 7)))).toBe(false)
    expect(ledger(nothingDriven).read(sdp(o(1000, 1)), sdp(o(7, 1)))).toBe(true)
  })
})
