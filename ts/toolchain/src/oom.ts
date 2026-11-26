/**
 * OOM EVIDENCE for a child killed by a signal: what this process's cgroup v2
 * says of its OOM killer over the child's run. The child inherits the cgroup,
 * so the `oom_kill` count of its `memory.events` (descendants included) is
 * read before the spawn and again after the death; a count that grew is the
 * evidence, an unchanged one says the OOM killer took nobody in that cgroup.
 * A SIGKILL alone is never read as an OOM kill: whoever reads the words is
 * told which of the two the cgroup recorded, or why it could not be asked.
 * A sibling child killed in the same cgroup over the same window counts too,
 * which the words say by naming the cgroup rather than the child.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** Where cgroup v2 is mounted. */
const CGROUP_ROOT = "/sys/fs/cgroup"

/** The `oom_kill` count of one cgroup at one instant, or why it does not read. */
export type Count =
  | { readonly _tag: "Read"; readonly cgroup: string; readonly kills: number }
  | { readonly _tag: "Unknown"; readonly why: string }

/** What the cgroup recorded over a run. */
export type Evidence =
  | { readonly _tag: "Recorded"; readonly cgroup: string; readonly kills: number; readonly memoryMax: string }
  | { readonly _tag: "None"; readonly cgroup: string }
  | { readonly _tag: "Unknown"; readonly why: string }

/** The cgroup v2 path of `/proc/self/cgroup`'s unified line (`0::<path>`); undefined where there is none. */
export const unifiedPath = (procCgroup: string): string | undefined =>
  procCgroup.split("\n").find((line) => line.startsWith("0::"))?.slice(3).trim()

/** The `oom_kill` count of a `memory.events` text; undefined where it names none. */
export const oomKillsOf = (events: string): number | undefined => {
  const value = /^oom_kill (\d+)$/m.exec(events)?.[1]
  return value === undefined ? undefined : Number(value)
}

const readText = (fs: FileSystem.FileSystem, file: string) =>
  fs.readFileString(file).pipe(Effect.option, Effect.map((text) => (text._tag === "Some" ? text.value : undefined)))

/** This process's cgroup and its `oom_kill` count now. */
export const count: Effect.Effect<Count, never, FileSystem.FileSystem> = Effect.gen(function*() {
  const fs = yield* FileSystem.FileSystem
  const proc = yield* readText(fs, "/proc/self/cgroup")
  if (proc === undefined) return { _tag: "Unknown", why: "/proc/self/cgroup does not read" } satisfies Count
  const cgroup = unifiedPath(proc)
  if (cgroup === undefined) return { _tag: "Unknown", why: "no cgroup v2 hierarchy holds this process" } satisfies Count
  const file = path.join(CGROUP_ROOT, cgroup, "memory.events")
  const events = yield* readText(fs, file)
  const kills = events === undefined ? undefined : oomKillsOf(events)
  if (kills === undefined) return { _tag: "Unknown", why: `${file} states no oom_kill count` } satisfies Count
  return { _tag: "Read", cgroup, kills } satisfies Count
})

/** What the cgroup recorded between two counts of it; `memoryMax` is its `memory.max` as read after. */
export const evidenceOf = (before: Count, after: Count, memoryMax: string): Evidence => {
  if (before._tag === "Unknown") return before
  if (after._tag === "Unknown") return after
  if (after.cgroup !== before.cgroup) {
    return { _tag: "Unknown", why: `the process moved from cgroup ${before.cgroup} to ${after.cgroup}` }
  }
  if (after.kills <= before.kills) return { _tag: "None", cgroup: after.cgroup }
  return { _tag: "Recorded", cgroup: after.cgroup, kills: after.kills - before.kills, memoryMax }
}

/** What the cgroup recorded between `before` and now. */
export const since = (before: Count): Effect.Effect<Evidence, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const after = yield* count
    const fs = yield* FileSystem.FileSystem
    const max = after._tag === "Read"
      ? yield* readText(fs, path.join(CGROUP_ROOT, after.cgroup, "memory.max"))
      : undefined
    return evidenceOf(before, after, max?.trim() ?? "unreadable")
  })

/** The evidence in words, for a failure line. */
export const words = (evidence: Evidence): string => {
  switch (evidence._tag) {
    case "Recorded":
      return `cgroup ${evidence.cgroup} recorded ${evidence.kills} OOM kill(s) while it ran (memory.max ${evidence.memoryMax})`
    case "None":
      return `cgroup ${evidence.cgroup} recorded no OOM kill while it ran`
    case "Unknown":
      return `OOM kills not readable: ${evidence.why}`
  }
}

export * as Oom from "./oom.js"
