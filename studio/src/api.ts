// The Studio API client: POST /studio/api/<op> with the session token, or with chronos studio --login the
// session cookie of a role signed in (src/studio.rs).

export type Row = Record<string, unknown>;

export interface World {
  name: string;
  id: string;
  parent: string | null;
  depth: number;
  created: number;
  version: number;
  flagged: boolean;
  meta: unknown;
  owner: string | null;
  expires: number | null;
  active: number;
  pinned: boolean;
  checkpoints: { name: string; at: number }[];
  /** rows changed since its fork (not for main) */
  changes?: number | null;
}

export interface Table {
  name: string;
  rows: number | null;
  /** made with CREATE TABLE; false for a table made by writing JSON rows */
  schema: boolean;
  /** a view or a materialized view (read only here); absent for a table */
  kind?: 'view' | 'materialized view';
}

/** How far back time travel reads (the status op's `history`). */
export interface HistoryWindow {
  /** history_retention, in ms */
  retention: number;
  /** the earliest moment kept (ms since 1970); null: none yet (or in memory) */
  from: number | null;
}

export interface SqlResult {
  command: string;
  columns: string[];
  /** each column's Postgres type name */
  types?: string[];
  rows: Row[];
}

/** Something in the database that reads columns a change changed (a view, a trigger, a key...). */
export interface Reader {
  table: string;
  columns: string[];
  kind: string;
  reader: string;
  detail: string;
  /** marked critical: a merge policy holds agents' merges changing what it reads */
  critical: boolean;
}

export interface Change {
  key: string;
  before: Row | null;
  after: Row | null;
  columns: string[] | null;
}

export interface Agent {
  name: string;
  can: string[];
  created: number;
  disabled: boolean;
  [quota: string]: unknown;
}

/** An error reply: `detail` is the server's `error` (a message, or an object with more). */
export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
    public detail: any,
  ) {
    super(message);
  }
}

const KEY = 'chronos-studio-token';

/** The token from the URL's #t=... (then taken out of the address bar), or this tab's saved one. */
function readToken(): string | null {
  const m = location.hash.match(/[#&]t=([0-9a-f]{64})/);
  if (m) {
    try {
      sessionStorage.setItem(KEY, m[1]);
    } catch {
      /* private mode: keep it in memory */
    }
    history.replaceState(null, '', location.pathname);
    return m[1];
  }
  try {
    return sessionStorage.getItem(KEY);
  } catch {
    return null;
  }
}

export const token = readToken();
// a new link pasted into this tab (same page, new #t=...): start over with it
addEventListener('hashchange', () => /[#&]t=[0-9a-f]{64}/.test(location.hash) && (readToken(), location.reload()));

type Listener = (ok: boolean) => void;
const listeners = { conn: new Set<Listener>(), gate: new Set<() => void>(), out: new Set<() => void>() };
export const onConnection = (f: Listener) => (listeners.conn.add(f), () => void listeners.conn.delete(f));
export const onGate = (f: () => void) => (listeners.gate.add(f), () => void listeners.gate.delete(f));
/** A call answered 401 (chronos studio --login): the session ended, or its role may no longer log in. */
export const onSignedOut = (f: () => void) => (listeners.out.add(f), () => void listeners.out.delete(f));

/** A role signed in to a Studio started with --login. */
export interface User {
  name: string;
  superuser: boolean;
}
/** How this Studio lets people in: the key in the link, or signing in with a database role (--login). */
export interface Who {
  mode: 'key' | 'login';
  user: User | null;
}
/** The mode once known: in login mode calls carry the session's cookie, never a key. */
let mode: Who['mode'] | null = null;

export function errText(e: unknown): string {
  if (e == null) return 'error';
  if (typeof e === 'string') return e;
  const o = e as { message?: string; code?: string };
  if (o.message) return o.message + (o.code ? ` (SQLSTATE ${o.code})` : '');
  return JSON.stringify(e);
}

export async function api<T = any>(op: string, args: object = {}): Promise<T> {
  let r: Response;
  const login = mode === 'login';
  try {
    r = await fetch('/studio/api/' + op, {
      method: 'POST',
      headers: login ? { 'Content-Type': 'application/json' } : { 'Content-Type': 'application/json', 'X-Studio-Token': token || '' },
      body: JSON.stringify(args),
      cache: 'no-store',
      // (before the mode is known too: whoami must see a session's cookie)
      credentials: mode === 'key' ? 'omit' : 'same-origin',
    });
  } catch {
    listeners.conn.forEach((f) => f(false));
    throw new ApiError('Studio is not reachable: is chronos studio still running?', 0, null);
  }
  listeners.conn.forEach((f) => f(true));
  let body: { error?: unknown } & Record<string, unknown>;
  try {
    body = await r.json();
  } catch {
    body = { error: `bad reply (${r.status})` };
  }
  if (!r.ok) {
    const msg = errText(body.error);
    // the server's own refusals (not an operation's): the page needs its session link
    if (r.status === 403 && /X-Studio-Token|Host|other web pages/.test(msg)) listeners.gate.forEach((f) => f());
    // signed out (the session ended, or its role was dropped or made NOLOGIN): the sign-in screen
    if (r.status === 401 && login && op !== 'login') listeners.out.forEach((f) => f());
    throw new ApiError(msg, r.status, body.error);
  }
  return body as T;
}

/** Which sign-in this Studio takes, and who is signed in (asked with no key or session too). */
export async function whoami(): Promise<Who> {
  const w = await api<Who>('whoami');
  mode = w.mode;
  return w;
}

/** Signs in with a database role and its password (chronos studio --login): the server sets the session's cookie. */
export async function signIn(user: string, password: string): Promise<User> {
  mode = 'login';
  const w = await api<Who>('login', { user, password });
  return w.user!;
}

export const signOut = () => api('logout').catch(() => {});

/** Runs SQL (one or more statements) in a world: one result per statement. */
export const sql = (q: string, world: string) =>
  api<{ results: SqlResult[] }>('sql', { sql: q, branch: world }).then((r) => r.results);
