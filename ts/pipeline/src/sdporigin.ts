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
 * the replayed side. A {@link Ledger} holds one leg's descriptions in wire
 * order and admits a pair only when both origins are minted, differ in the two
 * numbers alone, keep a one-to-one sess-id pairing across the leg (a session
 * the capture keeps, the replay keeps; a session the capture changes, the
 * replay changes), and step sess-version by the same amount since that
 * session's previous description on the leg (RFC 3264 §8).
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

/** One leg's origin reading, in wire order. */
export interface Ledger {
  /**
   * Reads one captured/replayed description pair of the leg and says whether
   * its origins are a minted origin and its counterpart. Every pair whose
   * origins parse is recorded, admitted or not, so a later pair is held to
   * the sessions the leg has already shown.
   */
  readonly read: (captured: string, replayed: string) => boolean
}

/** A ledger for one leg, over the identities each side drove into the endpoint. */
export const ledger = (driven: Driven): Ledger => {
  const toReplayed = new Map<bigint, bigint>()
  const toCaptured = new Map<bigint, bigint>()
  const lastCaptured = new Map<bigint, bigint>()
  const lastReplayed = new Map<bigint, bigint>()
  return {
    read: (capturedText, replayedText) => {
      const a = originOf(capturedText)
      const b = originOf(replayedText)
      if (a === undefined || b === undefined) return false
      const paired = (toReplayed.get(a.sessId) ?? b.sessId) === b.sessId &&
        (toCaptured.get(b.sessId) ?? a.sessId) === a.sessId
      const before = lastCaptured.get(a.sessId)
      const beforeReplayed = lastReplayed.get(b.sessId)
      const stepped = before === undefined || beforeReplayed === undefined
        ? before === beforeReplayed
        : a.sessVersion - before === b.sessVersion - beforeReplayed
      const admitted = paired &&
        stepped &&
        a.username === b.username &&
        a.network === b.network &&
        !driven.captured.has(identityOf(a)) &&
        !driven.replayed.has(identityOf(b))
      if (!toReplayed.has(a.sessId)) toReplayed.set(a.sessId, b.sessId)
      if (!toCaptured.has(b.sessId)) toCaptured.set(b.sessId, a.sessId)
      lastCaptured.set(a.sessId, a.sessVersion)
      lastReplayed.set(b.sessId, b.sessVersion)
      return admitted
    }
  }
}

/** The text with its first `o=` line replaced by the first one of `from`; the text itself where either states none. */
export const withOriginOf = (text: string, from: string): string => {
  const line = ORIGIN_LINE.exec(from)?.[0]
  return line === undefined ? text : text.replace(ORIGIN_LINE, () => line)
}
