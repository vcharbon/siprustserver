/**
 * ONE BATCH on a lane, whoever drives its cells (a capture replay, a
 * campaign): its row opened in the store (`@sip/store` `Batches`), what the
 * lane stands for it stood (`LaneHosting.standBatch`, then
 * `LaneHosting.standSut`), its collectors opened, its cells driven by the
 * caller, its end gate read, its collectors closed, then the SUT and the
 * batch process given back and the row closed with what the end said of the
 * batch. On a lane that stands nothing a batch is the cells its driver runs
 * in one pass, and its collectors still bracket it.
 *
 * The end gate is the batch process's, read once every cell ended: a process
 * that died is a cause of its own and takes no gate, a batch whose stop
 * interrupted cells takes none either, and a managed SUT that died is
 * reported as itself, never as a leak. A batch that fails or is interrupted
 * still closes its row, the failure as its detail.
 */
import { Batches } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import type * as Scope from "effect/Scope"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as LaneHosting from "../deployment/lane-hosting.js"
import type { BatchHost, ContextsReport, SutHandle } from "./batch-protocol.js"

/** Who stands the batch process of a lane; `undefined` where its cells run on their own. */
export type StandBatch = (
  lane: string,
  request: {
    readonly under: string
    readonly rowsDir?: string
    readonly memoryMax: string
    readonly cells: number
    readonly provisioned?: unknown
  }
) => Effect.Effect<BatchHost | undefined, LaneHosting.LaneUnready | LaneHosting.LaneUnstated>

/** Who stands the SUT of a lane; `undefined` where the cell builds its own. */
export type StandSut = (
  lane: string,
  under: string,
  variables?: Readonly<Record<string, string>>
) => Effect.Effect<SutHandle | undefined, LaneHosting.LaneUnready | LaneHosting.LaneUnstated>

/** What the lane stands for a batch. */
export interface Stand {
  /** Where the batch process keeps its scratch, and a managed SUT its log. */
  readonly under: string
  /** The one directory holding a side row for every case the batch's cells run, where they carry any. */
  readonly rowsDir?: string
  /** The memory box of one cell. */
  readonly memoryMax: string
  /** How many cells the batch process takes at once. */
  readonly cells: number
  /** The deployment's block for cells routed through its provisioned routing, as `LaneHosting` takes it. */
  readonly provisioned?: unknown
  /** Variables the SUT is stood with beside the deployment's own. */
  readonly sutVariables?: Readonly<Record<string, string>>
}

/** What the lane stood for the batch, as its cells are driven. */
export interface Stood {
  readonly host: BatchHost | undefined
  readonly sut: SutHandle | undefined
}

/** What driving the cells came to: the caller's own answer, and how many cells a stop interrupted mid-run. */
export interface Driven<A> {
  readonly value: A
  readonly interrupted: number
}

/** What one batch is asked to do: its cells driven under `E`/`R`, its collectors opened under `EC`/`RO`, closed under `RC`. */
export interface Request<A, E, R, EC = never, RO = never, RC = never> {
  readonly lane: string
  /** The batch's row: its owner, its attempt, its cells by name. */
  readonly row: Batches.OpenRequest
  /** What the lane stands; `undefined` stands nothing (a batch left with no cell to run). */
  readonly stand: Stand | undefined
  /** Who stands the batch process, where not the deployment's `LaneHosting`. */
  readonly host?: StandBatch
  /** Who stands the SUT, likewise. */
  readonly sut?: StandSut
  /** The batch's collectors: opened once the lane stood, their close run once the end gate read. */
  readonly collect?: Effect.Effect<Effect.Effect<void, never, RC>, EC, RO>
  /** Drives the batch's cells on what the lane stood. */
  readonly drive: (stood: Stood) => Effect.Effect<Driven<A>, E, R>
}

/** What one batch came to. */
export interface Ended<A> {
  readonly value: A
  /** What the SUT was still holding, or how a managed SUT died; `undefined` where it let go of everything. */
  readonly dirty: string | undefined
  /** How the batch process died, or `undefined` where it served the whole batch. */
  readonly deadHost: string | undefined
  /** Whether the end gate answered for every cell: false where a process stood and its end did not run to its end. */
  readonly judged: boolean
  /** What the gate read of the SUT's call contexts, where it read any. */
  readonly contexts: ContextsReport | undefined
  /** How many cells a stop interrupted mid-run. */
  readonly interrupted: number
  /** Whether a batch process ran the cells. */
  readonly hosted: boolean
}

/** What the batch's end said of the batch rather than of a cell; `null` on a clean end. */
export const detailOf = (ended: Pick<Ended<unknown>, "dirty" | "deadHost" | "interrupted">): string | null => {
  const said = [
    ended.deadHost,
    ended.dirty,
    ended.interrupted > 0 ? `${ended.interrupted} cell(s) interrupted` : undefined
  ].filter((part): part is string => part !== undefined)
  return said.length === 0 ? null : said.join("; ")
}

/** Why a batch ended without an answer, as its row states it. */
const failureOf = (cause: Cause.Cause<unknown>): string =>
  Cause.hasInterruptsOnly(cause) ? "interrupted" : Cause.pretty(cause).split("\n")[0] ?? "failed"

/** One batch, its row open from before the stand until what it stood is given back. */
export const run = <A, E, R, EC = never, RO = never, RC = never>(
  request: Request<A, E, R, EC, RO, RC>
): Effect.Effect<
  Ended<A>,
  E | EC | LaneHosting.LaneUnready | LaneHosting.LaneUnstated | SqlError,
  R | Exclude<RO, Scope.Scope> | RC | LaneHosting.Service | Batches.Service
> =>
  Effect.gen(function*() {
    const batches = yield* Batches.Service
    const id = yield* batches.open(request.row)
    const ended = yield* Effect.scoped(stood(request)).pipe(
      Effect.onExit((exit) => Exit.isSuccess(exit) ? Effect.void : Effect.ignore(batches.close(id, failureOf(exit.cause))))
    )
    yield* batches.close(id, detailOf(ended))
    return ended
  })

const stood = <A, E, R, EC, RO, RC>(request: Request<A, E, R, EC, RO, RC>) =>
  Effect.gen(function*() {
    const hosting = yield* LaneHosting.Service
    const standHost: StandBatch = request.host ?? ((lane, ask) => hosting.standBatch(lane, ask))
    const standSut: StandSut = request.sut ?? ((lane, under, variables) => hosting.standSut(lane, under, variables))
    const stand = request.stand
    const host: BatchHost | undefined = stand === undefined ? undefined : yield* Effect.acquireRelease(
      standHost(request.lane, {
        under: stand.under,
        ...(stand.rowsDir === undefined ? {} : { rowsDir: stand.rowsDir }),
        memoryMax: stand.memoryMax,
        cells: stand.cells,
        ...(stand.provisioned === undefined ? {} : { provisioned: stand.provisioned })
      }),
      (held) => held?.kill ?? Effect.void
    )
    const sut: SutHandle | undefined = stand === undefined ? undefined : yield* Effect.acquireRelease(
      standSut(request.lane, stand.under, stand.sutVariables),
      (held) => held?.stop ?? Effect.void
    )
    const close = request.collect === undefined ? undefined : yield* request.collect
    const driven = yield* request.drive({ host, sut })
    const deadHost = host?.died()
    const reapedBy = host === undefined || driven.interrupted > 0 ? Effect.succeed(undefined) : host.end
    const hostDirty = deadHost === undefined ? yield* reapedBy : undefined
    const dirty = sut?.died() ?? hostDirty
    const judged = host === undefined || (deadHost === undefined && driven.interrupted === 0 && (host.judged?.() ?? true))
    const contexts = host?.contexts()
    if (close !== undefined) yield* close
    return {
      value: driven.value,
      dirty,
      deadHost,
      judged,
      contexts,
      interrupted: driven.interrupted,
      hosted: host !== undefined
    } satisfies Ended<A>
  })
