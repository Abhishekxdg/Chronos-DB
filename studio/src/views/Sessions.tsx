// Sessions: the Postgres port's connections, as pg_stat_activity shows them, refreshed every 2 s while the page is
// in view. A session waiting for a row shows whose transaction holds it (pg_blocking_pids) and the table; long
// transactions and idle ones are marked. Cancel query and Terminate run pg_cancel_backend and
// pg_terminate_backend, with their checks: a refusal shows the server's error.
import { useEffect, useRef, useState } from 'react';
import { api, sql, type Agent } from '../api';
import { Empty, ErrorBox, toast, useDialogs } from '../ui';
import { fmtTime, n, plural } from '../util';
import './screens.css';

interface Session {
  pid: number;
  user: string;
  application_name: string;
  client_addr: string | null;
  client_port: number | null;
  world: string;
  state: string;
  query: string;
  backend_start: number;
  xact_start: number | null;
  query_start: number | null;
  state_change: number;
  wait_event_type: string | null;
  wait_event: string | null;
  blocking_pids: number[];
  waiting_on: string | null;
}

/** A transaction open longer than this is marked long (it holds history and row locks back). */
const LONG_MS = 60_000;
const EVERY_MS = 2000;

/** How long, from `ms` milliseconds: 4 s, 3 min 2 s, 2 h 5 min. */
export function took(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${s % 60} s`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
  return `${Math.floor(s / 86400)} d ${Math.floor((s % 86400) / 3600)} h`;
}

export function SessionsView() {
  const dlg = useDialogs();
  const [rows, setRows] = useState<{ sessions: Session[]; now: number } | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [agents, setAgents] = useState<Set<string>>(new Set());
  const [paused, setPaused] = useState(false);
  const busy = useRef(false);
  const load = async () => {
    if (busy.current || document.hidden) return;
    busy.current = true;
    try {
      setRows(await api<{ sessions: Session[]; now: number }>('sessions'));
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      busy.current = false;
    }
  };
  useEffect(() => {
    api<{ agents: Agent[] }>('agents').then((r) => setAgents(new Set(r.agents.map((a) => a.name))), () => {});
  }, []);
  useEffect(() => {
    if (paused) return;
    load();
    const t = window.setInterval(load, EVERY_MS);
    const seen = () => !document.hidden && load();
    document.addEventListener('visibilitychange', seen);
    return () => (clearInterval(t), document.removeEventListener('visibilitychange', seen));
  }, [paused]); // eslint-disable-line

  const signal = async (s: Session, terminate: boolean) => {
    if (
      terminate &&
      !(await dlg.confirm(
        `Terminate session ${s.pid}?`,
        `Its connection closes and its open transaction${s.xact_start ? ' rolls back' : ''}. ${s.user}'s program sees the connection drop.`,
        'Terminate',
        { danger: true },
      ))
    )
      return;
    const f = terminate ? 'pg_terminate_backend' : 'pg_cancel_backend';
    try {
      const [r] = await sql(`select ${f}(${Number(s.pid)}) as ok`, 'main');
      const ok = r?.rows[0]?.ok === true;
      toast(ok ? (terminate ? `Terminated ${s.pid}` : `Asked ${s.pid} to cancel its query`) : `${s.pid} is gone`);
      load();
    } catch (e) {
      // (the server's refusal, with its SQLSTATE: 42501 for a role that may not signal it)
      toast((e as Error).message);
    }
  };

  const list = rows?.sessions || [];
  const now = rows?.now || Date.now();
  const waiting = list.filter((s) => s.blocking_pids.length).length;
  return (
    <div className="scroll">
      <div className="pad stack">
        <div className="head-row">
          <h2 className="ident">Sessions</h2>
          <span className="muted">connections over the Postgres port{paused ? ', paused' : ', refreshed every 2 s'}</span>
          <span className="spacer" />
          {rows && (
            <span className="note">
              {plural(list.length, 'session')}
              {waiting ? ` · ${n(waiting)} waiting` : ''}
            </span>
          )}
          <button className="btn" type="button" onClick={() => setPaused(!paused)}>
            {paused ? 'Resume' : 'Pause'}
          </button>
        </div>
        {error ? <ErrorBox error={error} /> : null}
        {!rows ? (
          !error && <p className="note">Loading…</p>
        ) : !list.length ? (
          <Empty>
            No connections. Programs and psql that connect to the Postgres port (<code>chronos serve</code>, port 5433 by default) show here.
          </Empty>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th className="num">PID</th>
                  <th>User</th>
                  <th>Application</th>
                  <th>Client</th>
                  <th>World</th>
                  <th>State</th>
                  <th>Waiting for</th>
                  <th>Query</th>
                  <th>Running</th>
                  <th>Transaction</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {list.map((s) => {
                  const tx = s.xact_start ? now - s.xact_start : null;
                  const idleTx = s.state.startsWith('idle in transaction');
                  const long = tx != null && tx > LONG_MS;
                  const waits = s.blocking_pids.length > 0;
                  return (
                    <tr key={s.pid} className={waits ? 'waits' : idleTx ? 'idle-tx' : ''}>
                      <td className="num">{s.pid}</td>
                      <td className="mono nowrap">
                        {s.user}
                        {agents.has(s.user) && (
                          <span className="tag" title="An agent: its rights are its capabilities">
                            agent
                          </span>
                        )}
                      </td>
                      <td className="mono">{s.application_name}</td>
                      <td className="mono nowrap">{s.client_addr ? `${s.client_addr}${s.client_port != null && s.client_port >= 0 ? ':' + s.client_port : ''}` : 'local'}</td>
                      <td className="mono">{s.world}</td>
                      <td className="nowrap">
                        <span className={'tag' + (idleTx ? ' bad' : s.state === 'active' ? ' ok' : '')}>{s.state}</span>
                      </td>
                      <td className="nowrap">
                        {waits && (
                          <span
                            className="tag warn"
                            title={`${s.wait_event_type}: ${s.wait_event}. pg_blocking_pids(${s.pid}) = {${s.blocking_pids.join(',')}}`}
                          >
                            {s.wait_event === 'advisory' ? 'lock held by ' : 'row held by '}
                            {s.blocking_pids.join(', ')}
                            {s.waiting_on ? ` on ${s.waiting_on}` : ''}
                          </span>
                        )}
                      </td>
                      <td className="mono query" title={s.query}>
                        {s.query}
                      </td>
                      <td className="nowrap" title={s.query_start ? `started ${fmtTime(s.query_start)}` : ''}>
                        {s.state === 'active' && s.query_start ? took(now - s.query_start) : ''}
                      </td>
                      <td className="nowrap" title={s.xact_start ? `began ${fmtTime(s.xact_start)}` : 'no transaction open'}>
                        {tx != null && (
                          <>
                            {took(tx)}
                            {long && (
                              <span className="tag warn" title="Open over a minute: it holds its row locks, and the history it reads, until it ends">
                                long
                              </span>
                            )}
                          </>
                        )}
                      </td>
                      <td className="nowrap num">
                        <button className="btn small ghost" type="button" disabled={s.state !== 'active'} title="pg_cancel_backend: stop the statement it is running" onClick={() => signal(s, false)}>
                          Cancel query
                        </button>
                        <button className="btn small ghost danger" type="button" title="pg_terminate_backend: close the connection" onClick={() => signal(s, true)}>
                          Terminate
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <p className="note">
          The same as <code>select * from pg_stat_activity</code> with <code>pg_blocking_pids(pid)</code> in SQL. Studio cancels and terminates as the
          database’s owner; an agent may do neither.
        </p>
      </div>
    </div>
  );
}
