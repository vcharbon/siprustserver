/**
 * WHICH prep the evidence command replays as of. A named capture, whole or as
 * a case set, reuses its standing cut while that cut is current under the
 * campaign's staleness rule (`cut/cut-staleness.ts`): a stale or never-cut
 * capture is cut again and the log says why, and `--force-recut` cuts a
 * current one. A named set is cut whole once any capture of it is stale, so
 * one prep holds every capture the run rows pin. A derived universe (`--all`,
 * `--known`, `--standing`) is the sweep, and cuts.
 */
import { Cuts } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { type Cut, phase } from "../cut/cut.js"
import { cutStaleness } from "../cut/cut-staleness.js"
import type { Options } from "../options.js"
import { type Work, workList } from "../universe.js"
import { pin } from "./plan.js"

type CutPhase = ReturnType<typeof phase>

/** How a cut is run: the real phase, or a test's stand-in that answers a prep. */
export type Cutter = (
  captures: ReadonlyArray<string>,
  options: Options
) => Effect.Effect<Pick<Cut, "prep">, Effect.Error<CutPhase>, Effect.Services<CutPhase>>

/** The prep to replay as of, or why the invocation is refused, in one line. */
export type Answer =
  | { readonly _tag: "prep"; readonly prep: number }
  | { readonly _tag: "refused"; readonly why: string }

const answer = (prep: number): Answer => ({ _tag: "prep", prep })
const refused = (why: string): Answer => ({ _tag: "refused", why })

export const prepOf = (
  captures: ReadonlyArray<string>,
  options: Options,
  cut: Cutter = phase
) =>
  Effect.gen(function*() {
    if (options.selection.universe !== "named") return answer((yield* cut(captures, options)).prep)
    const stale = yield* cutStaleness(captures, options.paths.corpus, new Set(), options.forceRecut)
    if (stale.length > 0) {
      for (const entry of stale) yield* Console.log(`[evidence] cut: ${entry.capture} is cut again — ${entry.why}`)
      // The whole set, current captures included: the run rows pin the prep
      // this cut opens, and a prep a capture's row pins must have cut it.
      return answer((yield* cut(captures, options)).prep)
    }
    // The capture's own standing prep, so the run row pins the cut it read;
    // a ceiling named on the line is honoured as everywhere else.
    const latest = yield* Effect.map(Cuts.Service, (cuts) => cuts.latest).pipe(Effect.flatten)
    const standing = options.prep === undefined
      ? Math.max(...captures.map((capture) => latest.get(capture)?.prepId ?? 0))
      : yield* pin(options.prep)
    if (standing === null) return refused(`there is no prep ${options.prep} to replay as of`)
    yield* Console.log(`[evidence] cut: ${captures.join(", ")} stands cut on this code — replaying off prep${standing}`)
    return answer(standing)
  })

/**
 * The work list as of the prep the replay pins. A span bound rules off the
 * standing cut, and a cut `prepOf` ran stands above the one the list was read
 * from — new ids, new spans — so a bounded list is asked again there. A
 * selection with no bound reads the same list off any prep and is kept.
 */
export const workAsOf = (
  work: ReadonlyArray<Work>,
  options: Options,
  prep: number
) =>
  options.selection.maxSpanSeconds === undefined
    ? Effect.succeed(work)
    : workList(options.paths, options.lane, options.selection, prep)
