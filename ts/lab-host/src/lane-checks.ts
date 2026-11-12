/**
 * What must be TRUE on a box before a lane is driven, asked before the first
 * cell rather than discovered in a thousand identical red rows: the runner's
 * own checks, over the lane's typed view (`@sip/rig` `lane-view.ts`). Every
 * refusal carries what fixes it. Only the lane asked for is checked: a box
 * that runs one lane owes nothing to the others.
 *
 * - a lane with no harness on its SUT's routing path (`backend.mode` `none`)
 *   runs no cell here, forced or not;
 * - a `standing` SIP target must take SIP on its ingress, a `managed` one must
 *   find it free (the harness stands its worker there);
 * - the backend address the batch serves must be free, the container the
 *   pack's hints name as its likely holder named in the refusal.
 *
 * The pack's guard answers first ({@link Checks.guard}), so a check of a port
 * another lab holds never names that lab's container as the fix.
 */
import type { LaneView } from "@sip/rig"
import { spawnSync } from "node:child_process"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"

/** What a hosted lane's parts name as the fix of a refusal. */
export interface Hints {
  /** What brings the lane's SIP target up; the target's description where unstated. */
  readonly up?: string | undefined
  /** The name filter of the container likely to hold the backend address. */
  readonly holder?: string | undefined
}

/** What a pack adds to the box's own checks. */
export interface Checks<L extends LaneView.View> {
  /** The fixes `lane`'s parts name. */
  readonly hints: (lane: Extract<L, { readonly kind: "hosted" }>) => Hints
  /** Why `lane` is another lab's under `env`, asked first, or `null`. */
  readonly guard: (lane: L, env: NodeJS.ProcessEnv) => Effect.Effect<string | null, never, FileSystem.FileSystem>
}

/** The address:port pairs something is listening on, by transport. */
const listeners = (flag: string): ReadonlySet<string> => {
  const r = spawnSync("ss", [flag, "-n"], { encoding: "utf8" })
  const held = new Set<string>()
  if (r.status !== 0 || typeof r.stdout !== "string") return held
  for (const line of r.stdout.split("\n").slice(1)) {
    const local = localAddressOf(line.trim().split(/\s+/))
    if (local !== undefined) held.add(local)
  }
  return held
}

/**
 * The Local Address:Port column of one `ss -l` row: the first column after
 * the state that ends in a port, UDP and TCP putting it in different places.
 */
const localAddressOf = (columns: ReadonlyArray<string>): string | undefined =>
  columns.find((column, at) => at > 0 && /:\d+$/.test(column))

/** Whether `address` is taken, a wildcard bind on its port included. */
const isBound = (held: ReadonlySet<string>, address: string): boolean => {
  const port = portOf(address)
  for (const one of held) {
    if (portOf(one) !== port) continue
    const host = one.slice(0, one.lastIndexOf(":"))
    if (one === address || host === "0.0.0.0" || host === "*" || host === "[::]") return true
  }
  return false
}

const portOf = (address: string): string => address.slice(address.lastIndexOf(":") + 1)

const udp = (): ReadonlySet<string> => listeners("-lun")
const tcp = (): ReadonlySet<string> => listeners("-ltn")

/** The running container whose name matches `filter`, where docker names one. */
const container = (filter: string): string | null => {
  const r = spawnSync("docker", ["ps", "--filter", `name=${filter}`, "--format", "{{.Names}}"], {
    encoding: "utf8"
  })
  const names = String(r.stdout ?? "").trim()
  return r.status === 0 && names !== "" ? names : null
}

/** Why no cell runs on `lane`, whose harness is not on its SUT's routing path. */
export const noRoutingPath = (lane: LaneView.Hosted): string =>
  `${lane.id} has no harness on its SUT's routing path (backend.mode = ${lane.backendMode} in ${lane.file}): ` +
  "no cell can run against it here, forced or not"

/** Why `lane`'s SIP target and backend address are not as the lane needs them, or `null`. */
const hostedRefusal = Effect.fn("LaneChecks.hosted")(function*(lane: LaneView.Hosted, hints: Hints) {
  const sip = lane.sip
  if (lane.backendListen === undefined) return noRoutingPath(lane)
  const listening = isBound(yield* Effect.sync(udp), sip.ingress)
  if (sip.lifecycle === "standing" && !listening) {
    return `nothing takes SIP on ${sip.ingress}: the SIP target ${sip.name} is down — ` +
      (hints.up ?? `bring ${sip.description} up`)
  }
  if (sip.lifecycle === "managed" && listening) {
    return `${sip.ingress} is taken and this lane stands its own worker there — ` +
      `stop the process holding it (\`ss -lunp | grep ${portOf(sip.ingress)}\`), or wait for the replay that owns it`
  }
  const backend = lane.backendListen
  if (isBound(yield* Effect.sync(tcp), backend)) {
    const holder = hints.holder
    const held = holder === undefined ? null : yield* Effect.sync(() => container(holder))
    return `${backend} is taken and the batch process serves the backend there` +
      (held === null
        ? ` — stop whatever holds it (\`ss -ltnp | grep ${portOf(backend)}\`)`
        : ` — the compose container ${held} holds it: \`docker stop ${held}\``)
  }
  return null
})

/**
 * Why this lane cannot be driven on this box right now, or `null`. The
 * simulated lane builds its SUT inside the cell and binds nothing, so it is
 * always ready; a hosted lane is first the pack's guard's.
 */
export const refusal = <L extends LaneView.View>(
  checks: Checks<L>,
  lane: L,
  env: NodeJS.ProcessEnv = process.env
): Effect.Effect<string | null, never, FileSystem.FileSystem> =>
  lane.kind === "simulated" ? Effect.succeed(null) : Effect.gen(function*() {
    const foreign = yield* checks.guard(lane, env)
    return foreign ?? (yield* hostedRefusal(lane, checks.hints(lane as Extract<L, { readonly kind: "hosted" }>)))
  })
