/**
 * One run set's report: the `sip.run-report/1` document the server serves for
 * the URL's selector (`/report?run=@run:N`), drawn. A selector stated in the
 * path (`/report/@run:N`) is read as the query form. A cited run whose row is
 * gone reads the summary its citation kept.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, useAsync } from "../api.ts"
import { RunReportView } from "../components/RunReportView.tsx"
import { href, navigate, redirect, useQuery } from "../router.ts"

export const ReportPage = ({ run: inPath }: { run?: string }): JSX.Element => {
  const query = useQuery()
  useEffect(() => {
    if (inPath !== undefined) redirect(href.report({ run: inPath, lane: query.get("lane") ?? undefined }))
  }, [inPath])
  const run = inPath ?? query.get("run") ?? ""
  const lane = query.get("lane") ?? ""
  const [typed, setTyped] = useState(run)
  useEffect(() => setTyped(run), [run])
  const doc = useAsync(
    () => (run === "" ? Promise.resolve(undefined) : api.report(run, lane === "" ? undefined : lane)),
    `${run}|${lane}`
  )
  return (
    <div className="page report-page">
      <header className="page-head">
        <div>
          <h1>report</h1>
          <p className="muted">{run === "" ? "one run set: how its calls ended" : run}</p>
        </div>
        {run !== "" && (
          <div className="page-actions">
            <a href={`/api/report${window.location.search}`}>the JSON</a>
          </div>
        )}
      </header>
      <form
        className="selector-form"
        onSubmit={(event) => {
          event.preventDefault()
          navigate(href.report({ run: typed, lane: lane === "" ? undefined : lane }))
        }}
      >
        <label>
          run set <input value={typed} placeholder="@run:N" onChange={(event) => setTyped(event.target.value)} />
        </label>
        <button type="submit">report</button>
      </form>
      {run === ""
        ? <p className="muted">name a run set: <code>@run:N</code>, <code>@sweep:N</code>, <code>@env:X</code> …</p>
        : doc.state === "loading"
        ? <p className="muted">reading…</p>
        : doc.state === "error"
        ? <p className="error">refused: {doc.message}</p>
        : doc.value === undefined
        ? null
        : <RunReportView doc={doc.value} />}
    </div>
  )
}
