/**
 * A replay's FIRST CUT: the named captures no cut has taken — one the line
 * just ingested from a pcap root (`ingest/adopt.ts`), or one an earlier line
 * ingested and left uncut — cut before the replay pins its prep. The pin is
 * the highest prep, so it then reads them; a stale cut is not this rule's,
 * the replay alone reads what stands.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { neverCut } from "../cut/cut-staleness.js"
import { phase } from "../cut/cut.js"
import type { Options } from "../options.js"
import type { Cutter } from "./evidence-prep.js"

/**
 * Why `uncut` cannot be cut before this replay, or `null`: a `--prep` the
 * line names stands below the prep the cut would open, so the replay could
 * never read it. The line says the one that cuts it.
 */
export const refusal = (uncut: ReadonlyArray<string>, prep: number | undefined, replay: string): string | null =>
  uncut.length === 0 || prep === undefined
    ? null
    : `${uncut.join(", ")} has no cut, and --prep ${prep} stands below the cut it needs: ` +
      `\`${replay} ${uncut.join(" ")}\` with no --prep cuts it first`

/** The captures of `captures` no cut has taken, cut by `cut` under `options`; answers them, none where there were none. */
export const cutFirst = (captures: ReadonlyArray<string>, options: Options, cut: Cutter = phase) =>
  Effect.gen(function*() {
    const uncut = yield* neverCut(captures)
    if (uncut.length === 0) return uncut
    yield* Console.log(`[replay] cut: ${uncut.join(", ")} — no cut yet: cut first, before the replay`)
    yield* cut(uncut, options)
    return uncut
  })
