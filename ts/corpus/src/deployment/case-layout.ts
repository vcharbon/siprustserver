/**
 * `CaseLayout` — how a case directory carries its SIDE ROWS: the numbered
 * documents a case hangs off its pivot for the interpreter's peers to serve
 * (the store's `ledger_rows`), the call key those rows state, and the variable
 * that hands the interpreter the one directory a batch's rows are merged into.
 *
 * The cut reads the rows out of the generator's case directory into the
 * store, and a case is named over them too, so a cut that moved only a row is
 * a case the store has never seen. The materialiser lays them back out where
 * the interpreter looks.
 *
 * The default is a deployment whose cases carry no side rows: the directory
 * is named, nothing in it states a call key, and the interpreter is told of
 * no rows directory.
 */
import type { Model } from "@sip/store"
import * as Context from "effect/Context"
import * as Layer from "effect/Layer"

export interface Interface {
  /** The directory, inside a case directory, holding its side rows. */
  readonly rowsDir: string
  /** A side row's file name by its number. */
  readonly rowFile: (number: string) => string
  /** The number a file named `name` in the rows directory is the row of: {@link rowFile}'s inverse; `undefined` for no row. */
  readonly rowNumberOf: (name: string) => string | undefined
  /** The call key one row states; `undefined` where it states none. A case's rows state exactly one. */
  readonly callKeyOf: (row: Model.LedgerRow) => string | undefined
  /** The variable naming a batch's merged rows directory to the interpreter; `undefined` where none is passed. */
  readonly interpreterVariable: string | undefined
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/CaseLayout") {}

/** The default: rows under `rows/`, one `<number>.json` each, stating no call key and passed to no interpreter. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    rowsDir: "rows",
    rowFile: (number) => `${number}.json`,
    rowNumberOf: (name) => /^(.+)\.json$/.exec(name)?.[1],
    callKeyOf: () => undefined,
    interpreterVariable: undefined
  })
)

/** A deployment's own layout. */
export const layerWith = (layout: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(layout))
