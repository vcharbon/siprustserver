/**
 * A `sip.compare/1` document, drawn: the headline, both sides, the class
 * counts, what one side alone holds, then one row per listed call — both
 * cells' outcomes and repeats, the links to each cell, and the before, after
 * and reference ladders side by side as the text export renders them. A call
 * whose repeats within one side disagree is classed `unstable`.
 *
 * It draws the document and derives nothing: what the page says is what
 * `compare A B --json` printed.
 */
import type { CompareReport } from "@sip/contracts"
import type { JSX } from "react"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/** A viewer link the document carries, as a path on this viewer: the page is wherever it is served. */
export const localPath = (url: string): string => {
  try {
    const parsed = new URL(url)
    return `${parsed.pathname}${parsed.search}`
  } catch {
    return url
  }
}

const short = (sha: string | null): string => (sha === null ? "unknown" : sha.slice(0, 10))

const list = (values: ReadonlyArray<string | null>): string =>
  values.length === 0 ? "—" : values.map((value) => value ?? "unknown").join(", ")

/** How a cell ended, in the words the text export uses. */
export const outcomeOf = (cell: CompareReport.Cell): string => {
  if (cell.skipped !== null) return `skipped ${cell.skipped}`
  const at = cell.failedStep === null ? "" : ` at ${cell.failedStep}`
  if (!cell.passed) return `failed${at}`
  return cell.verdict === "ok" || cell.verdict === "ok-negative" ? "passed" : `passed · verdict ${cell.verdict}${at}`
}

/** The tone a cell's outcome is drawn in. */
export const toneOf = (cell: CompareReport.Cell): string =>
  cell.skipped !== null ? "muted" : cell.passed ? "matched" : "missing"

/** Repeats beyond the cell itself, as `repeats P/N passed`; empty for a cell run once. */
export const repeatsOf = (cell: CompareReport.Cell): string =>
  cell.repeats.runs <= 1 ? "" : `repeats ${cell.repeats.passed}/${cell.repeats.runs} passed`

export const errorOf = (cell: CompareReport.Cell): string => {
  const error = cell.mainError
  if (error === null) return ""
  return `${error.kind}${error.rule === null ? "" : ` ${error.rule}`}` +
    (error.step === null ? "" : ` at ${error.step}`) +
    (error.expected === null ? "" : ` waiting ${error.expected}`)
}

const SideTable = ({ doc }: { doc: CompareReport.CompareReport }): JSX.Element => {
  const sides = [["before", doc.before], ["after", doc.after]] as const
  const rows: ReadonlyArray<readonly [string, (side: CompareReport.Side) => JSX.Element | string]> = [
    ["selector", (side) => <Link to={href.report({ run: side.selector })}>{side.selector}</Link>],
    ["runs · calls", (side) => `${side.runs} run(s) · ${side.cells} call(s)`],
    ["env", (side) => list(side.envs)],
    ["branch", (side) => list(side.branches)],
    [
      "code",
      (side) =>
        side.components.length === 0
          ? "unknown"
          : side.components.map((one) => `${one.name} ${one.shas.map(short).join(", ")}`).join("; ")
    ],
    ["lane", (side) => list(side.lanes)],
    [
      "sweeps",
      (side) =>
        side.sweeps.length === 0
          ? "—"
          : side.sweeps.map((sweep) => `${sweep.id} (${sweep.selection ?? `--${sweep.universe}`})`).join(", ")
    ]
  ]
  return (
    <table className="listing compare-sides">
      <thead>
        <tr>
          <th></th>
          {sides.map(([label]) => <th key={label}>{label}</th>)}
        </tr>
      </thead>
      <tbody>
        {rows.map(([label, cell]) => (
          <tr key={label}>
            <th>{label}</th>
            {sides.map(([side, value]) => <td key={side}>{cell(value)}</td>)}
          </tr>
        ))}
      </tbody>
    </table>
  )
}

const ClassCounts = ({ doc }: { doc: CompareReport.CompareReport }): JSX.Element => {
  const classes = (Object.keys(doc.counts) as ReadonlyArray<CompareReport.MoveClass>).filter((one) => {
    const count = doc.counts[one]
    return count.sameDocument + count.documentChanged > 0
  })
  return (
    <table className="listing compare-counts">
      <thead>
        <tr>
          <th>class</th>
          <th>calls</th>
          <th>same document</th>
          <th>document changed</th>
        </tr>
      </thead>
      <tbody>
        {classes.length === 0 && (
          <tr>
            <td colSpan={4} className="muted">no call both sides ran</td>
          </tr>
        )}
        {classes.map((one) => {
          const count = doc.counts[one]
          return (
            <tr key={one} className={`move-${one === "unchanged" ? "unchanged" : "moved"}`}>
              <td>
                <span className="tag">{one}</span>
              </td>
              <td>{count.sameDocument + count.documentChanged}</td>
              <td>{count.sameDocument}</td>
              <td>{count.documentChanged}</td>
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}

const LadderText = ({ title, lines }: { title: string; lines: ReadonlyArray<string> | null }): JSX.Element => (
  <div className="compare-ladder">
    <h4>{title}</h4>
    {lines === null
      ? <p className="muted">not read: the bundle is gone, the store holds no document, or the call is past the ladder budget</p>
      : <pre className="ladder-text">{lines.join("\n")}</pre>}
  </div>
)

const CellLine = ({ label, cell }: { label: string; cell: CompareReport.Cell }): JSX.Element => (
  <p className="compare-cell">
    <span className="muted">{label}</span>{" "}
    <a href={localPath(cell.links.text)}>run{cell.run}</a>{" "}
    <span className="muted">({cell.env ?? "unknown"}@{cell.branch ?? "unknown"})</span>{" "}
    <span className={`tag ${toneOf(cell)}`}>{outcomeOf(cell)}</span>
    {errorOf(cell) !== "" && <> <code>{errorOf(cell)}</code></>}
    {repeatsOf(cell) !== "" && <> <span className="tag repeats">{repeatsOf(cell)}</span></>}
    {" "}<span className="tag">{cell.bless}</span>
    {cell.accepted !== null && <span className="muted"> · accepted {cell.accepted}</span>}
    {cell.knownBugs !== null && cell.knownBugs.length > 0 && (
      <span className="muted">
        {" "}· known-bug {cell.knownBugs.map((bug) => `${bug.rule}${bug.ticket === null ? "" : ` (${bug.ticket})`}`).join(", ")}
      </span>
    )}
    {" "}<Link to={localPath(cell.links.study)}>study</Link>
  </p>
)

const CallRow = ({ call }: { call: CompareReport.Call }): JSX.Element => (
  <details className="compare-call">
    <summary>
      <span className="tag">{call.class}</span> <code>{call.caseId}</code>{" "}
      <span className={`tag ${toneOf(call.before)}`}>{outcomeOf(call.before)}</span> →{" "}
      <span className={`tag ${toneOf(call.after)}`}>{outcomeOf(call.after)}</span>{" "}
      <span className="muted">
        document {call.document} · code {call.code} · lane {call.lane}
      </span>
    </summary>
    <CellLine label="before" cell={call.before} />
    <CellLine label="after" cell={call.after} />
    <div className="compare-ladders">
      <LadderText title={`before · run${call.before.run}`} lines={call.before.ladder} />
      <LadderText title={`after · run${call.after.run}`} lines={call.after.ladder} />
      <LadderText title="reference" lines={call.ref} />
    </div>
  </details>
)

const Only = ({ label, only }: { label: string; only: CompareReport.Only }): JSX.Element => (
  <p className="muted">
    {label}: {only.captures} capture(s) the other side never ran; {only.count} call(s) within the captures both ran
    {only.calls.length > 0 && (
      <>
        {" "}— <code>{only.calls.slice(0, 10).join(", ")}</code>
        {only.calls.length > 10 && ` … ${only.calls.length - 10} more listed in the JSON`}
      </>
    )}
  </p>
)

export const CompareReportView = ({ doc }: { doc: CompareReport.CompareReport }): JSX.Element => (
  <div className="compare-report">
    <p className="compare-headline">
      <strong>{doc.headline}</strong> · {doc.compared} call(s) both ran
      {doc.unexplained > 0 && (
        <span className="tag missing">
          {doc.unexplained} verdict flip(s) on the same code, document and lane: the harness
        </span>
      )}
      {doc.counts.unstable.sameDocument + doc.counts.unstable.documentChanged > 0 && (
        <span className="tag extra">
          {doc.counts.unstable.sameDocument + doc.counts.unstable.documentChanged} unstable call(s): repeats disagree
        </span>
      )}
    </p>
    <SideTable doc={doc} />
    <ClassCounts doc={doc} />
    <Only label="only before" only={doc.onlyBefore} />
    <Only label="only after" only={doc.onlyAfter} />
    <h3>{doc.calls.length} call(s) listed</h3>
    {doc.calls.map((call) => <CallRow key={`${call.capture}/${call.caseId}`} call={call} />)}
  </div>
)
