/**
 * A run set ASIDE: evidence recorded for one question (a forced replay, say),
 * reached by naming it — its run id, its cell — and never by a reading that
 * chooses a run: no standing, no call's latest or recent run, no lane's count,
 * no capture a lane already ran, no run set but one naming its run or its
 * environment.
 *
 * Both halves of a run carry the mark, because rows and bundles are read apart:
 * its row is under the environment {@link ENV}, which {@link excluded} keeps out
 * of every row reading that chooses, and its bundle sits under its capture's
 * {@link DIR} directory, where no walker of `<capture>/run<id>` looks.
 */
import type { SqlClient } from "effect/unstable/sql/SqlClient"
import type { Fragment } from "effect/unstable/sql/Statement"
import type { RunIdentity } from "./run-identity.js"

/** The environment an aside run is recorded under; no current population names it. */
export const ENV = "aside"

/** An aside run's identity. */
export const IDENTITY: RunIdentity = { env: ENV, branch: null, laneHash: null, load: null }

/** The directory under a capture's session directory its aside bundles sit in. */
export const DIR = "aside"

/** `<alias>` is not an aside run: the clause every row reading that chooses a run carries. */
export const excluded = (sql: SqlClient, alias: string): Fragment =>
  sql`${sql(alias)}.env IS DISTINCT FROM ${ENV}`
