/**
 * The top bar's identity: the rig's name, the title of the pack the server
 * runs (once its config answers), and the build stamp of the bundle shown.
 */
import type { JSX } from "react"
import { type BuildStamp, stampLine } from "../build-stamp.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/** The rig's name every page opens with. */
export const RIG = "siprig"

/** The browser tab's title: the rig's name, then the pack's where known. */
export const tabTitle = (title: string | undefined): string => (title === undefined ? RIG : `${RIG} · ${title}`)

/** The rig's name, linking to the index, and the pack's `title` beside it where known. */
export const Brand = ({ title }: { readonly title: string | undefined }): JSX.Element => (
  <span className="brand-block">
    <Link to={href.index()} className="brand">{RIG}</Link>
    {title !== undefined && <span className="pack-title">{title}</span>}
  </span>
)

/** The bundle's build stamp: commit and build time. */
export const Stamp = ({ stamp }: { readonly stamp: BuildStamp | undefined }): JSX.Element => (
  <span className="build-stamp" title="build: commit · time">{stampLine(stamp)}</span>
)
