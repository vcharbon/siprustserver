/**
 * What a tooling states of the host its lab runs on: its checkout, the
 * variables it reads, where its roots default, the wrapper its interpreter
 * runs in and what its build wrapper is told. Every generic module of this
 * package reads these through it and names none itself.
 */
import type * as BinaryRule from "@sip/toolchain/binary-rule"
import type { WorkspacePack } from "@sip/workspace"

/** The variables the host states beside the workspace's, by role. */
export interface Variables {
  /** Set to `none` by an install that runs no user session by design. */
  readonly userSession: string
  /** Names the checkout's environment: the integration checkout, or a task worktree. */
  readonly environment: string
}

/** The roots' defaults where neither the invocation nor the host states one, absolute. */
export interface RootDefaults {
  readonly corpus: string
  readonly sessions: string
  readonly census: string
  readonly registry: string
  /** The bench's runs. */
  readonly bench: string
}

/** The wrapper the interpreter runs in: its own memory box, and the ledger the cells dial against. */
export interface Interpreter {
  /** The wrapper's path: what the interpreter variable names ahead of anything the environment says. */
  readonly wrapper: string
  /** The variable the wrapper sizes its scope by. */
  readonly memoryVariable: string
  /** The variable naming the directory holding a row for every number the cells dial. */
  readonly ledgerVariable: string
}

/** A build's capped scope: its memory ceiling, which the build wrapper is told it runs under, and its other systemd properties. */
export interface BuildScope {
  readonly memoryMax: string
  /** `Key=Value` scope properties beside `MemoryMax` (`MemoryHigh=…`, `CPUQuota=…`, `IOWeight=…`). */
  readonly properties: ReadonlyArray<string>
}

/** A tooling's host facts. */
export interface LabPack {
  /** The checkout the tooling runs from, absolute. */
  readonly checkout: string
  /** The generic SIP tree's checkout, absolute: its tools build there. */
  readonly sipTree: string
  /** The workspace's facts: the variables the roots are read from among them. */
  readonly workspace: WorkspacePack.WorkspacePack
  /** Where each Rust binary is. */
  readonly binaries: BinaryRule.BinaryRule
  readonly variables: Variables
  readonly roots: RootDefaults
  /** The box-wide lock root under the user's runtime directory, relative. */
  readonly locksUnder: string
  readonly interpreter: Interpreter
  /** The capped scope a build runs in where a user session answers. */
  readonly buildScope: BuildScope
  /**
   * What the build wrapper is told when a build already runs in a capped
   * scope of `memoryMax`: it then opens no scope of its own and leaves the
   * box-wide pool.
   */
  readonly buildScoped: (memoryMax: string) => Readonly<Record<string, string>>
}
