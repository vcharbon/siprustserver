/**
 * The top bar's identity: the site's name as the server's config states it,
 * and the build stamp of the bundle shown.
 */
import type { JSX } from "react"
import { type BuildStamp, stampLine } from "../build-stamp.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/** The site's `name`, linking to the index. */
export const Brand = ({ name }: { readonly name: string }): JSX.Element => (
  <Link to={href.index()} className="brand">{name}</Link>
)

/** The bundle's build stamp: commit and build time. */
export const Stamp = ({ stamp }: { readonly stamp: BuildStamp | undefined }): JSX.Element => (
  <span className="build-stamp" title="build: commit · time">{stampLine(stamp)}</span>
)
