/**
 * The step ids of a v3 case document in flow order, read loosely off the
 * document as a cell wrote it (`pivot.json`): every flow node carrying an `id`
 * and an `op`, depth first. A document this reading does not recognise yields
 * none, so an order taken from it places nothing.
 */
const asRecord = (value: unknown): Record<string, unknown> | null =>
  typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null

export const stepOrderOf = (document: unknown): ReadonlyArray<string> => {
  const out: Array<string> = []
  const walk = (value: unknown): void => {
    if (Array.isArray(value)) {
      for (const item of value) walk(item)
      return
    }
    const node = asRecord(value)
    if (node === null) return
    if (typeof node["id"] === "string" && typeof node["op"] === "string") out.push(node["id"])
    for (const child of Object.values(node)) walk(child)
  }
  walk(asRecord(document)?.["flow"])
  return out
}
