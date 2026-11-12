/**
 * A quick run (`--quick`), for writing cases: the simulated lane only, under
 * the pack's quick collection level (what its collectors skip there), each
 * cell stating what it was not given so a reader never takes it for a pass of
 * it; the run is partial (`scope.ts`). A quick run writes no JUnit file
 * elsewhere and is never a campaign job: nothing it reports leaves the run's
 * own record.
 */
import * as Arr from "effect/Array"

/** Why a quick run's cell holds no log. */
export const QUICK_LOG = "not fetched (--quick)"

/**
 * The lanes a run replays on: the ones `--lanes` names (or why they do not
 * read), `byDefault` where it names none; a quick run's the simulated lane
 * `simulated`, and any other lane named beside `--quick` refused.
 */
export const runLanes = <L extends string>(
  simulated: L,
  named: Arr.NonEmptyReadonlyArray<L> | string | undefined,
  quick: boolean,
  byDefault: ReadonlyArray<L>
): Arr.NonEmptyReadonlyArray<L> | string => {
  if (typeof named === "string") return named
  if (!quick) {
    return named ?? (Arr.isReadonlyArrayNonEmpty(byDefault) ? byDefault : "no lane of this environment runs a campaign")
  }
  if (named === undefined) return [simulated]
  const other = named.filter((lane) => lane !== simulated)
  return other.length === 0 ? named : `--quick runs the ${simulated} lane only, not ${other.join(", ")}`
}

/**
 * Why a quick run refuses what it is launched with: a JUnit file elsewhere, a
 * campaign job or a collection level of its own; undefined where none.
 */
export const quickRefusal = (
  quick: boolean,
  launched: { readonly junit: boolean; readonly job: boolean; readonly collection?: boolean }
): string | undefined => {
  if (!quick) return undefined
  const named = [
    launched.junit ? "--junit" : undefined,
    launched.job ? "--job" : undefined,
    launched.collection === true ? "--collection" : undefined
  ].filter((x) => x !== undefined)
  return named.length === 0 ? undefined : `--quick runs for writing cases: no ${named.join(" or ")}`
}

/** The collection level a run takes: a quick run the pack's `quickLevel`, any other the one `--collection` names, else the deployment's default. */
export const levelOf = (quickLevel: string, quick: boolean, named: string | undefined): string | undefined =>
  quick ? quickLevel : named
