/** A greeting's tone. */
export type Tone = "plain" | "loud"

/** What a greeting states. */
export type Greeting = { readonly name: string; readonly tone: Tone }

/** How many greetings of each tone. */
export type Counts = Readonly<Record<Tone, number>>
