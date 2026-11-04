/** A two-leg run recording laid out as the call details draw it: the caller's INVITE sent twice at one step. */
import { buildRecordingLayout, type LadderLayout, parseRecording } from "@sip/viewer-core"

const line = (seq: number, dir: "in" | "out", atUs: number, step: string | undefined, raw: string): string =>
  JSON.stringify({ seq, dir, at_us: atUs, ...(step === undefined ? {} : { step }), raw })

const INVITE = "INVITE sip:b@example.test SIP/2.0\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n"
const OK = "SIP/2.0 200 OK\r\nCall-ID: b\r\nCSeq: 1 INVITE\r\n\r\n"

/** A#1 at 0 µs (s1), B#1 at 16 ms (s3), B#2 at 40 ms (s4), A#2 at 500 ms (s1 again), B#3 at 900 ms (no step). */
export const ladder = (): LadderLayout =>
  buildRecordingLayout(parseRecording({
    A: [line(1, "out", 0, "s1", INVITE), line(2, "out", 500_000, "s1", INVITE)].join("\n"),
    B: [
      line(1, "in", 16_000, "s3", INVITE.replace("Call-ID: a", "Call-ID: b")),
      line(2, "out", 40_000, "s4", OK),
      line(3, "out", 900_000, undefined, OK)
    ].join("\n")
  }))
