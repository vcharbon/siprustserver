/**
 * The commit a build stamps (`src/build-stamp.ts`), read when the build
 * starts: `SIPRIG_BUILD_COMMIT` where the builder states one (a build context
 * that ships no repository), else the short commit of the outermost checkout
 * holding the app (a superproject that consumes it as a submodule stamps its
 * own commit), `-dirty` where that checkout's tree differs from it.
 */
import { execFileSync } from "node:child_process"
import type { BuildStamp } from "./src/build-stamp.ts"

/** The variable a builder states the commit in. */
export const COMMIT_VARIABLE = "SIPRIG_BUILD_COMMIT"

/** `git -C dir args…`'s output, trimmed; it throws where git refuses. */
const git = (dir: string, args: ReadonlyArray<string>): string =>
  execFileSync("git", ["-C", dir, ...args], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim()

/** The top of the outermost checkout holding `dir`, through every superproject above its own. */
const outermost = (dir: string): string => {
  const own = git(dir, ["rev-parse", "--show-toplevel"])
  const above = git(own, ["rev-parse", "--show-superproject-working-tree"])
  return above === "" ? own : outermost(above)
}

/** The commit a build from `dir` under `env` stamps; null where neither the variable nor git states one. */
export const commitOf = (dir: string, env: NodeJS.ProcessEnv): string | null => {
  const stated = env[COMMIT_VARIABLE]
  if (stated !== undefined && stated !== "") return stated
  try {
    const top = outermost(dir)
    const short = git(top, ["rev-parse", "--short", "HEAD"])
    return git(top, ["status", "--porcelain"]) === "" ? short : `${short}-dirty`
  } catch {
    return null
  }
}

/** The stamp of a build starting at `now` from `dir` under `env`. */
export const stampOf = (dir: string, env: NodeJS.ProcessEnv, now: Date): BuildStamp => ({
  commit: commitOf(dir, env),
  builtAt: now.toISOString()
})
