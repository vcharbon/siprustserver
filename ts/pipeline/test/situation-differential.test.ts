/**
 * The situation reading's forward pass against the rules applied literally
 * (`./situation-reference.ts`): the same placement for every message of
 * thousands of generated calls, and a cost that doubles, no more, when the
 * call's messages double.
 */
import { describe, expect, it } from "vitest"
import { type Observation, place, placeCounted } from "../src/situation.js"
import { placeReference } from "./situation-reference.js"

/** A seeded generator of floats in [0, 1), so a failing seed replays. */
const seeded = (seed: number) => {
  let state = seed >>> 0
  return (): number => {
    state = (state + 0x6d2b79f5) >>> 0
    let t = state
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

type Random = () => number

const pick = <T>(random: Random, items: ReadonlyArray<T>): T => items[Math.floor(random() * items.length)]!

const chance = (random: Random, p: number): boolean => random() < p

const SDP_PRESENCE: ReadonlyArray<boolean | undefined> = [true, false, undefined]

interface RequestShape {
  readonly dialogCreating?: boolean
  readonly cseq?: number | undefined
}

interface ResponseShape {
  readonly cseq?: number | undefined
  readonly sdp?: boolean | undefined
}

const request = (leg: string, emitted: boolean, method: string, shape: RequestShape = {}): Observation => ({
  leg,
  emitted,
  request: { method, dialogCreating: shape.dialogCreating ?? false, cseq: shape.cseq }
})

const response = (
  leg: string,
  emitted: boolean,
  status: number,
  method: string,
  shape: ResponseShape = {}
): Observation => ({ leg, emitted, response: { status, method, cseq: shape.cseq, sdp: shape.sdp } })

/**
 * Messages drawn independently: any leg, either way, any method, status, CSeq
 * and description presence, so every rule meets inputs no real call holds.
 */
const soup = (random: Random): Array<Observation> => {
  const legs = ["A", "B", "C", "D"]
  const methods = ["INVITE", "ACK", "BYE", "CANCEL", "PRACK", "OPTIONS", "INFO"]
  const statuses = [100, 180, 183, 200, 401, 486, 487, 503]
  const cseq = (): number | undefined => (chance(random, 0.3) ? undefined : 1 + Math.floor(random() * 3))
  const length = 1 + Math.floor(random() * 48)
  return Array.from({ length }, (): Observation => {
    const leg = pick(random, legs)
    const emitted = chance(random, 0.5)
    const kind = random()
    if (kind < 0.03) return { leg, emitted }
    if (kind < 0.5) {
      return request(leg, emitted, pick(random, methods), { dialogCreating: chance(random, 0.4), cseq: cseq() })
    }
    return response(leg, emitted, pick(random, statuses), pick(random, methods.slice(0, 5)), {
      cseq: cseq(),
      sdp: pick(random, SDP_PRESENCE)
    })
  })
}

/**
 * A call's messages as a relaying system exchanges them: received messages in
 * order, retransmitted now and then, and the system's relays held back a
 * random while, so they cross later arrivals.
 */
class CallScript {
  readonly messages: Array<Observation> = []
  private readonly held: Array<Observation> = []
  private readonly cseqs = new Map<string, number>()

  constructor(private readonly random: Random) {}

  /** The next CSeq number on a leg, written on a message or left out as a document does. */
  cseq(leg: string): number {
    const next = (this.cseqs.get(leg) ?? 0) + 1
    this.cseqs.set(leg, next)
    return next
  }

  shown(n: number): number | undefined {
    return chance(this.random, 0.7) ? n : undefined
  }

  now(o: Observation): void {
    this.messages.push(o)
    if (chance(this.random, 0.05)) this.messages.push(o)
    if (this.held.length > 0 && chance(this.random, 0.5)) this.messages.push(this.held.shift()!)
  }

  later(o: Observation): void {
    this.held.push(o)
  }

  release(): void {
    this.messages.push(...this.held.splice(0))
  }
}

/** One attempt's early exchanges: a challenge, provisionals relayed or absorbed, their PRACKs. */
const ringing = (script: CallScript, random: Random, leg: string): void => {
  let cseq = script.cseq(leg)
  script.now(request(leg, true, "INVITE", { dialogCreating: true, cseq }))
  if (chance(random, 0.5)) script.now(response(leg, false, 100, "INVITE", { cseq: script.shown(cseq) }))
  if (chance(random, 0.2)) {
    script.now(response(leg, false, pick(random, [401, 407]), "INVITE", { cseq: script.shown(cseq) }))
    script.now(request(leg, true, "ACK", { cseq }))
    cseq = script.cseq(leg)
    script.now(request(leg, true, "INVITE", { dialogCreating: true, cseq }))
  }
  const provisionals = Math.floor(random() * 4)
  for (let i = 0; i < provisionals; i++) {
    const status = pick(random, [180, 183])
    const sdp = pick(random, SDP_PRESENCE)
    script.now(response(leg, false, status, "INVITE", { cseq: script.shown(cseq), sdp }))
    if (chance(random, 0.25)) continue
    const relayedSdp = chance(random, 0.1) ? pick(random, SDP_PRESENCE) : sdp
    script.later(response("A", true, status, "INVITE", { sdp: relayedSdp }))
    if (status === 183 && chance(random, 0.4)) {
      script.release()
      const prack = script.cseq("A")
      script.now(request("A", false, "PRACK", { cseq: prack }))
      const relayed = script.cseq(leg)
      script.later(request(leg, true, "PRACK", { cseq: relayed }))
      script.release()
      script.now(response(leg, false, 200, "PRACK", { cseq: script.shown(relayed) }))
      script.later(response("A", true, 200, "PRACK", { cseq: script.shown(prack) }))
    }
  }
}

/** The caller gives up: its CANCEL crosses into the attempt, which ends with a 487. */
const cancelled = (script: CallScript, random: Random, leg: string): void => {
  script.now(request("A", false, "CANCEL", { dialogCreating: true }))
  script.now(response("A", true, 200, "CANCEL"))
  script.later(request(leg, true, "CANCEL", { dialogCreating: true }))
  script.release()
  script.now(response(leg, false, 200, "CANCEL"))
  if (chance(random, 0.2)) script.now(response(leg, false, 200, "INVITE", { sdp: true }))
  else script.now(response(leg, false, 487, "INVITE"))
  script.now(request(leg, true, "ACK"))
  script.later(response("A", true, 487, "INVITE"))
  script.release()
  script.now(request("A", false, "ACK"))
}

/** One in-dialog exchange between the caller's leg and the answering one. */
const exchange = (script: CallScript, random: Random, answered: string, kind: number): void => {
  const [from, to] = chance(random, 0.5) ? ["A", answered] : [answered, "A"]
  if (kind === 0) {
    const cseq = script.cseq(from)
    script.now(request(from, true, "OPTIONS", { cseq }))
    script.now(response(from, false, 200, "OPTIONS", { cseq: script.shown(cseq) }))
  } else if (kind === 1) {
    const cseq = script.cseq(from)
    script.now(request(from, false, "OPTIONS", { cseq }))
    script.now(response(from, true, 200, "OPTIONS", { cseq: script.shown(cseq) }))
  } else if (kind === 2) {
    const asked = script.cseq(from)
    const relayed = script.cseq(to)
    script.now(request(from, false, "INVITE", { cseq: asked }))
    script.later(request(to, true, "INVITE", { cseq: relayed }))
    script.release()
    script.now(response(to, false, 200, "INVITE", { cseq: script.shown(relayed), sdp: true }))
    script.later(response(from, true, 200, "INVITE", { cseq: script.shown(asked), sdp: true }))
    script.release()
    script.now(request(from, false, "ACK", { cseq: asked }))
    script.now(request(to, true, "ACK", { cseq: relayed }))
  } else {
    const asked = script.cseq(from)
    const relayed = script.cseq(to)
    script.now(request(from, false, "INFO", { cseq: asked }))
    if (chance(random, 0.2)) script.now(request(to, false, "INFO", { cseq: script.cseq(to) }))
    script.later(request(to, true, "INFO", { cseq: relayed }))
    script.now(response(to, false, 200, "INFO", { cseq: script.shown(relayed) }))
    script.later(response(from, true, 200, "INFO", { cseq: script.shown(asked) }))
  }
}

/** A BYE from one side, answered locally and relayed to the other. */
const hangUp = (script: CallScript, random: Random, answered: string): void => {
  const [from, to] = chance(random, 0.5) ? ["A", answered] : [answered, "A"]
  const cseq = script.cseq(from)
  script.now(request(from, false, "BYE", { cseq }))
  script.now(response(from, true, 200, "BYE", { cseq: script.shown(cseq) }))
  const relayed = script.cseq(to)
  script.now(request(to, true, "BYE", { cseq: relayed }))
  script.release()
  script.now(response(to, false, 200, "BYE", { cseq: script.shown(relayed) }))
}

/**
 * A call from leg A hunted over up to three legs, perhaps with a media
 * resource answered first: challenges, provisionals, a CANCEL, failures
 * relayed or consumed by the hunt, then `rounds` in-dialog exchanges and a BYE.
 */
const call = (random: Random, rounds: number) => {
  const script = new CallScript(random)
  script.now(request("A", false, "INVITE", { dialogCreating: true, cseq: script.cseq("A") }))
  if (chance(random, 0.7)) script.now(response("A", true, 100, "INVITE"))
  const resource = chance(random, 0.2)
  if (resource) {
    script.now(request("M", true, "INVITE", { dialogCreating: true }))
    script.now(response("M", false, 200, "INVITE", { sdp: true }))
    script.now(request("M", true, "ACK"))
  }
  const attempts = ["B", "C", "D"].slice(0, 1 + Math.floor(random() * 3))
  for (const [k, leg] of attempts.entries()) {
    ringing(script, random, leg)
    if (resource && k === 0) {
      script.now(request("M", true, "BYE"))
      script.now(response("M", false, 200, "BYE"))
    }
    if (chance(random, 0.1)) {
      cancelled(script, random, leg)
      return { messages: script.messages }
    }
    const last = k === attempts.length - 1
    const status = last ? pick(random, [200, 200, 486, 603]) : pick(random, [486, 480, 503, 408])
    script.now(response(leg, false, status, "INVITE", { sdp: status === 200 }))
    if (status !== 200) {
      script.now(request(leg, true, "ACK"))
      if (!last) continue
      script.later(response("A", true, status, "INVITE"))
      script.release()
      script.now(request("A", false, "ACK"))
      return { messages: script.messages }
    }
    script.later(response("A", true, 200, "INVITE", { sdp: true }))
    script.release()
    script.now(request("A", false, "ACK"))
    script.now(request(leg, true, "ACK"))
    for (let round = 0; round < rounds; round++) exchange(script, random, leg, Math.floor(random() * 4))
    hangUp(script, random, leg)
  }
  script.release()
  return { messages: script.messages }
}

const callerOfFor = (random: Random) => {
  const named = new Map(["A", "B", "C", "D", "M"].map((leg) => [leg, chance(random, 0.9) ? "A" : pick(random, ["B", "C"])]))
  return (leg: string): string | undefined => (chance(random, 0.05) ? undefined : named.get(leg))
}

/** Places `messages` both ways, the caller legs drawn once per call. */
const bothWays = (messages: ReadonlyArray<Observation>, random: Random) => {
  const callerOf = callerOfFor(random)
  const callers = new Map<string, string | undefined>()
  const stable = (leg: string): string | undefined => {
    if (!callers.has(leg)) callers.set(leg, callerOf(leg))
    return callers.get(leg)
  }
  return { forward: place(messages, stable), reference: placeReference(messages, stable) }
}

describe("the situation reading's forward pass", () => {
  it("places every message of independent random messages as the literal rules do", () => {
    for (let seed = 1; seed <= 20000; seed++) {
      const random = seeded(seed)
      const { forward, reference } = bothWays(soup(random), random)
      expect({ seed, placed: forward }).toEqual({ seed, placed: reference })
    }
  })

  it("places every message of generated calls as the literal rules do", () => {
    for (let seed = 1; seed <= 3000; seed++) {
      const random = seeded(seed)
      const { messages } = call(random, Math.floor(random() * 12))
      const { forward, reference } = bothWays(messages, random)
      expect({ seed, placed: forward }).toEqual({ seed, placed: reference })
    }
  })

  it("pairs a long run of identical provisionals, each relayed as it arrives, in arrival order", () => {
    const run = 300
    const messages: Array<Observation> = [
      request("A", false, "INVITE", { dialogCreating: true }),
      request("B", true, "INVITE", { dialogCreating: true })
    ]
    for (let i = 0; i < run; i++) {
      messages.push(response("B", false, 183, "INVITE", { sdp: i % 3 !== 0 }))
      messages.push(response("A", true, 183, "INVITE", { sdp: i % 3 !== 0 }))
    }
    const forward = place(messages, () => "A")
    expect(forward).toEqual(placeReference(messages, () => "A"))
    expect(forward[messages.length - 1]).toEqual({ situation: "P18", pair: messages.length - 2 })
  })
})

/**
 * A call from A answered by B, then `rounds` exchanges in turn: the system's
 * keepalive toward A, A's keepalive answered locally, a re-INVITE from A and an
 * INFO from B relayed, and a BYE from B. Every BYE, keepalive answer and late
 * relay sits far from the messages a backward scan would reach first.
 */
const answeredCall = (rounds: number): Array<Observation> => {
  const messages: Array<Observation> = [
    request("A", false, "INVITE", { dialogCreating: true }),
    request("B", true, "INVITE", { dialogCreating: true }),
    response("B", false, 180, "INVITE"),
    response("A", true, 180, "INVITE"),
    response("B", false, 200, "INVITE", { sdp: true }),
    response("A", true, 200, "INVITE", { sdp: true }),
    request("A", false, "ACK"),
    request("B", true, "ACK")
  ]
  for (let round = 0; round < rounds; round++) {
    messages.push(
      request("A", true, "OPTIONS"),
      response("A", false, 200, "OPTIONS"),
      request("A", false, "OPTIONS"),
      response("A", true, 200, "OPTIONS"),
      request("A", false, "INVITE"),
      request("B", true, "INVITE"),
      response("B", false, 200, "INVITE", { sdp: true }),
      response("A", true, 200, "INVITE", { sdp: true }),
      request("A", false, "ACK"),
      request("B", true, "ACK"),
      request("B", false, "INFO"),
      request("A", true, "INFO"),
      response("A", false, 200, "INFO"),
      response("B", true, 200, "INFO")
    )
  }
  messages.push(
    request("B", false, "BYE"),
    response("B", true, 200, "BYE"),
    request("A", true, "BYE"),
    response("A", false, 200, "BYE")
  )
  return messages
}

describe("the situation reading's cost", () => {
  it("places a long answered call as the literal rules do", () => {
    const messages = answeredCall(50)
    expect(place(messages, () => "A")).toEqual(placeReference(messages, () => "A"))
  })

  it("visits twice the messages at most 2.2 times as often", () => {
    const visitsOf = (rounds: number) => placeCounted(answeredCall(rounds), () => "A").visits
    expect(visitsOf(8000)).toBeLessThanOrEqual(2.2 * visitsOf(4000))
  })
})
