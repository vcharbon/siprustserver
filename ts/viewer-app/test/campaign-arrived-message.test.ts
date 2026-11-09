/** A check on a datagram that arrived leads to that datagram: the verdict's failure says what came on which leg, the recording holds it. */
import { parseRecording } from "@sip/viewer-core"
import { describe, expect, it } from "vitest"
import { arrivedMessages } from "../src/campaign-arrived-message.ts"

const line = (seq: number, dir: "in" | "out", raw: string, extra: Readonly<Record<string, string>> = {}): string =>
  JSON.stringify({ seq, dir, at_us: seq * 1000, raw, ...extra })

const bye = "BYE sip:b@example.test SIP/2.0\r\nCall-ID: b\r\nCSeq: 7 BYE\r\n\r\n"
const terminated = "SIP/2.0 487 Request Terminated\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n"
const ringing = "SIP/2.0 180 Ringing\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n"
const invite = "INVITE sip:a@example.test SIP/2.0\r\nCall-ID: c\r\nCSeq: 9 INVITE\r\n\r\n"

const { messages } = parseRecording({
  A: [
    line(1, "in", ringing, { step: "s2" }),
    line(2, "in", ringing),
    line(3, "in", terminated)
  ].join("\n"),
  B: [
    line(1, "in", bye, { note: "arrived during the close" }),
    line(2, "out", "SIP/2.0 200 OK\r\nCall-ID: b\r\nCSeq: 7 BYE\r\n\r\n"),
    line(3, "in", bye, { note: "names no dialog this leg holds" })
  ].join("\n"),
  unattributed: line(1, "in", invite, { note: "no leg claims it" })
})

const response = (status: number) => ({ kind: "response", status, reason: "x", cseq_method: "INVITE", cseq: 1 })
const check = (field: string, arrived: unknown) => ({ field, seen: JSON.stringify(arrived) })
const verdict = (...failures: ReadonlyArray<unknown>) => ({ failures })

describe("the datagram a check names", () => {
  it("is the response that arrived on the failure's leg", () => {
    const arrived = response(487)
    expect(arrivedMessages(
      [check("unmatched-datagram", arrived)],
      verdict({ failure: "unmatched-datagram", step: "s9", leg: "A", arrived }),
      messages
    )).toEqual(["A#3"])
  })

  it("is the copy no step took, where a step took an earlier one", () => {
    const arrived = response(180)
    expect(arrivedMessages(
      [check("unexpected-datagram", arrived)],
      verdict({ failure: "unexpected-datagram", leg: "A", arrived }),
      messages
    )).toEqual(["A#2"])
  })

  it("is the line whose note the failure's detail states, among the same request twice", () => {
    const arrived = { kind: "request", method: "BYE", cseq: 7 }
    expect(arrivedMessages(
      [check("unexpected-datagram", arrived)],
      verdict({ failure: "unexpected-datagram", leg: "B", arrived, detail: "names no dialog this leg holds" }),
      messages
    )).toEqual(["B#3"])
  })

  it("is sought on every leg where the failure names no recorded one", () => {
    const arrived = { kind: "request", method: "INVITE", cseq: 9 }
    expect(arrivedMessages(
      [check("unexpected-datagram", arrived)],
      verdict({ failure: "unexpected-datagram", leg: "(actor alice)", arrived, detail: "no leg claims it" }),
      messages
    )).toEqual(["unattributed#1"])
  })

  it("is told apart per check: two failures of one kind read their own datagram", () => {
    const first = response(487)
    const second = { kind: "request", method: "BYE", cseq: 7 }
    expect(arrivedMessages(
      [check("unexpected-datagram", second), check("unexpected-datagram", first)],
      verdict(
        { failure: "unexpected-datagram", leg: "A", arrived: first },
        { failure: "unexpected-datagram", leg: "B", arrived: second }
      ),
      messages
    )).toEqual(["B#1", "A#3"])
  })

  it("is none for another check, an unreadable datagram, a message never recorded or no verdict", () => {
    const unreadable = { kind: "unreadable" }
    const lost = response(503)
    expect(arrivedMessages(
      [{ field: "header(Allow)", seen: "x" }, check("unexpected-datagram", unreadable), check("unmatched-datagram", lost)],
      verdict(
        { failure: "unexpected-datagram", leg: "A", arrived: unreadable },
        { failure: "unmatched-datagram", leg: "A", arrived: lost }
      ),
      messages
    )).toEqual([undefined, undefined, undefined])
    expect(arrivedMessages([check("unexpected-datagram", lost)], undefined, messages)).toEqual([undefined])
  })
})
