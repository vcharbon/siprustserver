/**
 * The postgres server a run of the DB-bound suites states: `STORE_TEST_URL`
 * from the environment, else the last `STORE_TEST_URL=` line of the
 * workspace's untracked env file ({@link ENV_FILE}), else none. An exported
 * value wins over the file, and an empty value states nothing. A host names
 * its server in the file, so a plain `vitest run` finds it with nothing
 * exported. Imported as `@sip/store/test-server-url`.
 */
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import { fileURLToPath } from "node:url"

/** The workspace's env file, `ts/.env.local`: untracked, written by the host. */
export const ENV_FILE = fileURLToPath(new URL("../../.env.local", import.meta.url))

const KEY = "STORE_TEST_URL"

const nonEmpty = (value: string | undefined): Option.Option<string> =>
  value === undefined || value.trim() === "" ? Option.none() : Option.some(value.trim())

/** The url the last `STORE_TEST_URL=` line of an env file's `text` states. */
export const fromEnvFile = (text: string): Option.Option<string> => {
  const values = text.split("\n").filter((line) => line.startsWith(`${KEY}=`))
  return nonEmpty(values.at(-1)?.slice(KEY.length + 1))
}

/** The url `env`, else the env file at `file`, states; a missing file states none. */
export const stated = (
  env: NodeJS.ProcessEnv,
  file: string = ENV_FILE
): Effect.Effect<Option.Option<string>, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const exported = nonEmpty(env[KEY])
    if (Option.isSome(exported)) return exported
    const fs = yield* FileSystem.FileSystem
    if (!(yield* fs.exists(file))) return Option.none()
    return fromEnvFile(yield* fs.readFileString(file))
  }).pipe(Effect.orDie)
