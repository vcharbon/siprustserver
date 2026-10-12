/**
 * One case: its ladder, its capture source, its raw document, and the step a
 * click selects.
 *
 * An EXCLUDED entry is served here too, and honestly: the generator refused to
 * cut it, so the refusal takes the ladder's place and the affordances that
 * would act on a case — variants, runs, compare — are absent rather than
 * broken. Its capture is still readable, which is the whole point of listing it.
 */
import { buildScenarioLayout, type LadderEvent } from "@sip/viewer-core"
import { Flags } from "../components/Flags.tsx"
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { api, useAsync } from "../api.ts"
import { ExclusionPane } from "../components/ExclusionPane.tsx"
import { JsonView } from "../components/JsonView.tsx"
import { LadderPane } from "../components/LadderPane.tsx"
import { Profile } from "../components/Profile.tsx"
import { Link } from "../components/Link.tsx"
import { RunsOfCase } from "../components/Related.tsx"
import { SourcePane } from "../components/SourcePane.tsx"
import { readPivot } from "../document.ts"
import { href, navigate } from "../router.ts"

type Tab = "ladder" | "source" | "json"

/** The tab rides the fragment, so a source view can be pasted into a review. */
const tabFromHash = (): Tab => {
  const hash = window.location.hash.replace("#", "")
  return hash === "source" || hash === "json" ? hash : "ladder"
}

export const CasePage = ({ id }: { id: string }): JSX.Element => {
  const [variant, setVariant] = useState("scenario")
  const [selected, setSelected] = useState<LadderEvent | undefined>(undefined)
  const [tab, setTabState] = useState<Tab>(tabFromHash)
  const setTab = (next: Tab): void => {
    setTabState(next)
    window.history.replaceState({}, "", `${window.location.pathname}${next === "ladder" ? "" : `#${next}`}`)
  }
  const detail = useAsync(() => api.case(id, variant), `${id}#${variant}`)
  const runs = useAsync(() => api.runs(), "runs")

  // An excluded entry has no document by construction, so it is never read as
  // a pivot: "not a JSON object" would be a false complaint about a real answer.
  const read = useMemo(
    () =>
      detail.state === "ok" && detail.value.excluded === undefined
        ? readPivot(detail.value.document)
        : undefined,
    [detail]
  )
  const layout = useMemo(
    () => (read?.ok ? buildScenarioLayout(read.document) : undefined),
    [read]
  )

  if (detail.state === "loading") return <p className="page muted">loading {id}…</p>
  if (detail.state === "error") return <p className="page error">{detail.message}</p>

  const value = detail.value
  const excluded = value.excluded
  const document = read?.ok ? read.document : undefined

  return (
    <div className={`page case-page${excluded ? " case-excluded" : ""}`}>
      <header className="page-head">
        <div>
          <h1>
            {excluded && <span className="exclusion-badge">excluded</span>}
            {value.title}
          </h1>
          {excluded
            ? <p className="muted">{value.id} · {value.origin} · no case was generated</p>
            : (
              <p className="muted">
                {value.id} · {value.origin} · {value.family} · {value.variant} ·{" "}
                {Object.entries(value.lanes).map(([lane, verdict]) => `${lane}:${verdict}`).join(" ")}
              </p>
            )}
          <p className="muted path">{value.path}</p>
          {!excluded && (
            <RunsOfCase
              caseKey={value.caseKey}
              runs={runs.state === "ok" ? runs.value : undefined}
            />
          )}
        </div>
        <div className="page-actions">
          {value.variants.length > 1 && (
            <select value={variant} onChange={(event) => setVariant(event.target.value)}>
              {value.variants.map((name) => <option key={name} value={name}>{name}</option>)}
            </select>
          )}
          <button type="button" className={tab === "ladder" ? "on" : ""} onClick={() => setTab("ladder")}>
            {excluded ? "exclusion" : "ladder"}
          </button>
          <button type="button" className={tab === "source" ? "on" : ""} onClick={() => setTab("source")}>
            source
          </button>
          <button type="button" className={tab === "json" ? "on" : ""} onClick={() => setTab("json")}>
            raw json
          </button>
          {/* Nothing replayed a case that was never generated, so no compare. */}
          {!excluded && runs.state === "ok" && runs.value.length > 0 && (
            <select
              value=""
              onChange={(event) => {
                if (event.target.value !== "") {
                  navigate(href.compare({ kind: "case", id }, { kind: "run", id: event.target.value }))
                }
              }}
            >
              <option value="">compare with a run…</option>
              {runs.value.map((run) => <option key={run.id} value={run.id}>{run.runId}</option>)}
            </select>
          )}
        </div>
      </header>

      {!excluded && <Profile ledger={value.ledger} />}

      {!excluded && <Flags annotations={document?.case.annotations} />}

      {read && !read.ok && <p className="error">{read.reason} — the raw JSON tab still works.</p>}

      {tab === "source" && (
        <div className={excluded ? "source-only" : "source-grid"}>
          {!excluded && (
            <div>
              <h3>pivot ladder — the interpretation</h3>
              {layout && (
                <LadderPane
                  layout={layout}
                  selectedId={selected?.id}
                  onSelect={(event) => setSelected(event)}
                />
              )}
            </div>
          )}
          <div>
            <h3>capture source — what the wire carried</h3>
            <SourcePane id={id} variant={variant} pivot={document} />
          </div>
        </div>
      )}

      {tab !== "source" && (
      <div className="split">
        <div className="split-main">
          {tab === "ladder" && excluded && <ExclusionPane exclusion={excluded} />}
          {tab === "ladder" && !excluded && layout && (
            <LadderPane
              layout={layout}
              selectedId={selected?.id}
              onSelect={(event) => setSelected(event)}
            />
          )}
          {tab === "json" && (
            <JsonView
              value={excluded ? excluded.document : value.document}
              label={excluded ? `${value.id} (excluded.json)` : `${value.id} (${variant})`}
            />
          )}
        </div>

        <aside className="split-side">
          {excluded
            ? (
              <>
                <h3>no case</h3>
                <p className="muted">
                  This directory holds the pipeline's refusal and nothing else. Evidence is kept
                  rather than deleted, so the call can still be ruled on from the capture.
                </p>
                <p className="muted">
                  the <strong>source</strong> tab draws the whole capture, with the refused call's
                  groups selectable.
                </p>
              </>
            )
            : selected
            ? (
              <>
                <h3>{selected.id}</h3>
                <JsonView value={selected.source} label="flow node" />
              </>
            )
            : (
              <>
                <h3>case</h3>
                <JsonView value={document?.case ?? value.document} label="case block" />
                {document?.deviations && (
                  <JsonView value={document.deviations} label="deviations" />
                )}
                {document?.postconditions && (
                  <JsonView value={document.postconditions} label="postconditions" />
                )}
                {value.resources.length > 0 && (
                  <JsonView value={value.resources} label="resources" />
                )}
                <p className="muted">click a message in the ladder to inspect its step.</p>
              </>
            )}
          <p className="muted">
            <Link to={href.index()}>back to the index</Link>
          </p>
        </aside>
      </div>
      )}
    </div>
  )
}
