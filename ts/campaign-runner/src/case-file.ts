/**
 * The case files a run reads, as the runner sees them: each loaded (its name
 * in the run, its absolute path, its default export, the cells a case source
 * yields compiled) or broken with why; a glob selects them by name, or by
 * absolute path where it is absolute. A pack reads its roots and adds what
 * its cases resolve through to its {@link Selected}.
 */
import type { CompiledCell } from "@sip/rig"
import * as path from "node:path"

/** One case file loaded: its name in a run, its absolute path, its default export. */
export interface CaseFile {
  readonly file: string
  readonly absolute: string
  readonly value: unknown
  /** The cells it yields compiled, where it is a case source the pack names. */
  readonly cells?: ReadonlyArray<CompiledCell.CompiledCell>
}

/** A case file that did not load or was refused, and why. */
export interface Broken {
  readonly file: string
  readonly absolute: string
  readonly reason: string
}

/** What a run's glob selects. */
export interface Selected {
  readonly selected: ReadonlyArray<CaseFile>
  readonly broken: ReadonlyArray<Broken>
  /** Whether a case file of the campaign was left out: the run is then partial. */
  readonly partial: boolean
  /** How many case files the campaign's globs match, broken ones included. */
  readonly inCampaign: number
  /** Every case file read, selected or not: what a glob naming files is held against. */
  readonly files: ReadonlyArray<{ readonly file: string; readonly absolute: string }>
}

/** `pattern` as a glob over the names (or, absolute, the paths) of case files. */
export const normalized = (pattern: string): string => pattern.replace(/^\.\//, "")

/** Whether the case file named `file` at `absolute` matches `pattern`. */
export const matches = (pattern: string, file: string, absolute: string): boolean => {
  const p = normalized(pattern)
  return path.isAbsolute(p) ? path.matchesGlob(absolute, p) : path.matchesGlob(file, p)
}

/** The `ref` a case file's default export states, if any. */
export const refOf = (value: unknown): string | undefined => {
  const ref = typeof value === "object" && value !== null ? (value as { ref?: unknown }).ref : undefined
  return typeof ref === "string" ? ref : undefined
}
