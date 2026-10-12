/**
 * Regenerates the pivot document types from the Rust-owned JSON Schema.
 *
 * The schema owner is the `pivot-schema` binary (`pivot-schema schema pivot`);
 * its export is copied to `schema/pivot-v3.schema.json` and compiled to
 * `src/generated/pivot-v3.ts`. Both outputs are committed. The document types
 * are NEVER hand-written.
 *
 * Usage:
 *   PIVOT_SCHEMA_BIN=/path/to/pivot-schema pnpm regen-types
 *   pnpm regen-types -- /path/to/pivot-schema
 */
import { execFileSync } from "node:child_process"
import { mkdirSync, writeFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import { compile } from "json-schema-to-typescript"

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const bin = process.env["PIVOT_SCHEMA_BIN"] ?? process.argv[2] ?? "pivot-schema"

const schemaPath = resolve(packageRoot, "schema/pivot-v3.schema.json")
const typesPath = resolve(packageRoot, "src/generated/pivot-v3.ts")

const raw = execFileSync(bin, ["schema", "pivot"], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 })
const schema = JSON.parse(raw) as Record<string, unknown>

mkdirSync(dirname(schemaPath), { recursive: true })
writeFileSync(schemaPath, `${JSON.stringify(schema, null, 2)}\n`)

const banner = [
  "/* eslint-disable */",
  "/**",
  " * DO NOT EDIT. Generated from the Rust-owned JSON Schema by `pnpm regen-types`.",
  " * Source of truth: `pivot-schema schema pivot` (crate `pivot-schema`).",
  " * Hand edits are lost on the next regeneration.",
  " */"
].join("\n")

const ts = await compile(schema as never, "PivotV3", {
  bannerComment: banner,
  additionalProperties: false,
  unreachableDefinitions: true,
  declareExternallyReferenced: true,
  style: { semi: false, singleQuote: false }
})

mkdirSync(dirname(typesPath), { recursive: true })
writeFileSync(typesPath, ts)

process.stdout.write(`wrote ${schemaPath}\nwrote ${typesPath}\n`)
