/**
 * The environment's values in a target file's strings: `${VAR}` is the
 * variable's value, `${VAR:-default}` its value or, where it is unset or
 * empty, the default. A `${VAR}` whose variable is unset or empty refuses the
 * whole value, naming the dotted key and the variable. A `$` outside that form
 * is kept as written.
 */
import * as Result from "effect/Result"

const REFERENCE = /\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}/g

/** `text` with every reference resolved from `env`, or the first variable it lacks. */
export const expandText = (
  text: string,
  env: Readonly<Record<string, string | undefined>>
): Result.Result<string, string> => {
  let missing: string | undefined
  const out = text.replace(REFERENCE, (_, name: string, fallback: string | undefined) => {
    const value = env[name]
    if (value !== undefined && value !== "") return value
    if (fallback !== undefined) return fallback
    missing ??= name
    return ""
  })
  return missing === undefined ? Result.succeed(out) : Result.fail(missing)
}

/**
 * Every string of `value`, at any depth, expanded from `env`; a key holding a
 * reference to an unset variable answers `<dotted key>: ${VAR} is not set`.
 */
export const expandAll = <A>(value: A, env: Readonly<Record<string, string | undefined>>): Result.Result<A, string> => {
  const walk = (node: unknown, at: ReadonlyArray<string>): Result.Result<unknown, string> => {
    if (typeof node === "string") {
      return Result.mapError(expandText(node, env), (name) => `${at.join(".")}: \${${name}} is not set`)
    }
    if (Array.isArray(node)) {
      const out: Array<unknown> = []
      for (const [i, item] of node.entries()) {
        const one = walk(item, [...at, String(i)])
        if (Result.isFailure(one)) return one
        out.push(one.success)
      }
      return Result.succeed(out)
    }
    if (typeof node === "object" && node !== null) {
      const out: Record<string, unknown> = {}
      for (const [key, item] of Object.entries(node)) {
        const one = walk(item, [...at, key])
        if (Result.isFailure(one)) return one
        out[key] = one.success
      }
      return Result.succeed(out)
    }
    return Result.succeed(node)
  }
  return walk(value, []) as Result.Result<A, string>
}
