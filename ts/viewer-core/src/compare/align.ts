/**
 * Scenario-to-recording alignment: which scripted message a recorded one is,
 * which the run never produced, and which the run produced unasked.
 *
 * Alignment is per leg and order-preserving. Where the runner attributed every
 * recorded message to a step, that attribution is the alignment and nothing is
 * inferred; otherwise the two sequences are matched on their discriminators by
 * longest common subsequence, so one missing message does not desynchronise
 * everything after it.
 */
import { isMessageEvent, type LadderLayout, type MessageEvent } from "../layout/model.ts"

export type CompareStatus = "matched" | "missing" | "extra"

export interface CompareRow {
  readonly status: CompareStatus
  readonly leg: string
  readonly scenarioEventId: string | undefined
  readonly recordingEventId: string | undefined
  readonly label: string
}

export interface CompareResult {
  readonly rows: readonly CompareRow[]
  readonly scenarioStatus: Readonly<Record<string, CompareStatus>>
  readonly recordingStatus: Readonly<Record<string, CompareStatus>>
  readonly counts: { readonly matched: number; readonly missing: number; readonly extra: number }
}

const discriminator = (event: MessageEvent): string => `${event.op}:${event.label.split(" ")[0]}`

const byLeg = (layout: LadderLayout): Map<string, MessageEvent[]> => {
  const out = new Map<string, MessageEvent[]>()
  for (const event of layout.events) {
    if (!isMessageEvent(event)) continue
    const existing = out.get(event.legId)
    if (existing) existing.push(event)
    else out.set(event.legId, [event])
  }
  return out
}

const stepIdOf = (event: MessageEvent): string | undefined => {
  const decoration = event.decorations.find((entry) => entry.kind === "step")
  return decoration?.kind === "step" ? decoration.id : undefined
}

/** Longest common subsequence over discriminators, returned as index pairs. */
const lcsPairs = (
  left: readonly MessageEvent[],
  right: readonly MessageEvent[]
): readonly (readonly [number, number])[] => {
  const table: number[][] = Array.from({ length: left.length + 1 }, () => new Array<number>(right.length + 1).fill(0))
  for (let i = left.length - 1; i >= 0; i--) {
    for (let j = right.length - 1; j >= 0; j--) {
      table[i]![j] = discriminator(left[i]!) === discriminator(right[j]!)
        ? table[i + 1]![j + 1]! + 1
        : Math.max(table[i + 1]![j]!, table[i]![j + 1]!)
    }
  }
  const pairs: [number, number][] = []
  let i = 0
  let j = 0
  while (i < left.length && j < right.length) {
    if (discriminator(left[i]!) === discriminator(right[j]!)) {
      pairs.push([i, j])
      i++
      j++
    } else if (table[i + 1]![j]! >= table[i]![j + 1]!) i++
    else j++
  }
  return pairs
}

const alignLeg = (
  leg: string,
  scenario: readonly MessageEvent[],
  recorded: readonly MessageEvent[]
): readonly CompareRow[] => {
  const scenarioIds = new Set(scenario.map((event) => event.id))
  const attributed = recorded.length > 0 &&
    recorded.every((event) => {
      const stepId = stepIdOf(event)
      return stepId !== undefined && scenarioIds.has(stepId)
    })

  const pairs: (readonly [number, number])[] = attributed
    ? recorded.flatMap((event, right) => {
      const left = scenario.findIndex((candidate) => candidate.id === stepIdOf(event))
      return left < 0 ? [] : [[left, right] as const]
    })
    : [...lcsPairs(scenario, recorded)]

  const rows: CompareRow[] = []
  const matchedLeft = new Set(pairs.map(([left]) => left))
  const matchedRight = new Set(pairs.map(([, right]) => right))
  let left = 0
  let right = 0
  for (const [pl, pr] of pairs) {
    while (left < pl) {
      if (!matchedLeft.has(left)) {
        rows.push({
          status: "missing",
          leg,
          scenarioEventId: scenario[left]!.id,
          recordingEventId: undefined,
          label: scenario[left]!.label
        })
      }
      left++
    }
    while (right < pr) {
      if (!matchedRight.has(right)) {
        rows.push({
          status: "extra",
          leg,
          scenarioEventId: undefined,
          recordingEventId: recorded[right]!.id,
          label: recorded[right]!.label
        })
      }
      right++
    }
    rows.push({
      status: "matched",
      leg,
      scenarioEventId: scenario[pl]!.id,
      recordingEventId: recorded[pr]!.id,
      label: scenario[pl]!.label
    })
    left = pl + 1
    right = pr + 1
  }
  for (; left < scenario.length; left++) {
    if (matchedLeft.has(left)) continue
    rows.push({
      status: "missing",
      leg,
      scenarioEventId: scenario[left]!.id,
      recordingEventId: undefined,
      label: scenario[left]!.label
    })
  }
  for (; right < recorded.length; right++) {
    if (matchedRight.has(right)) continue
    rows.push({
      status: "extra",
      leg,
      scenarioEventId: undefined,
      recordingEventId: recorded[right]!.id,
      label: recorded[right]!.label
    })
  }
  return rows
}

export const compareLayouts = (scenario: LadderLayout, recording: LadderLayout): CompareResult => {
  const scenarioLegs = byLeg(scenario)
  const recordingLegs = byLeg(recording)
  const legs = [...new Set([...scenarioLegs.keys(), ...recordingLegs.keys()])].sort()

  const rows = legs.flatMap((leg) =>
    alignLeg(leg, scenarioLegs.get(leg) ?? [], recordingLegs.get(leg) ?? [])
  )

  const scenarioStatus: Record<string, CompareStatus> = {}
  const recordingStatus: Record<string, CompareStatus> = {}
  let matched = 0
  let missing = 0
  let extra = 0
  for (const row of rows) {
    if (row.scenarioEventId) scenarioStatus[row.scenarioEventId] = row.status
    if (row.recordingEventId) recordingStatus[row.recordingEventId] = row.status
    if (row.status === "matched") matched++
    else if (row.status === "missing") missing++
    else extra++
  }

  return { rows, scenarioStatus, recordingStatus, counts: { matched, missing, extra } }
}
