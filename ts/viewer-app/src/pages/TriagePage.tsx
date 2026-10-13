/**
 * The triage queue, per CALL: one row per case of the standing run of each
 * capture on the lane, plus one per call the standing cut refused, each with
 * its MAIN ERROR — the one line the row is picked on. Every row's ref is
 * copyable as the deployment's triage command (`commands.ts`), where it states one.
 *
 * The filter bar and the chips are the whole page's question — which LANE
 * (each is a separate SUT and answers only for itself; the browser remembers
 * the last one picked), which scope, which captures, and per chip group (rule,
 * expected message, refusal, outcome) one value to show and any number to hide
 * — and they live in the URL (`queue-filter.ts`), so a slice of the queue is a
 * link. The server applies them, because a lane read whole is thousands of
 * green calls; the chip counts it returns are taken over the scope, the
 * capture and the hidden values, never over the includes — so a chip's count
 * is what including it shows while no other group has an include, and an
 * excluded chip counts the calls it hides. A chip cycles none → include →
 * exclude → none on click. The last served table stays on screen through a
 * refetch.
 *
 * The queue is also where a replay is asked for: the filter bar, the ticked
 * rows or their captures whole become a request through the replay control
 * (`RequestBuilder`), and each row's button replays that one call.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, type TriageCallRow, type TriageFacets, type TriageQueue } from "../api.ts"
import { Age } from "../components/Age.tsx"
import { DropCaptures } from "../components/DropCaptures.tsx"
import { Link } from "../components/Link.tsx"
import { ReplayButton } from "../components/ReplayButton.tsx"
import { RequestBuilder } from "../components/RequestBuilder.tsx"
import { useHeld } from "../held.ts"
import { laneOf, rememberLane } from "../lane-memory.ts"
import { useCommands } from "../commands.ts"
import { launchRef } from "../launch.ts"
import { callStudyOf, captureStudyOf, deltasOf } from "../queue-links.ts"
import {
  type ChipFilter,
  type ChipState,
  cycle,
  filterKey,
  filterParams,
  type Group,
  readFilter,
  select,
  stateOf
} from "../queue-filter.ts"
import { alsoBadBody } from "../bad-body.ts"
import { rowKey } from "../request-builder.ts"
import { setQuery, useQuery } from "../router.ts"

/**
 * Every scope the server knows, in the order the picker lists them. Two
 * audiences read the queue: a FAILED call is a B2B finding for a SIP reader,
 * and a call the cut REFUSED is pipeline work that reader never opens — so
 * the queue opens on the failures alone, and the refusals are a pick away.
 */
const SCOPES = [
  ["failed", "failed: the SUT's failing calls"],
  ["refused", "refused by the cut (cut work, not SIP)"],
  ["red", "red: failed and refused together"],
  ["all", "every call, green included"],
  ["passed", "passed"],
  ["skipped", "skipped"]
] as const

const DEFAULT_SCOPE = "failed"

/**
 * A value that settles before it is used, and a `flush` that settles it now.
 * Typing a capture name must not put a query per keystroke on a table of
 * thousands; Enter sends what is typed without the wait.
 */
const useSettled = <A,>(value: A, delay: number): readonly [A, () => void] => {
  const [settled, setSettled] = useState(value)
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(value), delay)
    return () => window.clearTimeout(timer)
  }, [value, delay])
  return [settled, () => setSettled(value)]
}

/** The row's launch ref: a case row hands over its one call, a refused row the capture. */
const CopyRef = ({ row, lane }: { row: TriageCallRow; lane?: string }): JSX.Element | null => {
  const [copied, setCopied] = useState(false)
  const command = useCommands().triage
  if (command === undefined) return null
  const ref = launchRef(command, row.capture, lane, row.kind === "case" ? row.caseId : undefined)
  return (
    <button
      type="button"
      className="copy-ref"
      title={`copy "${ref}"`}
      onClick={() => {
        void navigator.clipboard.writeText(ref).then(() => {
          setCopied(true)
          window.setTimeout(() => setCopied(false), 1200)
        })
      }}
    >
      {copied ? "copied" : "copy ref"}
    </button>
  )
}

/** `capture-case1` under `capture.pcap.gz` reads as `…-case1`. */
const shortCase = (row: TriageCallRow): string => row.caseId.replace(row.capture, "…")

/** A green or skipped row is nobody's triage work; it must not read as a red one. */
const toneOf = (row: TriageCallRow): string =>
  row.kind === "refused" ? "call-refused" : row.skipped !== null ? "call-skipped" : row.passed ? "status-passing" : "call-failed"

/**
 * The main error as a chip, which SETS the matching filter: a charged rule sets
 * `rule`, a died call sets `expected`, a failure sets `rule` to its tag, a
 * refused CALL sets `refusal` to its first reason — the rest of its reasons
 * follow as tags. A case the interpreter refused is a case: its chip sets
 * `rule`, where the chip row counts it. A passed, skipped or unmeasured call
 * has no chip. A call that carries a failing body row its main error does not
 * name shows `+ bad body` beside it, which sets `rule` to `bad-body`.
 */
const MainError = (
  { row, onChip }: { row: TriageCallRow; onChip: (group: Group, value: string) => void }
): JSX.Element => (
  <>
    <MainErrorChip row={row} onChip={onChip} />
    {row.kind === "case" && alsoBadBody(row.mainError, row.bodyFailing) && (
      <button
        type="button"
        className="tag main-error error-bad-body also"
        title={`${row.bodyFailing} body row(s) no rule accepts\n\nclick: show every call with a bad body`}
        onClick={() => onChip("rule", "bad-body")}
      >
        + bad body
      </button>
    )}
  </>
)

const MainErrorChip = (
  { row, onChip }: { row: TriageCallRow; onChip: (group: Group, value: string) => void }
): JSX.Element => {
  if (row.kind === "case" && row.skipped !== null) {
    return <span className="tag muted" title={row.skipped}>skipped</span>
  }
  if (row.kind === "case" && row.passed) return <span className="tag status-passing">passed</span>
  const error = row.mainError
  if (error === null) return <span className="tag muted" title="this run predates the measure">not measured</span>
  const chip = (target: Group, value: string | null, label: JSX.Element): JSX.Element =>
    value === null
      ? <span className={`tag main-error error-${error.kind}`} title={error.detail}>{label}</span>
      : (
        <button
          type="button"
          className={`tag main-error error-${error.kind}`}
          title={`${error.detail}\n\nclick: show every call with this ${target}`}
          onClick={() => onChip(target, value)}
        >
          {label}
        </button>
      )
  switch (error.kind) {
    case "rfc":
      return chip(
        "rule",
        error.rule,
        <>{error.rule ?? "rfc"}{error.rfc !== null && <span className="cite"> · {error.rfc}</span>}</>
      )
    case "harness-fault":
      return chip(
        "rule",
        error.rule,
        <>harness fault · {error.rule ?? "rfc"}{error.rfc !== null && <span className="cite"> · {error.rfc}</span>}</>
      )
    case "died":
      return chip(
        "expected",
        error.expected,
        <>
          died{error.step !== null && <> at {error.step}</>}
          {error.expected !== null && <span className="cite"> · expected {error.expected}</span>}
        </>
      )
    case "failure":
      return chip(
        "rule",
        error.rule,
        <>{error.rule ?? "failed"}{error.step !== null && <span className="cite"> at {error.step}</span>}</>
      )
    case "bad-body":
      return chip(
        "rule",
        error.rule,
        <>bad body{error.step !== null && <span className="cite"> at {error.step}</span>}</>
      )
    case "refused":
      return (
        <>
          {chip(row.kind === "case" ? "rule" : "refusal", error.rule, <>{error.rule ?? "refused"}</>)}
          {row.kind === "refused" && row.reasons.slice(1).map((reason) => (
            <span key={reason} className="tag error-refused also" title="also refused under this reason">
              {reason}
            </span>
          ))}
        </>
      )
  }
}

const QueueTable = (
  { rows, lane, ticked, onTick, onTickAll, onChip }: {
    rows: readonly TriageCallRow[]
    /** Carried into every study link, so a row opens on the lane it was read on. */
    lane: string
    /** The rows ticked for a request, by `rowKey`. */
    ticked: ReadonlySet<string>
    onTick: (row: TriageCallRow) => void
    /** Tick every row on screen, or none where every one is. */
    onTickAll: () => void
    onChip: (group: Group, value: string) => void
  }
): JSX.Element => (
  <table className="listing triage-listing">
    <thead>
      <tr>
        <th className="tick">
          <input
            type="checkbox"
            title="tick every row on screen"
            checked={rows.length > 0 && rows.every((row) => ticked.has(rowKey(row)))}
            onChange={onTickAll}
          />
        </th>
        <th>capture</th>
        <th>call</th>
        <th>outcome</th>
        <th>main error</th>
        <th>run</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {rows.map((row) => (
        <tr key={rowKey(row)} className={toneOf(row)}>
          <td className="tick">
            <input type="checkbox" checked={ticked.has(rowKey(row))} onChange={() => onTick(row)} />
          </td>
          {/* Two pages: the capture's study, and the study of this one call. */}
          <td className="mono break">
            <Link to={captureStudyOf(row, lane)}>{row.capture}</Link>
          </td>
          <td className="mono call-cell">
            <Link to={callStudyOf(row, lane)}>{shortCase(row)}</Link>
            {row.dialled !== null && <div className="muted">{row.dialled}</div>}
            {row.family !== null && (
              <div className="muted">
                {row.family}
                {row.legs !== null && <> · {row.legs} leg{row.legs === 1 ? "" : "s"}</>}
                {row.attempts !== null && <> · {row.attempts} attempt{row.attempts === 1 ? "" : "s"}</>}
              </div>
            )}
            {row.features.length > 0 && (
              <div className="row-features">
                {row.features.map((feature) => (
                  <button
                    type="button"
                    key={feature}
                    className="facet"
                    title={`show only the calls with ${feature}`}
                    onClick={() => onChip("feature", feature)}
                  >
                    {feature}
                  </button>
                ))}
              </div>
            )}
          </td>
          <td className="mono">
            {row.kind === "refused"
              ? <span className="tag error-refused">refused</span>
              : row.skipped !== null
              ? <span className="tag muted" title={row.skipped}>skipped</span>
              : row.outcome ?? <span className="muted">—</span>}
          </td>
          <td className="queue-why">
            <MainError row={row} onChip={onChip} />
          </td>
          <td className="muted mono">
            {row.kind === "refused"
              ? `cut ${row.prepId ?? "?"}`
              : `run${row.runId ?? "?"} ${row.setKind ?? ""}` + (row.bundle ? "" : " (evidence cleaned)")}
            {/* When it ran: what says whether a re-run has landed yet. */}
            {row.finishedAt !== null && <div><Age at={row.finishedAt} /></div>}
          </td>
          <td className="row-actions">
            <CopyRef row={row} lane={lane} />
            {row.kind === "case" && <ReplayButton capture={row.capture} lane={lane} caseId={row.caseId} />}
          </td>
        </tr>
      ))}
    </tbody>
  </table>
)

/**
 * What the page is showing, and out of what. `matched` is the filter's own
 * count and `facets.captures` the scope's, so neither the limit nor a chip can
 * make the queue look smaller than it is.
 */
const Counts = ({ value }: { value: TriageQueue }): JSX.Element => (
  <p className="muted">
    showing <b>{value.rows.length}</b> of <b>{value.matched}</b> matching call(s) on lane{" "}
    <code>{value.lane}</code>
    {value.matched > value.rows.length && <> — raise the row cap ({value.limit}) to see the rest</>}
    {" · "}
    {value.facets.captures} capture(s) in scope <code>{value.scope}</code>
  </p>
)

/** What a click on a chip in each state does, as its title says. */
const CHIP_TITLE: Readonly<Record<ChipState, (value: string, count: number) => string>> = {
  none: (value, count) => `show the ${count} call(s) on ${value} — click again to hide them`,
  include: () => "included — click to hide these calls instead",
  exclude: () => "excluded — click to clear"
}

/** One chip: its value, its count over the population, and what it is to the filter. */
const FacetChip = (
  { value, count, cite, state, onToggle }: {
    value: string
    count: number
    cite?: string | null
    state: ChipState
    onToggle: () => void
  }
): JSX.Element => (
  <button
    type="button"
    className={`facet${state === "include" ? " active" : state === "exclude" ? " excluded" : ""}`}
    title={CHIP_TITLE[state](value, count)}
    onClick={onToggle}
  >
    {value}
    {cite != null && <span className="cite"> · {cite}</span>}
    <span className="count">{count}</span>
  </button>
)

/**
 * The chip row: rules, failures, expected messages, refusals, outcomes, each
 * `value (count)` — an excluded one counting the calls it hides. Rules and
 * failures both set the `rule` key — one is the charged RFC rule, the other
 * any other failure tag. An active chip, included or excluded, stays visible
 * even when the server lists no count for it, so a filter can always be seen
 * and cleared: it is pinned at zero into its own group, and an active `rule`
 * neither group counts is pinned into the rules alone.
 */
const FacetRow = (
  { facets, filter, onToggle }: {
    facets: TriageFacets
    filter: ChipFilter
    onToggle: (group: Group, value: string) => void
  }
): JSX.Element | null => {
  const group = (
    key: Group,
    label: string,
    counts: readonly { value: string; count: number; rfc?: string | null }[],
    /** The groups whose listing satisfies an active chip; the first of them pins it. */
    siblings: readonly (readonly { value: string }[])[] = [counts]
  ): JSX.Element | null => {
    const active = [...new Set([filter.include[key], ...filter.exclude[key]])].filter((value) => value !== "")
    const pinned = siblings[0] === counts
      ? active.filter((value) => !siblings.some((one) => one.some((entry) => entry.value === value)))
      : []
    const listed = [...pinned.map((value) => ({ value, count: 0, rfc: null })), ...counts]
    if (listed.length === 0) return null
    return (
      <div className="facet-group" key={label}>
        <span className="facet-label">{label}</span>
        {listed.map((one) => (
          <FacetChip
            key={one.value}
            value={one.value}
            count={one.count}
            cite={one.rfc}
            state={stateOf(filter, key, one.value)}
            onToggle={() => onToggle(key, one.value)}
          />
        ))}
      </div>
    )
  }
  const ruleGroups = [facets.rules, facets.failures]
  const groups = [
    group("rule", "rules", facets.rules, ruleGroups),
    group("rule", "failures", facets.failures, ruleGroups),
    group("expected", "expected", facets.expected),
    group("refusal", "refusals", facets.refusals),
    group("outcome", "outcome", facets.outcomes),
    group("feature", "features", facets.features)
  ].filter((one) => one !== null)
  return groups.length === 0 ? null : <div className="facets">{groups}</div>
}

/**
 * The page's question, as controls. Top-level, not nested in the page: a
 * component redefined per render remounts the search box and eats the
 * keystroke that caused the render. The page keeps it mounted through a
 * fetch for the same reason.
 */
const Filters = (
  { lanes, lane, scope, needle, limit, onLane, onScope, onNeedle, onSearch, onLimit }: {
    lanes: TriageQueue["lanes"]
    lane: string
    scope: string
    needle: string
    limit: string
    onLane: (value: string) => void
    onScope: (value: string) => void
    onNeedle: (value: string) => void
    /** Enter in the capture box: send the needle now rather than after the settle. */
    onSearch: () => void
    onLimit: (value: string) => void
  }
): JSX.Element => (
  <div className="page-actions queue-filters">
    <label>
      lane{" "}
      <select value={lane} onChange={(event) => onLane(event.target.value)}>
        <option value="">configured lane</option>
        {lanes.map((entry) => (
          <option key={entry.lane} value={entry.lane}>
            {entry.lane} ({entry.captures})
          </option>
        ))}
      </select>
    </label>
    <label>
      calls{" "}
      <select value={scope} onChange={(event) => onScope(event.target.value)}>
        {SCOPES.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
      </select>
    </label>
    <label>
      capture{" "}
      <input
        type="search"
        value={needle}
        placeholder="name contains…"
        onChange={(event) => onNeedle(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") onSearch()
        }}
      />
    </label>
    <label>
      cap{" "}
      <input
        type="number"
        min={1}
        className="row-cap"
        value={limit}
        placeholder="300"
        onChange={(event) => onLimit(event.target.value)}
      />
    </label>
  </div>
)

export const TriagePage = (): JSX.Element => {
  const initial = useQuery()
  const [lane, setLane] = useState(() => laneOf(initial))
  const [scope, setScope] = useState(() => initial.get("scope") ?? DEFAULT_SCOPE)
  const [needle, setNeedle] = useState(() => initial.get("q") ?? "")
  const [filter, setFilter] = useState<ChipFilter>(() => readFilter(initial))
  const [limit, setLimit] = useState(() => initial.get("limit") ?? "")
  const [refresh, setRefresh] = useState(0)
  const [ticked, setTicked] = useState<ReadonlyMap<string, TriageCallRow>>(new Map())
  const [settled, search] = useSettled(needle, 250)

  // The filter IS the URL: this slice of the queue is a link like any other view.
  useEffect(() => {
    setQuery({ lane, scope, q: needle, ...filterParams(filter), limit })
  }, [lane, scope, needle, filter, limit])
  // The lane outlives the page: the next lane-less URL opens on it.
  useEffect(() => rememberLane(lane), [lane])

  // The last served table is held through a refetch: blanking the page would
  // unmount the search box mid-word and lose the keystrokes typed meanwhile.
  const queue = useHeld(
    () =>
      api.triageQueue({
        lane,
        scope,
        q: settled,
        ...filterParams(filter),
        limit: limit === "" ? undefined : Number(limit)
      }),
    `triage-queue#${lane}#${scope}#${settled}#${filterKey(filter)}#${limit}#${refresh}`
  )
  const onFresh = (): void => setRefresh((n) => n + 1)

  /** A tick keeps the row itself: a request of ticked rows outlives the page's next fetch. */
  const tick = (row: TriageCallRow): void =>
    setTicked((held) => {
      const next = new Map(held)
      if (next.has(rowKey(row))) next.delete(rowKey(row))
      else next.set(rowKey(row), row)
      return next
    })
  const tickAll = (rows: readonly TriageCallRow[]): void =>
    setTicked((held) => {
      const every = rows.length > 0 && rows.every((row) => held.has(rowKey(row)))
      const next = new Map(held)
      for (const row of rows) {
        if (every) next.delete(rowKey(row))
        else next.set(rowKey(row), row)
      }
      return next
    })

  /** A chip click on the row SETS its include; on the chip row it CYCLES the chip. */
  const setChip = (group: Group, value: string): void => setFilter((held) => select(held, group, value))
  const toggleChip = (group: Group, value: string): void => setFilter((held) => cycle(held, group, value))

  const value = queue.current
  if (value === undefined) {
    if (queue.error !== undefined) return <p className="page error">{queue.error}</p>
    return <p className="page muted">loading the triage queue…</p>
  }
  if (!value.available) {
    return (
      <div className="page">
        <h1>triage queue</h1>
        <p className="muted">not available — {value.detail}</p>
      </div>
    )
  }

  return (
    <div className="page triage-page">
      <header className="page-head">
        <div>
          <h1>triage queue</h1>
          {queue.loading && <div className="counts"><span className="tag">reloading…</span></div>}
          {queue.error !== undefined && <p className="error">{queue.error}</p>}
          {value.detail !== "" && <p className="error">{value.detail}</p>}
        </div>
        <Filters
          lanes={value.lanes}
          lane={lane}
          scope={scope}
          needle={needle}
          limit={limit}
          onLane={setLane}
          onScope={setScope}
          onNeedle={setNeedle}
          onSearch={search}
          onLimit={setLimit}
        />
      </header>

      <FacetRow facets={value.facets} filter={filter} onToggle={toggleChip} />

      <Counts value={value} />
      {/* Built from the page's own state: a held answer may be of the slice before the last pick. */}
      <p className="muted">
        <Link to={deltasOf({ lane: lane === "" ? value.lane ?? "" : lane, scope, q: settled, filter })}>
          delta sheet of this slice
        </Link>{" "}
        — every header, body and shape difference of its calls, one line per signature with samples
      </p>

      <RequestBuilder
        lane={value.lane ?? ""}
        lanes={value.lanes.map((entry) => entry.lane)}
        scope={scope}
        q={settled}
        filter={filter}
        ticked={[...ticked.values()]}
        shown={value.rows}
      />

      <DropCaptures
        lane={value.lane}
        lanes={value.lanes.map((entry) => entry.lane)}
        onFresh={onFresh}
      />

      <QueueTable
        rows={value.rows}
        lane={value.lane ?? ""}
        ticked={new Set(ticked.keys())}
        onTick={tick}
        onTickAll={() => tickAll(value.rows)}
        onChip={setChip}
      />
    </div>
  )
}
