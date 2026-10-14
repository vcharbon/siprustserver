/**
 * The store's schema, as numbered migrations in the store's own migrations
 * table ({@link TABLE}), so a deployment's own migrations never share a
 * numbering with it.
 *
 * Two records, one database. The REPLAY record: `runs` (one replay invocation
 * over one capture on one lane), `case_results` (what each cut case said in
 * that run, or the lane verdict that kept it from running), `sweeps` (one
 * invocation over a population) and `run_rule_hits` (how often each named rule
 * of the deployment's roster landed on a run). The CUT record: `preps` (one
 * cut invocation), `capture_cuts`, `cut_cases` and `cut_refusals` (what that
 * cut made of one capture), over the content-addressed stores a case rebuilds
 * from — `case_assets`, `ledger_rows`, `flows_documents` and `flows_legs`.
 * Around them: the replay `population`, replay `requests` and the `jobs` that
 * ran them, and the {@link Current} population setting.
 *
 * `case_documents` (the v3 case JSON) is shared by both, content-hash deduped
 * so a corpus question is a query and a re-cut or re-run that changed nothing
 * stores nothing. `cases` names a case over everything it carries — document,
 * assets and ledger rows — so a re-cut that moved only a row is a different
 * case rather than a conflict.
 *
 * A run's id is DB-assigned and its bundle directory is named after it, so
 * `bundle_path` is filled the moment the row exists and never contends; it is
 * kept relative to the session tree (`sessions.ts`). `components` is the code
 * that made a run or a prep (`model.ts` `Provenance`), `[]` where it was never
 * taken. `case_features(doc)` is the deployment's reading of a document's
 * features: the baseline reads the pivot's `detected:` annotations, and a
 * deployment replaces the function and re-adds the generated column.
 *
 * The views, for SQL questions the commands do not class:
 * - `current_runs`: the clean, finished runs of the current population.
 * - `current_cells`: one row per (capture, lane, case) — the call's CURRENT
 *   result, its latest `current_runs` row holding the case, no older than the
 *   capture's latest full `current_runs` reading on the lane (whose case set is
 *   the capture's) where one exists.
 * - `run_cells`: every case result beside its run's identity, code and load.
 * - `current_documents` (migration {@link CURRENT_DOCUMENTS}): every case of
 *   each capture's standing cut with its document, for shape surveys.
 */
import * as Effect from "effect/Effect"
import * as Migrator from "effect/unstable/sql/Migrator"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import { seedFromReadings } from "./case-ids.js"
import { exportReadings } from "./case-maps.js"

/** The store's migrations table. */
export const TABLE = "store_migrations"

/** The key of the migration that creates the whole schema of a store made from nothing. */
export const BASELINE = "1_baseline"

/** The columns of `runs`, in the order every view lists them. */
const RUN_COLUMNS = [
  "id",
  "capture",
  "lane",
  "mode",
  "status",
  "bundle_path",
  "detail",
  "started_at",
  "finished_at",
  "sweep_id",
  "prep_id",
  "job_id",
  "set_kind",
  "cases",
  "failed",
  "skipped",
  "rule_hits",
  "env",
  "branch",
  "lane_hash",
  "concurrency",
  "batch_size",
  "starts_per_second",
  "max_span_seconds",
  "memory_max",
  "components"
] as const

/** The case-result columns the baseline's cell views carry, read off `cr`. */
const BASELINE_CELL_COLUMNS = [
  "passed",
  "verdict",
  "skipped",
  "failed_step",
  "rfc_gating",
  "classification",
  "document_hash",
  "error_kind",
  "error_rule",
  "error_rfc",
  "error_step",
  "error_expected",
  "error_detail",
  "body_failing"
] as const

/** The case-result columns every cell view carries, read off `cr`: the source fault beside a main error included. */
const CELL_COLUMNS = [...BASELINE_CELL_COLUMNS, "error_source"] as const

const listed = (alias: string, columns: ReadonlyArray<string>): string =>
  columns.map((column) => `${alias}.${column}`).join(", ")

/**
 * The three views, (re)created whole, as the baseline states them. Their
 * definitions name every column, so a store whose tables gained their columns
 * in another order still reads them the same, and a deployment that built its
 * tables by migrations of its own creates the identical views by calling this;
 * the store's later migrations restate them over the columns they add.
 */
export const views: Effect.Effect<void, SqlError, SqlClient> = Effect.suspend(() => viewsOf(BASELINE_CELL_COLUMNS))

/** The three views over `cellColumns`, the case-result columns each cell view carries. */
const viewsOf = (cellColumns: ReadonlyArray<string>): Effect.Effect<void, SqlError, SqlClient> =>
  Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`DROP VIEW IF EXISTS current_cells`
  yield* sql`DROP VIEW IF EXISTS run_cells`
  yield* sql`DROP VIEW IF EXISTS current_runs`
  yield* sql.unsafe(`
    CREATE VIEW current_runs AS
    SELECT ${listed("r", RUN_COLUMNS)}
    FROM runs r
    JOIN current_population p ON p.env = r.env AND p.branch = r.branch
    WHERE r.finished_at IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(r.components) c WHERE c->>'dirty' = 'true')
  `)
  yield* sql.unsafe(`
    CREATE VIEW current_cells AS
    SELECT DISTINCT ON (r.capture, r.lane, cr.case_id)
           r.capture, r.lane, cr.case_id, r.id AS run_id, r.set_kind, r.status AS run_status, r.prep_id,
           r.sweep_id, r.bundle_path, r.finished_at, ${listed("cr", cellColumns)}
    FROM current_runs r
    JOIN case_results cr ON cr.run_id = r.id
    WHERE r.id >= coalesce((
      SELECT max(f.id) FROM current_runs f
      WHERE f.capture = r.capture AND f.lane = r.lane AND f.set_kind = 'full' AND f.status <> 'interrupted'
    ), 0)
    ORDER BY r.capture, r.lane, cr.case_id, r.id DESC
  `)
  yield* sql.unsafe(`
    CREATE VIEW run_cells AS
    SELECT r.id AS run_id, r.capture, r.lane, r.set_kind, r.status AS run_status, r.prep_id, r.sweep_id,
           r.job_id, r.started_at, r.finished_at, r.bundle_path,
           r.env, r.branch, r.lane_hash, r.concurrency, r.batch_size, r.starts_per_second,
           r.max_span_seconds, r.memory_max, r.components,
           cr.case_id, ${listed("cr", cellColumns)}
    FROM runs r
    JOIN case_results cr ON cr.run_id = r.id
  `)
})

/**
 * The indexes on `runs` that serve the current population and the resolver:
 * a current run is found by (env, branch) first, the setting being a row the
 * planner joins against rather than a literal a partial index could name.
 */
export const runIndexes: Effect.Effect<void, SqlError, SqlClient> = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`CREATE INDEX runs_env_idx ON runs (env, id DESC)`
  yield* sql`CREATE INDEX runs_current_idx ON runs (env, branch, capture, lane, id DESC)`
})

/** The one-row setting the current population is read from (`current.ts`). */
export const currentPopulation: Effect.Effect<void, SqlError, SqlClient> = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE current_population (
      one    boolean PRIMARY KEY DEFAULT true CHECK (one),
      env    text NOT NULL,
      branch text NOT NULL
    )
  `
})

/** The generic reading of a document's features: its pivot `detected:` annotations, prefix dropped. */
const features = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE FUNCTION case_features(doc jsonb) RETURNS text[]
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
      SELECT coalesce(array_agg(DISTINCT substr(flag->>'kind', 10) ORDER BY substr(flag->>'kind', 10)), '{}')
      FROM jsonb_array_elements(coalesce(doc->'case'->'annotations'->'flags', '[]'::jsonb)) flag
      WHERE flag->>'kind' LIKE 'detected:%'
    $$
  `
})

const baseline = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE sweeps (
      id          bigserial PRIMARY KEY,
      started_at  timestamptz NOT NULL DEFAULT now(),
      finished_at timestamptz,
      lane        text NOT NULL,
      universe    text NOT NULL,
      captures    integer NOT NULL,
      concurrency integer NOT NULL,
      batch_size  integer NOT NULL,
      memory_max  text NOT NULL,
      selection   text
    )
  `
  yield* sql`
    CREATE TABLE requests (
      id         bigserial PRIMARY KEY,
      lane       text NOT NULL,
      selection  jsonb NOT NULL,
      knobs      jsonb NOT NULL,
      created_at timestamptz NOT NULL DEFAULT now(),
      created_by text NOT NULL
    )
  `
  yield* sql`
    CREATE TABLE request_cells (
      request_id bigint NOT NULL REFERENCES requests (id) ON DELETE CASCADE,
      capture    text NOT NULL,
      call_key   text NOT NULL,
      origin     text NOT NULL CHECK (origin IN ('selected', 'added_by_prep')),
      PRIMARY KEY (request_id, capture, call_key)
    )
  `
  yield* sql`
    CREATE TABLE jobs (
      id                bigserial PRIMARY KEY,
      kind              text NOT NULL CHECK (kind IN ('replay', 'cut', 'ingest', 'retire', 'delete')),
      request_id        bigint REFERENCES requests (id) ON DELETE SET NULL,
      lane              text,
      args              jsonb NOT NULL DEFAULT '{}'::jsonb,
      status            text NOT NULL
        CHECK (status IN ('queued', 'running', 'done', 'failed', 'interrupted', 'died')),
      created_at        timestamptz NOT NULL DEFAULT now(),
      created_by        text NOT NULL,
      pid               integer,
      started_at        timestamptz,
      finished_at       timestamptz,
      exit              integer,
      log_path          text,
      hooks_path        text,
      waiting_on        text,
      sweep_id          bigint REFERENCES sweeps (id) ON DELETE SET NULL,
      batch_at          integer,
      batches           integer,
      stop_requested_at timestamptz,
      stop_grace        integer,
      restart_of        bigint REFERENCES jobs (id) ON DELETE SET NULL,
      planned           integer
    )
  `
  yield* sql`CREATE INDEX jobs_status_idx ON jobs (status)`
  yield* sql`
    CREATE TABLE job_cells (
      job_id     bigint NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
      request_id bigint NOT NULL REFERENCES requests (id) ON DELETE CASCADE,
      capture    text NOT NULL,
      call_key   text NOT NULL,
      case_id    text,
      case_hash  text,
      outcome    text NOT NULL
        CHECK (outcome IN ('planned', 'gone: refused', 'gone: not in this cut', 'ran')),
      reasons    text[] NOT NULL DEFAULT '{}',
      PRIMARY KEY (job_id, capture, call_key)
    )
  `
  yield* sql`CREATE INDEX job_cells_request_idx ON job_cells (request_id, job_id)`
  yield* sql`CREATE INDEX job_cells_job_outcome_idx ON job_cells (job_id, outcome)`

  yield* sql`
    CREATE TABLE preps (
      id              bigserial PRIMARY KEY,
      started_at      timestamptz NOT NULL DEFAULT now(),
      finished_at     timestamptz,
      cut_fingerprint text,
      job_id          bigint REFERENCES jobs (id) ON DELETE SET NULL,
      components      jsonb NOT NULL DEFAULT '[]'::jsonb
    )
  `
  yield* sql`CREATE INDEX preps_job_idx ON preps (job_id)`

  yield* sql`
    CREATE TABLE runs (
      id                bigserial PRIMARY KEY,
      capture           text NOT NULL,
      lane              text NOT NULL,
      mode              text NOT NULL,
      status            text NOT NULL,
      bundle_path       text NOT NULL,
      detail            text,
      started_at        timestamptz NOT NULL DEFAULT now(),
      finished_at       timestamptz,
      sweep_id          bigint REFERENCES sweeps (id) ON DELETE SET NULL,
      prep_id           bigint REFERENCES preps (id),
      job_id            bigint REFERENCES jobs (id) ON DELETE SET NULL,
      set_kind          text NOT NULL DEFAULT 'full' CHECK (set_kind IN ('full', 'cases', 'timeline')),
      cases             integer NOT NULL DEFAULT 0,
      failed            integer NOT NULL DEFAULT 0,
      skipped           integer NOT NULL DEFAULT 0,
      rule_hits         boolean NOT NULL DEFAULT false,
      env               text,
      branch            text,
      lane_hash         text,
      concurrency       integer,
      batch_size        integer,
      starts_per_second double precision,
      max_span_seconds  double precision,
      memory_max        text,
      components        jsonb NOT NULL DEFAULT '[]'::jsonb
    )
  `
  yield* sql`CREATE INDEX runs_capture_lane_id_idx ON runs (capture, lane, id DESC)`
  yield* sql`CREATE INDEX runs_lane_capture_id_idx ON runs (lane, capture, id DESC)`
  yield* sql`CREATE INDEX runs_sweep_idx ON runs (sweep_id)`
  yield* sql`CREATE INDEX runs_job_idx ON runs (job_id)`
  yield* runIndexes

  yield* features
  yield* sql`
    CREATE TABLE case_documents (
      hash     text PRIMARY KEY,
      document jsonb NOT NULL,
      features text[] GENERATED ALWAYS AS (case_features(document)) STORED
    )
  `
  yield* sql`
    CREATE TABLE case_results (
      run_id         bigint NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
      case_id        text NOT NULL,
      passed         boolean NOT NULL,
      verdict        text NOT NULL,
      failed_step    text,
      classification jsonb,
      document_hash  text REFERENCES case_documents (hash),
      skipped        text,
      rfc_gating     integer,
      error_kind     text,
      error_rule     text,
      error_rfc      text,
      error_step     text,
      error_expected text,
      error_detail   text,
      body_failing   integer NOT NULL DEFAULT 0,
      PRIMARY KEY (run_id, case_id)
    )
  `
  yield* sql`CREATE INDEX case_results_document_idx ON case_results (document_hash)`

  yield* sql`
    CREATE TABLE run_rule_hits (
      run_id bigint NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
      family text NOT NULL,
      rule   text NOT NULL,
      bucket text NOT NULL,
      hits   integer NOT NULL,
      cells  integer NOT NULL,
      sample text NOT NULL DEFAULT '',
      PRIMARY KEY (run_id, family, rule, bucket)
    )
  `
  yield* sql`CREATE INDEX run_rule_hits_rule_idx ON run_rule_hits (family, rule)`

  yield* sql`CREATE TABLE flows_documents (hash text PRIMARY KEY, head bytea NOT NULL, tail bytea NOT NULL)`
  yield* sql`
    CREATE TABLE flows_legs (
      flows_hash text NOT NULL REFERENCES flows_documents (hash) ON DELETE CASCADE,
      idx        integer NOT NULL,
      bytes      bytea NOT NULL,
      PRIMARY KEY (flows_hash, idx)
    )
  `
  yield* sql`
    CREATE TABLE cases (
      hash          text PRIMARY KEY,
      document_hash text NOT NULL REFERENCES case_documents (hash)
    )
  `
  yield* sql`CREATE INDEX cases_document_idx ON cases (document_hash)`
  yield* sql`
    CREATE TABLE case_assets (
      case_hash text NOT NULL REFERENCES cases (hash),
      path      text NOT NULL,
      bytes     bytea NOT NULL,
      PRIMARY KEY (case_hash, path)
    )
  `
  yield* sql`
    CREATE TABLE ledger_rows (
      case_hash text NOT NULL REFERENCES cases (hash),
      number    text NOT NULL,
      document  jsonb NOT NULL,
      PRIMARY KEY (case_hash, number)
    )
  `

  yield* sql`
    CREATE TABLE capture_cuts (
      prep_id         bigint NOT NULL REFERENCES preps (id) ON DELETE CASCADE,
      capture         text NOT NULL,
      flows_hash      text REFERENCES flows_documents (hash),
      broke           text,
      cut_at          timestamptz NOT NULL DEFAULT now(),
      cut_fingerprint text,
      PRIMARY KEY (prep_id, capture)
    )
  `
  yield* sql`CREATE INDEX capture_cuts_capture_idx ON capture_cuts (capture, prep_id DESC)`
  yield* sql`
    CREATE TABLE cut_cases (
      prep_id         bigint NOT NULL,
      capture         text NOT NULL,
      case_id         text NOT NULL,
      case_hash       text NOT NULL REFERENCES cases (hash),
      egress_endpoint text,
      call_key        text NOT NULL,
      PRIMARY KEY (prep_id, capture, case_id),
      FOREIGN KEY (prep_id, capture) REFERENCES capture_cuts (prep_id, capture) ON DELETE CASCADE
    )
  `
  yield* sql`CREATE INDEX cut_cases_call_key_idx ON cut_cases (capture, call_key)`
  yield* sql`
    CREATE INDEX cut_cases_capture_case_prep_idx
    ON cut_cases (capture, case_id, prep_id DESC) INCLUDE (call_key)
  `
  yield* sql`CREATE INDEX cut_cases_case_idx ON cut_cases (case_id text_pattern_ops)`
  yield* sql`CREATE INDEX cut_cases_key_idx ON cut_cases (call_key text_pattern_ops)`
  yield* sql`
    CREATE TABLE cut_refusals (
      prep_id  bigint NOT NULL,
      capture  text NOT NULL,
      case_id  text NOT NULL,
      call_ids text[] NOT NULL,
      detail   text NOT NULL,
      reasons  text[] NOT NULL,
      PRIMARY KEY (prep_id, capture, case_id),
      FOREIGN KEY (prep_id, capture) REFERENCES capture_cuts (prep_id, capture) ON DELETE CASCADE
    )
  `
  yield* sql`CREATE INDEX cut_refusals_case_idx ON cut_refusals (case_id text_pattern_ops)`
  yield* sql`
    CREATE TABLE cut_refusal_calls (
      prep_id bigint NOT NULL,
      capture text NOT NULL,
      call_id text NOT NULL,
      case_id text NOT NULL,
      PRIMARY KEY (prep_id, capture, call_id, case_id),
      FOREIGN KEY (prep_id, capture, case_id)
        REFERENCES cut_refusals (prep_id, capture, case_id) ON DELETE CASCADE
    )
  `
  yield* sql`CREATE INDEX cut_refusal_calls_call_idx ON cut_refusal_calls (call_id)`

  yield* sql`
    CREATE TABLE population (
      capture        text PRIMARY KEY,
      source_path    text,
      added_at       timestamptz NOT NULL DEFAULT now(),
      ingested_at    timestamptz,
      extractor_hash text,
      ingest_failed  text
    )
  `
  yield* currentPopulation
  yield* views
})

/**
 * What outlives the cuts and the runs it was read from:
 * - `case_ids`, the FROZEN case ids: each (capture, case id) names one call
 *   key for good, and each call key of a capture one case id (`case-ids.ts`).
 * - `case_maps`, every prep's case id → call map, kept after the prep's cut
 *   rows go (`case-maps.ts`); `runs.prep_id` stops referencing `preps` so a
 *   kept run outlives the cut it read.
 * - `citations` and `cell_summaries`: the runs, jobs, sweeps, preps and case
 *   ids an outside record cites, and one row per cell of every cited run
 *   (`citations.ts`).
 * - `pins`: the runs kept by hand (`pins.ts`).
 * - `cut_refusals.call_key`, the call a refusal names; `runs.cleanup`, what the
 *   lane's post-run cleanup gate found on the batch the run's cells ran in;
 *   `jobs.settled`, the units a job decided with nothing to land.
 */
const frozenIds = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE case_ids (
      capture   text NOT NULL,
      case_id   text NOT NULL,
      call_key  text NOT NULL,
      origin    text NOT NULL,
      frozen_at timestamptz NOT NULL DEFAULT now(),
      PRIMARY KEY (capture, case_id),
      UNIQUE (capture, call_key)
    )
  `
  yield* sql`CREATE INDEX case_ids_case_idx ON case_ids (case_id text_pattern_ops)`
  yield* sql`CREATE INDEX case_ids_key_idx ON case_ids (call_key text_pattern_ops)`
  yield* sql`
    CREATE TABLE case_maps (
      prep_id  bigint NOT NULL,
      capture  text NOT NULL,
      case_id  text NOT NULL,
      kind     text NOT NULL CHECK (kind IN ('case', 'refused')),
      call_key text,
      call_ids text[] NOT NULL DEFAULT '{}',
      reasons  text[] NOT NULL DEFAULT '{}',
      cut_at   timestamptz NOT NULL,
      first_at timestamptz,
      src      text,
      dst      text,
      PRIMARY KEY (prep_id, capture, case_id)
    )
  `
  yield* sql`CREATE INDEX case_maps_capture_idx ON case_maps (capture, case_id, prep_id DESC)`
  yield* sql`CREATE INDEX case_maps_case_idx ON case_maps (case_id text_pattern_ops)`
  yield* sql`CREATE INDEX case_maps_key_idx ON case_maps (call_key text_pattern_ops)`
  yield* sql`CREATE INDEX case_maps_call_ids_idx ON case_maps USING gin (call_ids)`
  yield* sql`
    CREATE TABLE citations (
      kind     text NOT NULL CHECK (kind IN ('run', 'job', 'sweep', 'prep', 'case')),
      ref      text NOT NULL,
      cited_by text NOT NULL,
      cited_on date,
      PRIMARY KEY (kind, ref, cited_by)
    )
  `
  yield* sql`
    CREATE TABLE cell_summaries (
      run_id      bigint NOT NULL,
      case_id     text NOT NULL,
      capture     text NOT NULL,
      lane        text NOT NULL,
      call_key    text,
      passed      boolean NOT NULL,
      verdict     text NOT NULL,
      skipped     text,
      failed_step text,
      main_error  jsonb,
      components  jsonb NOT NULL DEFAULT '[]'::jsonb,
      env         text,
      branch      text,
      sweep_id    bigint,
      job_id      bigint,
      prep_id     bigint,
      started_at  timestamptz NOT NULL,
      PRIMARY KEY (run_id, case_id)
    )
  `
  yield* sql`CREATE INDEX cell_summaries_case_idx ON cell_summaries (case_id text_pattern_ops)`
  yield* sql`
    CREATE TABLE pins (
      run_id    bigint PRIMARY KEY REFERENCES runs (id) ON DELETE CASCADE,
      ticket    text,
      pinned_at timestamptz NOT NULL DEFAULT now()
    )
  `
  yield* sql`ALTER TABLE cut_refusals ADD COLUMN call_key text`
  yield* sql`ALTER TABLE runs ADD COLUMN cleanup text`
  yield* sql`ALTER TABLE jobs ADD COLUMN settled integer`
  yield* sql`ALTER TABLE runs DROP CONSTRAINT IF EXISTS runs_prep_id_fkey`
})

/** The key of the migration that freezes case ids and keeps what outlives a cut. */
export const FROZEN_IDS = "2_frozen_ids"

/**
 * `current_documents`, for SQL surveys of the documents' shapes: one row per
 * case of each capture's standing cut (its newest reading), with its call key
 * and its case document and features.
 */
const currentDocuments = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE VIEW current_documents AS
    SELECT c.capture, c.case_id, c.call_key, c.prep_id, d.hash AS document_hash, d.document, d.features
    FROM (SELECT DISTINCT ON (capture) capture, prep_id FROM capture_cuts ORDER BY capture, prep_id DESC) s
    JOIN cut_cases c ON c.prep_id = s.prep_id AND c.capture = s.capture
    JOIN cases k ON k.hash = c.case_hash
    JOIN case_documents d ON d.hash = k.document_hash
  `
})

/** The key of the migration that states the current documents' view. */
export const CURRENT_DOCUMENTS = "3_current_documents"

/**
 * `run_summaries`: one row per cited run — its capture, lane, status, detail,
 * identity and code — so a cited run outlives its row whether or not it holds
 * cells (`citations.ts`).
 */
const runSummaries = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE run_summaries (
      run_id      bigint PRIMARY KEY,
      capture     text NOT NULL,
      lane        text NOT NULL,
      set_kind    text NOT NULL,
      status      text NOT NULL,
      detail      text,
      cases       integer NOT NULL,
      failed      integer NOT NULL,
      env         text,
      branch      text,
      components  jsonb NOT NULL DEFAULT '[]'::jsonb,
      sweep_id    bigint,
      job_id      bigint,
      prep_id     bigint,
      started_at  timestamptz NOT NULL,
      finished_at timestamptz
    )
  `
})

/** The key of the migration that keeps a summary of every cited run. */
export const RUN_SUMMARIES = "4_run_summaries"

/**
 * `frozen_captures`, the captures whose earlier readings' ids are all frozen;
 * and, for a store whose readings predate the frozen ids, every reading copied
 * into the case maps and every keyed id of them frozen, newest reading first
 * (`case-ids.ts`). What it cannot key is the deployment's to seed: no capture
 * is marked seeded here.
 */
const seedFrozenIds = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE frozen_captures (
      capture    text PRIMARY KEY,
      seeded_at  timestamptz NOT NULL DEFAULT now()
    )
  `
  yield* exportReadings
  yield* seedFromReadings
})

/** The key of the migration that seeds the frozen ids from the readings a store already holds. */
export const SEED_FROZEN_IDS = "5_seed_frozen_ids"

/**
 * `id_gaps`: the ids a sequence skipped when the wipe raised it (`wipe.ts`
 * `idFloor`), `[first, next)`: ids this store never issued, so a citation of
 * one names another store's row.
 */
const idGaps = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`
    CREATE TABLE id_gaps (
      sequence   text   NOT NULL,
      first      bigint NOT NULL,
      next       bigint NOT NULL,
      skipped_at timestamptz NOT NULL DEFAULT now(),
      PRIMARY KEY (sequence, first)
    )
  `
})

/** The key of the migration that records the ids a raised sequence skipped. */
export const ID_GAPS = "6_id_gaps"

/**
 * A cut case the frozen ids do not name — its (capture, case id, call key)
 * absent from `case_ids` when its transaction commits — is refused: what a
 * writer that predates the frozen ids would write, naming calls by position.
 * Checked at commit, so `Cuts.record` freezes a new call's id after its row.
 */
const frozenCutGuard = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql.unsafe(`
    CREATE FUNCTION cut_cases_frozen() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      IF EXISTS (
           SELECT 1 FROM cut_cases
           WHERE prep_id = NEW.prep_id AND capture = NEW.capture AND case_id = NEW.case_id
         )
         AND NOT EXISTS (
           SELECT 1 FROM case_ids
           WHERE capture = NEW.capture AND case_id = NEW.case_id AND call_key = NEW.call_key
         ) THEN
        RAISE EXCEPTION 'cut case % of % (call %) is not frozen: the cut writing it predates the frozen case ids; rebase it onto the store code that freezes them',
          NEW.case_id, NEW.capture, NEW.call_key
          USING ERRCODE = 'integrity_constraint_violation';
      END IF;
      RETURN NULL;
    END
    $$
  `)
  yield* sql.unsafe(`
    CREATE CONSTRAINT TRIGGER cut_cases_frozen AFTER INSERT ON cut_cases
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION cut_cases_frozen()
  `)
})

/** The key of the migration that refuses a cut case the frozen ids do not name. */
export const FROZEN_CUT_GUARD = "7_frozen_cut_guard"

/**
 * A scripted party's violation the document does not state is a SOURCE FAULT:
 * information that fails no cell and is never a main error.
 * `case_results.error_source` holds a case's source fault, passed or failed
 * (`CaseResult.sourceFault`, `MainError.source`), null where it has none, and
 * the cell views carry it. A row whose main error names the class under its
 * former token keeps the fault there, its main error unmeasured; a summary's
 * main error naming it is unmeasured too; the `rfc` rule counts file it under
 * `source-fault`.
 */
const sourceFault = Effect.gen(function*() {
  const sql = yield* SqlClient
  yield* sql`ALTER TABLE case_results ADD COLUMN error_source jsonb`
  yield* sql`
    UPDATE case_results
    SET error_source = jsonb_build_object('rule', error_rule, 'rfc', error_rfc, 'detail', coalesce(error_detail, '')),
        error_kind = NULL, error_rule = NULL, error_rfc = NULL, error_step = NULL, error_expected = NULL,
        error_detail = NULL
    WHERE error_kind = 'harness-fault'
  `
  yield* sql`UPDATE cell_summaries SET main_error = NULL WHERE main_error->>'kind' = 'harness-fault'`
  yield* sql`UPDATE run_rule_hits SET bucket = 'source-fault' WHERE family = 'rfc' AND bucket = 'harness-fault'`
  yield* viewsOf(CELL_COLUMNS)
})

/** The key of the migration that names the source fault and keeps it beside a main error. */
export const SOURCE_FAULT = "8_source_fault"

/**
 * The migration record, static so `tsx` and a bundler both resolve it: the
 * filesystem loader would make the package's own layout a runtime dependency.
 */
export const record: Record<string, Effect.Effect<void, unknown, SqlClient>> = {
  [BASELINE]: baseline,
  [FROZEN_IDS]: frozenIds,
  [CURRENT_DOCUMENTS]: currentDocuments,
  [RUN_SUMMARIES]: runSummaries,
  [SEED_FROZEN_IDS]: seedFrozenIds,
  [ID_GAPS]: idGaps,
  [FROZEN_CUT_GUARD]: frozenCutGuard,
  [SOURCE_FAULT]: sourceFault
}

export const loader: Migrator.Loader = Migrator.fromRecord(record)

/**
 * Records the baseline as applied without running it, for a store whose
 * schema a deployment's own migrations already brought to the baseline's
 * shape: the store's later migrations then apply on top of it as on any other.
 */
export const adoptBaseline = Migrator.make({})({
  loader: Migrator.fromRecord({ [BASELINE]: Effect.void }),
  table: TABLE
})
