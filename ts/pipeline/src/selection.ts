/**
 * A VANTAGE: where an actor of a case observes its call — the captured leg,
 * and the hop on that leg the actor's socket pair sits at.
 */
export interface Vantage {
  readonly leg: number
  readonly hop: number
}
