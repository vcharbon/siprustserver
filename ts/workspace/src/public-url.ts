/**
 * The site's public URL: the one origin every link the tooling prints or
 * writes for a human is built under, and the address the site binds.
 *
 * - The public-URL variable, where stated: an http(s) URL, kept without its
 *   trailing slash.
 * - Else the address the site answers on (the bind variable, default
 *   `127.0.0.1`) and its port (the port variable, default the tooling's): a
 *   wildcard bind (`0.0.0.0`, `::`) answers on every interface and is named by
 *   the machine's `hostname -f`; any other bind is named by its own address,
 *   so a loopback site never prints a link only another interface would answer.
 *
 * {@link make} binds the functions to a tooling's variables.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import { spawnSync } from "node:child_process"
import * as os from "node:os"
import type { Variables } from "./workspace-pack.js"

/** The address the site binds where its variable states none: loopback only. */
export const DEFAULT_BIND = "127.0.0.1"

/** The variables the public URL is read from, and the site's port where none is stated. */
export interface SiteVariables extends Pick<Variables, "publicUrl" | "bind" | "port"> {
  readonly defaultPort: number
}

/** What the public URL is made of. */
export interface Setting {
  /** The public-URL variable's value; undefined where unstated. */
  readonly stated: string | undefined
  /** The address the site binds, as `listen` takes it. */
  readonly bind: string
  readonly port: number
}

export class Invalid extends Schema.TaggedError<Invalid>()("PublicUrl.Invalid", {
  reason: Schema.String
}) {
  override get message(): string {
    return this.reason
  }
}

/** A value stated as empty is unstated. */
const statedOf = (value: string | undefined): string | undefined => {
  const trimmed = value?.trim()
  return trimmed === undefined || trimmed === "" ? undefined : trimmed
}

/** Whether `bind` answers on every interface rather than on one address. */
export const isWildcard = (bind: string): boolean => bind === "0.0.0.0" || bind === "::"

/** Whether {@link derive} reads the machine's name for `setting`. */
export const needsHost = (setting: Setting): boolean => setting.stated === undefined && isWildcard(setting.bind)

/** The machine's fully qualified name (`hostname -f`), else its own name where that command answers nothing within 2 s. */
export const hostName: Effect.Effect<string> = Effect.sync(() => {
  const r = spawnSync("hostname", ["-f"], { encoding: "utf8", timeout: 2000 })
  const named = r.status === 0 ? r.stdout.trim() : ""
  return named === "" ? os.hostname() : named
})

/** The address the site binds: `site`'s bind variable, else {@link DEFAULT_BIND}. */
export const bindOf = (site: SiteVariables, env: NodeJS.ProcessEnv = process.env): string =>
  statedOf(env[site.bind]) ?? DEFAULT_BIND

/** The site's port: `site`'s port variable, else its default; a value that is no port is refused. */
export const portOf = (site: SiteVariables, env: NodeJS.ProcessEnv = process.env): Result.Result<number, string> => {
  const stated = statedOf(env[site.port])
  if (stated === undefined) return Result.succeed(site.defaultPort)
  const port = Number(stated)
  return Number.isInteger(port) && port > 0 && port < 65536
    ? Result.succeed(port)
    : Result.fail(`${site.port} is not a port: ${stated}`)
}

/** The setting `env` states; the site's own port where the caller knows it, else the port variable. */
export const settingOf = (
  site: SiteVariables,
  env: NodeJS.ProcessEnv = process.env,
  port?: number
): Result.Result<Setting, string> =>
  Result.map(port === undefined ? portOf(site, env) : Result.succeed(port), (port) => ({
    stated: statedOf(env[site.publicUrl]),
    bind: bindOf(site, env),
    port
  }))

/**
 * The public URL of `setting`, `host` naming the machine; a stated value that
 * is no http(s) URL, or carries a query or a fragment, is refused naming
 * `site`'s public-URL variable.
 */
export const derive = (site: Pick<SiteVariables, "publicUrl">, setting: Setting, host: string): Result.Result<string, string> => {
  if (setting.stated !== undefined) {
    const url = URL.parse(setting.stated)
    if (url === null || (url.protocol !== "http:" && url.protocol !== "https:")) {
      return Result.fail(`${site.publicUrl} is not an http(s) URL: ${setting.stated}`)
    }
    if (url.search !== "" || url.hash !== "") {
      return Result.fail(`${site.publicUrl} carries a query or a fragment: ${setting.stated}`)
    }
    return Result.succeed(`${url.origin}${url.pathname.replace(/\/+$/, "")}`)
  }
  const name = isWildcard(setting.bind) ? host : setting.bind
  return Result.succeed(`http://${name.includes(":") ? `[${name}]` : name}:${setting.port}`)
}

/** The public URL of `setting`, the machine's name read only where the setting needs it. */
export const resolve = (site: SiteVariables, setting: Setting): Effect.Effect<string, Invalid> =>
  Effect.flatMap(needsHost(setting) ? hostName : Effect.succeed(""), (host) =>
    Result.match(derive(site, setting, host), {
      onSuccess: Effect.succeed,
      onFailure: (reason) => Effect.fail(new Invalid({ reason }))
    }))

/** The public URL `env` states or derives; `port` is the site's own where the caller knows it. */
export const fromEnv = (
  site: SiteVariables,
  env: NodeJS.ProcessEnv = process.env,
  port?: number
): Effect.Effect<string, Invalid> =>
  Result.match(settingOf(site, env, port), {
    onSuccess: (setting) => resolve(site, setting),
    onFailure: (reason) => Effect.fail(new Invalid({ reason }))
  })

/** {@link fromEnv} for a caller whose contract is synchronous; an invalid setting throws its {@link Invalid}. */
export const fromEnvSync = (site: SiteVariables, env: NodeJS.ProcessEnv = process.env): string =>
  Effect.runSync(fromEnv(site, env))

/** Every function above bound to `site`, its arguments otherwise the same. */
export const make = (site: SiteVariables) => ({
  bindOf: (env?: NodeJS.ProcessEnv) => bindOf(site, env),
  portOf: (env?: NodeJS.ProcessEnv) => portOf(site, env),
  settingOf: (env?: NodeJS.ProcessEnv, port?: number) => settingOf(site, env, port),
  derive: (setting: Setting, host: string) => derive(site, setting, host),
  resolve: (setting: Setting) => resolve(site, setting),
  fromEnv: (env?: NodeJS.ProcessEnv, port?: number) => fromEnv(site, env, port),
  fromEnvSync: (env?: NodeJS.ProcessEnv) => fromEnvSync(site, env)
})
