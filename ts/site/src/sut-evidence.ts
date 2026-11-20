/**
 * What a hosted-lane run keeps of the SUT's own records, as the study reads it:
 * the call contexts the SUT still held after each batch (`contexts.ndjson`),
 * and for one cell the Call-IDs it minted, the window it drove the SUT in
 * (`sut-join.json`) and the SUT's log over that window, the cell's log
 * attachments the pack names its SUT's (`SitePack.sutLogs`), each's two ends
 * where it is long.
 */
import * as HeldContexts from "@sip/corpus/replay/held-contexts"
import * as LogWindow from "@sip/corpus/log-window"
import * as SutJoin from "@sip/corpus/replay/sut-join"
import { Attachments } from "@sip/store"
import { Effect } from "effect"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import * as LogText from "./log-text.ts"

/** What joins a cell to the SUT's records, its window as ISO instants. */
export interface CellJoin {
  readonly callIds: readonly string[]
  readonly from: string
  readonly to: string
}

/** One SUT log attachment of a cell (`sut-log`, `lane-log`): the SUT's log over the cell's padded window. */
export interface SutLog {
  /** The attachment's name: the collector that read it. */
  readonly name: string
  /** The attachment's title: the log's container. */
  readonly title: string
  /** Its head and tail where it is long (`log-text.ts`), else the whole log. */
  readonly text: string
  /** The whole log's size. */
  readonly bytes: number
}

export interface CellSut {
  /** `null` where the cell states no join: every simulated-lane cell. */
  readonly join: CellJoin | null
  /** Its SUT log attachments, in their order; [] where it has none. */
  readonly sutLogs: readonly SutLog[]
  /** The contexts the SUT still held after a batch whose call this cell placed. */
  readonly held: readonly HeldContexts.HeldLine[]
}

const readText = (file: string) =>
  Effect.flatMap(FileSystem, (fs) => fs.readFileString(file)).pipe(Effect.orElseSucceed(() => undefined))

/** The batch readings a run keeps, oldest first; [] where it keeps none. */
export const ofRun = Effect.fn("SutEvidence.ofRun")(function*(runDir: string) {
  const path = yield* Path
  const text = yield* readText(path.join(runDir, HeldContexts.FILE))
  if (text === undefined) return [] as readonly HeldContexts.Line[]
  return text.split("\n").flatMap((line): readonly HeldContexts.Line[] => {
    if (line.trim() === "") return []
    try {
      return [JSON.parse(line) as HeldContexts.Line]
    } catch {
      return []
    }
  })
})

/**
 * The SUT logs the cell at `cellDir` lists, its log attachments named in
 * `names` in their order, each read bounded; one whose file cannot be read is
 * left out.
 */
export const sutLogsOf = Effect.fn("SutEvidence.sutLogsOf")(function*(cellDir: string, names: ReadonlyArray<string>) {
  const path = yield* Path
  const held = yield* Attachments.listed(cellDir)
  const logs: SutLog[] = []
  for (const attachment of held.attachments) {
    if (!names.includes(attachment.name) || attachment.kind !== "log") continue
    const read = yield* LogText.bounded(path.join(cellDir, attachment.file), `the cell's ${attachment.name} attachment`)
    if (read !== undefined) logs.push({ name: attachment.name, title: attachment.title ?? attachment.name, ...read })
  }
  return logs as readonly SutLog[]
})

/** The SUT's records of the cell at `cellDir`, under the run that holds it, its SUT logs the attachments named in `names`. */
export const ofCell = Effect.fn("SutEvidence.ofCell")(function*(cellDir: string, names: ReadonlyArray<string>) {
  const path = yield* Path
  const runDir = path.dirname(cellDir)
  const name = path.basename(cellDir)
  const held = (yield* ofRun(runDir)).flatMap((line) => line.held.filter((one) => one.cells.includes(name)))
  const sutLogs = yield* sutLogsOf(cellDir, names)
  const join = yield* SutJoin.read(cellDir)
  if (join === undefined) return { join: null, sutLogs, held } satisfies CellSut
  return {
    join: {
      callIds: join.callIds,
      from: new Date(join.window.fromMs).toISOString(),
      to: new Date(join.window.toMs).toISOString()
    },
    sutLogs,
    held
  } satisfies CellSut
})

/** A cell's own window, padded as its SUT log is cut; undefined where the cell states none. */
export const cellWindow = (
  detail: { readonly join: CellJoin | null } | undefined
): { readonly startedAt: string; readonly finishedAt: string } | undefined => {
  const join = detail?.join
  if (join === null || join === undefined) return undefined
  return {
    startedAt: new Date(Date.parse(join.from) - LogWindow.PAD_MS).toISOString(),
    finishedAt: new Date(Date.parse(join.to) + LogWindow.PAD_MS).toISOString()
  }
}

export * as SutEvidence from "./sut-evidence.ts"
