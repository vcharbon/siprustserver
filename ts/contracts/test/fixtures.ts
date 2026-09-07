/**
 * Where the conformance oracles live. The Rust crates' own committed fixtures
 * ARE the oracle: a mirror is checked against the bytes the Rust formatter
 * wrote, never against a copy someone kept in sync by hand.
 */
import * as fs from "node:fs"
import * as path from "node:path"

const here = path.dirname(new URL(import.meta.url).pathname)

/** `crates/pivot-schema/tests/fixtures` — the pivot documents and the run bundle. */
export const PIVOT_FIXTURES = path.resolve(here, "../../../crates/pivot-schema/tests/fixtures")

/** `e2e/schemas` — the committed JSON Schemas the e2e records are cross-checked against. */
export const E2E_SCHEMAS = path.resolve(here, "../../../e2e/schemas")

/** This package's own fixtures, for the contracts whose oracle is not committed upstream. */
export const LOCAL_FIXTURES = path.resolve(here, "fixtures")

export const read = (...at: Array<string>): string => fs.readFileSync(path.join(...at), "utf8")

export const readJson = (...at: Array<string>): unknown => JSON.parse(read(...at))

/** Every `*.v3.json` document, sorted, so a new fixture joins the sweep by existing. */
export const pivotDocuments = (): Array<string> =>
  fs
    .readdirSync(PIVOT_FIXTURES)
    .filter((name) => name.endsWith(".v3.json"))
    .sort()
