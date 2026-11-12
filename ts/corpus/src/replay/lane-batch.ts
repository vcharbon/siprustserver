/**
 * ONE BATCH on a lane, whoever drives its cells (a capture replay, a
 * campaign): its row opened in the store (`@sip/store` `Batches`) where its
 * caller files one, what the lane stands for it stood (its batch process
 * first, `LaneHosting.standBatch`, as it serves what the SUT asks of its
 * peers, then `LaneHosting.standSut`), its collectors opened, its cells
 * driven by the caller, its end gate read, its collectors closed, then the
 * SUT and the batch process given back and the row closed with what the end
 * said of the batch. On a lane that stands nothing a batch is the cells its
 * driver runs in one pass, and its collectors still bracket it.
 *
 * The end gate is the batch process's, read once every cell ended: a process
 * that died is a cause of its own and takes no gate, a batch whose stop
 * interrupted cells takes none either, and a managed SUT that died is
 * reported beside what the gate read. A batch whose end did not run to its
 * end line is unjudged, and its row says so. A batch that fails or is
 * interrupted still closes its row, the failure as its detail. A batch a
 * process stood for holds one verification line per gate its end read, and
 * one for its end itself where that says what no gate does ({@link linesOf}).
 *
 * The rig's batch-scope collectors (`@sip/rig` `CollectorRun`, ADR-0060
 * appendix §3) open once the lane stood, collect over the batch's cells once
 * the end gate read and the caller's own collectors closed, and close before
 * the SUT and the batch process are given back, whatever ended the batch.
 * What they answered of each cell is the caller's to land ({@link Ended}
 * `collected`); what they answered of the batch joins its lines and its
 * attachments. Its pre-checks (`at = "before"`) run once its row opened,
 * before the lane stands anything: a `fail` closes the row with their lines
 * and refuses the batch before any call (`LaneUnready`, the message theirs).
 */
import type { Attachment, Verification } from "@sip/contracts"
import { CollectorRun, PreCheck } from "@sip/rig"
import { Attachments, Batches } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import type * as Scope from "effect/Scope"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as LaneHosting from "../deployment/lane-hosting.js"
import type { BatchHost, ContextsReport, Gate, SutHandle } from "./batch-protocol.js"

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

/** The rig's collectors of a batch: their instances, what they run with, and the batch's cells as they read them. */
export interface Collectors<A, RC> {
  /** Its pre-checks among them. */
  readonly instances: ReadonlyArray<CollectorRun.Instance>
  readonly options: CollectorRun.Options
  /** The batch's own directory, which holds the attachments that name no cell. */
  readonly holder: string
  /** The batch's cells once driven, as `cell.v1` describes them, and those it could not describe. */
  readonly cells: (value: A) => Effect.Effect<CollectorRun.Over, never, RC>
}

/** What one batch is asked to do: its cells driven under `E`/`R`, its collectors opened under `EC`/`RO`, closed under `RC`. */
export interface Request<A, E, R, EC = never, RO = never, RC = never> {
  readonly lane: string
  /** The batch's row: its owner, its attempt, its cells by name; none is written where absent. */
  readonly row?: Batches.OpenRequest
  /** What the lane stands; `undefined` stands nothing (a batch left with no cell to run). */
  readonly stand: Stand | undefined
  /** Who stands the batch process, where not the deployment's `LaneHosting`. */
  readonly host?: StandBatch
  /** Who stands the SUT, likewise. */
  readonly sut?: StandSut
  /** The caller's own collectors: opened once the lane stood, their close run once the end gate read. */
  readonly collect?: Effect.Effect<Effect.Effect<void, never, RC>, EC, RO>
  /** The rig's collectors of the batch; none runs where absent. */
  readonly collectors?: Collectors<A, RC>
  /** Drives the batch's cells on what the lane stood. */
  readonly drive: (stood: Stood) => Effect.Effect<Driven<A>, E, R>
}

/** What one batch came to. */
export interface Ended<A> {
  readonly value: A
  /** What the end gate read the SUT still holding, and how a managed SUT died; `undefined` where neither. */
  readonly dirty: string | undefined
  /** Each gate the end read, apart; none where it states none or did not run. */
  readonly gates: ReadonlyArray<Gate>
  /** How a managed SUT died, or `undefined` where it lived through the batch. */
  readonly sutDied: string | undefined
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
  /** What each of the rig's collectors answered, in their order; its cells' answers are the caller's to land. */
  readonly collected: ReadonlyArray<CollectorRun.Collected>
  /** The labels of the rig's collectors whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
}

/** What the batch's end says of each rig collector whose `close` left the lab changed; `undefined` where none did. */
export const leftChangedOf = (ended: Pick<Ended<unknown>, "leftChanged">): string | undefined =>
  joined(ended.leftChanged.map((label) => `the collector ${label} left the lab changed`))

/** What an unjudged batch's end states where nothing else says why. */
export const NO_END_LINE = "the batch's end gate wrote no end line"

/** The parts said, joined; `undefined` where none is. */
const joined = (parts: ReadonlyArray<string | undefined>): string | undefined => {
  const said = parts.filter((part): part is string => part !== undefined)
  return said.length === 0 ? undefined : said.join("; ")
}

/**
 * What the batch's end said of the batch rather than of a cell; `null` on a
 * clean end. An unjudged batch always says why, {@link NO_END_LINE} where
 * nothing else does.
 */
export const detailOf = (ended: Pick<Ended<unknown>, "dirty" | "deadHost" | "interrupted" | "judged">): string | null =>
  joined([
    ended.deadHost,
    ended.dirty,
    ended.interrupted > 0 ? `${ended.interrupted} cell(s) interrupted` : undefined
  ]) ?? (ended.judged ? null : NO_END_LINE)

/** The line a batch process's end answers where no gate's line says it. */
export const END_GATE = "end-gate"

/** Who answers a batch's lines. */
const SOURCE = "batch process"

/** One gate the end read, as the batch's line. */
const gateLine = (gate: Gate): Verification.Verification => ({
  name: gate.name,
  source: SOURCE,
  answer: gate.answer,
  ...(gate.message === undefined ? {} : { message: gate.message }),
  ...(gate.answer === "fail" && gate.message !== undefined ? { detail: { seen: gate.message, side: "sut" } } : {})
})

/**
 * The batch's verification lines: none where no process stood, else one per
 * gate its end read, then its {@link END_GATE} line where the end says what
 * no gate does — a failure where the managed SUT died; else not checkable
 * where the end did not run to its end line, whatever it said in its place;
 * else a failure where the end read the SUT dirty with no gate failing. An
 * end that ran to its end line, read no gate and said nothing holds no line:
 * nothing was verified.
 */
export const linesOf = (
  ended: Pick<Ended<unknown>, "dirty" | "deadHost" | "interrupted" | "judged" | "hosted" | "gates" | "sutDied">
): ReadonlyArray<Verification.Verification> => {
  if (!ended.hosted) return []
  const gates = ended.gates.map(gateLine)
  const line = { name: END_GATE, source: SOURCE }
  const failed = (failure: string): ReadonlyArray<Verification.Verification> => [
    ...gates,
    { ...line, answer: "fail", message: failure, detail: { seen: failure, side: "sut" } }
  ]
  if (ended.sutDied !== undefined) return failed(ended.sutDied)
  if (!ended.judged) return [...gates, { ...line, answer: "not-checkable", message: detailOf(ended) ?? NO_END_LINE }]
  const unstated = gates.some((gate) => gate.answer === "fail") ? undefined : ended.dirty
  return unstated === undefined ? gates : failed(unstated)
}

/** Why a batch ended without an answer, as its row states it. */
const failureOf = (cause: Cause.Cause<unknown>): string =>
  Cause.hasInterruptsOnly(cause) ? "interrupted" : Cause.pretty(cause).split("\n")[0] ?? "failed"

/** What the rig's collectors answered of the batch itself: their lines and their attachments, in their order. */
export const scopeAnswersOf = (collected: ReadonlyArray<CollectorRun.Collected>): CollectorRun.Answers => ({
  verifications: collected.flatMap((one) => one.scope.verifications),
  attachments: collected.flatMap((one) => one.scope.attachments)
})

/**
 * One batch, its row (where it files one) open from before its pre-checks
 * until what it stood is given back, closed with its lines and the rig's
 * collectors' lines of it, their attachments of it added; closed however the
 * batch ended once it opened, a failure or an interruption included.
 */
export const run = <A, E, R, EC = never, RO = never, RC = never>(
  request: Request<A, E, R, EC, RO, RC>
): Effect.Effect<
  Ended<A>,
  E | EC | LaneHosting.LaneUnready | LaneHosting.LaneUnstated | SqlError,
  R | Exclude<RO, Scope.Scope> | RC | LaneHosting.Service | Batches.Service | Attachments.Service
> =>
  Effect.gen(function*() {
    const unready = (refusal: string) => new LaneHosting.LaneUnready({ lane: request.lane, detail: refusal })
    const row = request.row
    if (row === undefined) {
      const outcome = yield* checkedThenStood(request)
      return outcome._tag === "Refused" ? yield* unready(refusedDetailOf(outcome)) : outcome.ended
    }
    const batches = yield* Batches.Service
    return yield* Effect.uninterruptibleMask((restore) =>
      Effect.gen(function*() {
        const id = yield* batches.open(row)
        const outcome = yield* restore(checkedThenStood(request)).pipe(
          Effect.onExit((exit) => Exit.isSuccess(exit) ? Effect.void : Effect.ignore(batches.close(id, failureOf(exit.cause))))
        )
        if (outcome._tag === "Refused") {
          const detail = refusedDetailOf(outcome)
          yield* batches.close(id, detail, outcome.answers.verifications)
          yield* attach(request.collectors, id, outcome.answers.attachments)
          return yield* unready(detail)
        }
        const ended = outcome.ended
        const answered = scopeAnswersOf(ended.collected)
        yield* batches.close(id, detailOf(ended), [...linesOf(ended), ...answered.verifications])
        yield* attach(request.collectors, id, answered.attachments)
        return ended
      })
    )
  })

/**
 * The batch's pre-checks, then, where they refuse nothing, what the lane
 * stands for its cells: what its pre-checks refused it with, or how it ended,
 * their answers and leftovers before its collectors'.
 */
const checkedThenStood = <A, E, R, EC, RO, RC>(request: Request<A, E, R, EC, RO, RC>) =>
  Effect.gen(function*() {
    const checked = yield* preChecked(request.collectors)
    if (checked.refusal !== undefined) {
      return { _tag: "Refused" as const, refusal: checked.refusal, answers: checked.answers, leftChanged: checked.leftChanged }
    }
    const { ended, opened } = yield* Effect.scoped(stood(request))
    return {
      _tag: "Ended" as const,
      ended: {
        ...ended,
        collected: [...checked.collected, ...ended.collected],
        leftChanged: [...checked.leftChanged, ...(opened?.leftChanged() ?? [])]
      } satisfies Ended<A>
    }
  })

/** What a batch its pre-checks refused states: the refusal, then each pre-check whose `close` left the lab changed. */
const refusedDetailOf = (refused: { readonly refusal: string; readonly leftChanged: ReadonlyArray<string> }): string => {
  const left = leftChangedOf(refused)
  return left === undefined ? refused.refusal : `${refused.refusal}; ${left}`
}

/** A batch's rig collectors, what runs them and holds what they attach, its cells aside. */
type RigOf = Pick<Collectors<unknown, never>, "instances" | "options" | "holder"> | undefined

/** `attachments` added to batch `id`'s, `rig`'s holder holding their files; nothing where none is. */
const attach = (rig: RigOf, id: number, attachments: ReadonlyArray<Attachment.Attachment>) =>
  attachments.length === 0 || rig === undefined
    ? Effect.void
    : Effect.flatMap(Attachments.Service, (store) => store.add({ scope: "batch", batchId: id }, { dir: rig.holder, attachments }))

/**
 * The batch's pre-checks (`@sip/rig` `PreCheck`): its rig collectors `at =
 * "before"`, run before the lane stands anything, with the batch's sink and
 * locker: the batch's judges of exports judge what its cells did, not them.
 */
const preChecked = (rig: RigOf) =>
  rig === undefined
    ? Effect.succeed(PreCheck.NOTHING)
    : PreCheck.run(rig.instances.filter(PreCheck.isPreCheck), [], { sink: rig.options.sink, locker: rig.options.locker })

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
    const rig = request.collectors
    const opened = rig === undefined
      ? undefined
      : yield* CollectorRun.opened(rig.instances.filter((one) => !PreCheck.isPreCheck(one)), rig.options)
    const close = request.collect === undefined ? undefined : yield* request.collect
    const driven = yield* request.drive({ host, sut })
    const deadHost = host?.died()
    const reapedBy = host === undefined || driven.interrupted > 0 ? Effect.succeed(undefined) : host.end
    const hostDirty = deadHost === undefined ? yield* reapedBy : undefined
    const sutDied = sut?.died()
    const dirty = joined([hostDirty, sutDied])
    const judged = host === undefined || (deadHost === undefined && driven.interrupted === 0 && (host.judged?.() ?? true))
    const contexts = host?.contexts()
    const gates = deadHost === undefined ? host?.gates?.() ?? [] : []
    if (close !== undefined) yield* close
    const collected = rig === undefined || opened === undefined
      ? []
      : yield* Effect.flatMap(rig.cells(driven.value), (over) => opened.collect(over.cells, over.unread))
    const ended: Omit<Ended<A>, "leftChanged"> = {
      value: driven.value,
      dirty,
      gates,
      sutDied,
      deadHost,
      judged,
      contexts,
      interrupted: driven.interrupted,
      hosted: host !== undefined,
      collected
    }
    return { ended, opened }
  })
