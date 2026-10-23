/**
 * Two run sets compared call by call: the `sip.compare/1` document the server
 * serves for the URL's selectors (`/compare?a=@develop&b=@env:X`), drawn.
 * Selectors stated in the path (`/compare/@develop/@env:X`) are read as the
 * query form, which the page then shows in the address bar. `ladderChanged=1`
 * lists only the calls whose two replay ladders differ, whatever their class.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, useAsync } from "../api.ts"
import { CompareReportView } from "../components/CompareReportView.tsx"
import { href, navigate, redirect, useQuery } from "../router.ts"

const SelectorForm = (
  { a, b, lane, all, ladderChanged }: { a: string; b: string; lane: string; all: boolean; ladderChanged: boolean }
): JSX.Element => {
  const [before, setBefore] = useState(a)
  const [after, setAfter] = useState(b)
  const [onLane, setOnLane] = useState(lane)
  const [every, setEvery] = useState(all)
  const [moved, setMoved] = useState(ladderChanged)
  return (
    <form
      className="selector-form"
      onSubmit={(event) => {
        event.preventDefault()
        navigate(href.compareRuns({
          a: before,
          b: after,
          lane: onLane,
          all: every ? "1" : undefined,
          ladderChanged: moved ? "1" : undefined
        }))
      }}
    >
      <label>
        before <input value={before} placeholder="@develop" onChange={(event) => setBefore(event.target.value)} />
      </label>
      <label>
        after <input value={after} placeholder="@env:dev-…" onChange={(event) => setAfter(event.target.value)} />
      </label>
      <label>
        lane <input value={onLane} placeholder="the server's" onChange={(event) => setOnLane(event.target.value)} />
      </label>
      <label>
        <input type="checkbox" checked={every} onChange={() => setEvery(!every)} /> unchanged calls too
      </label>
      <label>
        <input type="checkbox" checked={moved} onChange={() => setMoved(!moved)} /> only calls whose ladders differ
      </label>
      <button type="submit">compare</button>
      <span className="muted">
        selectors: <code>@develop</code>, <code>@env:X</code>, <code>@run:N</code>, <code>@sha:X</code>,{" "}
        <code>@lane:L</code>, <code>@sweep:N</code>, <code>@job:N</code>, joined by <code>/</code>
      </span>
    </form>
  )
}

export const RunComparePage = ({ path }: { path?: { readonly a: string; readonly b: string } }): JSX.Element => {
  const query = useQuery()
  useEffect(() => {
    if (path === undefined) return
    redirect(href.compareRuns({
      a: path.a,
      b: path.b,
      lane: query.get("lane") ?? undefined,
      all: query.get("all") ?? undefined,
      ladderChanged: query.get("ladderChanged") ?? undefined
    }))
  }, [path?.a, path?.b])
  const a = path?.a ?? query.get("a") ?? ""
  const b = path?.b ?? query.get("b") ?? ""
  const lane = query.get("lane") ?? ""
  const all = query.get("all") === "1"
  const ladderChanged = query.get("ladderChanged") === "1"
  const asked = a !== "" && b !== ""
  const doc = useAsync(
    () =>
      asked
        ? api.compare({ a, b, lane: lane === "" ? undefined : lane, all, ladderChanged })
        : Promise.resolve(undefined),
    `${a}|${b}|${lane}|${all}|${ladderChanged}`
  )
  return (
    <div className="page compare-runs-page">
      <header className="page-head">
        <div>
          <h1>compare</h1>
          <p className="muted">{asked ? <>{a} → {b}</> : "two run sets, call by call"}</p>
        </div>
        {asked && (
          <div className="page-actions">
            <a href={`/api/compare${window.location.search}`}>the JSON</a>
          </div>
        )}
      </header>
      <SelectorForm
        key={`${a}|${b}|${lane}|${all}|${ladderChanged}`}
        a={a}
        b={b}
        lane={lane}
        all={all}
        ladderChanged={ladderChanged}
      />
      {!asked
        ? <p className="muted">name both sides to compare them.</p>
        : doc.state === "loading"
        ? <p className="muted">comparing…</p>
        : doc.state === "error"
        ? <p className="error">refused: {doc.message}</p>
        : doc.value === undefined
        ? null
        : <CompareReportView doc={doc.value} />}
    </div>
  )
}
