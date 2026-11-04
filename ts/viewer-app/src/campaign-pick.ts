/**
 * The lanes a campaign start picks: only the lanes the server runs, of one
 * group, since the lanes of one run must run together. The default pick is
 * the group of the first runnable lane; a pick of two groups is refused here,
 * before any job starts, naming the groups that run.
 */
import type { LaunchLane } from "./api.ts"

/** The lanes that run here. */
export const runnable = (lanes: readonly LaunchLane[]): readonly LaunchLane[] => lanes.filter((l) => l.refused === null)

/** The runnable lanes by group, each group and its lanes in the order first met. */
export const groups = (lanes: readonly LaunchLane[]): readonly (readonly string[])[] => {
  const by = new Map<string, string[]>()
  for (const l of runnable(lanes)) by.set(l.group, [...(by.get(l.group) ?? []), l.lane])
  return [...by.values()]
}

/** The groups as a line: `a alone, or b + c`. */
export const groupsLine = (lanes: readonly LaunchLane[]): string =>
  groups(lanes).map((ids) => (ids.length === 1 ? `${ids[0]} alone` : ids.join(" + "))).join(", or ")

/** The pick a start opens with: every runnable lane of the first runnable lane's group. */
export const defaultPick = (lanes: readonly LaunchLane[]): readonly string[] => groups(lanes)[0] ?? []

/** Why `picked` cannot start, or null: no lane, a lane refused here, or lanes of two groups. */
export const pickRefusal = (picked: readonly string[], lanes: readonly LaunchLane[]): string | null => {
  if (picked.length === 0) return "no lane picked"
  const named = picked.map((id) => lanes.find((l) => l.lane === id))
  const refused = named.find((l) => l !== undefined && l.refused !== null)
  if (refused !== undefined) return refused.refused
  if (named.some((l) => l === undefined)) return `no lane ${picked.filter((id, i) => named[i] === undefined).join(", ")}`
  return new Set(named.map((l) => l!.group)).size > 1 ? `lanes of different groups run apart: ${groupsLine(lanes)}` : null
}
