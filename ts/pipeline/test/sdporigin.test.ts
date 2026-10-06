/**
 * The origin reading: an origin the replayed endpoint mints is read as the
 * captured one only where both sides minted it, the pairing of origin
 * identities holds one-to-one across the cell, a session opens in the same
 * form on both sides, and each session steps its version alike, each side in
 * its own order. A relayed origin, a session continued under another sess-id and
 * a version step of another size stay differences.
 */
import { describe, expect, it } from "vitest"
import { type Driven, identitiesIn, identityOf, originOf, parseOrigin, readOrigins, withOriginOf } from "../src/sdporigin.js"

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

/** The pairs in one order shared by both sides, each answered. */
const read = (pairs: ReadonlyArray<readonly [string, string]>, driven: Driven = nothingDriven) =>
  readOrigins(pairs.map(([captured, replayed], at) => ({ captured, replayed, capturedAt: at, replayedAt: at })), driven)

describe("readOrigins", () => {
  const S = (id: number, version: number, user = "-", address = "192.0.2.10") => sdp(o(id, version, user, address))

  it("admits an opened session and its +1 continuation, numbers apart", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1001), S(7, 8)]])).toEqual([true, true])
  })

  it("the same session continued under another sess-id at the right +1 step is refused", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1001), S(9, 8)]])).toEqual([true, false])
  })

  it("a new captured session the replay continues under its old sess-id is refused", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(2000, 2000), S(7, 8)]])).toEqual([true, false])
  })

  it("a version step of another size is refused", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1001), S(7, 9)]])).toEqual([true, false])
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1001), S(7, 7)]])).toEqual([true, false])
  })

  it("an unchanged version on both sides is the same step", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1000), S(7, 7)]])).toEqual([true, true])
  })

  it("a pair recorded as equal still binds the pairing", () => {
    expect(read([[S(1000, 1000), S(1000, 1000)], [S(1000, 1001), S(7, 1001)]])).toEqual([true, false])
  })

  it("a relayed peer origin with shifted numbers is refused on either side", () => {
    expect(read([[sdp(PEER), sdp(o(556, 556, "peer", "198.51.100.7"))]], peerDriven)).toEqual([false])
    // The capture minted it; the replay relayed a peer's.
    expect(read([[S(1000, 1000), sdp(PEER)]], peerDriven)).toEqual([false])
    // The capture relayed a peer's; the replay minted one.
    expect(read([[sdp(PEER), S(7, 7)]], peerDriven)).toEqual([false])
  })

  it("a username or network that differs is no numbers-only difference", () => {
    expect(read([[S(1000, 1000), S(7, 7, "other")]])).toEqual([false])
    expect(read([[S(1000, 1000), S(7, 7, "-", "192.0.2.11")]])).toEqual([false])
  })

  it("a description with no readable origin is no pair and binds nothing", () => {
    expect(read([["v=0\r\n", S(7, 7)], [S(1000, 1000), S(7, 7)]])).toEqual([false, true])
  })

  it("two captured sessions the replay folds into one are refused, whatever the step", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(2000, 2000), S(7, 8)]])).toEqual([true, false])
  })

  it("one captured session the replay splits into two is refused", () => {
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1001), S(8, 8)]])).toEqual([true, false])
  })

  it("a pairing broken on the third description is refused although its step matches", () => {
    expect(read([[S(100, 100), S(7, 7)], [S(200, 200), S(9, 9)], [S(100, 101), S(9, 10)]])).toEqual([true, true, false])
  })

  it("a session opened in another form is refused: sess-version equal to sess-id on one side only", () => {
    expect(read([[S(1000, 1000), S(7, 1)]])).toEqual([false])
    expect(read([[S(1000, 1), S(7, 7)]])).toEqual([false])
    expect(read([[S(1000, 1), S(7, 1)]])).toEqual([true])
  })

  it("pairs by origin identity: one sess-id under another username or address is another session", () => {
    // Same captured sess-id, two identities; the replay mints two sessions.
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1000, "other"), S(8, 8, "other")]])).toEqual([true, true])
    expect(read([[S(1000, 1000), S(7, 7)], [S(1000, 1000, "-", "192.0.2.11"), S(8, 8, "-", "192.0.2.11")]]))
      .toEqual([true, true])
    // Same replayed sess-id, two identities, against two captured sessions.
    expect(read([[S(1000, 1000), S(7, 7)], [S(2000, 2000, "other"), S(7, 7, "other")]])).toEqual([true, true])
  })

  it("steps each side in its own order: a replay that reaches two legs in the other order still steps alike", () => {
    // The capture sends +1 to one leg (flow position 0) then +2 to the other
    // (1); the replay's wire has the second leg first, with +1 there.
    const got = readOrigins([
      { captured: S(1000, 1000), replayed: S(7, 7), capturedAt: 0, replayedAt: 0 },
      { captured: S(1000, 1001), replayed: S(7, 9), capturedAt: 1, replayedAt: 2 },
      { captured: S(1000, 1002), replayed: S(7, 8), capturedAt: 2, replayedAt: 1 }
    ], nothingDriven)
    expect(got).toEqual([true, true, true])
  })
})
