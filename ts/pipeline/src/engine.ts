/**
 * The CORRELATION ENGINE: calls plus rules in, joins / groups / chains /
 * ambiguities out.
 *
 * Pure, and it holds no deployment knowledge. Every constant a callflow family
 * needs — the Call-ID shapes an application server mints, the finals that
 * trigger a reroute, the windows — arrives in the rule file. Each rule kind gets
 * its own matcher below and none of them is configurable beyond the fields its
 * schema declares.
 *
 * The engine never PICKS. A call joining several candidates under the same rule
 * in the same role keeps every join and flags them all: which one is right is a
 * judgement, and a judgement made silently here would look exactly like
 * evidence.
 */
import type { Rules } from "@sip/contracts"
import type { Call, DialogRef, IdentityKind, IdentityValue } from "./call.js"

export interface Join {
  readonly rule: string
  readonly kind: Rules.Rule["kind"]
  readonly left: string
  readonly right: string
  readonly dt_ms: number
  readonly evidence: Record<string, unknown>
  ambiguous: boolean
}

export interface Group {
  readonly id: string
  readonly calls: ReadonlyArray<string>
  readonly joins: ReadonlyArray<string>
}

/** One reroute ladder: the attempts of a chain, earliest first. */
export interface Chain {
  readonly id: string
  readonly calls: ReadonlyArray<{
    readonly call: string
    readonly position: number
    readonly final_status: number | null
  }>
}

export interface Ambiguity {
  readonly rule: string
  readonly call: string
  readonly role: "left" | "right"
  readonly candidates: ReadonlyArray<{
    readonly other: string
    readonly dt_ms: number
    readonly evidence: Record<string, unknown>
  }>
}

export interface Correlation {
  readonly joins: ReadonlyArray<Join>
  readonly groups: ReadonlyArray<Group>
  readonly chains: ReadonlyArray<Chain>
  readonly ambiguities: ReadonlyArray<Ambiguity>
}

const keysOf = (re: RegExp, values: Iterable<string>): Array<{ key: string; from: string }> => {
  const out: Array<{ key: string; from: string }> = []
  for (const v of values) {
    const m = re.exec(v)
    const key = m?.groups?.key
    if (key !== undefined) out.push({ key, from: v })
  }
  return out
}

/** `"486"` matches 486; `"5xx"` matches the class. */
const statusMatches = (patterns: ReadonlyArray<string>, status: number): boolean =>
  patterns.some((p) => {
    const lower = p.toLowerCase()
    if (/^\d{3}$/.test(lower)) return Number(lower) === status
    if (/^\dxx$/.test(lower)) return Math.floor(status / 100) === Number(lower[0])
    return false
  })

const sameDialog = (a: DialogRef, b: DialogRef): boolean => {
  if (a.callId !== b.callId) return false
  const tagOk = (x?: string, y?: string) => x === undefined || y === undefined || x === y
  return tagOk(a.toTag, b.toTag) && tagOk(a.fromTag, b.fromTag)
}

type Emit = (left: Call, right: Call, dt_ms: number, evidence: Record<string, unknown>) => void

/** Union-find over a fixed key set. */
const unionFind = (keys: Iterable<string>) => {
  const parent = new Map<string, string>()
  for (const k of keys) parent.set(k, k)
  const find = (x: string): string => {
    let r = x
    while (parent.get(r) !== r) r = parent.get(r)!
    while (parent.get(x) !== r) {
      const next = parent.get(x)!
      parent.set(x, r)
      x = next
    }
    return r
  }
  return {
    find,
    union: (a: string, b: string): void => {
      const [ra, rb] = [find(a), find(b)]
      if (ra !== rb) parent.set(ra, rb)
    }
  }
}

type JoinKind = Exclude<Rules.Rule["kind"], "retry">

const MATCHERS: {
  [K in JoinKind]: (
    rule: Extract<Rules.Rule, { kind: K }>,
    calls: ReadonlyArray<Call>,
    emit: Emit
  ) => void
} = {
  /** Anchor: every Call-ID the call owns, left regex against right regex. */
  "call-id": (rule, calls, emit) => {
    const left = new RegExp(rule.left)
    const right = new RegExp(rule.right)
    for (const a of calls) {
      for (const la of keysOf(left, a.callIds)) {
        for (const b of calls) {
          if (a.id === b.id) continue
          for (const rb of keysOf(right, b.callIds)) {
            if (la.key !== rb.key) continue
            emit(a, b, b.t0_ms - a.t0_ms, {
              key: la.key,
              left_call_id: la.from,
              right_call_id: rb.from
            })
          }
        }
      }
    }
  },

  /** Anchor: the initial INVITE of each call; symmetric, emitted once per pair. */
  "header-key": (rule, calls, emit) => {
    const re = new RegExp(rule.pattern)
    const indexed = calls.flatMap((c) =>
      c.invite === undefined
        ? []
        : keysOf(re, c.invite.headers(rule.header)).map(({ key, from }) => ({
            call: c,
            key,
            value: from
          }))
    )
    for (let i = 0; i < indexed.length; i++) {
      for (let j = i + 1; j < indexed.length; j++) {
        const a = indexed[i]!
        const b = indexed[j]!
        if (a.call.id === b.call.id || a.key !== b.key) continue
        const dt = Math.abs(b.call.invite!.ts_ms - a.call.invite!.ts_ms)
        if (rule.window_ms !== undefined && dt > rule.window_ms) continue
        emit(a.call, b.call, dt, { header: rule.header, key: a.key, value: a.value })
      }
    }
  },

  /** Anchor: the right call's initial INVITE carrying a `Replaces` the left owns. */
  replaces: (_rule, calls, emit) => {
    for (const b of calls) {
      const ref = b.invite?.replaces
      if (ref === undefined) continue
      for (const a of calls) {
        if (a.id === b.id || !a.dialogs.some((d) => sameDialog(ref, d))) continue
        emit(a, b, b.invite!.ts_ms - a.t0_ms, {
          replaces_call_id: ref.callId,
          to_tag: ref.toTag ?? null,
          from_tag: ref.fromTag ?? null
        })
      }
    }
  },

  /** Anchor: a REFER on the left call → a right call reaching the `Refer-To` target. */
  refer: (rule, calls, emit) => {
    for (const a of calls) {
      for (const ref of a.referrals) {
        if (ref.target === undefined) continue
        for (const b of calls) {
          if (a.id === b.id || b.invite === undefined) continue
          const reached = b.invite.ruriUser === ref.target || b.invite.toUser === ref.target
          if (!reached) continue
          const dt = b.invite.ts_ms - ref.ts_ms
          if (dt < 0 || dt > rule.window_ms) continue
          emit(a, b, dt, {
            refer_to: ref.target,
            via: b.invite.ruriUser === ref.target ? "ruri" : "to",
            attended: ref.replaces !== undefined
          })
        }
      }
    }
  }
}

type Retry = Extract<Rules.Rule, { kind: "retry" }>

/** Which value matched, and the INVITE each side read it from. */
interface IdentityMatch {
  readonly value: string
  readonly left: IdentityValue
  readonly right: IdentityValue
}

interface Edge {
  readonly rule: Retry
  readonly left: Call
  readonly right: Call
  readonly dt_ms: number
  readonly matched: Record<string, IdentityMatch>
}

/**
 * Identities are SETS, so two calls match when the sets intersect. Both sides
 * are in timestamp order, so the reported pair is the earliest-matching one —
 * the initial INVITE whenever it is the one that matches.
 */
const intersect = (a: Call, b: Call, which: IdentityKind): IdentityMatch | undefined => {
  for (const left of a.identities[which]) {
    for (const right of b.identities[which]) {
      if (left.value === right.value) return { value: left.value, left, right }
    }
  }
  return undefined
}

/**
 * Chain phase: each qualifying terminal failure claims the NEXT attempt only —
 * the earliest INVITE starting after it within the window. A ladder of N
 * attempts is N-1 ordered edges, never N² pairs; that order is what
 * `calls[].attempts` consumes. Candidates equally "next" (the same INVITE
 * instant) are all kept, and the generic ambiguity pass flags them.
 */
const chainEdges = (
  rule: Retry,
  calls: ReadonlyArray<Call>,
  sameGroup: (a: string, b: string) => boolean
): Array<Edge> => {
  const edges: Array<Edge> = []
  for (const a of calls) {
    if (a.final === undefined || !statusMatches(rule.finals, a.final.status)) continue
    const candidates: Array<{
      right: Call
      dt_ms: number
      matched: Record<string, IdentityMatch>
    }> = []
    for (const b of calls) {
      if (a.id === b.id || b.invite === undefined) continue
      const dt = b.invite.ts_ms - a.final.ts_ms
      if (dt < 0 || dt > rule.window_ms) continue
      // Membership: same group always qualifies (the match-less, primary form);
      // `match` additionally admits calls that no other rule related.
      const matched: Record<string, IdentityMatch> = {}
      let byIdentity = rule.match !== undefined && rule.match.length > 0
      for (const which of rule.match ?? []) {
        const hit = intersect(a, b, which)
        if (hit === undefined) {
          byIdentity = false
          break
        }
        matched[which] = hit
      }
      if (!byIdentity && !sameGroup(a.id, b.id)) continue
      candidates.push({ right: b, dt_ms: dt, matched })
    }
    if (candidates.length === 0) continue
    const next = Math.min(...candidates.map((c) => c.right.invite!.ts_ms))
    for (const c of candidates) {
      if (c.right.invite!.ts_ms === next) {
        edges.push({ rule, left: a, right: c.right, dt_ms: c.dt_ms, matched: c.matched })
      }
    }
  }
  return edges
}

/**
 * Attempt order over the chain edges of ALL retry rules at once: a ladder whose
 * cause differs per hop (486 then 503) is one chain, not one chain per rule.
 * Position is the longest path from a chain head, so a tie branch cannot leave
 * an attempt sharing a position with its own successor.
 */
const orderChains = (edges: ReadonlyArray<Edge>, calls: ReadonlyArray<Call>) => {
  const members = new Set(edges.flatMap((e) => [e.left.id, e.right.id]))
  const uf = unionFind(members)
  for (const e of edges) uf.union(e.left.id, e.right.id)

  const position = new Map<string, number>([...members].map((m) => [m, 0]))
  // Edges run forward in time, so relaxing at most |edges| times settles.
  for (let pass = 0; pass < edges.length; pass++) {
    let moved = false
    for (const e of edges) {
      const want = position.get(e.left.id)! + 1
      if (want > position.get(e.right.id)!) {
        position.set(e.right.id, want)
        moved = true
      }
    }
    if (!moved) break
  }

  const byRoot = new Map<string, Array<string>>()
  for (const m of members) {
    const r = uf.find(m)
    byRoot.set(r, [...(byRoot.get(r) ?? []), m])
  }
  const finalOf = new Map(calls.map((c) => [c.id, c.final?.status ?? null]))
  const chainId = new Map<string, string>()
  const chains: Array<Chain> = [...byRoot.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([root, ms], i) => {
      chainId.set(root, `chain${i}`)
      return {
        id: `chain${i}`,
        calls: ms
          .sort((x, y) => position.get(x)! - position.get(y)!)
          .map((call) => ({
            call,
            position: position.get(call)!,
            final_status: finalOf.get(call) ?? null
          }))
      }
    })
  return { chainOf: (call: string) => chainId.get(uf.find(call))!, chains, position }
}

export const correlate = (
  calls: ReadonlyArray<Call>,
  rules: ReadonlyArray<Rules.Rule>
): Correlation => {
  const joins: Array<Join> = []
  const seen = new Set<string>()
  const push = (
    rule: Rules.Rule,
    left: string,
    right: string,
    dt_ms: number,
    evidence: Record<string, unknown>
  ): void => {
    // One join per (rule, unordered pair): a symmetric matcher that finds the
    // same pair from both ends must not double-count it into an ambiguity.
    const dedupe = `${rule.name} ${[left, right].sort().join("|")}`
    if (seen.has(dedupe)) return
    seen.add(dedupe)
    joins.push({ rule: rule.name, kind: rule.kind, left, right, dt_ms, evidence, ambiguous: false })
  }

  // Phase 1 — join. Every kind but `retry` relates calls; the union-find closure
  // over the result is the membership the chain phase reads.
  for (const rule of rules) {
    if (rule.kind === "retry") continue
    const matcher = MATCHERS[rule.kind] as (
      r: Rules.Rule,
      c: ReadonlyArray<Call>,
      e: Emit
    ) => void
    matcher(rule, calls, (left, right, dt_ms, evidence) =>
      push(rule, left.id, right.id, dt_ms, evidence)
    )
  }
  const membership = unionFind(calls.map((c) => c.id))
  for (const j of joins) membership.union(j.left, j.right)

  // Phase 2 — chain. `retry` pairs each terminal failure with the next attempt;
  // a rule carrying `match` may reach outside its group, and that join feeds
  // membership back through the closure below.
  const edges = rules.flatMap((r) =>
    r.kind === "retry"
      ? chainEdges(r, calls, (a, b) => membership.find(a) === membership.find(b))
      : []
  )
  const { chainOf, chains, position } = orderChains(edges, calls)
  for (const e of edges) {
    push(e.rule, e.left.id, e.right.id, e.dt_ms, {
      status: e.left.final!.status,
      matched: e.matched,
      chain: chainOf(e.left.id),
      from_position: position.get(e.left.id)!,
      to_position: position.get(e.right.id)!
    })
  }

  // Ambiguity: one call joining several candidates under the SAME rule in the
  // SAME role. Every join is kept and flagged; the engine never picks.
  const ambiguities: Array<Ambiguity> = []
  for (const role of ["left", "right"] as const) {
    const byAnchor = new Map<string, Array<Join>>()
    for (const j of joins) {
      const k = `${j.rule} ${j[role]}`
      byAnchor.set(k, [...(byAnchor.get(k) ?? []), j])
    }
    for (const [k, group] of byAnchor) {
      if (group.length < 2) continue
      for (const j of group) j.ambiguous = true
      const space = k.lastIndexOf(" ")
      ambiguities.push({
        rule: k.slice(0, space),
        call: k.slice(space + 1),
        role,
        candidates: group.map((j) => ({
          other: role === "left" ? j.right : j.left,
          dt_ms: j.dt_ms,
          evidence: j.evidence
        }))
      })
    }
  }

  // Groups: union-find closure over ALL joins, ambiguous and chain ones included.
  const closure = unionFind(calls.map((c) => c.id))
  for (const j of joins) closure.union(j.left, j.right)
  const buckets = new Map<string, Array<string>>()
  for (const c of calls) {
    const r = closure.find(c.id)
    buckets.set(r, [...(buckets.get(r) ?? []), c.id])
  }
  const groups: Array<Group> = [...buckets.values()]
    .sort((a, b) => a[0]!.localeCompare(b[0]!))
    .map((members, i) => ({
      id: `case${i}`,
      calls: members,
      joins: joins
        .filter((j) => members.includes(j.left) && members.includes(j.right))
        .map((j) => `${j.rule}:${j.left}->${j.right}`)
    }))

  return { joins, groups, chains, ambiguities }
}
