/**
 * The `sdp` fold: sections by position, each line under its key, the `o=`
 * floor and the lane-owned fields masked only on a rebooked run, the
 * lane-owned ones under the tokens it applied — exactly the fields the render
 * writes. Two descriptions that differ in a serialization choice alone (a line
 * ending, whitespace, a blank line, attribute order, a direction left
 * implicit) state nothing on any run; every difference in meaning is one row
 * per key, verbatim, and an `o=` row is marked only where the caller read its
 * origins.
 */
import { describe, expect, it } from "vitest"
import { diffSdp, FLOOR, foldSdp, lineKey, maskLine, maskOf, type SdpMask, VERBATIM } from "../src/sdpfold.js"

const REBOOKED: SdpMask = { origin: true, connectionAddress: true, mediaPort: true }
const MASKS: ReadonlyArray<SdpMask> = [VERBATIM, FLOOR, REBOOKED]

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
const without = (lines: ReadonlyArray<string>, line: string): ReadonlyArray<string> => lines.filter((l) => l !== line)
const keysOf = (rows: ReadonlyArray<{ readonly section: string; readonly line: string }>) =>
  rows.map((d) => `${d.section}:${d.line}`)

describe("maskOf", () => {
  it("reads the tokens only where the run rebooked media", () => {
    expect(maskOf(["c=addr", "m=port"], "rebooked")).toEqual(REBOOKED)
    expect(maskOf(["c=addr"], "rebooked")).toEqual({ origin: true, connectionAddress: true, mediaPort: false })
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
  it("stars the `o=` session id and version under the floor, and the lane-owned fields under the mask", () => {
    expect(maskLine(FLOOR, "o=- 1234 5678 IN IP4 192.0.2.10")).toBe("o=- * * IN IP4 192.0.2.10")
    expect(maskLine(VERBATIM, "o=- 1234 5678 IN IP4 192.0.2.10")).toBe("o=- 1234 5678 IN IP4 192.0.2.10")
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

describe("diffSdp: a serialization choice states nothing, on every run", () => {
  // OFFER as a stack writes it: each format's rtpmap then fmtp, the others as written, the direction last.
  const IN_FORM = [...OFFER.slice(0, 9), OFFER[9]!, OFFER[11]!, OFFER[10]!]
  const SENDONLY_SESSION = [...OFFER.slice(0, 5), "a=sendonly", ...without(OFFER.slice(5), "a=sendrecv")]
  const cases: ReadonlyArray<readonly [string, string, string]> = [
    ["a bare-LF line ending", crlf(OFFER), OFFER.join("\n") + "\n"],
    ["line endings mixed, the last one missing", crlf(OFFER), `${OFFER.slice(0, 6).join("\r\n")}\n${OFFER.slice(6).join("\r\n")}`],
    ["whitespace trailing a line", crlf(OFFER), crlf(swap(OFFER, "a=ptime:20", "a=ptime:20 \t"))],
    ["a blank line", crlf(OFFER), crlf([...OFFER.slice(0, 5), "", ...OFFER.slice(5), ""])],
    ["attributes of different names in another order", crlf(OFFER), crlf([...OFFER.slice(0, 6), ...OFFER.slice(6).reverse()])],
    ["the description in the stack's attribute order", crlf(OFFER), crlf(IN_FORM)],
    ["the stack's attribute order against the author's", crlf(IN_FORM), crlf(OFFER)],
    ["a format's fmtp ahead of its rtpmap", crlf(OFFER), crlf([...OFFER.slice(0, 7), OFFER[8]!, OFFER[7]!, ...OFFER.slice(9)])],
    [
      "rtpmap lines across formats out of the `m=` order",
      crlf(OFFER),
      crlf([...OFFER.slice(0, 6), OFFER[7]!, OFFER[6]!, ...OFFER.slice(8)])
    ],
    ["`a=sendrecv` left implicit", crlf(OFFER), crlf(without(OFFER, "a=sendrecv"))],
    ["`a=sendrecv` written out where it was implicit", crlf(without(OFFER, "a=sendrecv")), crlf(OFFER)],
    [
      "a session-level direction written into the media section too",
      crlf(SENDONLY_SESSION),
      crlf([...SENDONLY_SESSION, "a=sendonly"])
    ],
    [
      "a session-level direction moved into the media section",
      crlf(SENDONLY_SESSION),
      crlf([...without(SENDONLY_SESSION, "a=sendonly"), "a=sendonly"])
    ],
    [
      "every choice at once",
      crlf(without(OFFER, "a=sendrecv")),
      [...IN_FORM.slice(0, 5), "", ...IN_FORM.slice(5).map((l) => `${l} `)].join("\n")
    ]
  ]
  for (const [name, captured, replayed] of cases) {
    it(name, () => {
      for (const mask of MASKS) {
        expect(diffSdp(mask, captured, replayed)).toEqual([])
        expect(foldSdp(mask, captured)).toBe(foldSdp(mask, replayed))
      }
    })
  }

  it("a session-level direction with no media section compares against the default", () => {
    const session = OFFER.slice(0, 5)
    expect(diffSdp(VERBATIM, crlf(session), crlf([...session, "a=sendrecv"]))).toEqual([])
    expect(diffSdp(VERBATIM, crlf(session), crlf([...session, "a=inactive"]))).toEqual([
      { section: "session", line: "a=direction", captured: [], replayed: ["a=inactive"] }
    ])
  })
})

describe("diffSdp: a difference in meaning is one row per key, on every run", () => {
  it("states nothing for the same description", () => {
    for (const mask of MASKS) expect(diffSdp(mask, crlf(OFFER), crlf(OFFER))).toEqual([])
  })

  const strict: ReadonlyArray<readonly [string, ReadonlyArray<string>, ReadonlyArray<string>, ReadonlyArray<string>]> = [
    ["a ptime", OFFER, swap(OFFER, "a=ptime:20", "a=ptime:30"), ["m0:a=ptime"]],
    ["the format preference of the `m=` line", OFFER, swap(OFFER, "m=audio 6000 RTP/AVP 8 101", "m=audio 6000 RTP/AVP 101 8"), ["m0:m="]],
    ["a codec", OFFER, swap(OFFER, "a=rtpmap:8 PCMA/8000", "a=rtpmap:8 PCMU/8000"), ["m0:a=rtpmap"]],
    ["the media type", OFFER, swap(OFFER, "m=audio 6000 RTP/AVP 8 101", "m=video 6000 RTP/AVP 8 101"), ["m0:m="]],
    ["a stream rejected", OFFER, swap(OFFER, "m=audio 6000 RTP/AVP 8 101", "m=audio 0 RTP/AVP 8 101"), ["m0:m="]],
    ["an explicit direction", OFFER, swap(OFFER, "a=sendrecv", "a=sendonly"), ["m0:a=direction"]],
    ["a direction against the implicit `sendrecv`", without(OFFER, "a=sendrecv"), swap(OFFER, "a=sendrecv", "a=inactive"), ["m0:a=direction"]],
    ["the `a=rtcp` port, which no lane writes", OFFER, swap(OFFER, "a=rtcp:6001 IN IP4 192.0.2.10", "a=rtcp:6003 IN IP4 192.0.2.10"), ["m0:a=rtcp"]],
    ["a session name", OFFER, swap(OFFER, "s=-", "s=call"), ["session:s="]],
    ["a bandwidth added", OFFER, [...OFFER.slice(0, 6), "b=AS:64", ...OFFER.slice(6)], ["m0:b=AS"]]
  ]
  for (const [name, captured, replayed, keys] of strict) {
    it(name, () => {
      for (const mask of MASKS) {
        expect(keysOf(diffSdp(mask, crlf(captured), crlf(replayed)))).toEqual(keys)
        expect(foldSdp(mask, crlf(captured))).not.toBe(foldSdp(mask, crlf(replayed)))
      }
    })
  }

  it("a direction inherited from the session against another one written in the media section", () => {
    const inherited = [...OFFER.slice(0, 5), "a=sendonly", ...without(OFFER.slice(5), "a=sendrecv")]
    const written = [...without(OFFER, "a=sendrecv"), "a=sendrecv"]
    expect(diffSdp(VERBATIM, crlf(inherited), crlf(written))).toEqual([
      { section: "m0", line: "a=direction", captured: ["a=sendonly"], replayed: ["a=sendrecv"] }
    ])
    // The session-level line dropped changes the media section's direction, and only there.
    expect(diffSdp(VERBATIM, crlf(inherited), crlf(without(inherited, "a=sendonly")))).toEqual([
      { section: "m0", line: "a=direction", captured: ["a=sendonly"], replayed: [] }
    ])
  })

  it("same-name attributes swapped (a=crypto, a preference order) are their key's row", () => {
    const keys = [
      "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
      "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
    ]
    for (const mask of MASKS) {
      expect(diffSdp(mask, crlf([...OFFER, ...keys]), crlf([...OFFER, keys[1]!, keys[0]!]))).toEqual([
        { section: "m0", line: "a=crypto", captured: keys, replayed: [keys[1]!, keys[0]!] }
      ])
    }
  })

  it("one format's two lines of one name swapped are their key's row", () => {
    const fmtps = ["a=fmtp:101 0-15", "a=fmtp:101 16"]
    const rows = diffSdp(VERBATIM, crlf([...OFFER, fmtps[1]!]), crlf([...without(OFFER, fmtps[0]!), fmtps[1]!, fmtps[0]!]))
    expect(rows).toEqual([{ section: "m0", line: "a=fmtp", captured: fmtps, replayed: [fmtps[1]!, fmtps[0]!] }])
  })

  it("a line RFC 4566 §5 orders moved among the attributes is one `order` row, both sections whole", () => {
    const withC = [...OFFER.slice(0, 6), "c=IN IP4 192.0.2.10", ...OFFER.slice(6)]
    const cLast = [...OFFER, "c=IN IP4 192.0.2.10"]
    for (const mask of MASKS) {
      expect(diffSdp(mask, crlf(withC), crlf(cLast))).toEqual([
        { section: "m0", line: "order", captured: withC.slice(5), replayed: cLast.slice(5) }
      ])
      expect(foldSdp(mask, crlf(withC))).not.toBe(foldSdp(mask, crlf(cLast)))
    }
    const twoTimes = [...OFFER.slice(0, 5), "r=7d 1h 0 25h", "t=1 2", ...OFFER.slice(5)]
    const regrouped = [...OFFER.slice(0, 5), "t=1 2", "r=7d 1h 0 25h", ...OFFER.slice(5)]
    expect(keysOf(diffSdp(VERBATIM, crlf(twoTimes), crlf(regrouped)))).toEqual(["session:order"])
  })

  describe("the origin", () => {
    const MINTED = "o=- 99 100 IN IP4 192.0.2.10"
    const originRow = { section: "session", line: "o=", captured: [OFFER[1]!], replayed: [MINTED] }

    it("a version move is a `session:o=` row on a verbatim run, masked on a rebooked one", () => {
      const stepped = crlf(swap(OFFER, OFFER[1]!, "o=- 1234 5679 IN IP4 192.0.2.10"))
      expect(keysOf(diffSdp(VERBATIM, crlf(OFFER), stepped))).toEqual(["session:o="])
      expect(diffSdp(FLOOR, crlf(OFFER), stepped)).toEqual([])
      expect(keysOf(diffSdp(FLOOR, crlf(OFFER), crlf(swap(OFFER, OFFER[1]!, "o=peer 1234 5678 IN IP4 192.0.2.10"))))).toEqual([
        "session:o="
      ])
    })

    it("a `session:o=` row carries the caller's reading, and only where the origin is compared", () => {
      const replayed = crlf(swap(OFFER, OFFER[1]!, MINTED))
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed)).toEqual([originRow])
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed, "minted")).toEqual([{ ...originRow, mintedOrigin: true }])
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed, "not-continued")).toEqual([{ ...originRow, originNotContinued: true }])
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed, "version-step-one")).toEqual([{ ...originRow, versionStepOne: true }])
      expect(diffSdp(FLOOR, crlf(OFFER), replayed, "minted")).toEqual([])
      const renamed = crlf(swap(OFFER, OFFER[1]!, "o=peer 99 100 IN IP4 192.0.2.10"))
      expect(diffSdp(FLOOR, crlf(OFFER), renamed, "minted")).toEqual([
        { section: "session", line: "o=", captured: [OFFER[1]!], replayed: ["o=peer 99 100 IN IP4 192.0.2.10"] }
      ])
    })

    it("an origin row beside a serialization choice is the origin row alone", () => {
      const replayed = [...swap(OFFER, OFFER[1]!, MINTED).slice(0, 6), ...OFFER.slice(6).reverse()].join("\n")
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed, "version-step-one")).toEqual([
        { ...originRow, versionStepOne: true }
      ])
    })

    it("a mark asked for a row that is no `o=` row marks nothing, and another row stands beside a marked one", () => {
      expect(diffSdp(VERBATIM, crlf(OFFER), crlf(swap(OFFER, "s=-", "s=call")), "minted")).toEqual([
        { section: "session", line: "s=", captured: ["s=-"], replayed: ["s=call"] }
      ])
      const replayed = crlf(swap(swap(OFFER, OFFER[1]!, MINTED), "a=ptime:20", "a=ptime:30"))
      expect(diffSdp(VERBATIM, crlf(OFFER), replayed, "minted")).toEqual([
        { ...originRow, mintedOrigin: true },
        { section: "m0", line: "a=ptime", captured: ["a=ptime:20"], replayed: ["a=ptime:30"] }
      ])
    })
  })

  it("erases the lane-owned fields only under the mask, and only the fields the render writes", () => {
    const rebooked = swap(
      swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"),
      "m=audio 6000 RTP/AVP 8 101",
      "m=audio 40000 RTP/AVP 8 101"
    )
    expect(diffSdp(REBOOKED, crlf(OFFER), crlf(rebooked))).toEqual([])
    expect(keysOf(diffSdp(FLOOR, crlf(OFFER), crlf(rebooked)))).toEqual(["session:c=", "m0:m="])
    expect(keysOf(diffSdp(VERBATIM, crlf(OFFER), crlf(rebooked)))).toEqual(["session:c=", "m0:m="])
    // The pair count rides beside a rebooked port.
    const paired = swap(OFFER, "m=audio 6000 RTP/AVP 8 101", "m=audio 6000/2 RTP/AVP 8 101")
    expect(diffSdp(REBOOKED, crlf(paired), crlf(swap(paired, "m=audio 6000/2 RTP/AVP 8 101", "m=audio 40000/2 RTP/AVP 8 101")))).toEqual([])
    expect(diffSdp(REBOOKED, crlf(paired), crlf(swap(paired, "m=audio 6000/2 RTP/AVP 8 101", "m=audio 40000 RTP/AVP 8 101")))).toHaveLength(1)
    // An IP6 connection line is never written by the render.
    const six = swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP6 ::1")
    for (const mask of MASKS) {
      expect(keysOf(diffSdp(mask, crlf(six), crlf(swap(six, "c=IN IP6 ::1", "c=IN IP6 ::2"))))).toEqual(["session:c="])
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
    expect(keysOf(rows)).toEqual([
      "m0:m=",
      "m0:a=rtpmap",
      "m0:a=fmtp",
      "m0:a=ptime",
      "m0:a=rtcp",
      "m1:m=",
      "m1:a=rtpmap",
      "m1:a=fmtp",
      "m1:a=ptime",
      "m1:a=rtcp"
    ])
  })

  it("a side that is empty or no session description is one `document` row with both texts whole", () => {
    expect(diffSdp(REBOOKED, crlf(OFFER), "")).toEqual([
      { section: "document", line: "sdp", captured: [crlf(OFFER)], replayed: [""] }
    ])
    expect(diffSdp(VERBATIM, crlf(OFFER), "<not/>")).toEqual([
      { section: "document", line: "sdp", captured: [crlf(OFFER)], replayed: ["<not/>"] }
    ])
    expect(diffSdp(REBOOKED, "", "")).toEqual([])
  })
})

describe("foldSdp", () => {
  it("folds two texts equal exactly when diffSdp states nothing", () => {
    const cases: ReadonlyArray<readonly [string, string]> = [
      [crlf(OFFER), crlf(OFFER)],
      [crlf(OFFER), OFFER.join("\n")],
      [crlf(OFFER), crlf(without(OFFER, "a=sendrecv"))],
      [crlf(OFFER), crlf(swap(OFFER, "c=IN IP4 192.0.2.10", "c=IN IP4 127.0.0.2"))],
      [crlf(OFFER), crlf(swap(OFFER, "a=ptime:20", "a=ptime:30"))],
      [crlf(OFFER), crlf(swap(OFFER, "o=- 1234 5678 IN IP4 192.0.2.10", "o=- 9 9 IN IP4 192.0.2.10"))],
      [crlf(OFFER), ""],
      ["", ""],
      ["<a/>", "<a/>"],
      ["<a/>", "<b/>"]
    ]
    for (const mask of MASKS) {
      for (const [a, b] of cases) {
        expect(foldSdp(mask, a) === foldSdp(mask, b)).toBe(diffSdp(mask, a, b).length === 0)
      }
    }
  })

  it("leaves a text that is no session description verbatim", () => {
    expect(foldSdp(VERBATIM, crlf(OFFER))).not.toBe(crlf(OFFER))
    expect(foldSdp(FLOOR, "<a/>")).toBe("<a/>")
    expect(foldSdp(FLOOR, "")).toBe("")
  })
})
