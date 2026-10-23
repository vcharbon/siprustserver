/**
 * The tag roster of one leg: the real tag pair every message states, read as
 * ORIGINATOR against ANSWERER and named by ordinal.
 *
 * A leg's originator holds one tag for its whole life, the From-tag of the
 * dialog-forming request — but it rides the To header on a reverse-direction
 * request (RFC 3261 §12.2.1.1), so a message is anchored before it is paired.
 * The answering tags are the distinct values the leg stated, in the order it
 * first stated them: a second value is a second dialog on the leg, whether the
 * far side forked or the near side re-tagged. Both the capture ladder and the
 * replay ladder name their tags through this one core, so the two cannot
 * drift in what they call a dialog.
 */

/** The two tags one message carries, as they were read off the wire. */
export interface TaggedMessage {
  readonly fromTag: string | undefined
  readonly toTag: string | undefined
}

/** The two tags of one message, as originator and answerer. */
export interface AnchoredPair {
  readonly from: string | undefined
  readonly to: string | undefined
  /**
   * True where neither of the message's tags is the leg's originator tag, so
   * the pairing is the message's own From/To rather than a derived one.
   */
  readonly unanchored: boolean
}

export interface LegTagRoster {
  /** The originator's tag; absent where the leg never stated one. */
  readonly origin: string | undefined
  /** Every answering tag the leg stated, distinct, in first-seen order. */
  readonly answering: readonly string[]
  /** One pair per input message, in input order. */
  readonly pairs: readonly AnchoredPair[]
  /** The bead-sized form of every tag of the leg, distinct within the leg. */
  readonly shortOf: ReadonlyMap<string, string>
  /** How many messages state `value` on the given side. */
  readonly count: (value: string, side: "from" | "to") => number
}

/**
 * A tag short enough for a bead, head and tail kept.
 *
 * Both ends carry the distinguishing bytes: vendors put a constant prefix on
 * one side and the per-dialog entropy on the other, and which side is which
 * differs per vendor, so neither end may be dropped.
 */
export const shortTag = (tag: string, max = 10): string =>
  tag.length <= max ? tag : `${tag.slice(0, max - 4)}…${tag.slice(-3)}`

/** Head, tail and difference-window widths of a disambiguated short form. */
const HEAD = 3
const TAIL = 3
const SPAN = 6

/** Head, the bytes around `at`, and tail: what tells two near-identical tags apart. */
const around = (value: string, at: number): string => {
  if (value.length <= HEAD + SPAN + TAIL + 2) return value
  const start = Math.max(HEAD, Math.min(at, value.length - TAIL - SPAN))
  return `${value.slice(0, HEAD)}…${value.slice(start, start + SPAN)}…${value.slice(-TAIL)}`
}

/**
 * Shortens a leg's tags to forms that still tell them APART, without letting
 * one grow to a width a bead cannot hold.
 *
 * Two forks of one leg routinely differ only in the MIDDLE — same platform
 * prefix, same suffix — and shortened head-and-tail they read as one tag, which
 * is exactly the fork the bead exists to show. So the head-and-tail form is
 * widened once, then abandoned for the earliest window that separates every tag
 * of the leg; the ordinal is the last resort, for tags no window separates.
 */
export const distinctShorts = (values: readonly string[]): readonly string[] => {
  const distinct = (shorts: readonly string[]): boolean => new Set(shorts).size === shorts.length
  for (const max of [10, 14]) {
    const shorts = values.map((value) => shortTag(value, max))
    if (distinct(shorts)) return shorts
  }
  const longest = Math.max(...values.map((value) => value.length))
  for (let at = 0; at < longest; at++) {
    const windowed = values.map((value) => around(value, at))
    if (distinct(windowed)) return windowed
  }
  return values.map((value, index) => `${around(value, HEAD)}#${index + 1}`)
}

/** The tag the leg's originator holds: the From-tag of its first tagged message. */
export const originatorTag = (messages: readonly TaggedMessage[]): string | undefined =>
  messages.find((message) => message.fromTag !== undefined)?.fromTag

/**
 * The two tags of one message, as originator and answerer.
 *
 * A response and a request in the leg's own direction state the originator on
 * From; a reverse-direction request — the callee's BYE, its re-INVITE — states
 * it on To. Where neither matches, the message is reported unanchored and its
 * own From/To are kept, so an unexpected wire still draws something true.
 */
export const anchor = (message: TaggedMessage, origin: string | undefined): AnchoredPair => {
  if (origin !== undefined && message.toTag === origin && message.fromTag !== origin) {
    return { from: message.toTag, to: message.fromTag, unanchored: false }
  }
  const unanchored = origin !== undefined && message.fromTag !== undefined && message.fromTag !== origin
  return { from: message.fromTag, to: message.toTag, unanchored }
}

/**
 * Names the tags of one leg. `messages` are the drawn ones, in ladder order;
 * `wholeLeg`, when given, is every message of the leg, since a filter may hide
 * the dialog-forming request and the originator's tag does not change with it.
 */
export const legTagRoster = (
  messages: readonly TaggedMessage[],
  wholeLeg: readonly TaggedMessage[] = messages
): LegTagRoster => {
  const origin = originatorTag(wholeLeg)
  const pairs = messages.map((message) => anchor(message, origin))
  const answering: string[] = []
  for (const pair of pairs) {
    if (pair.to !== undefined && !answering.includes(pair.to)) answering.push(pair.to)
  }
  // From and to are shortened together: a from-bead and a to-bead reading
  // alike on one arrow is the same confusion as two forks reading alike.
  const values = [...(origin === undefined ? [] : [origin]), ...answering]
  const shorts = distinctShorts(values)
  const shortOf = new Map(values.map((value, index) => [value, shorts[index] ?? value]))
  const count = (value: string, side: "from" | "to"): number =>
    pairs.filter((pair) => (side === "from" ? pair.from : pair.to) === value).length
  return { origin, answering, pairs, shortOf, count }
}
