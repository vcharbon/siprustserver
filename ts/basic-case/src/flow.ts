/**
 * The pivot step builder a shape writes its call with: one `send` or `expect`
 * at a time, each step's id, its dwell and its dialog marking derived here
 * (`PCAP2TEST_PIVOT_V3.md` §6.1).
 *
 * A leg's 2xx to its INVITE confirms its dialog: every later step of the leg
 * is `in_dialog`, and the leg's first ACK after it is the one that
 * `confirms_dialog`. An ACK to a non-2xx final belongs to the INVITE's own
 * transaction (RFC 3261 §17.1.1.3) and is neither.
 */
import { type Check, type Flow, type Msg, Tokens } from "@sip/contracts"

/** What a step waits on: the step its dwell is measured from (the call's start where absent) and the dwell. */
export interface Timing {
  readonly from?: string
  readonly ms?: number
}

/**
 * The early dialog a step rides, where its leg's INVITE forked: the label the
 * leg gives that fork's To-tag (RFC 3261 §12.1, §13.2.2.4). Absent, the step
 * rides the leg's one dialog.
 */
export interface Fork {
  readonly early?: string
}

/** Where a step sits: its timing and the early dialog it rides. */
export interface Placement extends Timing, Fork {}

/** What an `expect` may state beside its placement. */
export interface ExpectOptions extends Placement {
  /** The message is the stack's own (a 100 Trying, a hop's ACK): recorded, never asserted. */
  readonly auto?: boolean
  readonly checks?: ReadonlyArray<Check.Check>
}

/** What a `send` may state beside its placement. */
export interface SendOptions extends Placement {
  /** The stack sends it unasked (the ACK to a final). */
  readonly auto?: boolean
}

const isInvite2xx = (msg: Msg.MsgSpec): boolean =>
  msg["cseq-method"] === "INVITE" && msg.status !== undefined && msg.status >= 200 && msg.status < 300

/** The steps of one call, in the order they were added. */
export class CallFlow {
  readonly steps: Array<Flow.Step> = []
  private readonly confirmed = new Set<string>()
  private readonly ackOwed = new Set<string>()
  private next = 1

  /** Send `msg` on `leg`: the step's id. */
  send(leg: string, msg: Msg.MsgSpec, options: SendOptions = {}): string {
    return this.add(leg, "send", msg, options, options.auto === true ? { auto: true } : {})
  }

  /** Expect `msg` on `leg`: the step's id. */
  expect(leg: string, msg: Msg.MsgSpec, options: ExpectOptions = {}): string {
    const checks = options.checks ?? []
    return this.add(leg, "expect", msg, options, {
      ...(options.auto === true ? { auto: true } : {}),
      check: options.auto === true ? "record" : "assert",
      ...(checks.length > 0 ? { checks: [...checks] } : {})
    })
  }

  private add(leg: string, op: Flow.Op, msg: Msg.MsgSpec, timing: Placement, fields: Partial<Flow.Step>): string {
    const id = `s${this.next++}`
    const inDialog = this.confirmed.has(leg)
    const confirms = inDialog && msg.method === "ACK" && this.ackOwed.delete(leg)
    this.steps.push({
      id,
      leg,
      op,
      ...(timing.early === undefined ? {} : { early: timing.early }),
      ...(inDialog ? { in_dialog: true } : {}),
      ...(confirms ? { confirms_dialog: true } : {}),
      ...fields,
      msg,
      delay: {
        ms: timing.ms ?? 0,
        from: Tokens.anchorToken(timing.from === undefined ? { _tag: "trigger" } : { _tag: "step", step: timing.from }),
        compressible: true,
        timer_linked: false
      }
    })
    if (isInvite2xx(msg) && !this.confirmed.has(leg)) {
      this.confirmed.add(leg)
      this.ackOwed.add(leg)
    }
    return id
  }
}
