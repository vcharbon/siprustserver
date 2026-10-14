/**
 * A case result's main error as `case_results` stores it, one column per
 * field, and the case's source fault as `error_source` — stored for a passing
 * case too, where it is the only thing the columns hold — read back into the
 * {@link MainError} and source fault the writer stored.
 */
import type { MainError as MainErrors } from "@sip/contracts"
import type { MainError, MainErrorKind } from "./model.js"

/** The `error_*` columns of a case-result row. */
export interface ErrorColumns {
  readonly error_kind: string | null
  readonly error_rule: string | null
  readonly error_rfc: string | null
  readonly error_step: string | null
  readonly error_expected: string | null
  readonly error_detail: string | null
  readonly error_source: MainErrors.SourceFault | null
}

/** The main error the columns hold, the source fault beside it; null where the row stores none. */
export const errorOf = (row: ErrorColumns): MainError | null =>
  row.error_kind === null
    ? null
    : {
      kind: row.error_kind as MainErrorKind,
      rule: row.error_rule,
      rfc: row.error_rfc,
      step: row.error_step,
      expected: row.error_expected,
      detail: row.error_detail ?? "",
      ...(row.error_source === null ? {} : { source: row.error_source })
    }
