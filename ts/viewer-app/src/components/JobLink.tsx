/** A job named where it was started: the link to its page, with its one-line label. */
import type { JSX } from "react"
import type { JobStarted } from "../api.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

export const JobLink = ({ job }: { readonly job: JobStarted }): JSX.Element => (
  <Link to={href.job(job.id)} className="job-link">
    job {job.id} <span className="muted">— {job.label}</span>
  </Link>
)

/** The jobs a start answered, in launch order, each a link. */
export const JobLinks = ({ jobs }: { readonly jobs: readonly JobStarted[] }): JSX.Element => (
  <span className="job-links">
    {jobs.map((job) => <JobLink key={job.id} job={job} />)}
  </span>
)
