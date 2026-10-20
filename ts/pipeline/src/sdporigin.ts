/**
 * The session origin (RFC 4566 §5.2 `o=`) of a description, and whether the
 * origins a capture and a replay carry on one leg are an origin the replayed
 * endpoint MINTS, read the same way on both sides.
 *
 * An endpoint that opens a session picks its sess-id and first sess-version
 * itself (§5.2 suggests an NTP timestamp), so no replay reproduces those two
 * numbers. Every other field of the line, and every number of an origin the
 * endpoint only relays, is reproducible and compared byte for byte.
 *
 * An origin is MINTED on a side when no description that side drove into the
 * endpoint carries its identity (username, sess-id, nettype, addrtype,
 * address): the capture's sends for the captured side, the run's own sends for
 * the replayed side. {@link readOrigins} reads one cell's description pairs
 * and admits a pair only when both origins are minted, differ in the two
 * numbers alone, keep a one-to-one pairing of origin identities across the
 * cell (a session the capture keeps, the replay keeps, on whichever leg; a
 * session the capture changes, the replay changes), and step sess-version by
 * the same amount since that session's previous description, each side in its
 * OWN order: the capture's by the flow, the replay's by its wire (RFC 3264
 * §8). A session's first description opens in the same form on both sides
 * (sess-version equal to sess-id on both or on neither).
 *
 * The complementary reading is per leg: where the capture's description
 * CONTINUES the session its previous one on that leg carried, the replay's
 * does not continue its own previous one there — another identity, or a
 * version step other than the capture's — {@link notContinued} reads it as the
 * replayed endpoint breaking the session it holds toward that peer.
 */

/** One parsed `o=` line: the two numbers apart from the fields every run reproduces. */
export interface Origin {
  readonly username: string
  readonly sessId: bigint
  readonly sessVersion: bigint
  /** `<nettype> <addrtype> <unicast-address>`, as written. */
  readonly network: string
}

const ORIGIN_LINE = /^o=[^\r\n]*/m
const ORIGIN_LINES = /^o=[^\r\n]*/gm
const NUMBER = /^[0-9]+$/

/** One `o=` line read as six fields with numeric sess-id and sess-version; undefined otherwise. */
export const parseOrigin = (line: string): Origin | undefined => {
  const fields = line.slice(2).trimEnd().split(" ")
  if (fields.length !== 6) return undefined
  const [username, id, version, nettype, addrtype, address] = fields as [string, string, string, string, string, string]
  if (!NUMBER.test(id) || !NUMBER.test(version)) return undefined
  return { username, sessId: BigInt(id), sessVersion: BigInt(version), network: `${nettype} ${addrtype} ${address}` }
}

/** The origin of a session description's text: its first `o=` line; undefined where it states none readable. */
export const originOf = (text: string): Origin | undefined => {
  const line = ORIGIN_LINE.exec(text)?.[0]
  return line === undefined ? undefined : parseOrigin(line)
}

/** The identity §5.2 makes globally unique: every field but sess-version. */
export const identityOf = (origin: Origin): string => `${origin.username} ${origin.sessId} ${origin.network}`

/** The identities of every readable `o=` line the texts carry. */
export const identitiesIn = (texts: Iterable<string>): ReadonlySet<string> => {
  const out = new Set<string>()
  for (const text of texts) {
    for (const line of text.match(ORIGIN_LINES) ?? []) {
      const origin = parseOrigin(line)
      if (origin !== undefined) out.add(identityOf(origin))
    }
  }
  return out
}

/** The identities each side drove into the replayed endpoint. */
export interface Driven {
  readonly captured: ReadonlySet<string>
  readonly replayed: ReadonlySet<string>
}

/** One captured/replayed description pair, with where it sits in each side's own order. */
export interface Sighting {
  readonly captured: string
  readonly replayed: string
  /** Its place in the capture's order: the flow step's position. */
  readonly capturedAt: number
  /** Its place in the replay's order: the wire position of its reception. */
  readonly replayedAt: number
  /** The leg that received it. */
  readonly leg: number
}

/** Each sighting's predecessor on its own side: the index of the same identity's previous one in `order`. */
const predecessors = (
  identities: ReadonlyArray<string | undefined>,
  order: ReadonlyArray<number>
): ReadonlyArray<number | undefined> => {
  const out: Array<number | undefined> = identities.map(() => undefined)
  const last = new Map<string, number>()
  for (const at of order) {
    const identity = identities[at]
    if (identity === undefined) continue
    out[at] = last.get(identity)
    last.set(identity, at)
  }
  return out
}

const orderBy = (sightings: ReadonlyArray<Sighting>, key: (s: Sighting) => number): ReadonlyArray<number> =>
  sightings.map((_, at) => at).sort((a, b) => key(sightings[a]!) - key(sightings[b]!) || a - b)

/**
 * Whether each sighting's origins are a minted origin and its counterpart.
 * Every pair whose origins parse takes part in the pairing and the steps,
 * admitted or not, so a pair is held to every session the cell shows.
 */
export const readOrigins = (sightings: ReadonlyArray<Sighting>, driven: Driven): ReadonlyArray<boolean> => {
  const left = sightings.map((s) => originOf(s.captured))
  const right = sightings.map((s) => originOf(s.replayed))
  const parsed = (at: number): boolean => left[at] !== undefined && right[at] !== undefined
  const leftId = left.map((o, at) => (o === undefined || !parsed(at) ? undefined : identityOf(o)))
  const rightId = right.map((o, at) => (o === undefined || !parsed(at) ? undefined : identityOf(o)))
  const byReplay = orderBy(sightings, (s) => s.replayedAt)
  const beforeLeft = predecessors(leftId, orderBy(sightings, (s) => s.capturedAt))
  const beforeRight = predecessors(rightId, byReplay)
  // The pairing is one-to-one over the cell; the replay's order says which
  // pair a broken pairing is charged to.
  const paired: Array<boolean> = sightings.map(() => false)
  const toReplayed = new Map<string, string>()
  const toCaptured = new Map<string, string>()
  for (const at of byReplay) {
    const l = leftId[at]
    const r = rightId[at]
    if (l === undefined || r === undefined) continue
    paired[at] = (toReplayed.get(l) ?? r) === r && (toCaptured.get(r) ?? l) === l
    if (!toReplayed.has(l)) toReplayed.set(l, r)
    if (!toCaptured.has(r)) toCaptured.set(r, l)
  }
  return sightings.map((_, at) => {
    const a = left[at]
    const b = right[at]
    if (a === undefined || b === undefined) return false
    const pa = beforeLeft[at]
    const pb = beforeRight[at]
    const stepped = pa === undefined || pb === undefined
      ? pa === pb && (a.sessVersion === a.sessId) === (b.sessVersion === b.sessId)
      : a.sessVersion - left[pa]!.sessVersion === b.sessVersion - right[pb]!.sessVersion
    return paired[at]! &&
      stepped &&
      a.username === b.username &&
      a.network === b.network &&
      !driven.captured.has(identityOf(a)) &&
      !driven.replayed.has(identityOf(b))
  })
}

/**
 * Whether each sighting is a session the capture continues on its leg and the
 * replay does not: the capture's previous sighting on the leg (capture order)
 * carries the same identity, and the replay's previous one there (replay
 * order) carries another identity, or a version the replay steps by another
 * amount than the capture. A side whose origin does not parse, here or at the
 * predecessor, reads nothing.
 */
export const notContinued = (sightings: ReadonlyArray<Sighting>): ReadonlyArray<boolean> => {
  const left = sightings.map((s) => originOf(s.captured))
  const right = sightings.map((s) => originOf(s.replayed))
  const onLeg = (key: (s: Sighting) => number): ReadonlyArray<number | undefined> => {
    const out: Array<number | undefined> = sightings.map(() => undefined)
    const last = new Map<number, number>()
    for (const at of orderBy(sightings, key)) {
      const leg = sightings[at]!.leg
      out[at] = last.get(leg)
      last.set(leg, at)
    }
    return out
  }
  const beforeLeft = onLeg((s) => s.capturedAt)
  const beforeRight = onLeg((s) => s.replayedAt)
  return sightings.map((_, at) => {
    const a = left[at]
    const b = right[at]
    const pa = beforeLeft[at]
    const pb = beforeRight[at]
    const priorA = pa === undefined ? undefined : left[pa]
    const priorB = pb === undefined ? undefined : right[pb]
    if (a === undefined || b === undefined || priorA === undefined || priorB === undefined) return false
    if (identityOf(priorA) !== identityOf(a)) return false
    return identityOf(priorB) !== identityOf(b) ||
      b.sessVersion - priorB.sessVersion !== a.sessVersion - priorA.sessVersion
  })
}

/**
 * How one description pair's origins read: an origin the replayed endpoint
 * mints and its counterpart, a session the replay did not continue where the
 * capture did, or neither (`undefined`).
 */
export type OriginReading = "minted" | "not-continued" | undefined

/** Every sighting's reading, {@link readOrigins} and {@link notContinued} together. */
export const readings = (sightings: ReadonlyArray<Sighting>, driven: Driven): ReadonlyArray<OriginReading> => {
  const minted = readOrigins(sightings, driven)
  const broken = notContinued(sightings)
  return sightings.map((_, at) => (minted[at] ? "minted" : broken[at] ? "not-continued" : undefined))
}

/**
 * The origin reading of one reception, as the body confrontation asks it: one
 * `read` per description pair the reception carries, in the order it carries
 * them.
 */
export interface Reception {
  readonly read: (captured: string, replayed: string) => OriginReading
}

/** Where a reception sits in each side's own order, and the leg that received it. */
export interface Place {
  readonly capturedAt: number
  readonly replayedAt: number
  readonly leg: number
}

/**
 * The two passes of a cell's origin reading over one deterministic walk of
 * its receptions: `collecting` notes every pair and answers nothing, then
 * `answering` answers the same walk, call for call, from {@link readings}.
 */
export interface Ledger {
  readonly at: (place: Place) => Reception
}

/** The first pass: every pair noted, in walk order. */
export const collecting = (into: Array<Sighting>): Ledger => ({
  at: (place) => ({
    read: (captured, replayed) => {
      into.push({ captured, replayed, ...place })
      return undefined
    }
  })
})

/** The second pass: the answers of the first pass's pairs, in the same walk order. */
export const answering = (answers: ReadonlyArray<OriginReading>): Ledger => {
  let next = 0
  return { at: () => ({ read: () => answers[next++] }) }
}

/** The text with its first `o=` line replaced by the first one of `from`; the text itself where either states none. */
export const withOriginOf = (text: string, from: string): string => {
  const line = ORIGIN_LINE.exec(from)?.[0]
  return line === undefined ? text : text.replace(ORIGIN_LINE, () => line)
}
