/**
 * The queue (§6 J8): the running jobs, the queued ones, and the ended ones
 * newest first, paged. Each row is its kind, its label, its lane and its
 * request, a progress bar with the red and gone counts inline, the lock
 * holder's line while it waits, and the verbs — stop, restart, delete
 * results — beside a link to the job's own page. The list is re-read every
 * two seconds while anything runs or waits; everything on it is a row.
 */
import type { JSX } from "react"
import { useState } from "react"
import { api, type JobListed } from "../api.ts"
import { useCommands } from "../commands.ts"
import { Age } from "../components/Age.tsx"
import { JobActions } from "../components/JobActions.tsx"
import { Link } from "../components/Link.tsx"
import { ProgressBar } from "../components/ProgressBar.tsx"
import { progressOfListed, unitOf } from "../job-progress.ts"
import { usePolled } from "../poll.ts"
import { href, setQuery, useQuery } from "../router.ts"

const POLL_MS = 2_000
const DEFAULT_ENDED = 30

/** The word for a status, toned: an end is quiet or red; running and queued ask for attention. */
const Status = ({ job }: { readonly job: JobListed }): JSX.Element => (
  <span
    className={`tag ${
      job.status === "done"
        ? "status-passing"
        : job.status === "running" || job.status === "queued"
        ? "state-stale"
        : "error"
    }`}
    title={job.exit === null ? undefined : `exit ${job.exit}`}
  >
    {job.status}
    {job.stopRequestedAt !== null && job.status === "running" && " · stopping"}
    {job.exit !== null && job.exit !== 0 && ` (${job.exit})`}
  </span>
)

const Row = ({ job, onChanged }: { readonly job: JobListed; readonly onChanged: () => void }): JSX.Element => (
  <tr key={job.id}>
    <td className="mono"><Link to={href.job(job.id)}>job {job.id}</Link></td>
    <td>{job.kind}</td>
    <td className="mono">
      {job.label}
      {job.mode !== null && job.restartOf !== null && (
        <div className="muted">{job.mode} of <Link to={href.job(job.restartOf)}>job {job.restartOf}</Link></div>
      )}
      {job.requestId !== null && job.lane !== null && (
        <div className="muted">
          request {job.requestId} · <Link to={href.triage({ lane: job.lane })}>queue on {job.lane}</Link>
        </div>
      )}
    </td>
    <td><Status job={job} /></td>
    <td className="progress-cell">
      <ProgressBar progress={progressOfListed(job)} unit={unitOf(job.kind)} compact />
      {job.batchAt !== null && job.batches !== null && job.status === "running" && (
        <div className="muted">batch {job.batchAt}/{job.batches}</div>
      )}
      {job.waitingOn !== null && <div className="muted waiting">waiting on {job.waitingOn}</div>}
    </td>
    <td className="muted"><Age at={job.startedAt ?? job.createdAt} /></td>
    <td className="muted">{job.finishedAt === null ? "—" : <Age at={job.finishedAt} />}</td>
    <td className="row-actions">
      <JobActions
        id={job.id}
        kind={job.kind}
        status={job.status}
        requestId={job.requestId}
        stopRequestedAt={job.stopRequestedAt}
        onChanged={onChanged}
      />
    </td>
  </tr>
)

const Group = (
  { title, jobs, onChanged, empty }: {
    readonly title: string
    readonly jobs: readonly JobListed[]
    readonly onChanged: () => void
    readonly empty: string
  }
): JSX.Element => (
  <section className="jobs-group">
    <h2>{title} <span className="muted">({jobs.length})</span></h2>
    {jobs.length === 0 ? <p className="muted">{empty}</p> : (
      <table className="listing jobs-listing">
        <thead>
          <tr>
            <th>job</th>
            <th>kind</th>
            <th>what</th>
            <th>status</th>
            <th>progress</th>
            <th>started</th>
            <th>finished</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {jobs.map((job) => <Row key={job.id} job={job} onChanged={onChanged} />)}
        </tbody>
      </table>
    )}
  </section>
)

export const JobsPage = (): JSX.Element => {
  const commands = useCommands()
  const initial = useQuery()
  const [limit, setLimit] = useState(() => initial.get("limit") ?? "")
  const [offset, setOffset] = useState(() => Number(initial.get("offset") ?? "0") || 0)
  const [refresh, setRefresh] = useState(0)
  const perPage = limit === "" ? DEFAULT_ENDED : Math.max(1, Number(limit) || DEFAULT_ENDED)

  const active = usePolled(() => api.jobs({ status: "active", limit: 500 }), `jobs-active#${refresh}`, POLL_MS)
  // The ended page follows the active one: a job that ends leaves one list for the other within a poll.
  const anythingActive = active.current === undefined || active.current.rows.length > 0
  const ended = usePolled(
    () => api.jobs({ status: "ended", limit: perPage, offset }),
    `jobs-ended#${perPage}#${offset}#${refresh}`,
    anythingActive ? POLL_MS : null
  )
  const onChanged = (): void => setRefresh((n) => n + 1)

  const url = (next: { limit?: string; offset?: number }): void =>
    setQuery({ limit: next.limit ?? limit, offset: (next.offset ?? offset) === 0 ? "" : String(next.offset ?? offset) })

  if (active.current === undefined || ended.current === undefined) {
    const error = active.error ?? ended.error
    if (error !== undefined) return <p className="page error">{error}</p>
    return <p className="page muted">loading the jobs…</p>
  }
  const running = active.current.rows.filter((job) => job.status === "running")
  const queued = active.current.rows.filter((job) => job.status === "queued")
  const page = ended.current
  const last = Math.min(page.total, page.offset + page.rows.length)

  return (
    <div className="page jobs-page">
      <header className="page-head">
        <div>
          <h1>jobs</h1>
          <p className="muted">
            {anythingActive ? "re-read every two seconds while anything runs" : "nothing runs"}
            {commands.job !== undefined && <>{" · "}<code>{commands.job} list</code> is the same queue</>}
          </p>
          {(active.error ?? ended.error) !== undefined && <p className="error">{active.error ?? ended.error}</p>}
        </div>
      </header>

      <Group title="running" jobs={running} onChanged={onChanged} empty="no job runs" />
      <Group title="queued" jobs={queued} onChanged={onChanged} empty="no job waits for its process" />

      <section className="jobs-group">
        <h2>
          finished <span className="muted">({page.total})</span>
        </h2>
        <p className="muted">
          showing <b>{page.rows.length === 0 ? 0 : page.offset + 1}–{last}</b> of <b>{page.total}</b>
          {" · "}
          <button
            type="button"
            className="copy-ref"
            disabled={page.offset === 0}
            onClick={() => {
              const next = Math.max(0, offset - perPage)
              setOffset(next)
              url({ offset: next })
            }}
          >
            previous
          </button>{" "}
          <button
            type="button"
            className="copy-ref"
            disabled={last >= page.total}
            onClick={() => {
              const next = offset + perPage
              setOffset(next)
              url({ offset: next })
            }}
          >
            next
          </button>
          {" · "}
          <label>
            per page{" "}
            <input
              type="number"
              min={1}
              className="row-cap"
              value={limit}
              placeholder={String(DEFAULT_ENDED)}
              onChange={(event) => {
                setLimit(event.target.value)
                url({ limit: event.target.value })
              }}
            />
          </label>
        </p>
        {page.rows.length === 0 ? <p className="muted">no job has ended</p> : (
          <table className="listing jobs-listing">
            <thead>
              <tr>
                <th>job</th>
                <th>kind</th>
                <th>what</th>
                <th>status</th>
                <th>progress</th>
                <th>started</th>
                <th>finished</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {page.rows.map((job) => <Row key={job.id} job={job} onChanged={onChanged} />)}
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}
