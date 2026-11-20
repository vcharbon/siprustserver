/**
 * Which cells a campaign run takes, by the tags their cases carry: a case's
 * own `tags` and those `tag_cases` puts on its file. A lane's file says what it
 * does with a tag where it differs from the tag's default; `--allow` lifts an
 * exclusion for one run; a case run on its own (`--case`) is never excluded.
 * Every tag named anywhere must be declared, so a misspelt name refuses the run
 * and can never let a case through on a lab that should not take it.
 */
import type { LaneView } from "@sip/rig"
import type { Declared } from "@sip/workspace/campaign-tags"
import { type CaseFile, matches } from "./case-file.js"
import type { Heal } from "./cell.js"
import * as Results from "@sip/store/campaign-results"
import type { Alone } from "./schedule.js"

/** What a run selects by: the deployment's declarations, and the tags it opted into. */
export interface Tagging {
  readonly declared: Declared
  readonly allow: ReadonlyArray<string>
}

/** A lane as selection reads it. */
type LaneLists = Pick<LaneView.View, "id" | "file" | "tags">

/** A case file as selection reads it. */
type NamedFile = Pick<CaseFile, "file" | "absolute">

/** `--allow a,b` → the tags named. */
export const allowOf = (text: string): ReadonlyArray<string> => [
  ...new Set(text.split(",").map((tag) => tag.trim()).filter((tag) => tag !== ""))
]

const isDeclared = (declared: Declared, tag: string): boolean => Object.hasOwn(declared.tags, tag)

const undeclaredIn = (declared: Declared, tags: ReadonlyArray<string>): ReadonlyArray<string> =>
  tags.filter((tag) => !isDeclared(declared, tag))

const knownLine = (declared: Declared): string => {
  const names = Object.keys(declared.tags)
  return names.length === 0 ? "none is declared" : `declared: ${names.join(", ")}`
}

/** Why `allow` names an undeclared tag, or `undefined`. */
export const allowRefusal = (declared: Declared, allow: ReadonlyArray<string>): string | undefined => {
  const unknown = undeclaredIn(declared, allow)
  return unknown.length === 0 ? undefined : `--allow names ${unknown.join(", ")}, no declared tag (${knownLine(declared)})`
}

/** The tags a case file's default export states; a value of another shape is the compiler's to refuse. */
const statedTagsOf = (value: unknown): ReadonlyArray<string> => {
  const tags = typeof value === "object" && value !== null ? (value as { tags?: unknown }).tags : undefined
  return Array.isArray(tags) ? tags.filter((tag): tag is string => typeof tag === "string") : []
}

/** The heal times a case file's default export states. */
const statedHealOf = (value: unknown): Heal => {
  const stated = typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {}
  const ms = (field: unknown): number => (typeof field === "number" ? field : 0)
  return { healMs: ms(stated.healMs), healRealClockOnSimulatedMs: ms(stated.healRealClockOnSimulatedMs) }
}

/** The tags a case file's case carries: its own, then those `tag_cases` puts on the file. */
export const tagsOfFile = (declared: Declared, file: NamedFile, stated: ReadonlyArray<string>): ReadonlyArray<string> => [
  ...new Set([
    ...stated,
    ...declared.tagCases
      .filter(({ cases }) => cases.some((pattern) => matches(pattern, file.file, file.absolute)))
      .map(({ tag }) => tag)
  ])
]

/** Whether a case carrying `tags` runs with nothing else on its lane. */
export const isAlone = (declared: Declared, tags: ReadonlyArray<string>): boolean =>
  tags.some((tag) => isDeclared(declared, tag) && declared.tags[tag]!.alone === true)

/** Why a case file's case does not hold: an undeclared tag, or a heal time other cells would run through. */
export const caseRefusal = (
  declared: Declared,
  file: string,
  tags: ReadonlyArray<string>,
  heal: Heal
): string | undefined => {
  const unknown = undeclaredIn(declared, tags)
  if (unknown.length > 0) return `${file}: names ${unknown.join(", ")}, no declared tag (${knownLine(declared)})`
  return (heal.healMs > 0 || heal.healRealClockOnSimulatedMs > 0) && !isAlone(declared, tags)
    ? `${file}: states a heal time with no tag stating alone: other cells would run through the wait`
    : undefined
}

/**
 * Why the run's tagging does not hold, before anything runs: an undeclared tag
 * in `--allow`, a lane's lists, `tag_cases` or a selected case; a `tag_cases`
 * glob that matches no file of `files`, every file the deployment reads; a
 * selected case's heal time no alone tag backs.
 */
export const refusalOf = (
  tagging: Tagging,
  lanes: ReadonlyArray<LaneLists>,
  files: ReadonlyArray<NamedFile>,
  selected: ReadonlyArray<CaseFile>
): string | undefined => {
  const { allow, declared } = tagging
  const allowed = allowRefusal(declared, allow)
  if (allowed !== undefined) return allowed
  for (const lane of lanes) {
    const unknown = undeclaredIn(declared, [...lane.tags.run, ...lane.tags.exclude])
    if (unknown.length > 0) return `${lane.file ?? `the lane ${lane.id}`}: names ${unknown.join(", ")}, no declared tag (${knownLine(declared)})`
  }
  for (const { cases, tag } of declared.tagCases) {
    if (!isDeclared(declared, tag)) return `${declared.file}: tag_cases names ${tag}, no declared tag (${knownLine(declared)})`
    const idle = cases.find((pattern) => !files.some((one) => matches(pattern, one.file, one.absolute)))
    if (idle !== undefined) return `${declared.file}: tag_cases of ${tag}: ${idle} matches no case file`
  }
  for (const file of selected) {
    const tags = tagsOfFile(declared, file, statedTagsOf(file.value))
    const refused = caseRefusal(declared, file.file, tags, statedHealOf(file.value))
    if (refused !== undefined) return refused
  }
  return undefined
}

const excludes = (declared: Declared, lane: LaneLists, tag: string): boolean =>
  lane.tags.exclude.includes(tag) || (!lane.tags.run.includes(tag) && declared.tags[tag]?.default === "exclude")

/** The tags a campaign leaves out on `lane`: excluded there and not allowed, sorted. */
export const leftOutOn = (tagging: Tagging, lane: LaneLists): ReadonlyArray<string> =>
  Object.keys(tagging.declared.tags)
    .filter((tag) => excludes(tagging.declared, lane, tag) && !tagging.allow.includes(tag))
    .sort()

/** The tags `lane` excludes by itself, whatever a run allows, sorted. */
export const excludedOn = (declared: Declared, lane: LaneLists): ReadonlyArray<string> =>
  leftOutOn({ declared, allow: [] }, lane)

/**
 * Why a cell carrying `tags` is left out of the run on `lane`, or `undefined`:
 * every tag that excludes it, and the `--allow` that runs it. A case run on its
 * own (`individually`) is never left out.
 */
export const exclusionOf = (
  tagging: Tagging,
  lane: LaneLists,
  tags: ReadonlyArray<string>,
  individually: boolean
): string | undefined => {
  if (individually) return undefined
  const left = leftOutOn(tagging, lane)
  const by = tags.filter((tag) => left.includes(tag))
  return by.length === 0
    ? undefined
    : `${Results.EXCLUDED}: ${by.join(", ")} on ${lane.id} (--allow ${by.join(",")} runs it, --case runs it on its own)`
}

/** Whether a case file's cells run on `lane`, its tags considered. */
export const fileRunsOn = (tagging: Tagging, lane: LaneLists, file: CaseFile, individually: boolean): boolean =>
  exclusionOf(tagging, lane, tagsOfFile(tagging.declared, file, statedTagsOf(file.value)), individually) === undefined

/**
 * What the run's log states of `cells`' tags on `lane`: one line per tag and
 * the cells it left out, or, for a case run on its own, the excluded tags it
 * ran past.
 */
export const logLines = (
  tagging: Tagging,
  lane: LaneLists,
  cells: ReadonlyArray<{ readonly tags: ReadonlyArray<string> }>,
  individually: boolean
): ReadonlyArray<string> =>
  leftOutOn(tagging, lane).flatMap((tag) => {
    const count = cells.filter((cell) => cell.tags.includes(tag)).length
    if (count === 0) return []
    return [
      individually
        ? `[tags] ${count} cell(s) run on ${lane.id} past ${tag}, which it excludes: the case was asked for on its own`
        : `[excluded] ${count} cell(s) of ${tag} on ${lane.id}`
    ]
  })

/** What the run's row records of its tagging: the tags it allowed and what each lane left out; nothing is left out of a case run on its own. */
export const recordOf = (
  tagging: Tagging,
  lanes: ReadonlyArray<LaneLists>,
  individually: boolean
): Results.RunSelection => ({
  allow: tagging.allow,
  lanes: Object.fromEntries(lanes.map((lane) => [lane.id, individually ? [] : leftOutOn(tagging, lane)]))
})

/**
 * How a cell holds its lane when it runs alone: a real-clock lane by `healMs`;
 * the simulated lane, whose clock ends with the cell, by the real sleep it states.
 */
export const aloneOn = (
  lane: Pick<LaneView.View, "kind">,
  cell: { readonly id: string; readonly alone: boolean; readonly heal: Heal }
): Alone | undefined =>
  !cell.alone
    ? undefined
    : { name: cell.id, healMs: lane.kind === "simulated" ? cell.heal.healRealClockOnSimulatedMs : cell.heal.healMs }
