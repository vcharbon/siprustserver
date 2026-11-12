/**
 * What the store KEEPS of what regeneration cannot remake, and the wipe of
 * the rest:
 *
 * - `scan`: every ticket's citations, read off the tracker or a citations
 *   file, dated against the store's first cut.
 * - `wipe`: the store's wipe (`@sip/store` `Wipe`) over the pack's
 *   real-clock lanes, then the files only dropped rows named: the dropped
 *   runs' bundles, every cut's log (`_preps/`) and the content cache
 *   (`_content/`). Without `confirm` it plans in a read-only transaction and
 *   counts the files, touching nothing; with it, the files are listed first
 *   (a directory it cannot read stops it, nothing written), then the pack's
 *   freeze runs, and no id below the pack's floor is minted after it.
 */
import { Tracker } from "@sip/corpus"
import { CONTENT } from "@sip/corpus/content/content"
import { citationsOf } from "@sip/corpus/ids/citations"
import * as CitationsFile from "@sip/corpus/ids/citations-file"
import { PREPS } from "@sip/corpus/paths"
import { type Citations, StoreBinding, Wipe } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/**
 * Every citation the deployment's tracker (`Tracker.texts`) holds, dated
 * against the store's first cut; or, where `file` is named, the citations it
 * holds (`@sip/corpus` `CitationsFile`), so no tracker is needed.
 */
export const scan = Effect.fn("StoreKeep.scan")(function*(file?: string) {
  const since = yield* Effect.flatMap(Wipe.Service, (wipe) => wipe.since)
  if (file !== undefined) {
    const citations = yield* CitationsFile.read(file)
    console.log(`[keep] ${file} holds ${citations.length} citation(s) (store ids began ${since ?? "never"})`)
    return { since, citations }
  }
  const tickets = yield* Effect.flatMap(Tracker.Service, (tracker) => tracker.texts(since))
  const { grammar } = yield* StoreBinding.Service
  const citations = tickets.flatMap((ticket) => citationsOf(ticket, grammar))
  console.log(`[keep] ${tickets.length} ticket(s) cite ${citations.length} id(s) (store ids began ${since ?? "never"})`)
  return { since, citations }
})

/**
 * The entries directly under `dir`, absolute; none where it does not exist. A
 * directory that exists and cannot be read fails: a wipe that skipped it
 * would drop the rows and leave their files.
 */
export const entriesOf = (dir: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readDirectory(dir)).pipe(
    Effect.map((entries): ReadonlyArray<string> => entries.map((entry) => path.join(dir, entry))),
    Effect.catch((error) =>
      error.reason._tag === "NotFound" ? Effect.succeed<ReadonlyArray<string>>([]) : Effect.fail(error)
    )
  )

/** The files a wipe removes after its rows, as they stand on disk now. */
const filesOf = Effect.fn("StoreKeep.filesOf")(function*(sessions: string, plan: Wipe.Plan) {
  const fs = yield* FileSystem.FileSystem
  const exists = (at: string) => fs.exists(at).pipe(Effect.orElseSucceed(() => false))
  const content = path.join(sessions, CONTENT)
  return {
    bundles: yield* Effect.filter(plan.bundles, exists),
    preps: yield* entriesOf(path.join(sessions, PREPS)),
    content,
    contentHeld: yield* exists(content)
  }
})

/** The plan's lines: what is kept, what goes, and what would refuse it. */
const planLines = (plan: Wipe.Plan, files: Effect.Success<ReturnType<typeof filesOf>>): ReadonlyArray<string> => [
  `[wipe] kept runs: ${plan.kept.lane} on a real-clock lane, ${plan.kept.pinned} pinned, ${plan.kept.open} open; ` +
  `${plan.keptCells} cell(s), ${plan.unmapped} of them named by no case map`,
  `[wipe] dropped runs: ${plan.dropped.runs} (${plan.dropped.caseResults} case result(s), ` +
  `${plan.dropped.runRuleHits} rule-hit row(s)); ${plan.cited} of them cited, ${plan.unsummarised} with no summary yet`,
  `[wipe] dropped cuts: ${plan.dropped.preps} prep(s), ${plan.dropped.captureCuts} capture cut(s), ` +
  `${plan.dropped.cutCases} cut case(s), ${plan.dropped.cutRefusals} refusal(s) (${plan.dropped.cutRefusalCalls} ` +
  `refusal call(s)); content ${plan.dropped.cases} case(s), ${plan.dropped.caseAssets} asset(s), ` +
  `${plan.dropped.ledgerRows} ledger row(s), ${plan.dropped.flowsDocuments} flows document(s) ` +
  `(${plan.dropped.flowsLegs} leg(s))`,
  `[wipe] files: ${files.bundles.length} bundle(s) on disk of ${plan.bundles.length} dropped run(s), ` +
  `${files.preps.length} cut log(s) under ${PREPS}/, the ${CONTENT}/ cache` +
  (files.contentHeld ? "" : " (absent)"),
  `[wipe] export: ${plan.unexported} reading(s) with no case map`,
  ...plan.holders.map((holder) => `[wipe] held: ${holder}`)
]

/** What a pack keeps through a wipe or a reset. */
export interface KeepPack<E, R, EL = E, RL = R> {
  /** Every cited and named id frozen and every cited run summarised, under the session tree `sessions`. */
  readonly freeze: (citations: ReadonlyArray<Citations.Citation>, sessions: string) => Effect.Effect<void, E, R>
  /** The real-clock lanes whose runs a wipe keeps. */
  readonly keepLanes: Effect.Effect<ReadonlyArray<string>, EL, RL>
  /** The lowest run, job, sweep and prep id minted after a wipe: above every id a ticket cites. */
  readonly idFloor: number
}

export interface WipeRequest {
  readonly sessions: string
  /** Without it the wipe plans, read-only, and removes nothing. */
  readonly confirm: boolean
  /** A citations file read instead of the tracker. */
  readonly citations?: string
}

/**
 * Remove what the wipe's dropped rows named, once the rows are gone: the
 * files are listed before anything is written, so a directory the wipe
 * could not read stops it with its rows still in place.
 */
const executed = <E, R>(
  pack: Pick<KeepPack<E, R>, "freeze" | "idFloor">,
  request: WipeRequest,
  keepLanes: ReadonlyArray<string>,
  citations: ReadonlyArray<Citations.Citation>
) =>
  Effect.gen(function*() {
    const store = yield* Wipe.Service
    yield* filesOf(request.sessions, yield* store.plan({ keepLanes, citations }))
    yield* pack.freeze(citations, request.sessions)
    const done = yield* store.execute({ keepLanes, idFloor: pack.idFloor })
    const files = yield* filesOf(request.sessions, done)
    for (const line of planLines(done, files)) console.log(line)
    const fs = yield* FileSystem.FileSystem
    for (const dir of [...files.bundles, ...files.preps, files.content]) {
      yield* fs.remove(dir, { recursive: true, force: true })
    }
    console.log(
      `[wipe] done: ${done.dropped.runs} run(s) and every cut dropped; ${files.bundles.length} bundle(s), ` +
        `${files.preps.length} cut log(s) and the content cache removed`
    )
    return done
  })

/** The wipe: planned, or — confirmed — frozen, executed, and its files removed. */
export const wipe = <E, R, EL, RL>(pack: KeepPack<E, R, EL, RL>, request: WipeRequest) =>
  Effect.gen(function*() {
    const store = yield* Wipe.Service
    const { citations } = yield* scan(request.citations)
    const keepLanes = yield* pack.keepLanes
    if (request.confirm) return yield* executed(pack, request, keepLanes, citations)
    const plan = yield* store.plan({ keepLanes, citations })
    for (const line of planLines(plan, yield* filesOf(request.sessions, plan))) console.log(line)
    console.log("[wipe] dry run: nothing written; --confirm exports, freezes and wipes")
    return plan
  }).pipe(Effect.withSpan("StoreKeep.wipe"))
