/**
 * The **run's clock**, as `timing.json` in the run bundle, mirroring
 * `pivot_schema::bundle::timing`: when the run started, when it settled, and the
 * ceiling it had.
 *
 * Named apart from the document's `timing` block, which is the expect and settle
 * BUDGETS: one states what a run is allowed, the other what one run took.
 */
import * as Schema from "effect/Schema"

/** When the run started, when it settled, and the ceiling it had. */
export const RunTiming = Schema.Struct({
  started_at_ms: Schema.Int,
  /** Absent where the run never settled — itself a failure the verdict states. */
  settled_at_ms: Schema.optionalKey(Schema.Int),
  settle_budget_ms: Schema.Int
})
export interface RunTiming extends Schema.Schema.Type<typeof RunTiming> {}
