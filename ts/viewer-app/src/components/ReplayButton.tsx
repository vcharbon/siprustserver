/**
 * Replay ONE call, or one capture whole, from where it is read: a request of
 * that one cell (a `cases` run of that call) or of the capture whole, on the
 * lane on screen, and a replay job over it. The button links the job it
 * started; the jobs page says how it goes. A refusal — no cut names the call
 * — is stated in place.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type JobStarted } from "../api.ts"
import { JobLinks } from "./JobLink.tsx"

type Phase =
  | { readonly kind: "idle" }
  | { readonly kind: "starting" }
  | { readonly kind: "started"; readonly jobs: readonly JobStarted[] }
  | { readonly kind: "refused"; readonly detail: string }

export const ReplayButton = (
  { capture, lane, caseId, onStarted }: {
    readonly capture: string
    /** The lane to replay on; "" = the configured one is not enough: a request names a lane. */
    readonly lane: string
    /** The one case to replay alone; absent replays the capture whole. */
    readonly caseId?: string | undefined
    /** Called once the job is started, with what was started. */
    readonly onStarted?: (jobs: readonly JobStarted[]) => void
  }
): JSX.Element => {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" })

  const start = async (): Promise<void> => {
    setPhase({ kind: "starting" })
    try {
      const { created } = await api.requestCreate({
        lanes: [lane],
        selection: caseId === undefined
          ? { kind: "captures", captures: [capture] }
          : { kind: "cells", cells: [{ capture, caseId }] }
      })
      const request = created[0]
      if (request === undefined) {
        setPhase({ kind: "refused", detail: "no request was created" })
        return
      }
      const { jobs } = await api.jobStart({ kind: "replay", requestId: request.requestId })
      setPhase({ kind: "started", jobs })
      onStarted?.(jobs)
    } catch (error) {
      setPhase({ kind: "refused", detail: error instanceof Error ? error.message : String(error) })
    }
  }

  return (
    <span className="rerun">
      <button
        type="button"
        className="copy-ref"
        disabled={phase.kind === "starting" || lane === ""}
        title={lane === ""
          ? "pick a lane first: a request names one"
          : caseId === undefined
          ? `replay ${capture} whole on ${lane}: a request of every case of its cut, and a job over it`
          : `replay this call alone on ${lane}: a one-cell request and a cases run over it — the capture's standing does not move`}
        onClick={() => void start()}
      >
        {phase.kind === "starting"
          ? "starting…"
          : phase.kind === "started"
          ? "again"
          : caseId === undefined
          ? "replay whole"
          : "replay this call"}
      </button>
      {phase.kind === "started" && <> <JobLinks jobs={phase.jobs} /></>}
      {phase.kind === "refused" && <span className="error"> {phase.detail}</span>}
    </span>
  )
}
