/**
 * The generations of a capture name: a capture anonymised anew — under
 * another seed, or into other bytes — is a new capture name, never the old
 * one, whose rows, artifacts and runs stay as they are. The first generation
 * is the name the pcap gives; the next ones count on: `<stem>.anon` →
 * `<stem>.anon2`, `.anon3`…, and a name without `.anon` takes `.anon2`,
 * `.anon3`… behind it. A capture stands at the highest generation the store
 * holds, whichever generations below it were forgotten.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"

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

/** Which generation of `first` the name `name` is ({@link nameOf} read back), if any. */
export const generationOf = (first: string, name: string): number | undefined => {
  if (name === first) return 1
  const stem = first.endsWith(".anon") ? first : `${first}.anon`
  if (!name.startsWith(stem)) return undefined
  const digits = name.slice(stem.length)
  if (!/^[1-9][0-9]*$/.test(digits)) return undefined
  const n = Number(digits)
  return n >= 2 ? n : undefined
}

/** The highest generation of `first` the artifact store holds anything under, or 0 for none. */
const highest = (fs: FileSystem.FileSystem, artifacts: string, first: string) =>
  fs.readDirectory(artifacts).pipe(
    Effect.orElseSucceed((): ReadonlyArray<string> => []),
    Effect.map((names: ReadonlyArray<string>) => {
      let top = 0
      for (const name of names) top = Math.max(top, generationOf(first, name) ?? 0)
      return top
    })
  )

/**
 * The generation the capture the pcap names `first` stands at in the store:
 * the highest one the store holds, or the first.
 */
export const current = Effect.fn("Generation.current")(function*(artifacts: string, first: string) {
  const fs = yield* FileSystem.FileSystem
  const generation = Math.max(1, yield* highest(fs, artifacts, first))
  return { capture: nameOf(first, generation), generation } satisfies Generation
})

/** The first generation after `at` and after every one the store holds. */
export const after = Effect.fn("Generation.after")(function*(artifacts: string, first: string, at: number) {
  const fs = yield* FileSystem.FileSystem
  const generation = Math.max(at, yield* highest(fs, artifacts, first)) + 1
  return { capture: nameOf(first, generation), generation } satisfies Generation
})
