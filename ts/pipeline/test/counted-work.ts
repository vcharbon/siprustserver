/**
 * Work counted at the source, never on a clock and never by what the code
 * under test reports of itself: the reads of a flows document's messages, and
 * the array elements stepped over while a piece of work runs.
 */
import type { Flows } from "@sip/contracts"

/** A flows document whose message reads are counted. */
export interface Watched {
  /** The same legs, each `msgs` answering through a counting proxy. */
  readonly flows: Flows.FlowsDoc
  /** How many times a message was read by its index so far. */
  readonly reads: () => number
  /** How many times each message of leg `leg` was read so far, by its index in the leg. */
  readonly readsOf: (leg: number) => ReadonlyArray<number>
}

/** `flows` with every read of a message by its index counted, iteration included. */
export const watched = (flows: Flows.FlowsDoc): Watched => {
  let reads = 0
  const perLeg = flows.legs.map((leg) => leg.msgs.map(() => 0))
  const counting = <T extends object>(items: T, leg: number): T =>
    new Proxy(items, {
      get: (target, key, receiver) => {
        if (typeof key === "string" && /^\d+$/.test(key)) {
          reads++
          const counts = perLeg[leg]!
          const at = Number(key)
          if (at < counts.length) counts[at] = counts[at]! + 1
        }
        return Reflect.get(target, key, receiver)
      }
    })
  return {
    flows: { ...flows, legs: flows.legs.map((leg, i) => ({ ...leg, msgs: counting(leg.msgs, i) })) },
    reads: () => reads,
    readsOf: (leg) => [...perLeg[leg]!]
  }
}

const DRIVING = [
  "every",
  "filter",
  "find",
  "findIndex",
  "findLast",
  "findLastIndex",
  "flatMap",
  "forEach",
  "map",
  "reduce",
  "reduceRight",
  "some",
  "sort",
  "toSorted"
] as const

type Callback = (...args: Array<unknown>) => unknown
type Driving = (this: unknown, callback: Callback, ...rest: Array<unknown>) => unknown

/**
 * Runs `work` and counts the array elements it steps over: each step of an
 * array's iterator (`for…of`, a spread, a destructuring) and each call of a
 * callback an array method drives, a sort's comparator included. A sort given
 * no comparator is not counted. `Array.prototype` is patched for the
 * synchronous run of `work` only. A scan of an array the code under test built
 * for itself counts here like any other.
 */
export const arraySteps = <T>(work: () => T): { readonly result: T; readonly steps: number } => {
  let steps = 0
  const proto = Array.prototype as unknown as Record<string | symbol, unknown>
  const kept = new Map<string | symbol, unknown>()
  for (const name of DRIVING) {
    const original = proto[name] as Driving
    kept.set(name, original)
    proto[name] = function (this: unknown, callback: Callback, ...rest: Array<unknown>) {
      if (typeof callback !== "function") return original.call(this, callback, ...rest)
      return original.call(
        this,
        (...args: Array<unknown>) => {
          steps++
          return callback(...args)
        },
        ...rest
      )
    }
  }
  const iterate = proto[Symbol.iterator] as (this: unknown) => Iterator<unknown>
  kept.set(Symbol.iterator, iterate)
  proto[Symbol.iterator] = function (this: unknown) {
    const iterator = iterate.call(this)
    const next = iterator.next.bind(iterator)
    iterator.next = () => {
      steps++
      return next()
    }
    return iterator
  }
  try {
    const result = work()
    return { result, steps }
  } finally {
    for (const [name, original] of kept) proto[name] = original
  }
}
