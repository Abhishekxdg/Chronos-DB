// Chronos DB client for TypeScript/JavaScript: Node 18+, Next.js, browsers and edge runtimes.
// Talks to `chronos serve <folder>`; one file, no dependencies.
//
//   const db = new Chronos("http://127.0.0.1:7070", { token: process.env.CHRONOS_TOKEN });
//   const agent = await db.fork("agent-7");            // instant copy of main
//   await agent.put("users", "1", { name: "Ada L." });
//   console.log(await agent.diff());
//   await agent.merge();                                // or agent.discard()

export type Row = Record<string, unknown>;

export interface Hit<R = Row> {
  id: string;
  score: number;
  row: R;
}

export interface Found<R = Row> {
  total: number; // rows passing the filters
  hits: Hit<R>[];
  nextOffset: number | null; // pass as `offset` for the next page
}

export interface FindOptions {
  where?: Record<string, unknown>; // exact match on top-level fields
  text?: string; // BM25 over string fields, typo-tolerant
  vector?: number[]; // with vectorField: rank by dot product
  vectorField?: string;
  limit?: number;
  offset?: number;
}

/** A branch: its world ID, lineage, writes since its fork, and metadata. */
export interface World {
  name: string;
  id: string;
  parent: string | null;
  depth: number;
  created: number; // ms since 1970
  version: number;
  flagged: boolean;
  meta: Record<string, unknown>;
}

export interface Change<R = Row> {
  key: string;
  before: R | null;
  after: R | null;
}

export interface Conflict<R = Row> {
  key: string;
  base: R | null;
  ours: R | null;
  theirs: R | null;
}

/** How merge() and preview() settle and pick rows. */
export interface MergeOptions {
  columns?: boolean;
  picks?: Record<string, unknown>;
  /** another branch to merge into instead of the parent */
  into?: string;
  /** merge only these tables */
  onlyTables?: string[];
  /** merge only these rows ("table/id") */
  onlyKeys?: string[];
}

/** One world of a simulation, best first (see `simulate`). */
export interface SimWorld {
  world: string;
  id: string;
  index: number;
  seed: number; // $2: what its random() was seeded with
  score: number | null;
  error: string | null; // why its script or score failed
  kept: boolean;
}

export interface SimulateOptions {
  keep?: number | "all"; // worlds to keep, best first (default all)
  order?: "desc" | "asc"; // desc: the highest score is best (default)
  seed?: number; // seeds random() in every world (default 0)
  threads?: number; // default: every free core
}

/** An error from the server. `status` is 400/404/409/...; merge conflicts come back as data. */
export class ChronosError extends Error {
  status: number;
  conflicts: Conflict[];

  constructor(status: number, message: string, conflicts: Conflict[] = []) {
    super(message);
    this.name = "ChronosError";
    this.status = status;
    this.conflicts = conflicts;
  }
}

export class Chronos {
  private url: string;
  private headers: Record<string, string>;
  private opts: { token?: string; branch?: string };
  // the branch version our last write returned; merge sends it so writes lost in a crash are caught
  private version: number | null = null;

  constructor(url = "http://127.0.0.1:7070", opts: { token?: string; branch?: string } = {}) {
    this.opts = opts;
    this.url = url.replace(/\/$/, "");
    this.headers = { "Content-Type": "application/json" };
    if (opts.token) this.headers.Authorization = `Bearer ${opts.token}`;
  }

  /** The same client, working on another branch. */
  branch(name: string): Chronos {
    return new Chronos(this.url, { ...this.opts, branch: name });
  }

  get branchName(): string {
    return this.opts.branch ?? "main";
  }

  private async call<T>(op: string, body: Record<string, unknown> = {}): Promise<T> {
    const res = await fetch(`${this.url}/v1/${op}`, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ branch: this.opts.branch, ...body }),
      redirect: "error", // the server never redirects; following one would send the token along
    });
    const text = await res.text();
    let data;
    try {
      data = JSON.parse(text);
    } catch {
      throw new ChronosError(res.status, text.trim() || res.statusText); // not ours: a proxy's page, say
    }
    if (!res.ok) {
      const e = data.error;
      if (typeof e === "object" && e !== null) throw new ChronosError(res.status, e.message, e.conflicts ?? []);
      throw new ChronosError(res.status, String(e));
    }
    return data as T;
  }

  /** The row, or null if there is none. */
  async get<R = Row>(table: string, id: string): Promise<R | null> {
    try {
      return (await this.call<{ row: R }>("get", { table, id })).row;
    } catch (e) {
      if (e instanceof ChronosError && e.status === 404 && e.message.startsWith("no row")) return null;
      throw e;
    }
  }

  async put(table: string, id: string, record: Row): Promise<void> {
    this.version = (await this.call<{ version: number }>("put", { table, id, record })).version;
  }

  async delete(table: string, id: string): Promise<void> {
    this.version = (await this.call<{ version: number }>("delete", { table, id })).version;
  }

  /** Writes all rows or none; `record: null` deletes. */
  async batch(rows: { table: string; id: string; record: Row | null }[]): Promise<number> {
    const r = await this.call<{ count: number; version: number }>("batch", { rows });
    this.version = r.version;
    return r.count;
  }

  async find<R = Row>(table: string, o: FindOptions = {}): Promise<Found<R>> {
    const r = await this.call<{ total: number; hits: Hit<R>[]; next_offset: number | null }>("find", {
      table,
      where: o.where,
      text: o.text,
      vector: o.vector,
      vector_field: o.vectorField,
      limit: o.limit,
      offset: o.offset,
    });
    return { total: r.total, hits: r.hits, nextOffset: r.next_offset };
  }

  /** Creates a branch from this one (instant) and returns a client working on it. */
  async fork(name: string, meta?: Record<string, unknown>): Promise<Chronos> {
    await this.call("fork", { name, from: this.branchName, meta });
    const b = this.branch(name);
    b.version = 0;
    return b;
  }

  /** Runs SQL on this branch; `params` fill $1, $2, ... Returns the last statement's command
   *  (e.g. "INSERT 0 1") and rows as objects. Throws ChronosError with `code`, the SQLSTATE. */
  async sql<R = Row>(sql: string, params: unknown[] = []): Promise<{ command: string; rows: R[] }> {
    const r = await this.call<{ results: { command: string; rows: R[] }[]; version: number | null }>("sql", {
      sql,
      params,
    });
    if (r.version != null) this.version = r.version;
    return r.results[r.results.length - 1] ?? { command: "", rows: [] };
  }

  async branches(): Promise<World[]> {
    return (await this.call<{ branches: World[] }>("branches")).branches;
  }

  /** This branch as it was at `when` (a time like "2026-09-20 10:00", or "-5 minutes"): reads the past. */
  at(when: string): Chronos {
    return this.branch(`${this.branchName}@${when}`);
  }

  /** What happened to this branch, newest first. */
  async history(limit = 100): Promise<{ at: number; world: string; event: string; rows: number }[]> {
    return (await this.call<{ events: { at: number; world: string; event: string; rows: number }[] }>("history", { branch: this.branchName, limit })).events;
  }

  /** Puts this branch back as it was at `at`; returns rows written. */
  async restore(at: string): Promise<number> {
    return (await this.call<{ restored: number }>("restore", { branch: this.branchName, at })).restored;
  }

  /** Undoes the latest merge of `world` into its parent (within the history window). */
  async undoMerge(world: string, opts: { skipChanged?: boolean } = {}): Promise<{ parent: string; at: number; undone: number; skipped: number }> {
    return this.call("undo_merge", { branch: world, skip_changed: opts.skipChanged });
  }

  /** This branch's ID, lineage and metadata. */
  async world(): Promise<World> {
    return this.call<World>("world", { branch: this.branchName });
  }

  /** Merges `meta` into this branch's metadata (null removes a key); returns the branch. */
  async setMeta(meta: Record<string, unknown>): Promise<World> {
    return this.call<World>("set_meta", { branch: this.branchName, meta });
  }

  /** What changed on this branch since it was forked. */
  async diff<R = Row>(): Promise<Change<R>[]> {
    return (await this.call<{ changes: Change<R>[] }>("diff", { branch: this.branchName })).changes;
  }

  /** How many rows changed on this branch, per table, without reading them. */
  async diffCount(): Promise<{ total: number; tables: Record<string, number> }> {
    return this.call("diff", { branch: this.branchName, count: true });
  }

  /** Up to `limit` changes after cursor `after`; pass `next` back as `after` for the next page
   *  (null on the last one). Reads only this page, so it suits big diffs. */
  async diffPage<R = Row>(limit: number, after?: string): Promise<{ changes: Change<R>[]; next: string | null }> {
    return this.call("diff", { branch: this.branchName, limit, after: after ?? null });
  }

  /** Applies this branch to its parent and deletes it. Throws ChronosError (409) with `conflicts`
   *  if both changed the same rows (retry with resolve "ours" or "theirs"), or if the branch lost
   *  writes in a server crash (redo them; `confirm: true` merges it as it is after a crash). */
  /** Applies this branch to its parent and deletes it. Conflicts (409) come with `explain` per row;
   *  settle them with `resolve`, `columns` (combine rows where the sides changed different columns)
   *  or `picks` ({"table/id": "ours" | "theirs" | row | null}). `onlyTables` / `onlyKeys` merge just
   *  those rows, and `into` merges into another branch instead; either way this branch stays. */
  async merge(
    resolve: "fail" | "ours" | "theirs" = "fail",
    opts: MergeOptions & { confirm?: boolean } = {},
  ): Promise<number> {
    const version = opts.confirm ? undefined : (this.version ?? undefined);
    return (
      await this.call<{ merged: number }>("merge", {
        branch: this.branchName, resolve, version, confirm: opts.confirm, ...mergeArgs(opts),
      })
    ).merged;
  }

  /** What merge() would do, row by row, changing nothing. */
  async preview(
    resolve: "fail" | "ours" | "theirs" = "fail",
    opts: MergeOptions = {},
  ): Promise<{ rows: { key: string; outcome: string; detail: string; result: unknown }[]; conflicts: number; blocked: string | null }> {
    return this.call("merge", { branch: this.branchName, resolve, ...mergeArgs(opts), dry_run: true });
  }

  async discard(): Promise<void> {
    await this.call("discard", { branch: this.branchName });
  }

  /** Forks `worlds` worlds from this branch (at one moment), runs `script` in each in parallel
   *  ($1: the world's index, $2: its seed), scores each with `score` (a SELECT giving a number)
   *  and keeps the best. The same call gives the same worlds: random() is seeded per world. */
  async simulate(
    worlds: number,
    prefix: string,
    script: string,
    score: string,
    opts: SimulateOptions = {},
  ): Promise<{ worlds: SimWorld[]; base: string; at: number; timings: Record<string, number> }> {
    return this.call("simulate", { from: this.branchName, worlds, prefix, script, score, ...opts });
  }

  /** Makes a simulated world again from its recorded inputs and says whether it's identical;
   *  `as` keeps the replay as a new branch. */
  async replay(
    world: string,
    as?: string,
  ): Promise<{ world: string; replay: string | null; identical: boolean; rows_differing: number; score: number | null; recorded_score: number | null; error: string | null }> {
    return this.call("replay", { world, as });
  }
}

function mergeArgs(o: MergeOptions): Record<string, unknown> {
  return { columns: o.columns, picks: o.picks, into: o.into, only_tables: o.onlyTables, only_keys: o.onlyKeys };
}
