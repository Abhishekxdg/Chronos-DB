// A world's history, browsing its tables as they were at any moment, and restoring the world
// to a moment (confirmed; an ordinary write, so history keeps what it replaces).
import { useEffect, useState } from 'react';
import { api } from '../api';
import { useApp, worldOf, type App } from '../context';
import { Empty, ErrorBox, toast, type Dialogs, useDialogs } from '../ui';
import { fmtTime, historyStart, momentOf, n, outsideHistory, plural, span } from '../util';

interface Event {
  at: number;
  world: string;
  event: string;
  rows: number;
}

export function HistoryView() {
  const app = useApp();
  const dlg = useDialogs();
  const [events, setEvents] = useState<Event[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [at, setAt] = useState('');
  useEffect(() => {
    api<{ events: Event[] }>('history', { branch: app.world, limit: 200 }).then((r) => setEvents(r.events), setError);
  }, [app.world]);
  // moments before the history kept can't be read or restored to: greyed, with the reason
  const outside = (ms: number) => outsideHistory(app.history, ms);
  const typed = at.trim() ? outsideHistory(app.history, momentOf(at)) : null;
  const viewAsOf = (iso: string) => {
    if (!app.table) return toast('Pick a table first: the moment applies to it');
    app.setAsOf(iso);
    app.pickTable(app.table);
  };
  return (
    <div className="scroll">
      <div className="pad stack">
        <div className="head-row">
          <h2 className="ident">{app.world}</h2>
          <span className="muted">
            history, newest first
            {app.history &&
              (app.history.from === null
                ? ` · none kept yet (history_retention: ${span(app.history.retention)})`
                : ` · kept back to ${historyStart(app.history.from)} (history_retention: ${span(app.history.retention)})`)}
          </span>
          <span className="spacer" />
          <input
            type="text"
            placeholder="-1 hour, or 2026-09-20 10:00"
            aria-label="Moment to restore to"
            size={26}
            value={at}
            onChange={(e) => setAt(e.target.value)}
          />
          <button className="btn" type="button" disabled={!at.trim() || !!typed} title={typed || undefined} onClick={() => restore(app, dlg, at.trim())}>
            Restore to…
          </button>
          <button className="btn primary" type="button" onClick={() => saveCheckpoint(app, dlg)}>
            Save checkpoint
          </button>
        </div>
        {typed && <p className="err small">{typed}</p>}
        <Checkpoints app={app} dlg={dlg} onView={viewAsOf} outside={outside} />
        {error ? (
          <ErrorBox error={error} />
        ) : !events ? (
          <p className="note">Loading…</p>
        ) : !events.length ? (
          <Empty>No history yet: writes, forks and merges show here.</Empty>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>World</th>
                  <th>Event</th>
                  <th className="num">Rows</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {events.map((e, i) => {
                  const iso = new Date(e.at).toISOString();
                  const gone = outside(e.at);
                  return (
                    <tr key={i}>
                      <td className="mono nowrap" title={iso}>
                        {fmtTime(e.at)}
                      </td>
                      <td className="mono">{e.world}</td>
                      <td>{e.event}</td>
                      <td className="num">{n(e.rows)}</td>
                      <td className="nowrap">
                        <button
                          className="btn small ghost"
                          type="button"
                          disabled={!!gone}
                          title={gone || 'Browse the tables as they were right after this'}
                          onClick={() => viewAsOf(iso)}
                        >
                          View as of
                        </button>
                        <button
                          className="btn small ghost"
                          type="button"
                          disabled={!!gone}
                          title={gone || 'Put the whole world back to this moment'}
                          onClick={() => restore(app, dlg, iso)}
                        >
                          Restore
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

async function restore(app: App, dlg: Dialogs, at: string) {
  const w = app.world;
  const live = w === 'main' ? ' This is main, the live data.' : '';
  const msg = `Every table in ${w} goes back to how it was at ${at}. It’s an ordinary write: history keeps what it replaces.${live}`;
  if (!(await dlg.confirm(`Restore ${w}?`, msg, 'Restore world', { danger: true, typed: w === 'main' ? 'main' : null }))) return;
  try {
    const r = await api<{ restored: number }>('restore', { branch: w, at });
    toast(`Restored ${w}: ${plural(r.restored, 'row')} put back`);
    await app.reloadTables();
    await app.reloadWorlds();
    app.refresh();
  } catch (e) {
    toast((e as Error).message);
  }
}

/** Names this moment of the active world, to come back to by name. */
export async function saveCheckpoint(app: App, dlg: Dialogs) {
  const name = await dlg.prompt(
    `Save a checkpoint of ${app.world}`,
    'Name',
    `before-${new Date().toISOString().slice(5, 16).replace(/[-:T]/g, '')}`,
    'Save checkpoint',
    'A name for this moment. Restoring to it later puts every table back as it is now; history keeps what that replaces.',
  );
  if (!name) return;
  try {
    await api('checkpoint', { branch: app.world, name });
    toast(`Saved checkpoint ${name}`);
    await app.reloadWorlds();
    app.refresh();
  } catch (e) {
    toast((e as Error).message);
  }
}

function Checkpoints({ app, dlg, onView, outside }: { app: App; dlg: Dialogs; onView: (iso: string) => void; outside: (ms: number) => string | null }) {
  const w = worldOf(app, app.world);
  const cps = [...(w?.checkpoints || [])].sort((a, b) => b.at - a.at);
  const back = async (name: string) => {
    const live = app.world === 'main' ? ' This is main, the live data.' : '';
    if (
      !(await dlg.confirm(
        `Restore ${app.world} to ${name}?`,
        `Every table goes back to how it was at checkpoint ${name}. It's an ordinary write: history keeps what it replaces.${live}`,
        'Restore world',
        { danger: true, typed: app.world === 'main' ? 'main' : null },
      ))
    )
      return;
    try {
      const r = await api<{ restored: number }>('restore', { branch: app.world, checkpoint: name });
      toast(`Restored ${app.world} to ${name}: ${plural(r.restored, 'row')} put back`);
      await app.reloadTables();
      await app.reloadWorlds();
      app.refresh();
    } catch (e) {
      toast((e as Error).message);
    }
  };
  return (
    <div className="sec">
      <h3 className="label">Checkpoints</h3>
      {!cps.length ? (
        <p className="note">None yet. Save one before a risky change or an agent run, then restore to it by name.</p>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <tbody>
              {cps.map((c) => (
                <tr key={c.name}>
                  <td className="mono">{c.name}</td>
                  <td className="mono nowrap muted">{fmtTime(c.at)}</td>
                  <td className="nowrap num">
                    <button
                      className="btn small ghost"
                      type="button"
                      disabled={!!outside(c.at)}
                      title={outside(c.at) || undefined}
                      onClick={() => onView(new Date(c.at).toISOString())}
                    >
                      View as of
                    </button>
                    <button className="btn small ghost" type="button" disabled={!!outside(c.at)} title={outside(c.at) || undefined} onClick={() => back(c.name)}>
                      Restore
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
