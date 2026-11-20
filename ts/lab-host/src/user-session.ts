/**
 * The systemd user session this pipeline's long children run in, where one
 * answers: whether it does, and the command line that puts a child in a
 * transient scope of it with its caps.
 *
 * Where none answers (`XDG_RUNTIME_DIR` or `DBUS_SESSION_BUS_ADDRESS` unset,
 * `systemctl --user` silent or `offline`), a child runs with plain exec and no
 * per-child cap: whatever runs the install (a container, the host) caps its
 * memory. The session is asked once per process ({@link answers}), and its
 * absence is said once on stderr, naming how to get one back, unless the
 * install states it runs none (its pack's `userSession` variable `none`): a
 * tools image, whose container caps every child.
 */
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { spawnSync } from "node:child_process"

/** A command line, as a spawn takes it. */
export interface CommandLine {
  readonly command: string
  readonly args: ReadonlyArray<string>
}

/** How an operator gets the session back, named rather than implied. */
export const FIX = "start one and export it (`loginctl enable-linger $USER`, " +
  "`export XDG_RUNTIME_DIR=/run/user/$(id -u)`, " +
  "`export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus`), " +
  "or put those two keys in <repo>/.env.local"

/**
 * Why no user session answers under `env`, or `null` where one does: both its
 * variables set, and `systemctl --user is-system-running` printing a state
 * other than `offline` (what a host that is not systemd prints). The printed
 * state is read, not the exit code: a degraded session exits non-zero and is
 * a session.
 */
export const absence = (env: NodeJS.ProcessEnv): Effect.Effect<string | null> =>
  Effect.sync(() => {
    const missing = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"].filter((key) => env[key] === undefined)
    if (missing.length > 0) return `${missing.join(" and ")} unset`
    const r = spawnSync("systemctl", ["--user", "is-system-running"], { encoding: "utf8", env })
    const state = typeof r.stdout === "string" ? r.stdout.trim() : ""
    if (state === "offline") return "systemctl --user is offline"
    if (state !== "") return null
    return String(r.stderr || r.error?.message || "").trim().split("\n")[0] || "systemctl --user did not answer"
  })

/**
 * Whether a user session answers `env`, its absence said through `say` unless
 * `env` states the install runs none (`noneVariable` set to `none`).
 */
export const askedOf = (
  noneVariable: string,
  env: NodeJS.ProcessEnv,
  say: (line: string) => Effect.Effect<void>
): Effect.Effect<boolean> =>
  env[noneVariable] === "none"
    ? Effect.succeed(false)
    : Effect.flatMap(absence(env), (why) =>
      why === null
        ? Effect.succeed(true)
        : Effect.as(
          say(
            `[session] no systemd --user session (${why}): children run with plain exec, no per-child memory cap; ` +
              `to cap them, ${FIX}`
          ),
          false
        ))

/** Each none-variable's cached answer: the session is asked once per process, whichever module asks. */
const answered = new Map<string, Effect.Effect<boolean>>()

/**
 * Whether a user session answers this process's environment, `noneVariable`
 * stating an install that runs none: asked on first use, its absence said on
 * stderr, the answer kept for the process.
 */
export const answers = (noneVariable: string): Effect.Effect<boolean> => {
  const known = answered.get(noneVariable)
  if (known !== undefined) return known
  const fresh = Effect.runSync(Effect.cached(Effect.suspend(() => askedOf(noneVariable, process.env, Console.error))))
  answered.set(noneVariable, fresh)
  return fresh
}

/**
 * `command` inside a transient user scope with `properties` (`MemoryMax=…`),
 * where `session` holds; the command itself, uncapped, where it does not.
 */
export const scoped = (
  properties: ReadonlyArray<string>,
  command: string,
  args: ReadonlyArray<string>,
  session: boolean
): CommandLine =>
  session
    ? {
      command: "systemd-run",
      args: ["--user", "--scope", "-q", ...properties.flatMap((one) => ["-p", one]), command, ...args]
    }
    : { command, args }
