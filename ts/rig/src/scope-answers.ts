/**
 * A scope's answers made whole once its collectors and judges answered
 * (ADR-0060 appendix §6, `scope-judging.ts`): never a `pass` over input the
 * base did not hand whole, never a promised verification with no line.
 */
import { Verification } from "@sip/contracts"
import type * as Cell from "./cell.js"
import type * as CollectorRun from "./collector-run.js"
import type * as Exports from "./exports.js"

/** The verification the base answers `not checkable` on each holder whose input it did not hand whole. */
export const INPUT = "input"

/** The label of the base's own answers among a scope's collected entries. */
export const BASE_LABEL = Verification.BASE_SOURCES.rig

/** What an entry of a scope's collect promises: the verifications it answers `not checkable` on the cells it covers where nobody answered them. */
export interface Promised {
  readonly source: string
  readonly names: ReadonlyArray<string>
  readonly covers: (cell: Cell.Cell) => boolean
  /** Its filled lines never decide (an informative collector's promise). */
  readonly informative?: boolean
}

/** The names answered on each of `cells`, other than `pending`: its own lines', then `collected`'s. */
const answeredOn = (collected: ReadonlyArray<CollectorRun.Collected>, cells: ReadonlyArray<Cell.Cell>): Map<string, Set<string>> => {
  const answered = new Map(cells.map((cell) => [cell.id, new Set<string>()]))
  const add = (id: string, lines: ReadonlyArray<Pick<Verification.Verification, "name" | "answer">>) => {
    const names = answered.get(id)
    if (names === undefined) return
    for (const line of lines) if (line.answer !== "pending") names.add(line.name)
  }
  for (const cell of cells) add(cell.id, cell.verifications)
  for (const one of collected) for (const [id, held] of one.cells) add(id, held.verifications)
  return answered
}

/**
 * `collected` with, per entry's promise, each verification nobody answered
 * for one of the cells it covers answered `not checkable` by it: none of
 * `collected` answered it, nor did the cell's own lines, nor an earlier
 * entry's filling.
 */
export const unansweredFilled = (
  collected: ReadonlyArray<CollectorRun.Collected>,
  promised: ReadonlyArray<Promised | undefined>,
  cells: ReadonlyArray<Cell.Cell>
): ReadonlyArray<CollectorRun.Collected> => {
  const answered = answeredOn(collected, cells)
  return collected.map((one, at) => {
    const promise = promised[at]
    if (promise === undefined || promise.names.length === 0) return one
    const byCell = new Map(one.cells)
    for (const cell of cells) {
      if (!promise.covers(cell)) continue
      const names = answered.get(cell.id)!
      const missing = promise.names.filter((name) => !names.has(name))
      if (missing.length === 0) continue
      for (const name of missing) names.add(name)
      const held = byCell.get(cell.id) ?? { verifications: [], attachments: [] }
      byCell.set(cell.id, {
        ...held,
        verifications: [
          ...held.verifications,
          ...missing.map((name) => ({
            name,
            source: promise.source,
            answer: "not-checkable" as const,
            message: `nobody answered ${name} for the cell ${cell.id}`,
            ...(promise.informative === true ? { informative: true } : {})
          }))
        ]
      })
    }
    return { ...one, cells: byCell }
  })
}

/** A holder of answers: a cell's id, or `undefined` for the scope. */
type Holder = string | undefined

/** Per holder, why its input was not handed whole: on a cell, the scope's whys and its own; on the scope, every one. */
const whysOf = (notHanded: ReadonlyArray<Exports.Unread>): (holder: Holder) => ReadonlyArray<string> => {
  const ofScope = notHanded.flatMap((one) => (one.cell === undefined ? [one.why] : []))
  const ofCell = new Map<string, Array<string>>()
  for (const one of notHanded) {
    if (one.cell === undefined) continue
    const whys = ofCell.get(one.cell)
    if (whys === undefined) ofCell.set(one.cell, [...ofScope, one.why])
    else whys.push(one.why)
  }
  const every = notHanded.map((one) => one.why)
  return (holder) => (holder === undefined ? every : ofCell.get(holder) ?? ofScope)
}

/** The answers of `one`'s holders, the scope first, each with its holder. */
const holdersOf = (one: CollectorRun.Collected): ReadonlyArray<readonly [Holder, CollectorRun.Answers]> => [
  [undefined, one.scope],
  ...one.cells
]

/** `one` with the answers of each holder `whys` gives a why replaced by `edit`'s. */
const edited = (
  one: CollectorRun.Collected,
  whys: (holder: Holder) => ReadonlyArray<string>,
  edit: (holder: Holder, answers: CollectorRun.Answers) => CollectorRun.Answers
): CollectorRun.Collected => {
  const editedOf = (holder: Holder, answers: CollectorRun.Answers) => (whys(holder).length === 0 ? answers : edit(holder, answers))
  return {
    ...one,
    scope: editedOf(undefined, one.scope),
    cells: new Map([...one.cells].map(([id, answers]) => [id, editedOf(id, answers)]))
  }
}

/** The base's own {@link INPUT} line, `not checkable` saying `message`. */
const inputLine = (message: string): Verification.Verification => ({
  name: INPUT,
  source: Verification.BASE_SOURCES.rig,
  answer: "not-checkable",
  message
})

/**
 * `collected` with, on each holder whose input was not handed whole
 * (`notHanded`: a cell's concerns that cell, the scope's every one of `cells`;
 * the scope holds every one), each `pass` answered `not checkable`, saying
 * why, and a `not checkable` added beside each other answer of a name the
 * holder holds no `not checkable` of: a `fail` is never hidden. The base's own
 * entry ({@link BASE_LABEL}) follows, its {@link INPUT} line on each such
 * holder whether or not an entry answered there: the holder never passes,
 * whatever a wider scope answers of it.
 */
export const notHandedMarked = (
  collected: ReadonlyArray<CollectorRun.Collected>,
  notHanded: ReadonlyArray<Exports.Unread>,
  cells: ReadonlyArray<string>
): ReadonlyArray<CollectorRun.Collected> => {
  if (notHanded.length === 0) return collected
  const whys = whysOf(notHanded)
  const messageOf = (holder: Holder) => `its input was not read whole: ${whys(holder).join("; ")}`
  const base: CollectorRun.Collected = {
    label: BASE_LABEL,
    scope: { verifications: [inputLine(messageOf(undefined))], attachments: [] },
    cells: new Map(
      cells.filter((id) => whys(id).length > 0).map((id) => [id, { verifications: [inputLine(messageOf(id))], attachments: [] }])
    ),
    keyed: [],
    assigned: [],
    keys: []
  }
  const passesMarked = collected.map((one) =>
    edited(one, whys, (holder, answers) => ({
      ...answers,
      verifications: answers.verifications.map((line) =>
        line.answer === "pass" ? { ...line, answer: "not-checkable" as const, message: messageOf(holder) } : line
      )
    }))
  )
  const marked = new Map<Holder, Set<string>>()
  for (const one of passesMarked) {
    for (const [holder, answers] of holdersOf(one)) {
      const names = marked.get(holder) ?? new Set<string>()
      marked.set(holder, names)
      for (const line of answers.verifications) if (line.answer === "not-checkable") names.add(line.name)
    }
  }
  return [
    ...passesMarked.map((one) =>
      edited(one, whys, (holder, answers) => {
        const names = marked.get(holder)!
        const beside: Array<Verification.Verification> = []
        for (const line of answers.verifications) {
          if (names.has(line.name)) continue
          names.add(line.name)
          beside.push({ name: line.name, source: line.source, answer: "not-checkable", message: messageOf(holder) })
        }
        return beside.length === 0 ? answers : { ...answers, verifications: [...answers.verifications, ...beside] }
      })
    ),
    base
  ]
}
