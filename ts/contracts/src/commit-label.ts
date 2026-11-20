/**
 * A run component's COMMIT as every view shows it: the commit's first ten
 * digits, `-dirty` after them where the tree it was built from differed from
 * it. A dirty run is shown and counted like any other; the suffix is the only
 * mark it carries. Free of imports, so the viewer's bundle takes it as is.
 */

/** What a component's commit is shown as; "unknown" where none was taken. */
export const commitLabel = (component: { readonly sha: string | null; readonly dirty?: boolean | null | undefined }): string =>
  component.sha === null ? "unknown" : `${component.sha.slice(0, 10)}${component.dirty === true ? "-dirty" : ""}`
