// Import from Postgres: the import `chronos import <folder> postgres://...` runs, into the open database, as a job
// on the server that this page polls (src/studio_import.rs). A database that has tables already gets the import in
// a new world, to review in Changes and merge. The password lives in this form until the import starts, goes only
// in that request's body, and is kept nowhere (not in this browser, not on the server).
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api, type ApiError } from '../api';
import { useApp } from '../context';
import { ErrorBox, Table, toast, useDialogs } from '../ui';
import { n, plural } from '../util';
import './screens.css';

interface Report {
  imported: number;
  total: number;
  rows: number;
  done: string[];
  carried: string[];
  partial: string[];
  skipped: string[];
  unenforced: string[];
  error: string | null;
}

interface Job {
  state: 'running' | 'canceling' | 'done' | 'failed' | 'canceled';
  world: string;
  forked: boolean;
  dry_run: boolean;
  database: string;
  tables_total: number;
  tables_done: number;
  rows: number;
  now: string;
  tables: { name: string; rows: number }[];
  report?: Report;
  error?: string;
}

/** The last import started here: the page shows it again when it opens. */
let lastJob: string | null = null;

const SSL: [string, string][] = [
  ['prefer', 'prefer: TLS if the server has it'],
  ['require', 'require: TLS, any certificate'],
  ['verify-full', 'verify-full: TLS, a certificate for this host'],
  ['verify-ca', 'verify-ca: TLS, a certificate from a trusted CA'],
  ['disable', 'disable: no TLS'],
];

export function PgImportView() {
  const app = useApp();
  const dlg = useDialogs();
  const [f, setF] = useState({ host: 'localhost', port: '5432', database: '', user: '', sslmode: 'prefer', include: '', dry: false, plain: false });
  const set = (k: keyof typeof f) => (v: string | boolean) => setF((x) => ({ ...x, [k]: v }));
  // (the password stays out of `f`: it is read from the field when the import starts, then the field is cleared)
  const pass = useRef<HTMLInputElement>(null);
  const [into, setInto] = useState<'new' | 'here'>('new');
  const [name, setName] = useState('');
  const [job, setJob] = useState<{ id: string; s: Job | null } | null>(lastJob ? { id: lastJob, s: null } : null);
  const [error, setError] = useState<ApiError | null>(null);
  const [starting, setStarting] = useState(false);
  const has = app.tables.length > 0;
  const running = job?.s ? job.s.state === 'running' || job.s.state === 'canceling' : !!job;

  // poll the job while it runs
  useEffect(() => {
    if (!job) return;
    let live = true;
    let t = 0;
    const poll = async () => {
      try {
        const s = await api<Job>('import_status', { job: job.id });
        if (!live) return;
        setJob({ id: job.id, s });
        if (s.state === 'running' || s.state === 'canceling') t = window.setTimeout(poll, 500);
        else {
          await app.reloadWorlds();
          await app.reloadTables();
        }
      } catch (e) {
        if (!live) return;
        lastJob = null;
        setJob(null);
        if ((e as ApiError).status !== 404) setError(e as ApiError);
      }
    };
    poll();
    return () => ((live = false), clearTimeout(t));
  }, [job?.id]); // eslint-disable-line

  const start = async () => {
    setError(null);
    if (!f.user.trim()) return toast('Give the Postgres user to read the database as');
    if (f.plain && !f.dry) {
      const ok = await dlg.confirm(
        'Send the password in the clear?',
        'If the server asks for it as plain text or MD5 over a connection without TLS, anyone on the network between here and it can read it, or log in with the MD5 hash. Allow it only on a network you trust.',
        'Allow and import',
        { danger: true },
      );
      if (!ok) return;
    }
    const password = pass.current?.value || '';
    setStarting(true);
    try {
      const r = await api<{ job: string; world: string; forked: boolean }>('import_postgres', {
        host: f.host.trim(),
        port: f.port.trim(),
        database: f.database.trim(),
        user: f.user.trim(),
        password,
        sslmode: f.sslmode,
        include: f.include,
        dry_run: f.dry,
        allow_plaintext_password: f.plain,
        world: app.world,
        new_world: into === 'new' && has ? name.trim() || null : null,
        same_world: into === 'here' || !has,
      });
      if (pass.current) pass.current.value = '';
      lastJob = r.job;
      setJob({ id: r.job, s: null });
      if (r.forked) toast(`Importing into a new world, ${r.world}`);
    } catch (e) {
      setError(e as ApiError);
    } finally {
      setStarting(false);
    }
  };
  const cancel = async () => {
    if (!job) return;
    try {
      await api('import_cancel', { job: job.id });
    } catch (e) {
      toast((e as Error).message);
    }
  };

  const s = job?.s;
  const pct = s && s.tables_total ? Math.round((s.tables_done / s.tables_total) * 100) : 0;
  const r = s?.report;
  return (
    <div className="scroll">
      <div className="pad stack">
        <div className="head-row">
          <h2 className="ident">Import from Postgres</h2>
          <span className="muted">a live Postgres database, moved in: schemas, types, tables and their rows, keys, indexes, views, triggers</span>
        </div>
        <div className="sim-form pg-form">
          <label className="field half">
            <span>Host</span>
            <input type="text" value={f.host} autoComplete="off" spellCheck={false} onChange={(e) => set('host')(e.target.value)} />
          </label>
          <label className="field">
            <span>Port</span>
            <input type="text" inputMode="numeric" value={f.port} onChange={(e) => set('port')(e.target.value)} />
          </label>
          <label className="field">
            <span>Database</span>
            <input type="text" value={f.database} placeholder="the user's name" autoComplete="off" spellCheck={false} onChange={(e) => set('database')(e.target.value)} />
          </label>
          <label className="field">
            <span>User</span>
            <input type="text" value={f.user} autoComplete="off" spellCheck={false} onChange={(e) => set('user')(e.target.value)} />
          </label>
          <label className="field">
            <span>Password</span>
            <input type="password" ref={pass} autoComplete="new-password" spellCheck={false} placeholder="none" />
          </label>
          <label className="field half">
            <span>SSL mode</span>
            <select value={f.sslmode} onChange={(e) => set('sslmode')(e.target.value)}>
              {SSL.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
          </label>
          <label className="field wide">
            <span>Schemas and tables to bring over (optional)</span>
            <input type="text" value={f.include} placeholder="all of them; or, say: sales, public.orders" spellCheck={false} onChange={(e) => set('include')(e.target.value)} />
            <small className="note">A schema brings all its tables; schema.table brings one. Separate them with commas.</small>
          </label>
          <div className="opts">
            {has && !f.dry && (
              <div className="field">
                <span>
                  {app.world} has {plural(app.tables.length, 'table')} already: import into
                </span>
                <label className="check">
                  <input type="radio" name="into" checked={into === 'new'} onChange={() => setInto('new')} /> a new world, forked from {app.world}, to review in Changes and then merge
                  (recommended)
                </label>
                {into === 'new' && (
                  <input type="text" value={name} placeholder={`import_${f.database || f.user || 'postgres'}_… (a name of your own, or this)`} spellCheck={false} onChange={(e) => setName(e.target.value)} />
                )}
                <label className="check">
                  <input type="radio" name="into" checked={into === 'here'} onChange={() => setInto('here')} /> {app.world} itself, beside its tables
                </label>
              </div>
            )}
            <label className="check">
              <input type="checkbox" checked={f.dry} onChange={(e) => set('dry')(e.target.checked)} /> Dry run: read Postgres and try every definition on an empty database, writing nothing
            </label>
            <label className="check">
              <input type="checkbox" checked={f.plain} onChange={(e) => set('plain')(e.target.checked)} /> Allow the password in the clear (or as MD5) over a connection without TLS
            </label>
            {f.plain && (
              <p className="warn-note">
                Only on a network you trust: anyone on it can read a password sent in the clear, or log in with its MD5 hash. Without this, such a server is refused,
                as <code>chronos import</code> refuses it without <code>--allow-plaintext-password</code>. SCRAM-SHA-256 never sends the password.
              </p>
            )}
          </div>
          <div className="sim-run">
            <button className="btn primary" type="button" disabled={starting || running} onClick={start}>
              {starting ? 'Connecting…' : f.dry ? 'Dry run' : 'Import'}
            </button>
            <span className="note">The password goes to the server for this import only: it isn't saved, here or there.</span>
          </div>
        </div>

        {error && <ErrorBox error={error} />}
        {job && !s && <p className="note">Starting…</p>}
        {s && (s.state === 'running' || s.state === 'canceling') && (
          <div className="progress" role="status">
            <div className="row">
              <b>
                {s.tables_total ? `Table ${n(Math.min(s.tables_done + 1, s.tables_total))} of ${n(s.tables_total)}` : 'Reading the catalog'}
              </b>
              <span className="note">
                {n(s.rows)} rows · {s.now}
              </span>
              <span className="spacer" />
              <button className="btn" type="button" disabled={s.state === 'canceling'} onClick={cancel}>
                {s.state === 'canceling' ? 'Canceling…' : 'Cancel'}
              </button>
            </div>
            <span className="sbar">
              <span style={{ width: `${pct}%` }} />
            </span>
            <span className="note">
              {s.dry_run ? 'A dry run: nothing is written.' : `Into ${s.world}${s.forked ? ', a new world' : ''}.`}
            </span>
          </div>
        )}
        {s && s.state !== 'running' && s.state !== 'canceling' && (
          <div className="sec">
            {s.state === 'failed' && <ErrorBox error={s.error} />}
            {s.state === 'canceled' && (
              <div className="banner">
                <b>Canceled.</b>
                <span className="note">
                  {plural(s.tables.length, 'table')} came over before it stopped{s.dry_run ? '' : `, in ${s.world}`}.{' '}
                  {s.forked && !s.dry_run ? 'Discard the world to drop them.' : ''}
                </span>
              </div>
            )}
            {r && (
              <>
                {r.error && <ErrorBox error={r.error} />}
                <div className="cards">
                  <div className="card">
                    <div className="k">{s.dry_run ? 'Tables it would import' : 'Tables imported'}</div>
                    <div className="v">
                      {n(r.imported)} of {n(r.total)}
                    </div>
                  </div>
                  {!s.dry_run && (
                    <div className="card">
                      <div className="k">Rows</div>
                      <div className="v">{n(r.rows)}</div>
                    </div>
                  )}
                  <div className="card">
                    <div className="k">Keys, indexes, views</div>
                    <div className="v">{n(r.done.length)}</div>
                  </div>
                  <div className="card">
                    <div className="k">In part</div>
                    <div className="v">{n(r.partial.length)}</div>
                  </div>
                  <div className="card">
                    <div className="k">Not imported</div>
                    <div className="v">{n(r.skipped.length)}</div>
                  </div>
                </div>
                {!s.dry_run && (
                  <div className="banner">
                    <span>
                      Imported into <b className="mono">{s.world}</b>
                      {s.forked ? ': a new world. Review it in Changes, then merge it, or discard it.' : '.'}
                    </span>
                    <span className="spacer" />
                    {s.world !== app.world && (
                      <button className="btn" type="button" onClick={() => app.setWorld(s.world)}>
                        Open {s.world}
                      </button>
                    )}
                    {s.forked && (
                      <button className="btn primary" type="button" onClick={() => app.setWorld(s.world).then(() => app.show('changes'))}>
                        Review changes
                      </button>
                    )}
                  </div>
                )}
                <Section title={s.dry_run ? 'Tables, with Postgres’s estimate of their rows' : 'Tables and their rows'}>
                  <Table heads={['Table', 'Rows']} rows={s.tables.map((t) => [t.name, n(t.rows)])} num={[1]} />
                </Section>
                <Section title={`Brought over (${n(r.done.length + r.carried.length)})`} items={[...r.done, ...r.carried]} />
                <Section title={`Brought over in part (${n(r.partial.length)})`} items={r.partial} />
                <Section title={`Not imported (${n(r.skipped.length)})`} items={r.skipped} />
                <p className="note">Roles, grants and passwords aren’t imported: give each program an agent (CREATE AGENT) with the rights it needs.</p>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function Section({ title, items, children }: { title: string; items?: string[]; children?: ReactNode }) {
  if (items && !items.length) return null;
  return (
    <div className="sec">
      <h3 className="label">{title}</h3>
      {children}
      {items && (
        <ul className="report-list">
          {items.map((x, i) => (
            <li key={i}>{x}</li>
          ))}
        </ul>
      )}
    </div>
  );
}
