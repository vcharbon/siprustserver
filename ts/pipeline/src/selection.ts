/**
 * The PCAP2TEST-SELECTION v2 block: the boundary/vantage decisions this package
 * CONSUMES rather than recomputes.
 *
 * TRANSITIONAL. There is no vantage suggester on this side yet, so an older
 * pipeline's `auto.selection.txt` is a hard input dependency: case assembly
 * downstream is ours, which socket pair each actor binds at is not.
 */
export interface Vantage {
  readonly leg: number
  readonly hop: number
}

export interface SelectionCase {
  readonly id: string
  readonly uac: Vantage
  readonly uas: ReadonlyArray<Vantage>
  readonly defects: ReadonlyArray<readonly [number, number]>
  readonly defectNote?: string
}

export interface SelectionDoc {
  readonly capture: string
  readonly cases: ReadonlyArray<SelectionCase>
}

/** UAC first, then the UAS chain — the order topology inference consumes. */
export const vantages = (c: SelectionCase): ReadonlyArray<Vantage> => [c.uac, ...c.uas]

/** The distinct captured legs a case references, sorted. */
export const referencedLegs = (c: SelectionCase): ReadonlyArray<number> =>
  [...new Set(vantages(c).map((v) => v.leg))].sort((a, b) => a - b)

export const parseSelection = (text: string): SelectionDoc => {
  const lines = text.split(/\r?\n/).map((l) => l.trim())
  if (lines[0] !== "PCAP2TEST-SELECTION v2") throw new Error("not a v2 selection block")
  let capture = ""
  const cases: Array<SelectionCase> = []
  let open: {
    id: string
    uac?: Vantage
    uas: Array<Vantage>
    defects: Array<readonly [number, number]>
    defectNote?: string
  } | null = null

  for (const line of lines.slice(1)) {
    if (line === "" || line.startsWith("#")) continue
    if (line === "END-SELECTION") break
    if (line === "END-CASE") {
      if (!open?.uac) throw new Error(`case ${open?.id} has no uac vantage`)
      cases.push({
        id: open.id,
        uac: open.uac,
        uas: open.uas,
        defects: open.defects,
        defectNote: open.defectNote
      })
      open = null
      continue
    }
    const colon = line.indexOf(":")
    if (colon < 0) continue
    const key = line.slice(0, colon).trim()
    const value = line.slice(colon + 1).trim()
    switch (key) {
      case "capture":
        capture = value
        break
      case "case":
        open = { id: value, uas: [], defects: [] }
        break
      case "uac":
        if (open) open.uac = kv(value, "leg", "hop") as Vantage
        break
      case "uas":
        if (open) open.uas.push(kv(value, "leg", "hop") as Vantage)
        break
      case "defect": {
        const d = kv(value, "leg", "msg")
        if (open) open.defects.push([d.leg, d.hop] as const)
        break
      }
      case "defect-note":
        if (open) open.defectNote = value
        break
      // `pairing:` verdicts are consumed by the vantage suggester, not here.
      default:
        break
    }
  }
  return { capture, cases }
}

/** Read `a=<n> b=<n>` into `{leg, hop}` (the second name lands on `hop`). */
const kv = (value: string, first: string, second: string): { leg: number; hop: number } => {
  const fields = new Map(
    value.split(/\s+/).map((t) => {
      const eq = t.indexOf("=")
      return [t.slice(0, eq), Number(t.slice(eq + 1))] as const
    })
  )
  const a = fields.get(first)
  const b = fields.get(second)
  if (a === undefined || b === undefined) throw new Error(`bad vantage line: ${value}`)
  return { leg: a, hop: b }
}
