/**
 * The verbs on a job (§6 J8, §7 K4): STOP while it runs, RESTART — resume or
 * again — once it has ended, DELETE RESULTS behind one confirmation on the
 * plan's counts. Each is a job command the server runs for the page; a
 * job it starts is linked. A verb the CLI refuses shows its sentence in place.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type JobDeletePlan, type JobStarted, type JobStatus } from "../api.ts"
import { JobLink } from "./JobLink.tsx"

const ENDED: ReadonlySet<string> = new Set<JobStatus>(["done", "failed", "interrupted", "died"])

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

export interface Props {
  readonly id: number
  readonly kind: string
  readonly status: JobStatus
  readonly requestId: number | null
  readonly stopRequestedAt: string | null
  /** Called once a verb changed something: the page re-reads. */
  readonly onChanged: () => void
}

type Phase =
  | { readonly kind: "idle" }
  | { readonly kind: "busy"; readonly what: string }
  | { readonly kind: "confirm"; readonly plan: JobDeletePlan }
  | { readonly kind: "started"; readonly job: JobStarted }
  | { readonly kind: "stopped" }
  | { readonly kind: "error"; readonly detail: string }

export const JobActions = ({ id, kind, status, requestId, stopRequestedAt, onChanged }: Props): JSX.Element => {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" })
  const running = status === "running"
  const ended = ENDED.has(status)
  const restartable = ended && kind === "replay" && requestId !== null

  const attempt = async (what: string, body: () => Promise<Phase>): Promise<void> => {
    setPhase({ kind: "busy", what })
    try {
      setPhase(await body())
      onChanged()
    } catch (error) {
      setPhase({ kind: "error", detail: said(error) })
    }
  }

  const stop = () =>
    attempt("stopping…", async () => {
      await api.jobStop(id)
      return { kind: "stopped" }
    })

  const restart = (mode: "resume" | "again") =>
    attempt(`${mode}…`, async () => ({ kind: "started", job: await api.jobRestart(id, mode) }))

  const plan = () =>
    attempt("counting…", async () => ({ kind: "confirm", plan: await api.jobDeletePlan(id) }))

  const del = () => attempt("deleting…", async () => ({ kind: "started", job: await api.jobDelete(id) }))

  if (phase.kind === "confirm") {
    const { plan: shown } = phase
    return (
      <span className="retire-confirm">
        <span className="mono">
          {shown.runs} run(s): rows runs {shown.rows.runs}, case results {shown.rows.caseResults}, rule hits{" "}
          {shown.rows.runRuleHits}; {shown.bundles} bundle(s) on disk
        </span>
        {shown.held.length > 0
          ? <span className="error"> held: {shown.held.map((one) => `run${one.id} (${one.why})`).join("; ")}</span>
          : shown.runs === 0
          ? <span className="muted"> nothing to delete</span>
          : (
            <button type="button" className="copy-ref danger" onClick={() => void del()}>
              delete the {shown.runs} run(s) of job {id}
            </button>
          )}
        <button type="button" className="copy-ref" onClick={() => setPhase({ kind: "idle" })}>keep</button>
      </span>
    )
  }

  const busy = phase.kind === "busy"
  return (
    <span className="job-actions">
      {running && (
        <button
          type="button"
          className="copy-ref danger"
          disabled={busy || stopRequestedAt !== null}
          title="stop: no new cell, a grace for the cells in flight, rows closed interrupted"
          onClick={() => void stop()}
        >
          {stopRequestedAt !== null ? "stopping" : "stop"}
        </button>
      )}
      {restartable && (
        <>
          <button
            type="button"
            className="copy-ref"
            disabled={busy}
            title="restart, resume: the request's cells no job of it ran, as cases sets"
            onClick={() => void restart("resume")}
          >
            resume
          </button>
          <button
            type="button"
            className="copy-ref"
            disabled={busy}
            title="restart, again: every cell of the request, kinds as it resolves"
            onClick={() => void restart("again")}
          >
            again
          </button>
        </>
      )}
      {ended && kind === "replay" && (
        <button
          type="button"
          className="copy-ref"
          disabled={busy}
          title="delete the runs this job opened — rows and bundles — after one confirmation on the counts"
          onClick={() => void plan()}
        >
          delete results…
        </button>
      )}
      {busy && <span className="muted"> {phase.what}</span>}
      {phase.kind === "stopped" && <span className="muted"> stop requested</span>}
      {phase.kind === "started" && <> <JobLink job={phase.job} /></>}
      {phase.kind === "error" && <span className="error"> {phase.detail}</span>}
    </span>
  )
}
