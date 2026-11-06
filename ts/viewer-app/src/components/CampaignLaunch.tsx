/**
 * Campaign runs started from its pages: the whole campaign on the lanes
 * picked (the header's control), or one case or sub-case on one lane or one
 * group of lanes (the case drawer's), each with the system's data rebuilt
 * first or not. Only the lanes the server runs are offered; the others show
 * disabled with their reason, and a pick of two groups is refused before any
 * job starts (`campaign-pick.ts`). A whole-campaign start offers the case tags
 * its lanes exclude, unchecked (`campaign-tags.ts`); one case always runs,
 * showing what its tags warn of. A start answers its job, linked to its page,
 * or the server's refusal.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type CampaignCase, type JobStarted, type LaunchLane, type LaunchTag } from "../api.ts"
import { defaultPick, groups, pickRefusal } from "../campaign-pick.ts"
import { asked as askedTags, carriedBy, offered } from "../campaign-tags.ts"
import { JobLinks } from "./JobLink.tsx"

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

/** What a campaign start asks beside its lanes: one case and sub-case, or every case. */
export interface Scope {
  readonly case?: string
  readonly subcase?: string
}

type Outcome = { readonly jobs: readonly JobStarted[] } | { readonly error: string } | undefined

const useStart = (capture: string) => {
  const [busy, setBusy] = useState(false)
  const [outcome, setOutcome] = useState<Outcome>(undefined)
  const start = async (lanes: readonly string[], scope: Scope, rebuild: boolean, allow: readonly string[] = []): Promise<void> => {
    setBusy(true)
    setOutcome(undefined)
    try {
      setOutcome(await api.jobStart({ kind: "campaign", capture, lanes, ...scope, rebuild, ...(allow.length === 0 ? {} : { allow }) }))
    } catch (error) {
      setOutcome({ error: said(error) })
    } finally {
      setBusy(false)
    }
  }
  return { busy, outcome, start }
}

const Said = ({ outcome }: { readonly outcome: Outcome }): JSX.Element | null =>
  outcome === undefined ? null : "error" in outcome
    ? <span className="cp-nok">run: {outcome.error}</span>
    : <JobLinks jobs={outcome.jobs} />

const Rebuild = ({ on, set }: { readonly on: boolean; readonly set: (on: boolean) => void }): JSX.Element => (
  <label className="cp-check" title="rebuild the system's data from scratch before the run">
    <input type="checkbox" checked={on} onChange={(event) => set(event.target.checked)} />
    rebuild data
  </label>
)

/** The lanes refused here, one line each: the server's reason, which names the lane. */
const RefusedLanes = ({ lanes }: { readonly lanes: readonly LaunchLane[] }): JSX.Element | null => {
  const refused = lanes.filter((l) => l.refused !== null)
  return refused.length === 0 ? null : (
    <ul className="cp-refused muted">
      {refused.map((l) => <li key={l.lane}>{l.refused}</li>)}
    </ul>
  )
}

/** What the tags warn of, one line each. */
const Warnings = ({ tags }: { readonly tags: readonly LaunchTag[] }): JSX.Element | null => {
  const warned = tags.filter((tag) => tag.warning !== null)
  return warned.length === 0 ? null : (
    <ul className="cp-refused cp-nok">
      {warned.map((tag) => <li key={tag.tag}>{tag.label} · {tag.warning}</li>)}
    </ul>
  )
}

/** The whole campaign on the lanes picked; `open` shows the lanes and the run control unfolded. */
export const CampaignStart = ({ capture, lanes, tags, cases, open = false }: {
  readonly capture: string
  readonly lanes: readonly LaunchLane[]
  readonly tags: readonly LaunchTag[]
  readonly cases: readonly CampaignCase[]
  readonly open?: boolean
}): JSX.Element => {
  const [picked, setPicked] = useState<readonly string[]>(() => defaultPick(lanes))
  const [rebuild, setRebuild] = useState(false)
  const [allowed, setAllowed] = useState<readonly string[]>([])
  const { busy, outcome, start } = useStart(capture)
  const toggle = (lane: string) =>
    setPicked((now) => (now.includes(lane) ? now.filter((l) => l !== lane) : [...now, lane]))
  const allow = (tag: string) =>
    setAllowed((now) => (now.includes(tag) ? now.filter((t) => t !== tag) : [...now, tag]))
  const refusal = pickRefusal(picked, lanes)
  const offers = offered(tags, picked, cases)
  const allowing = askedTags(allowed, offers)
  return (
    <details className="cp-launch" open={open}>
      <summary>start</summary>
      <div className="cp-launch-body">
        {lanes.map((l) => (
          <label key={l.lane} className="cp-check mono" title={l.refused ?? l.group}>
            <input
              type="checkbox"
              disabled={l.refused !== null}
              checked={picked.includes(l.lane)}
              onChange={() => toggle(l.lane)}
            />
            {l.lane}
          </label>
        ))}
        <Rebuild on={rebuild} set={setRebuild} />
        {offers.map((offer) => (
          <label key={offer.tag} className="cp-check" title={`excluded on ${offer.excludedOn.join(", ")}`}>
            <input type="checkbox" checked={allowing.includes(offer.tag)} onChange={() => allow(offer.tag)} />
            {offer.label} ({offer.cases})
          </label>
        ))}
        <button type="button" disabled={busy || refusal !== null} onClick={() => void start(picked, {}, rebuild, allowing)}>
          run campaign
        </button>
        {refusal !== null && picked.length > 0 ? <span className="cp-nok">run: {refusal}</span> : <Said outcome={outcome} />}
      </div>
      <Warnings tags={offers.filter((offer) => allowing.includes(offer.tag))} />
      <RefusedLanes lanes={lanes} />
    </details>
  )
}

/** One case's sub-case, or every sub-case of the case, on one lane or on one group of lanes. */
export const CaseReplay = ({ capture, lanes, scope, tags, carried }: {
  readonly capture: string
  readonly lanes: readonly LaunchLane[]
  readonly scope: Scope
  readonly tags: readonly LaunchTag[]
  readonly carried: readonly string[]
}): JSX.Element => {
  const [rebuild, setRebuild] = useState(false)
  const [whole, setWhole] = useState(false)
  const { busy, outcome, start } = useStart(capture)
  const asked: Scope = whole && scope.case !== undefined ? { case: scope.case } : scope
  return (
    <div className="cp-replay">
      <b>Replay</b>
      {lanes.map((l) => (
        <button
          key={l.lane}
          type="button"
          className="mono"
          disabled={busy || l.refused !== null}
          title={l.refused ?? undefined}
          onClick={() => void start([l.lane], asked, rebuild)}
        >
          {l.lane}
        </button>
      ))}
      {groups(lanes).filter((group) => group.length > 1).map((group) => (
        <button key={group.join("+")} type="button" disabled={busy} onClick={() => void start(group, asked, rebuild)}>
          {group.join(" + ")}
        </button>
      ))}
      {scope.subcase !== undefined && (
        <label className="cp-check" title="every sub-case of the case">
          <input type="checkbox" checked={whole} onChange={(event) => setWhole(event.target.checked)} />
          whole case
        </label>
      )}
      <Rebuild on={rebuild} set={setRebuild} />
      <Said outcome={outcome} />
      <Warnings tags={carriedBy(tags, carried)} />
    </div>
  )
}
