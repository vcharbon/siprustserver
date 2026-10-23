/**
 * The LADDER DELTA of two runs of one call: their replay ladders, in the line
 * shape `@sip/viewer-core`'s ladder text draws (`m<idx> +t from -> to |
 * message | detail | dialog | step`), compared event by event once each
 * ladder is normalised of what a run mints without the call differing.
 *
 * {@link normaliseLadder} keeps the EVENT lines alone and rewrites each:
 *
 * - the head (title, subtitle, warnings, column header) is not compared: the
 *   title names the run, and the counts it states follow from the events;
 * - `m<idx>` is dropped: an event's position is the comparison's own;
 * - the time column is dropped: two real-clock runs never share a microsecond,
 *   and the order of the events is what the ladder states of time;
 * - `CSeq <n>` reads `CSeq +<k>`, k counted from the first number its space
 *   showed: a space is the leg (the dialog column's from-token) and the party
 *   that sent the request (a response's CSeq is its request's). Each endpoint
 *   picks its first CSeq (RFC 3261 §8.1.1.5); how it steps is the call's.
 *
 * Everything else — parties, methods, statuses and reasons, dialog tokens
 * (already derived, never real tags), steps — is compared as drawn.
 *
 * {@link ladderDelta} states the first event where the normalised ladders
 * part, as the two normalised lines (null on the side that ran out), and how
 * many lines differ: the longer ladder's lines outside the two ladders'
 * longest common subsequence, so a changed line and a missing line each count
 * once.
 */

/** The first event two ladders part at: `m<line>` on both sides, each side's line normalised. */
export interface LadderDifference {
  readonly line: number
  readonly before: string | null
  readonly after: string | null
}

/** Where two ladders part first, and how many lines differ; no difference and 0 for one call drawn twice. */
export interface LadderDelta {
  readonly firstDifference: LadderDifference | null
  readonly differingLines: number
}

const EVENT = /^\s*m\d+(?: \+(?:\d+\.\d+|-\.-+))? (.*)$/
const HEADER = /^ladder \(.*?\bfrom -> to \| (.*)\):$/
const PARTIES = /^(.*?) -> (.*?) \| /
const CSEQ = /\bCSeq (\d+)\b/
const STATUS = /^\d{3}\b/

/** The columns a ladder draws after `from -> to`, read off its column header; none where it states none. */
const columnsOf = (lines: ReadonlyArray<string>): ReadonlyArray<string> => {
  for (const line of lines) {
    const header = HEADER.exec(line)
    if (header !== null) return header[1]!.split(" | ")
  }
  return []
}

/** The ladder's event lines, each normalised; the head is left out. */
export const normaliseLadder = (lines: ReadonlyArray<string>): ReadonlyArray<string> => {
  const columns = columnsOf(lines)
  const message = columns.indexOf("message")
  const dialog = columns.indexOf("dialog")
  const firsts = new Map<string, bigint>()
  const out: Array<string> = []
  for (const line of lines) {
    const event = EVENT.exec(line)?.[1]
    if (event === undefined) continue
    const parties = PARTIES.exec(event)
    const cseq = CSEQ.exec(event)
    if (parties === null || cseq === null) {
      out.push(event)
      continue
    }
    const cells = event.slice(parties[0].length).split(" | ")
    const label = message < 0 ? "" : cells[message] ?? ""
    const sender = STATUS.test(label) ? parties[2]! : parties[1]!
    const leg = dialog < 0 ? "" : (cells[dialog] ?? "").split(" · ")[0]!
    const space = `${leg}\u0000${sender}`
    const n = BigInt(cseq[1]!)
    const first = firsts.get(space) ?? n
    firsts.set(space, first)
    const step = n - first
    out.push(event.replace(CSEQ, `CSeq ${step < 0n ? "-" : "+"}${step < 0n ? -step : step}`))
  }
  return out
}

/** The length of the longest common subsequence of two line lists, in two rows of memory. */
const commonLength = (a: ReadonlyArray<string>, b: ReadonlyArray<string>): number => {
  let previous = new Array<number>(b.length + 1).fill(0)
  let current = new Array<number>(b.length + 1).fill(0)
  for (let i = 1; i <= a.length; i++) {
    for (let j = 1; j <= b.length; j++) {
      current[j] = a[i - 1] === b[j - 1] ? previous[j - 1]! + 1 : Math.max(previous[j]!, current[j - 1]!)
    }
    ;[previous, current] = [current, previous]
  }
  return previous[b.length]!
}

/** The delta of two ladders of one call, each as the ladder text draws it. */
export const ladderDelta = (before: ReadonlyArray<string>, after: ReadonlyArray<string>): LadderDelta => {
  const a = normaliseLadder(before)
  const b = normaliseLadder(after)
  const longer = Math.max(a.length, b.length)
  let at = 0
  while (at < longer && a[at] === b[at]) at++
  if (at === longer) return { firstDifference: null, differingLines: 0 }
  return {
    firstDifference: { line: at, before: a[at] ?? null, after: b[at] ?? null },
    differingLines: longer - commonLength(a, b)
  }
}
