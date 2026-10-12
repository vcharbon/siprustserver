/**
 * The run identities the DB-bound suites open runs under: the current
 * population's (the only runs a standing reads), and one other environment's.
 */
import type { Population } from "../src/current.js"
import type { RunIdentity } from "../src/run-identity.js"

/** The env and branch every suite's store states as current (`server.ts` `migrated`). */
export const CURRENT_POPULATION: Population = { env: "main-checkout", branch: "main" }

export const CURRENT: RunIdentity = { ...CURRENT_POPULATION, laneHash: null, load: null }

export const TASK_ENV: RunIdentity = { env: "dev-1-probe", branch: "fix/1-probe", laneHash: null, load: null }
