/**
 * The `sdp` fold: sections by position, lines per section as a multiset, the
 * `o=` floor masked always and the lane-owned fields only under the tokens a
 * rebooked run applied. Every difference is one row per key, verbatim.
 */
import { describe, expect, it } from "vitest"
import { diffSdp, FLOOR, foldSdp, lineKey, maskLine, maskOf, type SdpMask } from "../src/sdpfold.js"

const REBOOKED: SdpMask = { connectionAddress: true, mediaPort: true }

const OFFER = [
  "v=0",
  "o=- 1234 5678 IN IP4 192.0.2.10",
  "s=-",
  "c=IN IP4 192.0.2.10",
  "t=0 0",
  "m=audio 6000 RTP/AVP 8 101",
  "a=rtpmap:8 PCMA/8000",
  "a=rtpmap:101 telephone-event/8000",
  "a=fmtp:101 0-15",
  "a=ptime:20",
  "a=sendrecv",
  "a=rtcp:6001 IN IP4 192.0.2.10"
]
const crlf = (lines: ReadonlyArray<string>): string => lines.map((l) => `${l}\r\n`).join("")
const swap = (lines: ReadonlyArray<string>, from: string, to: string): ReadonlyArray<string> =>
  lines.map((l) => (l === from ? to : l))

describe("maskOf", () => {
  it("reads the tokens only where the run rebooked media", () => {
    expect(maskOf(["c=addr", "m=port"], "rebooked")).toEqual(REBOOKED)
    expect(maskOf(["c=addr"], "rebooked")).toEqual({ connectionAddress: true, mediaPort: false })
    expect(maskOf(["c=addr", "m=port"], "verbatim")).toEqual(FLOOR)
    expect(maskOf(undefined, "rebooked")).toEqual(FLOOR)
    expect(maskOf(["x=unknown"], "rebooked")).toEqual(FLOOR)
  })
})

describe("lineKey", () => {
  it("keys a line by its type, an attribute by its name, the directions as one", () => {
    expect(lineKey("v=0")).toBe("v=")
    expect(lineKey("m=audio 6000 RTP/AVP 8")).toBe("m=")
    expect(lineKey("a=rtpmap:8 PCMA/8000")).toBe("a=rtpmap")
    expect(lineKey("a=ptime:20")).toBe("a=ptime")
    expect(lineKey("b=AS:64")).toBe("b=AS")
    for (const d of ["sendrecv", "sendonly", "recvonly", "inactive"]) expect(lineKey(`a=${d}`)).toBe("a=direction")
  })
})

describe("maskLine", () => {
  it("stars the `o=` session id and version always, and the lane-owned fields under the mask", () => {
    expect(maskLine(FLOOR, "o=- 1234 5678 IN IP4 192.0.2.10")).toBe("o=- * * IN IP4 192.0.2.10")
    expect(maskLine(FLOOR, "c=IN IP4 192.0.2.10")).toBe("c=IN IP4 192.0.2.10")
    expect(maskLine(REBOOKED, "c=IN IP4 192.0.2.10")).toBe("c=IN IP4 *")
    expect(maskLine(FLOOR, "m=audio 6000 RTP/AVP 8 101")).toBe("m=audio 6000 RTP/AVP 8 101")
    expect(maskLine(REBOOKED, "m=audio 6000/2 RTP/AVP 8 101")).toBe("m=audio * RTP/AVP 8 101")
    expect(maskLine(REBOOKED, "a=rtcp:6001 IN IP4 192.0.2.10")).toBe("a=rtcp:* IN IP4 *")
    expect(maskLine({ connectionAddress: false, mediaPort: true }, "a=rtcp:6001")).toBe("a=rtcp:*")
    expect(maskLine(REBOOKED, "a=ptime:20")).toBe("a=ptime:20")
  })
})

describe("diffSdp", () => {
  it("states nothing for the same description", () => {
    expect(diffSdp(FLOOR, crlf(OFFER), crlf(OFFER))).toEqual([])
  })

  it("erases the `o=` floor, line endings, trailing whitespace and attribute order inside a section", () => {
    const reordered = [
      ...OFFER.slice(0, 6),
      "a=sendrecv",
      "a=rtcp:6001 IN IP4 192.0.2.10 ",
      "a=fmtp:101 0-15",
      "a=ptime:20",
      "a=rtpmap:101 telephone-event/8000",
      "a=rtpmap:8 PCMA/8000"
    ]
    const other = swap(reordered, "o=- 1234 5678 IN IP4 192.0.2.10", "o=- 99 100 IN IP4 192.0.2.10")
    expect(diffSdp(FLOOR, crlf(OFFER), other.join("\n"))).toEqual([])
  })

  it("erases the lane-owned fields only under the mask", () => {
    const rebooked = swap(
      swap(swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"), "m=audio 6000 RTP/AVP 8 101", "m=audio 40000 RTP/AVP 8 101"),
      "a=rtcp:6001 IN IP4 192.0.2.10",
      "a=rtcp:40001 IN IP4 127.0.0.2"
    )
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(rebooked))).toEqual([])
    expect(diffSdp(FLOOR, crlf(OFFER), crlf(rebooked)).map((d) => `${d.section}:${d.line}`)).toEqual([
      "session:c=",
      "m0:m=",
      "m0:a=rtcp"
    ])
  })

  it("names each differing key with both sides' verbatim lines, in wire order", () => {
    const changed = swap(swap(OFFER, "a=ptime:20", "a=ptime:30"), "a=sendrecv", "a=sendonly")
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(changed))).toEqual([
      { section: "m0", line: "a=ptime", captured: ["a=ptime:20"], replayed: ["a=ptime:30"] },
      { section: "m0", line: "a=direction", captured: ["a=sendrecv"], replayed: ["a=sendonly"] }
    ])
  })

  it("a payload dropped from the format list is one `m=` row, its attributes following under their own keys", () => {
    const dropped = OFFER.filter((l) => !l.includes(":101")).map((l) => l.replace(" 8 101", " 8"))
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(dropped))).toEqual([
      { section: "m0", line: "m=", captured: ["m=audio 6000 RTP/AVP 8 101"], replayed: ["m=audio 6000 RTP/AVP 8"] },
      {
        section: "m0",
        line: "a=rtpmap",
        captured: ["a=rtpmap:8 PCMA/8000", "a=rtpmap:101 telephone-event/8000"],
        replayed: ["a=rtpmap:8 PCMA/8000"]
      },
      { section: "m0", line: "a=fmtp", captured: ["a=fmtp:101 0-15"], replayed: [] }
    ])
  })

  it("a media section on one side only is one `section` row, and sections never reorder", () => {
    const video = ["m=video 6002 RTP/AVP 96", "a=rtpmap:96 H264/90000"]
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf([...OFFER, ...video]))).toEqual([
      { section: "m1", line: "section", captured: [], replayed: video }
    ])
    expect(diffSdp(REBOOKED, crlf([...OFFER, ...video]), crlf(OFFER))).toEqual([
      { section: "m1", line: "section", captured: video, replayed: [] }
    ])
    const swapped = [...OFFER.slice(0, 5), ...video, ...OFFER.slice(5)]
    const rows = diffSdp(REBOOKED, crlf([...OFFER, ...video]), crlf(swapped))
    const audioKeys = ["m=", "a=rtpmap", "a=fmtp", "a=ptime", "a=direction", "a=rtcp"]
    expect(rows.map((d) => `${d.section}:${d.line}`)).toEqual([
      ...audioKeys.map((k) => `m0:${k}`),
      "m1:m=",
      "m1:a=rtpmap",
      ...audioKeys.slice(2).map((k) => `m1:${k}`)
    ])
  })

  it("a side that is empty or no session description is one `document` row with both texts whole", () => {
    expect(diffSdp(REBOOKED, crlf(OFFER), "")).toEqual([
      { section: "document", line: "sdp", captured: [crlf(OFFER)], replayed: [""] }
    ])
    expect(diffSdp(REBOOKED, crlf(OFFER), "<not/>")).toEqual([
      { section: "document", line: "sdp", captured: [crlf(OFFER)], replayed: ["<not/>"] }
    ])
    expect(diffSdp(REBOOKED, "", "")).toEqual([])
  })

  it("a session-level difference is its own row", () => {
    const named = swap(OFFER, "s=-", "s=call")
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(named))).toEqual([
      { section: "session", line: "s=", captured: ["s=-"], replayed: ["s=call"] }
    ])
  })
})

describe("foldSdp", () => {
  it("folds two texts equal exactly when diffSdp states nothing", () => {
    const cases: ReadonlyArray<readonly [SdpMask, string, string]> = [
      [FLOOR, crlf(OFFER), OFFER.join("\n")],
      [REBOOKED, crlf(OFFER), crlf(swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"))],
      [FLOOR, crlf(OFFER), crlf(swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"))],
      [FLOOR, crlf(OFFER), crlf(swap(OFFER, "a=ptime:20", "a=ptime:30"))],
      [FLOOR, crlf(OFFER), ""],
      [FLOOR, "", ""],
      [FLOOR, "<a/>", "<a/>"]
    ]
    for (const [mask, a, b] of cases) {
      expect(foldSdp(mask, a) === foldSdp(mask, b)).toBe(diffSdp(mask, a, b).length === 0)
    }
  })

  it("leaves a text that is no session description verbatim", () => {
    expect(foldSdp(FLOOR, "<a/>")).toBe("<a/>")
    expect(foldSdp(FLOOR, "")).toBe("")
  })
})
