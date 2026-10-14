/**
 * `cleanup` — the retention pass, by hand:
 *
 *   cleanup [--dry-run] [--measure] [--sessions <dir>]
 *           [--horizon HOURS] [--grace HOURS] [--keep HOURS] [--forget-unrecorded]
 *   cleanup --rebuild-cache
 *
 * The pass itself runs at the end of every sweep (`Cleanup.afterSweep`); this
 * is how a session runs it out of band, and the only way to see what it WOULD
 * do. `--measure` recurses what it drops to report the bytes, which no ruling
 * needs and the automatic pass never asks for. `--rebuild-cache` removes the
 * content cache and nothing else: every entry rebuilds from a row on the next
 * read, and no pass runs.
 */
import { GRACE_HOURS } from "@sip/corpus/content/content-gc"
import { pass, rebuildCache } from "@sip/corpus/housekeeping/cleanup"
import { HORIZON_HOURS } from "@sip/corpus/housekeeping/reap"
import * as Retention from "@sip/corpus/housekeeping/retention"
import * as Effect from "effect/Effect"
import { Command, Flag } from "effect/unstable/cli"
import { pathFlags } from "../flags.js"
import { overridesOf, pathsOf } from "../options.js"

export const flags = {
  ...pathFlags,
  dryRun: Flag.boolean("dry-run").pipe(
    Flag.withDescription("Say what the pass would drop and drop nothing"),
    Flag.withDefault(false)
  ),
  measure: Flag.boolean("measure").pipe(
    Flag.withDescription("Recurse what is dropped to report the bytes"),
    Flag.withDefault(false)
  ),
  horizon: Flag.float("horizon").pipe(
    Flag.withDescription("Hours an unfinished row and its bundle must have gone quiet before it is closed"),
    Flag.withDefault(HORIZON_HOURS)
  ),
  grace: Flag.float("grace").pipe(
    Flag.withDescription("Hours an unreferenced content object is kept"),
    Flag.withDefault(GRACE_HOURS)
  ),
  keep: Flag.float("keep").pipe(
    Flag.withDescription("Hours a paused-clock lane's bundle no other rule keeps survives its run"),
    Flag.withDefault(Retention.GRACE_HOURS)
  ),
  forgetUnrecorded: Flag.boolean("forget-unrecorded").pipe(
    Flag.withDescription("Drop the bundles no row names too: the store was rebuilt from nothing"),
    Flag.withDefault(false)
  ),
  rebuildCache: Flag.boolean("rebuild-cache").pipe(
    Flag.withDescription("Remove the whole _content cache and nothing else; it rebuilds on read"),
    Flag.withDefault(false)
  )
}

export const command = () =>
  Command.make("cleanup", flags, (input) =>
    Effect.gen(function*() {
      const paths = yield* pathsOf(overridesOf(input))
      if (input.rebuildCache) return yield* rebuildCache(paths.sessions)
      yield* pass({
        paths,
        dryRun: input.dryRun,
        measure: input.measure,
        horizonHours: input.horizon,
        graceHours: input.grace,
        keepHours: input.keep,
        forgetUnrecorded: input.forgetUnrecorded
      })
    })).pipe(Command.withDescription("The retention pass, by hand; --rebuild-cache drops the content cache alone"))
