/**
 * The order a scope's collectors run in (ADR-0060 appendix §5): a collector
 * runs after every collector of its `after` among those that run with it, so
 * it reads their keys and records; one its `after` names that does not run
 * with it orders nothing. Among the collectors free to run, the first by name
 * runs first. A cycle of `after` is refused when the path loads
 * (`rig-path.ts`, {@link cycleOf}).
 */
import type * as Collector from "./collector.js"

/** A collector as its order reads it: its name and its manifest's `after`. */
export interface Ordered {
  readonly name: string
  readonly manifest: Pick<Collector.Collector, "after">
}

const byName = (a: Ordered, b: Ordered): number => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)

/** `collectors` in the order they run: each after those of its `after` among them, else by name; a cycle's members by name, last. */
export const ordered = <C extends Ordered>(collectors: ReadonlyArray<C>): ReadonlyArray<C> => {
  const names = new Set(collectors.map((collector) => collector.name))
  const waits = new Map(collectors.map((collector) => [collector.name, new Set((collector.manifest.after ?? []).filter((name) => names.has(name)))]))
  const left = [...collectors].sort(byName)
  const out: Array<C> = []
  while (left.length > 0) {
    const at = left.findIndex((collector) => waits.get(collector.name)!.size === 0)
    if (at < 0) return [...out, ...left]
    const [next] = left.splice(at, 1)
    out.push(next!)
    for (const pending of waits.values()) pending.delete(next!.name)
  }
  return out
}

/** The names of a cycle `collectors`' `after` make, in order, or `undefined` where none does. */
export const cycleOf = (collectors: ReadonlyArray<Ordered>): ReadonlyArray<string> | undefined => {
  const after = new Map(collectors.map((collector) => [collector.name, collector.manifest.after ?? []]))
  const done = new Set<string>()
  const visit = (name: string, trail: ReadonlyArray<string>): ReadonlyArray<string> | undefined => {
    const at = trail.indexOf(name)
    if (at >= 0) return [...trail.slice(at), name]
    if (done.has(name) || !after.has(name)) return undefined
    for (const next of after.get(name)!) {
      const cycle = visit(next, [...trail, name])
      if (cycle !== undefined) return cycle
    }
    done.add(name)
    return undefined
  }
  for (const collector of [...collectors].sort(byName)) {
    const cycle = visit(collector.name, [])
    if (cycle !== undefined) return cycle
  }
  return undefined
}
