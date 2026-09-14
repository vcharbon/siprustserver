/**
 * The `sdp` fold: sections by position, lines per section as a multiset, the
 * `o=` floor masked always and the lane-owned fields only under the tokens a
 * rebooked run applied — exactly the fields the render writes. Every
 * structural difference is one row per key, verbatim; on a verbatim run two
 * descriptions the structure cannot tell apart must still be the same bytes.
 */
import { describe, expect, it } from "vitest"
import { diffSdp, FLOOR, foldSdp, lineKey, maskLine, maskOf, type SdpMask, VERBATIM } from "../src/sdpfold.js"

const REBOOKED: SdpMask = { connectionAddress: true, mediaPort: true, verbatim: false }

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
    expect(maskOf(["c=addr"], "rebooked")).toEqual({ connectionAddress: true, mediaPort: false, verbatim: false })
    expect(maskOf(["c=addr", "m=port"], "verbatim")).toEqual(VERBATIM)
    expect(maskOf(undefined, "verbatim")).toEqual(VERBATIM)
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
    expect(maskLine(REBOOKED, "m=audio 6000 RTP/AVP 8 101")).toBe("m=audio * RTP/AVP 8 101")
    expect(maskLine(REBOOKED, "a=ptime:20")).toBe("a=ptime:20")
  })

  it("stars exactly what the render writes: an IP4 address, a non-zero port with its pair count kept", () => {
    expect(maskLine(REBOOKED, "c=IN IP6 ::1")).toBe("c=IN IP6 ::1")
    expect(maskLine(REBOOKED, "m=audio 6000/2 RTP/AVP 8 101")).toBe("m=audio */2 RTP/AVP 8 101")
    expect(maskLine(REBOOKED, "m=audio 0 RTP/AVP 8")).toBe("m=audio 0 RTP/AVP 8")
    expect(maskLine(REBOOKED, "m=audio 0/2 RTP/AVP 8")).toBe("m=audio 0/2 RTP/AVP 8")
    expect(maskLine(REBOOKED, "m=audio port RTP/AVP 8")).toBe("m=audio port RTP/AVP 8")
    expect(maskLine(REBOOKED, "a=rtcp:6001 IN IP4 192.0.2.10")).toBe("a=rtcp:6001 IN IP4 192.0.2.10")
  })
})

describe("diffSdp", () => {
  it("states nothing for the same description", () => {
    expect(diffSdp(FLOOR, crlf(OFFER), crlf(OFFER))).toEqual([])
  })

  it("on a rebooked run erases the `o=` floor, line endings, trailing whitespace and attribute order inside a section", () => {
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

  describe("on a verbatim run the structure decides first, then the bytes", () => {
    const bytesRow = (replayed: string) => [
      { section: "document", line: "bytes", captured: [crlf(OFFER)], replayed: [replayed] }
    ]
    const cases: ReadonlyArray<readonly [string, string]> = [
      ["an attribute reorder inside a section", crlf([...OFFER.slice(0, 6), "a=sendrecv", ...OFFER.slice(6, 10), OFFER[11]!])],
      ["a bare-LF line ending", OFFER.join("\n") + "\n"],
      ["trailing whitespace on a line", crlf(swap(OFFER, "a=ptime:20", "a=ptime:20 "))],
      ["an inner blank line", crlf([...OFFER.slice(0, 5), "", ...OFFER.slice(5)])]
    ]
    for (const [name, replayed] of cases) {
      it(`${name} is one document:bytes row, and nothing on a rebooked run`, () => {
        expect(diffSdp(VERBATIM, crlf(OFFER), replayed)).toEqual(bytesRow(replayed))
        expect(diffSdp(FLOOR, crlf(OFFER), replayed)).toEqual([])
        expect(diffSdp(REBOOKED, crlf(OFFER), replayed)).toEqual([])
      })
    }

    it("a structural difference is its rows and never the bytes row", () => {
      const changed = crlf(swap(OFFER, "a=ptime:20", "a=ptime:30"))
      expect(diffSdp(VERBATIM, crlf(OFFER), changed)).toEqual([
        { section: "m0", line: "a=ptime", captured: ["a=ptime:20"], replayed: ["a=ptime:30"] }
      ])
      expect(diffSdp(VERBATIM, crlf(OFFER), crlf(OFFER))).toEqual([])
    })
  })

  it("erases the lane-owned fields only under the mask, and only the fields the render writes", () => {
    const rebooked = swap(
      swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"),
      "m=audio 6000 RTP/AVP 8 101",
      "m=audio 40000 RTP/AVP 8 101"
    )
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(rebooked))).toEqual([])
    expect(diffSdp(FLOOR, crlf(OFFER), crlf(rebooked)).map((d) => `${d.section}:${d.line}`)).toEqual([
      "session:c=",
      "m0:m="
    ])
    const keys = (mask: SdpMask, from: string, to: string) =>
      diffSdp(mask, crlf(OFFER), crlf(swap(OFFER, from, to))).map((d) => `${d.section}:${d.line}`)
    // A port-0 stream is never rebooked, so a port the system opened is its own.
    expect(keys(REBOOKED, "m=audio 6000 RTP/AVP 8 101", "m=audio 0 RTP/AVP 8 101")).toEqual(["m0:m="])
    // The pair count rides beside a rebooked port.
    const paired = swap(OFFER, "m=audio 6000 RTP/AVP 8 101", "m=audio 6000/2 RTP/AVP 8 101")
    expect(diffSdp(REBOOKED, crlf(paired), crlf(swap(paired, "m=audio 6000/2 RTP/AVP 8 101", "m=audio 40000/2 RTP/AVP 8 101")))).toEqual([])
    expect(diffSdp(REBOOKED, crlf(paired), crlf(swap(paired, "m=audio 6000/2 RTP/AVP 8 101", "m=audio 40000 RTP/AVP 8 101")))).toHaveLength(1)
    // `a=rtcp` and an IP6 connection line are never written by the render.
    for (const mask of [FLOOR, REBOOKED, VERBATIM]) {
      expect(keys(mask, "a=rtcp:6001 IN IP4 192.0.2.10", "a=rtcp:6003 IN IP4 192.0.2.10")).toEqual(["m0:a=rtcp"])
      const six = swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP6 ::1")
      expect(diffSdp(mask, crlf(six), crlf(swap(six, "c=IN IP6 ::1", "c=IN IP6 ::2"))).map((d) => `${d.section}:${d.line}`)).toEqual([
        "session:c="
      ])
    }
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
    const cases: ReadonlyArray<readonly [string, string]> = [
      [crlf(OFFER), crlf(OFFER)],
      [crlf(OFFER), OFFER.join("\n")],
      [crlf(OFFER), crlf(swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"))],
      [crlf(OFFER), crlf(swap(OFFER, "a=ptime:20", "a=ptime:30"))],
      [crlf(OFFER), crlf(swap(OFFER, "o=- 1234 5678 IN IP4 192.0.2.10", "o=- 9 9 IN IP4 192.0.2.10"))],
      [crlf(OFFER), ""],
      ["", ""],
      ["<a/>", "<a/>"]
    ]
    for (const mask of [FLOOR, REBOOKED, VERBATIM]) {
      for (const [a, b] of cases) {
        expect(foldSdp(mask, a) === foldSdp(mask, b)).toBe(diffSdp(mask, a, b).length === 0)
      }
    }
  })

  it("is the text itself on a verbatim run, and leaves a text that is no session description verbatim", () => {
    expect(foldSdp(VERBATIM, crlf(OFFER))).toBe(crlf(OFFER))
    expect(foldSdp(FLOOR, crlf(OFFER))).not.toBe(crlf(OFFER))
    expect(foldSdp(FLOOR, "<a/>")).toBe("<a/>")
    expect(foldSdp(FLOOR, "")).toBe("")
  })
})
