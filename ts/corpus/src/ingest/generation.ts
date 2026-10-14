/**
 * The generations of a capture name: a capture anonymised anew — under
 * another seed, or into other bytes — is a new capture name, never the old
 * one, whose rows, artifacts and runs stay as they are. The first generation
 * is the name the pcap gives; the next ones count on: `<stem>.anon` →
 * `<stem>.anon2`, `.anon3`…, and a name without `.anon` takes `.anon2`,
 * `.anon3`… behind it.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"

/** One generation of a capture: its name and its number. */
export interface Generation {
  readonly capture: string
  readonly generation: number
}

/** The name generation `n` of the first generation `first` takes. */
export const nameOf = (first: string, n: number): string => {
  if (n <= 1) return first
  return first.endsWith(".anon") ? `${first}${n}` : `${first}.anon${n}`
}

/** Whether the artifact store holds anything under a capture name. */
const held = (fs: FileSystem.FileSystem, artifacts: string, capture: string) =>
  fs.exists(path.join(artifacts, capture)).pipe(Effect.orElseSucceed(() => false))

/**
 * The generation the capture the pcap names `first` stands at in the store:
 * the highest one the store holds, counting on from the second without a gap,
 * or the first.
 */
export const current = Effect.fn("Generation.current")(function*(artifacts: string, first: string) {
  const fs = yield* FileSystem.FileSystem
  let generation = 1
  while (yield* held(fs, artifacts, nameOf(first, generation + 1))) generation++
  return { capture: nameOf(first, generation), generation } satisfies Generation
})

/** The first free generation after `at`. */
export const after = Effect.fn("Generation.after")(function*(artifacts: string, first: string, at: number) {
  const fs = yield* FileSystem.FileSystem
  let generation = at + 1
  while (yield* held(fs, artifacts, nameOf(first, generation))) generation++
  return { capture: nameOf(first, generation), generation } satisfies Generation
})
