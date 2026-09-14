import { describe, expect, it } from "vitest"
import { bodiesEqual, foldBody } from "../src/bodyfold.js"

describe("foldBody", () => {
  const declared = '<?xml version="1.0" encoding="utf-8"?>\r\n<a x="1" y="2">\r\n  <b>t</b>\r\n</a>\r\n'

  it("`exact` is identity, and the default", () => {
    expect(foldBody("exact", declared)).toBe(declared)
    expect(foldBody(undefined, declared)).toBe(declared)
  })

  it("`xml` drops the declaration and the whitespace between tags, and trims", () => {
    expect(foldBody("xml", declared)).toBe('<a x="1" y="2"><b>t</b></a>')
    expect(bodiesEqual("xml", declared, '<a x="1" y="2"><b>t</b></a>')).toBe(true)
  })

  it("`xml` erases nothing else: attribute order, text whitespace, comments and entities all count", () => {
    expect(bodiesEqual("xml", '<a x="1" y="2"/>', '<a y="2" x="1"/>')).toBe(false)
    expect(bodiesEqual("xml", "<b>t u</b>", "<b>t  u</b>")).toBe(false)
    expect(bodiesEqual("xml", "<a><!-- c --></a>", "<a></a>")).toBe(false)
    expect(bodiesEqual("xml", "<b>&amp;</b>", "<b>&#38;</b>")).toBe(false)
    expect(bodiesEqual("xml", "<a/>", "<a></a>")).toBe(false)
  })

  describe("`sdp` reads both sides as a session description under the mask", () => {
    const offer = (over: Partial<Record<"o" | "c" | "m" | "codec" | "ptime" | "dir", string>> = {}): string =>
      [
        "v=0",
        over.o ?? "o=- 1 2 IN IP4 192.0.2.10",
        "s=-",
        over.c ?? "c=IN IP4 192.0.2.10",
        "t=0 0",
        over.m ?? "m=audio 6000 RTP/AVP 8",
        over.codec ?? "a=rtpmap:8 PCMA/8000",
        over.ptime ?? "a=ptime:20",
        over.dir ?? "a=sendrecv",
        ""
      ].join("\r\n")
    const rebooked = { connectionAddress: true, mediaPort: true, verbatim: false }
    const verbatim = { connectionAddress: false, mediaPort: false, verbatim: true }

    it("is true across attribute order, line endings, the `o=` floor and the masked fields", () => {
      expect(bodiesEqual("sdp", offer(), offer().replace(/\r\n/g, "\n"))).toBe(true)
      expect(bodiesEqual("sdp", offer(), offer().replace("a=ptime:20\r\na=sendrecv", "a=sendrecv\r\na=ptime:20"))).toBe(true)
      expect(bodiesEqual("sdp", offer(), offer({ o: "o=- 77 78 IN IP4 192.0.2.10" }))).toBe(true)
      const moved = offer({ c: "c=IN IP4 127.0.0.2", m: "m=audio 40000 RTP/AVP 8" })
      expect(bodiesEqual("sdp", offer(), moved, rebooked)).toBe(true)
      expect(bodiesEqual("sdp", offer(), moved)).toBe(false)
    })

    it("is false on a codec, a ptime, a direction, an extra section or a missing body", () => {
      expect(bodiesEqual("sdp", offer(), offer({ m: "m=audio 6000 RTP/AVP 0", codec: "a=rtpmap:0 PCMU/8000" }), rebooked)).toBe(false)
      expect(bodiesEqual("sdp", offer(), offer({ ptime: "a=ptime:30" }), rebooked)).toBe(false)
      expect(bodiesEqual("sdp", offer(), offer({ dir: "a=sendonly" }), rebooked)).toBe(false)
      expect(bodiesEqual("sdp", offer(), `${offer()}m=video 0 RTP/AVP 96\r\n`, rebooked)).toBe(false)
      expect(bodiesEqual("sdp", offer(), "", rebooked)).toBe(false)
    })

    it("on a verbatim run is identity: the same bytes and nothing less", () => {
      expect(foldBody("sdp", offer(), verbatim)).toBe(offer())
      expect(bodiesEqual("sdp", offer(), offer(), verbatim)).toBe(true)
      expect(bodiesEqual("sdp", offer(), offer().replace(/\r\n/g, "\n"), verbatim)).toBe(false)
      expect(bodiesEqual("sdp", offer(), offer({ c: "c=IN IP4 127.0.0.2" }), verbatim)).toBe(false)
    })
  })
})
