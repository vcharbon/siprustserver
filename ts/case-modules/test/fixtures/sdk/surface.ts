import type { Greeting } from "./types.js"

/** A greeting as defined, its text built. */
export interface Defined extends Greeting {
  readonly text: string
}

/** What the SDK offers a workspace file. */
export interface Api {
  readonly defineGreeting: (greeting: Greeting) => Defined
}
