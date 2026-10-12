/**
 * The published JSON Schemas of the report formats this package owns, by the
 * file each is published as under `schemas/` (`bin/schemas.ts` writes them).
 */
import * as CompareReport from "./compare-report.js"
import * as RunReport from "./run-report.js"

const pretty = (schema: unknown): string => `${JSON.stringify(schema, null, 2)}\n`

export const PUBLISHED: Readonly<Record<string, () => string>> = {
  "compare-report.schema.json": () => pretty(CompareReport.jsonSchema()),
  "run-report.schema.json": () => pretty(RunReport.jsonSchema())
}
