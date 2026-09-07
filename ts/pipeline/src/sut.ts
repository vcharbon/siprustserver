/**
 * The SUT's own address set: which addresses of a capture ARE the system under
 * test, and how that is decided.
 *
 * A capture's SUT is the address that RECEIVED the call — the ingress. The
 * general form is a per-IP address SET, because an aSBC answers on several
 * addresses; the default detection finds the singleton that ingress is.
 * Everything outside the set is a peer, whatever subnet it sits in, and the
 * profile's CIDR list is only the candidate filter that says which addresses
 * could be ours at all.
 *
 * A stated set always wins over detection, and both are recorded so a reader can
 * see which decided. WHERE the statements live is the caller's: this module
 * takes a file path or an already-decoded document, never a repository layout.
 */
import { Flows } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import { ipOf, type SutProfile } from "./profile.js"

/** The addresses that are the system under test; ports never distinguish. */
export class SutSet {
  private readonly ips: ReadonlySet<string>

  constructor(addresses: ReadonlyArray<string>) {
    this.ips = new Set(addresses.map(ipOf))
  }

  /** Whether `sock` (`ip` or `ip:port`) is one of the SUT's own addresses. */
  has(sock: string): boolean {
    return this.ips.has(ipOf(sock))
  }

  get addresses(): ReadonlyArray<string> {
    return [...this.ips].sort()
  }

  get size(): number {
    return this.ips.size
  }
}

/**
 * Which side of the captured deployment an endpoint sits on. `unattributed` is
 * the honest answer where no SUT set was decided at all — never a default side.
 */
export type ChargedSide = "platform" | "peer" | "unattributed"

/**
 * The side `sut` places `sock` on, by the same per-IP test every other consumer
 * of the set uses. The SUT SET is the one authority: an address in it is the
 * platform, every other address is a peer, and a set that is absent or empty
 * attributes nothing.
 */
export const chargedSide = (sut: SutSet | undefined, sock: string): ChargedSide =>
  sut === undefined || sut.size === 0 ? "unattributed" : sut.has(sock) ? "platform" : "peer"

/** How this capture's SUT set was arrived at, in words the case carries. */
export interface SutDecision {
  readonly sut: SutSet
  /** Whether a human stated the set, rather than detection finding it. */
  readonly stated: boolean
  readonly evidence: string
  /** Every other in-profile address that also took a dialog-creating INVITE. */
  readonly runnersUp: ReadonlyArray<string>
}

/**
 * Per-capture SUT statements: the escape hatch for a deployment whose platform
 * answers on more than one address, where no single ingress describes it.
 */
export const StatedSut = Schema.Struct({
  $comment: Schema.optionalKey(Schema.String),
  captures: Schema.Record(
    Schema.String,
    Schema.Struct({
      addresses: Schema.NonEmptyArray(Schema.String),
      reason: Schema.String
    })
  )
})
export interface StatedSut extends Schema.Schema.Type<typeof StatedSut> {}

export class StatedSutLoadError extends Schema.TaggedError<StatedSutLoadError>()(
  "Pipeline.StatedSutLoadError",
  { file: Schema.String, reason: Schema.String }
) {}

const decode = Schema.decodeUnknownEffect(StatedSut, {
  onExcessProperty: "error",
  errors: "all"
})

/** Read a statements file. A checkout that keeps none states nothing. */
export const loadStatedSut = Effect.fn("Pipeline.loadStatedSut")(function* (file: string) {
  const exists = yield* Effect.sync(() => fs.existsSync(file))
  if (!exists) return { captures: {} } as StatedSut
  const text = yield* Effect.try({
    try: () => fs.readFileSync(file, "utf8"),
    catch: (cause) => new StatedSutLoadError({ file, reason: `unreadable: ${String(cause)}` })
  })
  const json = yield* Effect.try({
    try: () => JSON.parse(text) as unknown,
    catch: (cause) => new StatedSutLoadError({ file, reason: `not JSON: ${String(cause)}` })
  })
  return yield* decode(json).pipe(
    Effect.mapError((issue) => new StatedSutLoadError({ file, reason: String(issue) }))
  )
})

/** An INVITE that opens a dialog: no To tag, and not a repeat of one. */
export const isDialogCreatingInvite = (m: Flows.Msg): boolean =>
  Flows.isInvite(m) && !m.retx && m.summary.to.tag === null

interface Candidate {
  readonly ip: string
  readonly firstAt_us: number
  readonly invites: number
}

/** Every in-profile address that TOOK a dialog-creating INVITE, ingress first. */
export const sutCandidates = (
  flows: Flows.FlowsDoc,
  profile: SutProfile
): ReadonlyArray<Candidate> => {
  const seen = new Map<string, { firstAt_us: number; invites: number }>()
  for (const leg of flows.legs) {
    for (const m of leg.msgs) {
      if (!isDialogCreatingInvite(m)) continue
      const ip = ipOf(m.dst)
      if (!profile.matches(ip)) continue
      const prev = seen.get(ip)
      if (prev === undefined) {
        seen.set(ip, { firstAt_us: m.ts_us, invites: 1 })
      } else {
        prev.invites += 1
        prev.firstAt_us = Math.min(prev.firstAt_us, m.ts_us)
      }
    }
  }
  return [...seen]
    .map(([ip, v]) => ({ ip, ...v }))
    .sort((a, b) => a.firstAt_us - b.firstAt_us || (a.ip < b.ip ? -1 : 1))
}

/**
 * This capture's SUT address set. A statement wins; otherwise the ingress is the
 * in-profile address that took the EARLIEST dialog-creating INVITE, and every
 * other candidate is named so a wrong pick is visible rather than silent.
 */
export const decideSut = (
  flows: Flows.FlowsDoc,
  profile: SutProfile,
  capture: string,
  stated?: StatedSut
): SutDecision => {
  const statement = stated?.captures[capture]
  if (statement !== undefined) {
    return {
      sut: new SutSet(statement.addresses),
      stated: true,
      evidence: `stated for ${capture}: ${statement.addresses.join(", ")} — ${statement.reason}`,
      runnersUp: []
    }
  }
  const candidates = sutCandidates(flows, profile)
  const first = candidates[0]
  if (first === undefined) {
    return {
      sut: new SutSet([]),
      stated: false,
      evidence:
        `no address in the cut profile took a dialog-creating INVITE in ${capture}: ` +
        `nothing here is the system under test`,
      runnersUp: []
    }
  }
  return {
    sut: new SutSet([first.ip]),
    stated: false,
    evidence:
      `detected: ${first.ip} took the capture's earliest dialog-creating INVITE ` +
      `(${first.invites} in all)`,
    runnersUp: candidates.slice(1).map((c) => `${c.ip} (${c.invites} INVITE(s) taken)`)
  }
}
