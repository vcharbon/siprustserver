/**
 * The `[timing]` line: one phase's duration, stated by the process that ran it.
 *
 * The ingest, the cut and the case generator each print one line per phase they
 * finish (`[timing] {"phase":"cut.mirror","ms":812,…}`) into the log they
 * already write, so a reader of that log — the replay bench above all — knows
 * where the time went without a profiler. A line is one JSON object after the
 * tag; `ms` is wall time, `maxRssKb` the peak resident set of the process that
 * printed it (never of its children), and `steps` splits `ms` by named step.
 */
import * as Schema from "effect/Schema"

/** What every timing line opens with. */
export const TAG = "[timing]"

export const TimingLine = Schema.Struct({
  phase: Schema.String,
  ms: Schema.Number,
  capture: Schema.optionalKey(Schema.String),
  prep: Schema.optionalKey(Schema.Number),
  part: Schema.optionalKey(Schema.Number),
  captures: Schema.optionalKey(Schema.Number),
  cases: Schema.optionalKey(Schema.Number),
  maxRssKb: Schema.optionalKey(Schema.Number),
  steps: Schema.optionalKey(Schema.Record(Schema.String, Schema.Number)),
  broke: Schema.optionalKey(Schema.String)
})
export interface TimingLine extends Schema.Schema.Type<typeof TimingLine> {}

const decode = Schema.decodeUnknownSync(TimingLine)

/** The line as a log carries it. */
export const format = (line: TimingLine): string => `${TAG} ${JSON.stringify(line)}`

/** The line, printed on stdout. */
export const emit = (line: TimingLine): void => {
  console.log(format(line))
}

/** Every well-formed timing line of a log, in order; any other line is passed over. */
export const parse = (text: string): ReadonlyArray<TimingLine> => {
  const lines: Array<TimingLine> = []
  for (const raw of text.split("\n")) {
    const at = raw.indexOf(TAG)
    if (at < 0) continue
    try {
      lines.push(decode(JSON.parse(raw.slice(at + TAG.length)) as unknown))
    } catch {
      continue
    }
  }
  return lines
}

/** This process's own peak resident set, in kilobytes. */
export const selfPeakKb = (): number => process.resourceUsage().maxRSS

/** A phase's clock: each lap charged to a named step, the total from the start. */
export interface Stopwatch {
  /** Charges the time since the previous lap (or the start) to `step`, adding to an earlier lap of it. */
  readonly lap: (step: string) => void
  /** Every step's milliseconds, rounded. */
  readonly steps: () => Readonly<Record<string, number>>
  /** Milliseconds since the start, rounded. */
  readonly total: () => number
}

export const stopwatch = (now: () => number = () => performance.now()): Stopwatch => {
  const start = now()
  let last = start
  const spent = new Map<string, number>()
  return {
    lap: (step) => {
      const at = now()
      spent.set(step, (spent.get(step) ?? 0) + at - last)
      last = at
    },
    steps: () => Object.fromEntries([...spent].map(([step, ms]) => [step, Math.round(ms)])),
    total: () => Math.round(now() - start)
  }
}
