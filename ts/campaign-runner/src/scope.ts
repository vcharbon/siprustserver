/**
 * What a run covers: every case file its glob selects, or the one case (and
 * sub-case) `--case` / `--subcase` keep. A run of less than every case, or a
 * quick one (`quick.ts`), is partial and states its scope.
 */
import type { Campaigns } from "@sip/workspace"
import { refOf, type Selected } from "./case-file.js"

/** The cells a run keeps: one case's, by its ref, and one of its sub-cases where named. */
export interface Only {
  readonly ref: string
  readonly subcase: string | null
}

/** `--case` and `--subcase` → the cells kept, null for every one, or why not. */
export const onlyOf = (ref: string | undefined, subcase: string | undefined): Only | null | string =>
  ref === undefined || ref === ""
    ? subcase === undefined || subcase === "" ? null : "--subcase is given with its --case"
    : { ref, subcase: subcase === undefined || subcase === "" ? null : subcase }

/**
 * What a run covers where it is not every case: the case and sub-case kept,
 * else the glob where it left a case file out (`partial`), either marked
 * `(--quick)` on a quick run (which is never full, whatever it compiles);
 * null for a full run.
 */
export const scopeOf = (glob: string, only: Only | null, partial: boolean, quick = false): string | null => {
  const narrowed = only !== null
    ? only.subcase === null ? only.ref : `${only.ref} [${only.subcase}]`
    : partial
    ? glob
    : null
  return quick ? `${narrowed ?? glob} (--quick)` : narrowed
}

/** Why a run has no case file to run: its campaign's globs match none, or `glob` keeps none of them. */
export const emptyRefusal = (
  campaign: Pick<Campaigns.Base, "name" | "cases">,
  glob: string,
  selection: Pick<Selected, "inCampaign" | "selected" | "broken">
): string | undefined => {
  if (selection.inCampaign === 0) return `campaign ${campaign.name}: no case file matches ${campaign.cases.join(", ")}`
  return selection.selected.length === 0 && selection.broken.length === 0
    ? `no case file of campaign ${campaign.name} matches ${glob}`
    : undefined
}

/**
 * Why `only` cannot run: every selected case file states another ref. A broken
 * or ref-less file might state it, so the compile phase answers for those.
 */
export const onlyRefusal = (
  campaign: { readonly name: string },
  only: Only | null,
  selection: Pick<Selected, "selected" | "broken">
): string | undefined => {
  if (only === null || selection.broken.length > 0) return undefined
  const refs = selection.selected.map((file) => refOf(file.value))
  if (refs.includes(only.ref) || refs.includes(undefined)) return undefined
  return `no selected case file of campaign ${campaign.name} states ${only.ref}`
}
