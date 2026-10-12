/**
 * Whether a call's row says `+ bad body` beside its main error: the main error
 * names another cause first, and the call still carries a body row no rule
 * accepts. A body delta is never invisible at the top level.
 */
import type { TriageMainError } from "./api.ts"

export const alsoBadBody = (error: TriageMainError | null, bodyFailing: number | undefined): boolean =>
  error !== null && error.kind !== "bad-body" && (bodyFailing ?? 0) > 0
