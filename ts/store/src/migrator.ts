/**
 * Applying the schema. No schema dump: the migrations ARE the schema, and a
 * dumped copy is one more thing to drift.
 */
import type * as Effect from "effect/Effect"
import * as Migrator from "effect/unstable/sql/Migrator"
import type { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import { loader, TABLE } from "./migrations.js"

export const run: Effect.Effect<
  ReadonlyArray<readonly [id: number, name: string]>,
  Migrator.MigrationError | SqlError,
  SqlClient
> = Migrator.make({})({ loader, table: TABLE })
