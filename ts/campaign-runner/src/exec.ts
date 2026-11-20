/** One process run to its exit: its code and what it wrote on stdout and stderr. */
import * as Effect from "effect/Effect"
import * as Stream from "effect/Stream"
import * as ChildProcess from "effect/unstable/process/ChildProcess"
import { ChildProcessSpawner } from "effect/unstable/process/ChildProcessSpawner"

export interface Ran {
  readonly code: number
  readonly stdout: string
  readonly stderr: string
}

export const exec = (command: string, args: ReadonlyArray<string>, env: Readonly<Record<string, string>> = {}) =>
  Effect.scoped(
    Effect.gen(function*() {
      const spawner = yield* ChildProcessSpawner
      const handle = yield* spawner.spawn(
        ChildProcess.make(command, [...args], { stdin: "ignore", env, extendEnv: true })
      )
      const [stdout, stderr, code] = yield* Effect.all([
        Stream.mkString(Stream.decodeText(handle.stdout)),
        Stream.mkString(Stream.decodeText(handle.stderr)),
        handle.exitCode
      ], { concurrency: "unbounded" })
      return { code: Number(code), stdout, stderr } satisfies Ran
    })
  )
