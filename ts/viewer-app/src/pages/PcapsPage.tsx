/**
 * The pcap root, file by file: size, date, where the pipeline stands with it
 * (new, ingested, cut with its case count), its last full run per lane, and
 * one action — replay on a lane, which starts an ingest with its cut and a
 * replay after it, both linked. The lane list says per lane whether a capture
 * replay fits it, runs there only forced (its results marked so), or runs
 * nowhere. The inbox above it lists what is being anonymised for this
 * network's tooling (public peers faked, SUT and private addresses kept)
 * before it lands in the root, each capture with its state and its job; a
 * drop here goes to the inbox.
 *
 * The page polls while the inbox holds something in flight or a job started
 * from it has not ended, and stops once nothing is in progress.
 */
import type { JSX } from "react"
import { useEffect, useState } from "react"
import { api, type InboxEntry, type JobStarted, type PcapFile, type PcapLane } from "../api.ts"
import { Age } from "../components/Age.tsx"
import { DropCaptures } from "../components/DropCaptures.tsx"
import { JobLinks } from "../components/JobLink.tsx"
import { Link } from "../components/Link.tsx"
import { formatBytes } from "../json-size.ts"
import { plural } from "../plural.ts"
import { usePolled } from "../poll.ts"
import { href } from "../router.ts"

const POLL_MS = 3000

const said = (error: unknown): string => (error instanceof Error ? error.message : String(error))

const STATE_TONE: Readonly<Record<PcapFile["state"], string>> = {
  new: "muted",
  ingested: "state-stale",
  cut: "class-accepted"
}

const INBOX_TONE: Readonly<Record<InboxEntry["state"], string>> = {
  settling: "muted",
  waiting: "muted",
  taking: "state-stale",
  refused: "error"
}

/** Where one file stands: its state, the case count once cut, the problem as a tooltip. */
const FileState = ({ file }: { readonly file: PcapFile }): JSX.Element => (
  <span className={`tag ${file.problem === null ? STATE_TONE[file.state] : "error"}`} title={file.problem ?? undefined}>
    {file.state}
    {file.state === "cut" && ` · ${plural(file.cases ?? 0, "case")}`}
  </span>
)

/** The last full run per lane: `lane=status(runN[, f failed])`, the lane to the capture's study, the run to its report. */
const Standings = ({ file }: { readonly file: PcapFile }): JSX.Element =>
  file.standings.length === 0 ? <span className="muted">—</span> : (
    <>
      {file.standings.map((one) => (
        <div key={one.lane} className="mono">
          <Link to={href.triageStudy(file.capture, { lane: one.lane })}>{one.lane}</Link>={one.status}(
          <Link to={href.report({ run: `@run:${one.runId}`, lane: one.lane })}>run{one.runId}</Link>
          {one.failed > 0 && `, ${one.failed} failed`})
        </div>
      ))}
    </>
  )

/** The option a lane is offered as: replay, forced replay, or not runnable. */
const laneLabel = (lane: PcapLane): string =>
  lane.fit === "fits" ? `replay on ${lane.lane}` : lane.fit === "forced" ? `replay on ${lane.lane} (forced)` : `${lane.lane}: not runnable`

/** The action of one file: a lane, then the two jobs it starts. */
const ReplayOn = (
  { file, lanes, onStarted }: {
    readonly file: PcapFile
    readonly lanes: readonly PcapLane[]
    readonly onStarted: (jobs: readonly JobStarted[]) => void
  }
): JSX.Element => {
  const first = lanes.find((lane) => lane.fit === "fits") ?? lanes.find((lane) => lane.fit === "forced")
  const [choice, setChoice] = useState(first?.lane ?? "")
  const [phase, setPhase] = useState<
    | { readonly kind: "idle" }
    | { readonly kind: "starting" }
    | { readonly kind: "started"; readonly jobs: readonly JobStarted[] }
    | { readonly kind: "error"; readonly detail: string }
  >({ kind: "idle" })
  const picked = lanes.find((lane) => lane.lane === choice)

  const start = async (): Promise<void> => {
    if (picked === undefined || picked.fit === "none") return
    setPhase({ kind: "starting" })
    try {
      const { jobs } = await api.jobStart({ kind: "pcap-replay", file: file.file, lane: picked.lane, force: picked.fit === "forced" })
      setPhase({ kind: "started", jobs })
      onStarted(jobs)
    } catch (error) {
      setPhase({ kind: "error", detail: said(error) })
    }
  }

  return (
    <span className="rerun">
      <select value={choice} onChange={(event) => setChoice(event.target.value)}>
        {lanes.map((lane) => (
          <option key={lane.lane} value={lane.lane} disabled={lane.fit === "none"} title={lane.line ?? undefined}>
            {laneLabel(lane)}
          </option>
        ))}
      </select>{" "}
      <button
        type="button"
        className="copy-ref"
        disabled={picked === undefined || picked.fit === "none" || phase.kind === "starting"}
        title={picked?.line ?? "ingest + cut, then replay"}
        onClick={() => void start()}
      >
        {phase.kind === "starting" ? "starting…" : "replay"}
      </button>
      {phase.kind === "started" && <> <JobLinks jobs={phase.jobs} /></>}
      {phase.kind === "error" && <span className="error"> {phase.detail}</span>}
    </span>
  )
}

const InboxTable = ({ entries }: { readonly entries: readonly InboxEntry[] }): JSX.Element => (
  <table className="listing pcaps-inbox">
    <thead>
      <tr>
        <th>files</th>
        <th>size</th>
        <th>modified</th>
        <th>state</th>
        <th>copy</th>
        <th>job</th>
      </tr>
    </thead>
    <tbody>
      {entries.map((entry) => (
        <tr key={entry.files.map((one) => one.name).join("\n")}>
          <td className="mono break">{entry.files.map((one) => <div key={one.name}>{one.name}</div>)}</td>
          <td className="mono">{formatBytes(entry.files.reduce((sum, one) => sum + one.size, 0))}</td>
          <td className="muted"><Age at={entry.files.map((one) => one.modifiedAt).sort().at(-1)!} /></td>
          <td><span className={`tag ${INBOX_TONE[entry.state]}`}>{entry.state}</span></td>
          <td className="mono break">{entry.copy}</td>
          <td>{entry.jobId === null ? <span className="muted">—</span> : <Link to={href.job(entry.jobId)}>job {entry.jobId}</Link>}</td>
        </tr>
      ))}
    </tbody>
  </table>
)

const FilesTable = (
  { files, lanes, onStarted }: {
    readonly files: readonly PcapFile[]
    readonly lanes: readonly PcapLane[]
    readonly onStarted: (jobs: readonly JobStarted[]) => void
  }
): JSX.Element => (
  <table className="listing pcaps-listing">
    <thead>
      <tr>
        <th>file</th>
        <th>size</th>
        <th>modified</th>
        <th>state</th>
        <th>last run per lane</th>
        <th></th>
      </tr>
    </thead>
    <tbody>
      {files.map((file) => (
        <tr key={file.file}>
          <td className="mono break">
            {file.state === "new" ? file.file : <Link to={href.triageStudy(file.capture)}>{file.file}</Link>}
          </td>
          <td className="mono">{formatBytes(file.size)}</td>
          <td className="muted"><Age at={file.modifiedAt} /></td>
          <td><FileState file={file} /></td>
          <td><Standings file={file} /></td>
          <td className="row-actions"><ReplayOn file={file} lanes={lanes} onStarted={onStarted} /></td>
        </tr>
      ))}
    </tbody>
  </table>
)

const IN_FLIGHT: ReadonlySet<InboxEntry["state"]> = new Set(["settling", "waiting", "taking"])

const ENDED: ReadonlySet<string> = new Set(["done", "failed", "interrupted", "died"])

export const PcapsPage = (): JSX.Element => {
  const [refresh, setRefresh] = useState(0)
  // The jobs started from this page that have not been seen ended.
  const [watching, setWatching] = useState<readonly number[]>([])
  const [busy, setBusy] = useState(true)
  const view = usePolled(
    async () => ({
      pcaps: await api.pcaps(),
      ended: (await Promise.all(watching.map((id) => api.job(id)))).filter((job) => ENDED.has(job.status)).map((job) => job.id)
    }),
    `pcaps#${refresh}#${watching.join(",")}`,
    busy || watching.length > 0 ? POLL_MS : null
  )
  const value = view.current?.pcaps
  const ended = view.current?.ended ?? []
  const inFlight = value === undefined || value.inboxEntries.some((entry) => IN_FLIGHT.has(entry.state))
  useEffect(() => setBusy(inFlight), [inFlight])
  useEffect(() => {
    if (ended.length > 0) setWatching((all) => all.filter((id) => !ended.includes(id)))
  }, [ended.join(",")])

  if (value === undefined) {
    if (view.error !== undefined) return <p className="page error">pcaps: {view.error}</p>
    return <p className="page muted">loading…</p>
  }
  const onStarted = (jobs: readonly JobStarted[] = []): void => {
    setWatching((all) => [...all, ...jobs.map((job) => job.id)])
    setRefresh((n) => n + 1)
  }

  return (
    <div className="page pcaps-page">
      <header className="page-head">
        <div>
          <h1>Pcaps</h1>
          <p className="muted">
            <b>{value.files.length}</b> {value.files.length === 1 ? "file" : "files"} · <b>{value.inboxEntries.length}</b> in inbox
            {value.pcaps !== null && <> · <code className="mono" title="pcap root">{value.pcaps}</code></>}
          </p>
          {view.error !== undefined && <p className="error">pcaps: {view.error}</p>}
        </div>
      </header>

      {value.pcaps === null
        ? <p className="muted">pcap root: none</p>
        : (
          <>
            <DropCaptures lanes={[]} laneless onFresh={() => onStarted()} />
            {value.inboxEntries.length > 0 && (
              <section>
                <h2>Inbox · {value.inboxEntries.length}</h2>
                <InboxTable entries={value.inboxEntries} />
              </section>
            )}
            <section>
              <h2>Files · {value.files.length}</h2>
              {value.files.length === 0
                ? <p className="muted">none</p>
                : <FilesTable files={value.files} lanes={value.lanes} onStarted={onStarted} />}
            </section>
          </>
        )}
    </div>
  )
}
