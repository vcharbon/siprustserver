/**
 * Content naming: the sha-256 a stored document, blob or CASE is keyed by.
 *
 * A document is hashed over CANONICAL JSON — keys sorted, `undefined` dropped —
 * so a re-cut or a re-run that only moved a key re-stores nothing. Bytes are
 * hashed as they arrive: a flows document and a case asset are stored
 * byte-identically, and canonicalising them would name two different blobs the
 * same.
 *
 * A CASE is named over everything it carries, because its assets and its ledger
 * rows are not derivable from its document — a rules change re-cuts one
 * document into a different routing answer — and a name blind to them calls two
 * different cases one case.
 *
 * One home for all three, because the cut and the replay must name the same
 * case the same.
 */
import * as crypto from "node:crypto"

/** Stable content hash of a case document: sorted keys, so key order never re-stores. */
export const hashDocument = (document: unknown): { readonly hash: string; readonly text: string } => {
  const text = canonical(document)
  return { hash: crypto.createHash("sha256").update(text).digest("hex"), text }
}

/** Content hash of a blob, over the bytes themselves. */
export const hashBytes = (bytes: Uint8Array): string => hashChunks([bytes])

/** The same name for a blob read in pieces: the bytes are hashed in order. */
export const hashChunks = (chunks: Iterable<Uint8Array>): string => {
  const digest = crypto.createHash("sha256")
  for (const chunk of chunks) digest.update(chunk)
  return digest.digest("hex")
}

/** Everything one case carries, which is everything its hash covers. */
export interface CaseParts {
  readonly document: unknown
  readonly assets: ReadonlyArray<{ readonly path: string; readonly bytes: Uint8Array }>
  readonly ledger: ReadonlyArray<{ readonly number: string; readonly document: unknown }>
}

/**
 * Stable content hash of a whole case, and of its document apart: the document
 * is stored and deduped under its own name, the case under a name that also
 * covers its assets and its ledger rows. Assets and rows are hashed in KEY
 * order, matching the store's own keys, so the order the cut read them in never
 * re-names the case.
 */
export const hashCase = (parts: CaseParts): {
  readonly hash: string
  readonly documentHash: string
  readonly documentText: string
} => {
  const { hash: documentHash, text: documentText } = hashDocument(parts.document)
  const carried = canonical({
    document: documentHash,
    assets: [...parts.assets]
      .map((asset) => [asset.path, hashBytes(asset.bytes)])
      .sort(byFirst),
    ledger: [...parts.ledger]
      .map((row) => [row.number, hashDocument(row.document).hash])
      .sort(byFirst)
  })
  return {
    hash: crypto.createHash("sha256").update(carried).digest("hex"),
    documentHash,
    documentText
  }
}

const byFirst = (a: ReadonlyArray<string>, b: ReadonlyArray<string>): number =>
  a[0]! < b[0]! ? -1 : a[0]! > b[0]! ? 1 : 0

const canonical = (value: unknown): string => {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null"
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`
}
