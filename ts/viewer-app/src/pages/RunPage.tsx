/** One run bundle: what actually crossed the wire, plus the verdict beside it. */
import type { LadderEvent } from "@sip/viewer-core"
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { api, useAsync } from "../api.ts"
import { JsonView } from "../components/JsonView.tsx"
import { LadderPane } from "../components/LadderPane.tsx"
import { Link } from "../components/Link.tsx"
import { CaseOfRun } from "../components/Related.tsx"
import { SipMessageView } from "../components/SipMessageView.tsx"
import { VerdictPane } from "../components/VerdictPane.tsx"
import { href, navigate } from "../router.ts"
import { loadSide } from "../side.ts"

export const RunPage = ({ id }: { id: string }): JSX.Element => {
  const [selected, setSelected] = useState<LadderEvent | undefined>(undefined)
  const [tab, setTab] = useState<"ladder" | "json">("ladder")
  const side = useAsync(() => loadSide({ kind: "run", id }), `run:${id}`)
  const cases = useAsync(() => api.cases(), "cases")

  const detail = side.state === "ok" ? side.value.detail : undefined
  const run = useMemo(
    () => (detail && "recording" in detail ? detail : undefined),
    [detail]
  )

  if (side.state === "loading") return <p className="page muted">loading {id}…</p>
  if (side.state === "error") return <p className="page error">{side.message}</p>

  return (
    <div className="page run-page">
      <header className="page-head">
        <div>
          <h1>run {run?.runId}</h1>
          <p className="muted">{id} · legs {run?.legs.join(", ")}</p>
          <p className="muted path">{run?.path}</p>
          <CaseOfRun
            caseKey={run?.caseKey}
            cases={cases.state === "ok" ? cases.value : undefined}
          />
        </div>
        <div className="page-actions">
          <button type="button" className={tab === "ladder" ? "on" : ""} onClick={() => setTab("ladder")}>
            ladder
          </button>
          <button type="button" className={tab === "json" ? "on" : ""} onClick={() => setTab("json")}>
            raw json
          </button>
          {cases.state === "ok" && (
            <select
              value=""
              onChange={(event) => {
                if (event.target.value !== "") {
                  navigate(href.compare({ kind: "case", id: event.target.value }, { kind: "run", id }))
                }
              }}
            >
              <option value="">compare with a case…</option>
              {cases.value.map((entry) => (
                <option key={entry.id} value={entry.id}>{entry.id}</option>
              ))}
            </select>
          )}
        </div>
      </header>

      <div className="split">
        <div className="split-main">
          {tab === "ladder" && side.value.layout && (
            <LadderPane
              layout={side.value.layout}
              selectedId={selected?.id}
              onSelect={(event) => setSelected(event)}
            />
          )}
          {tab === "json" && run && (
            <>
              <JsonView value={run.verdict} label="verdict.json" />
              <JsonView value={run.timing} label="timing.json" />
              <JsonView value={run.runConfig} label="run-config.json" />
              <JsonView value={run.recording} label="recording/*.jsonl (verbatim)" />
            </>
          )}
        </div>

        <aside className="split-side">
          {selected
            ? (
              <>
                <h3>{selected.id}</h3>
                <SipMessageView record={selected.source} label="recorded message" />
              </>
            )
            : (
              <>
                <h3>verdict</h3>
                <VerdictPane verdict={run?.verdict} />
                <JsonView value={run?.verdict} label="verdict.json" />
                <JsonView value={run?.timing} label="timing.json" />
                <p className="muted">click a message in the ladder to inspect the recorded line.</p>
              </>
            )}
          <p className="muted">
            <Link to={href.index()}>back to the index</Link>
          </p>
        </aside>
      </div>
    </div>
  )
}
