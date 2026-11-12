/**
 * The run's job-scope collectors (`@sip/corpus` `JobCollectors`, upstream
 * ADR-0060 appendix §3): those the deployment wires over the run's lanes open
 * once the setup operations are in, before the lanes' replays, and collect and
 * close once every lane's last attempt ended, before the report, whatever
 * ended the replays. Every step's context states the run's cells on each lane
 * planned. They read every attempt of every cell that wrote a bundle, each
 * attempt's bundle getting what they attach; what they answer of a cell's
 * kept attempt is folded into its result
 * (`cell.ts` `withCollected`) before the report lands it, so the JUnit file
 * and the exit code read it; of the run, into its campaign job's lines and
 * attachments, a line of the job that decides and did not pass (a failed
 * `db-dump`) an error of the run. Their directories, and the files they
 * attach to the job, are under the job's directory in the session tree; a
 * run that is no job keeps theirs under its own `collectors/`. The claims they
 * read of a cell and the pre-checks a run asks for are the pack's.
 */
import { Verification } from "@sip/contracts"
import { type CollectedCells, JobCollectors, PreChecks } from "@sip/corpus"
import { CollectorRun, type Context as RigContext } from "@sip/rig"
import * as Effect from "effect/Effect"
import { type Cell, type CellResult, withCollected } from "./cell.js"
import type { CampaignPack } from "./pack.js"
import { plannedOf as plannedCellOf } from "./planned.js"
import type { Record } from "./record.js"
import * as Results from "@sip/store/campaign-results"

/** What the job's collectors read off the run. */
export interface Job {
  readonly record: Record
  /** The run's directory: its job collectors' where the run is no job. */
  readonly root: string
  /** The catalog lane id of a lane (`lanes.ts` `resolve`). */
  readonly idOf: (lane: string) => string
}

/** What of the pack the job's collectors read: the claims of a cell. */
type Claims<C extends Cell> = Pick<CampaignPack<C>, "claimsOf">

/** The cells a run is to replay, `cells` on each of its lanes, as its job's collectors' contexts state them planned. */
export const plannedOf = <C extends Cell>(
  pack: Claims<C>,
  job: Pick<Job, "record" | "idOf">,
  cells: ReadonlyArray<C>
): ReadonlyArray<RigContext.Planned> =>
  job.record.lanes.flatMap((row) => cells.map((cell) => plannedCellOf(cell, job.idOf(row.lane), pack.claimsOf(cell))))

/** What the replays came to once the job's collectors closed. */
export interface Collected<C extends Cell = Cell> {
  /** Each kept result, what the job's collectors answered of its cell folded in. */
  readonly results: ReadonlyArray<CellResult<C>>
  /** The labels of the job's collectors whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
  /** Each line the job's collectors answered of the job itself that decides and did not pass, as the run's error states it. */
  readonly unanswered: ReadonlyArray<string>
}

/** The lines `collected` answered of the job itself that decide it and did not pass, each as `<name> (<source>) <answer>: <message>`. */
export const unansweredOf = (collected: ReadonlyArray<{ readonly scope: { readonly verifications: ReadonlyArray<Verification.Verification> } }>) =>
  collected.flatMap((one) => one.scope.verifications).filter((line) => Verification.decides(line) && line.answer !== "pass").map((line) =>
    `${line.name} (${line.source}) ${line.answer}${line.message === undefined ? "" : `: ${line.message}`}`
  )

/** The lines `result` holds by the job's end: its lane's, its audit's, its batch collectors'. */
const linesOf = (result: CellResult): ReadonlyArray<Verification.Verification> => [
  ...(result.answered ?? []),
  ...(result.audit === undefined ? [] : [result.audit]),
  ...(result.collected ?? [])
]

const failedStatus = (status: CellResult["status"]): boolean => status === "failure" || status === "error"

/** One replay of a result's cell as the job's collectors read it, and whether it is the replay its lane keeps. */
interface Landed<C extends Cell> {
  readonly result: CellResult<C>
  readonly landed: CollectedCells.LandedCell
  readonly kept: boolean
}

/**
 * Every replay of `result`'s cell that wrote a bundle, each landed under its
 * lane's run row `runId`: the kept one under the cell's id, with the result's
 * lines and status; every other one under `<id>~a<n>`, `n` its attempt, failed
 * where its own status is a failure or an error.
 */
const replaysOf = <C extends Cell>(
  pack: Claims<C>,
  job: Pick<Job, "idOf">,
  result: CellResult<C>,
  runId: number
): ReadonlyArray<Landed<C>> => {
  const of = (dir: string, attempt: number, kept: boolean, failed: boolean, verifications: ReadonlyArray<Verification.Verification>): Landed<C> => ({
    result,
    kept,
    landed: {
      id: kept ? result.cell.id : `${result.cell.id}~a${attempt}`,
      caseId: Results.caseIdOf(result.cell.ref, result.cell.subcase),
      lane: job.idOf(result.lane),
      dir,
      runId,
      attempt,
      verifications,
      failed,
      claims: pack.claimsOf(result.cell)
    }
  })
  const keptAt = result.attempts?.findIndex((attempt) => attempt.kept) ?? -1
  const kept = result.bundle === undefined ? [] : [of(result.bundle, keptAt < 0 ? 1 : keptAt + 1, true, failedStatus(result.status), linesOf(result))]
  const others = (result.attempts ?? []).flatMap((attempt, at) =>
    attempt.kept || attempt.bundle === undefined ? [] : [of(attempt.bundle, at + 1, false, failedStatus(attempt.status), [])]
  )
  return [...kept, ...others]
}

/** Every replay of `results`' cells that wrote a bundle, as the job's collectors read it, landed under its lane's run row. */
const landedReplaysOf = <C extends Cell>(
  pack: Claims<C>,
  job: Pick<Job, "record" | "idOf">,
  results: ReadonlyArray<CellResult<C>>
): ReadonlyArray<Landed<C>> =>
  results.flatMap((result) => {
    const row = job.record.lanes.find((one) => one.lane === result.lane)
    return row === undefined ? [] : replaysOf(pack, job, result, row.runId)
  })

/**
 * Every replay of `results`' cells that wrote a bundle, as the job's
 * collectors read it, each landed under its lane's run row: the kept one, and
 * every other attempt, so each attempt's bundle gets what they attach.
 */
export const landedOf = <C extends Cell>(pack: Claims<C>, job: Pick<Job, "record" | "idOf">, results: ReadonlyArray<CellResult<C>>) =>
  landedReplaysOf(pack, job, results).map((one) => one.landed)

/**
 * `results` with what `collected` answered of each kept replay folded in;
 * `cells` are the landed replays of `results` as the collectors read them, in
 * the order {@link landedOf} gives them. What they answered of another attempt
 * stays on its bundle.
 */
export const foldedOf = <C extends Cell>(
  pack: Claims<C>,
  job: Pick<Job, "record" | "idOf">,
  results: ReadonlyArray<CellResult<C>>,
  cells: ReadonlyArray<CollectedCells.LandedCell>,
  collected: ReadonlyArray<{ readonly cells: ReadonlyMap<string, { readonly verifications: ReadonlyArray<Verification.Verification> }> }>
): ReadonlyArray<CellResult<C>> => {
  const answered = new Map(landedReplaysOf(pack, job, results).flatMap((one, at) =>
    one.kept ? [[one.result, collected.flatMap((each) => each.cells.get(cells[at]!.id)?.verifications ?? [])] as const] : []
  ))
  return results.map((result) => withCollected(result, answered.get(result) ?? []))
}

/** What a run's record says of the job collectors `labels` whose close left the lab changed. */
export const leftChangedOf = (labels: ReadonlyArray<string>): string =>
  `collectors: lab left changed by ${labels.join(", ")}: ${CollectorRun.CLOSE_ATTEMPTS} close attempts failed`

/**
 * The run's job pre-checks (`@sip/corpus` `PreChecks`), those the pack asks
 * of every run by name among them: why the run is refused before its
 * provisioning, or `undefined`.
 */
export const preChecks = (pack: Pick<CampaignPack, "preChecks">, job: Job) =>
  PreChecks.job({
    job: job.record.jobId,
    dir: job.root,
    lanes: job.record.lanes.map((row) => job.idOf(row.lane)),
    asked: pack.preChecks
  })

/** `replays` of `cells`, every lane's kept results, with the job's collectors around them, `cells` on each lane stated planned. */
export const around = <C extends Cell, E, R>(
  pack: Claims<C>,
  job: Job,
  cells: ReadonlyArray<C>,
  replays: Effect.Effect<ReadonlyArray<CellResult<C>>, E, R>
) =>
  Effect.gen(function*() {
    let kept: ReadonlyArray<CellResult<C>> = []
    let folded: ReadonlyArray<CellResult<C>> | undefined
    let leftChanged: ReadonlyArray<string> = []
    let unanswered: ReadonlyArray<string> = []
    const results = yield* JobCollectors.around<ReadonlyArray<CellResult<C>>, E, R>({
      job: job.record.jobId,
      dir: job.root,
      lanes: job.record.lanes.map((row) => job.idOf(row.lane)),
      planned: plannedOf(pack, job, cells),
      cells: (results) => landedOf(pack, job, results),
      answered: (cells, collected) =>
        Effect.sync(() => {
          folded = foldedOf(pack, job, kept, cells, collected)
          unanswered = unansweredOf(collected)
        }),
      leftChanged: (labels) => Effect.sync(() => void (leftChanged = labels))
    }, Effect.tap(replays, (results) => Effect.sync(() => void (kept = results))))
    return { results: folded ?? results, leftChanged, unanswered } satisfies Collected<C>
  })
