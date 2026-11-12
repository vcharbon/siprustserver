/**
 * The on-demand collectors a cell's lane offers (ADR-0060 appendix §5), one
 * button each: a run asks the collector of this one cell now, its
 * attachments land on the cell (`onLanded`, so the list reads them), and its
 * lines are shown here, never merged into the cell's verdict. Nothing renders
 * where the lane offers none.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type CollectAnswered, type CollectAsked, useAsync } from "../../api.ts"

/** Where one run stands. */
type Run =
  | { readonly state: "idle" }
  | { readonly state: "running"; readonly collector: string }
  | { readonly state: "done"; readonly collector: string; readonly answered: CollectAnswered }
  | { readonly state: "refused"; readonly collector: string; readonly message: string }

/** What a finished run says, in one line. */
const doneLine = (collector: string, answered: CollectAnswered): string => {
  const lines = answered.verifications.map((line) => `${line.name} ${line.answer}${line.message === undefined ? "" : `: ${line.message}`}`)
  return [
    `${collector}: ${answered.attached} attachment(s) landed`,
    ...lines,
    ...(answered.leftChanged.length === 0 ? [] : [`lab left changed by ${answered.leftChanged.join(", ")}`])
  ].join(" · ")
}

export const CollectOnDemand = ({ cell, onLanded }: {
  readonly cell: Omit<CollectAsked, "collector">
  readonly onLanded: () => void
}): JSX.Element | null => {
  const offered = useAsync(() => api.collectOffered(cell.lane), cell.lane)
  const [run, setRun] = useState<Run>({ state: "idle" })
  if (offered.state !== "ok" || offered.value.length === 0) return null
  const ask = (collector: string): void => {
    setRun({ state: "running", collector })
    api.collect({ ...cell, collector })
      .then((answered) => {
        setRun({ state: "done", collector, answered })
        onLanded()
      })
      .catch((error: unknown) =>
        setRun({ state: "refused", collector, message: error instanceof Error ? error.message : String(error) })
      )
  }
  return (
    <section className="cp-panel">
      <h3>On demand</h3>
      <p className="muted cp-small">Asked of this call now; what it attaches lands below, its answers never change the verdict.</p>
      {offered.value.map((collector) => (
        <button key={collector} type="button" disabled={run.state === "running"} onClick={() => ask(collector)}>
          {run.state === "running" && run.collector === collector ? `${collector} …` : collector}
        </button>
      ))}
      {run.state === "done" && <p className="cp-ok mono">{doneLine(run.collector, run.answered)}</p>}
      {run.state === "refused" && <p className="cp-nok mono">{run.collector}: {run.message}</p>}
    </section>
  )
}
