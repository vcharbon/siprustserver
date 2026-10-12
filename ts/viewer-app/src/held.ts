/**
 * The held view of an async request: what a page keeps on screen while a new
 * answer is in flight. An `ok` answer replaces the value; a `loading` or an
 * `error` that follows keeps the last value and says so beside it, so a page
 * never unmounts what it was showing (a filter bar, a running button) to say
 * "loading".
 */
import { useRef } from "react"
import { useAsync, type Async } from "./api.ts"

export interface Held<A> {
  /** The last `ok` value, or nothing where none has arrived yet. */
  readonly current: A | undefined
  readonly loading: boolean
  /** The message of the latest answer where it failed; absent otherwise. */
  readonly error?: string
}

export const INITIAL: Held<never> = { current: undefined, loading: true }

/** Folds the next answer over what is held: the value survives a load and an error. */
export const hold = <A>(previous: Held<A>, next: Async<A>): Held<A> =>
  next.state === "ok"
    ? { current: next.value, loading: false }
    : next.state === "loading"
    ? { current: previous.current, loading: true }
    : { current: previous.current, loading: false, error: next.message }

/** `useAsync`, held: the request re-runs when its key changes, and the page keeps the last value meanwhile. */
export const useHeld = <A>(load: () => Promise<A>, key: string): Held<A> => {
  const result = useAsync(load, key)
  const previous = useRef<Held<A>>(INITIAL)
  const next = hold(previous.current, result)
  previous.current = next
  return next
}
