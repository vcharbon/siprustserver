/**
 * Where the Rust binaries are, and the env vars that override it.
 *
 * A binary this repository builds gets a default under its own `target/release`
 * — a checkout that ran `cargo build --release` needs no configuration. A binary
 * built ELSEWHERE gets no default at all: guessing a path outside this
 * repository would mean running whatever happened to be there.
 */
import * as Config from "effect/Config"
import * as path from "node:path"

/** The repository root, from this file's own location. */
export const REPO_ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../../..")

/** `<repo>/target/release/<name>` — where `cargo build --release` puts a binary. */
export const releaseBinary = (name: string): string => path.join(REPO_ROOT, "target", "release", name)

/** An env-var override over a built-in default. */
export const binaryPath = (envVar: string, fallback: string): Config.Config<string> =>
  Config.string(envVar).pipe(Config.withDefault(fallback))

/** An env-var path with NO default: the binary lives outside this repository. */
export const requiredBinaryPath = (envVar: string): Config.Config<string> => Config.string(envVar)

/** `sipflow` — capture → flows document. */
export const SIPFLOW = binaryPath("SIPFLOW", releaseBinary("sipflow"))

/** `pivot-schema` — schemas, the canonical formatter, lint, tier data. */
export const PIVOT_SCHEMA = binaryPath("PIVOT_SCHEMA_BIN", releaseBinary("pivot-schema"))

/** `replay` — run-spec in, run bundle out. Built by the deployment, not here. */
export const REPLAY = requiredBinaryPath("REPLAY_BIN")
