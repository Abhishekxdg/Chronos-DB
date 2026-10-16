// Settings: how the Studio looks and reads (kept in this browser), the session's security,
// and the database itself (its facts and an integrity check).
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api, signOut, token } from '../api';
import { useApp, type Prefs } from '../context';
import { toast, useDialogs } from '../ui';
import { bytes, mod, n, store } from '../util';
import { RetentionSection } from './Retention';

export const SHORTCUTS: [string, string][] = [
  [`${mod}K`, 'Command palette: worlds, tables, actions'],
  [`${mod}1 – ${mod}6`, 'Data, Diagram, SQL, Changes, History, Search'],
  [`${mod}B`, 'Collapse or expand the sidebar'],
  [`${mod},`, 'Settings'],
  [`${mod}Enter`, 'Run the SQL'],
  ['J / K, ↓ / ↑', 'Next or previous row'],
  ['← / →', 'Move between cells'],
  ['Enter', 'Open the row (in the diagram: the table’s data)'],
  ['E', 'Edit the open row'],
  [`${mod}C`, 'Copy the selected cell'],
  ['/', 'Filter the table'],
  ['Esc', 'Close the inspector, a menu or a dialog'],
];

const SECTIONS = ['Appearance', 'Layout', 'Data and time', 'Stored in this browser', 'Keyboard', 'Session', 'History', 'Database'] as const;

function Choice<T extends string>({ value, options, onChange, label }: { value: T; options: [T, string][]; onChange: (v: T) => void; label: string }) {
  return (
    <div className="seg" role="radiogroup" aria-label={label}>
      {options.map(([v, text]) => (
        <button key={v} type="button" role="radio" aria-checked={value === v} onClick={() => onChange(v)}>
          {text}
        </button>
      ))}
    </div>
  );
}

function Setting({ title, children, note }: { title: string; note?: ReactNode; children: ReactNode }) {
  return (
    <div className="setting">
      <div className="st-text">
        <div className="st-title">{title}</div>
        {note && <div className="st-note">{note}</div>}
      </div>
      <div className="st-control">{children}</div>
    </div>
  );
}

interface Status {
  version: string;
  folder?: string;
  encrypted: boolean;
  remote: string | null;
  counters: Record<string, number>;
}

export function SettingsView() {
  const app = useApp();
  const dlg = useDialogs();
  const { prefs, setPref } = app;
  const set =
    <K extends keyof Prefs>(k: K) =>
    (v: Prefs[K]) =>
      setPref(k, v);
  const [status, setStatus] = useState<Status | null>(null);
  const [check, setCheck] = useState<{ state: 'idle' | 'running' | 'ok' | 'failed'; text: string }>({ state: 'idle', text: '' });
  const [history, setHistory] = useState(() => {
    try {
      return (JSON.parse(store.get('chronos-studio-history') || '[]') as string[]).length;
    } catch {
      return 0;
    }
  });
  const [at, setAt] = useState<string>(SECTIONS[0]);
  const body = useRef<HTMLDivElement>(null);
  useEffect(() => {
    api<Status>('status').then(setStatus, () => {});
  }, []);
  // the nav follows the section in view
  useEffect(() => {
    const el = body.current;
    if (!el) return;
    const seen = () => {
      const secs = [...el.querySelectorAll<HTMLElement>('section[data-sec]')];
      const top = el.getBoundingClientRect().top + 80;
      const cur = secs.filter((s) => s.getBoundingClientRect().top <= top).pop() || secs[0];
      if (cur) setAt(cur.dataset.sec!);
    };
    el.addEventListener('scroll', seen, { passive: true });
    return () => el.removeEventListener('scroll', seen);
  }, []);
  const go = (s: string) => {
    body.current
      ?.querySelector(`section[data-sec="${s}"]`)
      ?.scrollIntoView({ behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth', block: 'start' });
    setAt(s);
  };
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone || 'this computer';

  const verify = async () => {
    setCheck({ state: 'running', text: 'Reading every page and log record…' });
    const t0 = performance.now();
    try {
      const r = await api<{ pages: number; bytes: number; log_records: number }>('verify');
      setCheck({
        state: 'ok',
        text: `All good: ${n(r.pages)} pages (${bytes(r.bytes)}) and ${n(r.log_records)} log records checked in ${((performance.now() - t0) / 1000).toFixed(1)} s.`,
      });
    } catch (e) {
      setCheck({ state: 'failed', text: (e as Error).message });
    }
  };
  const forget = async () => {
    if (!(await dlg.confirm('Forget this tab’s key?', 'The page locks until you open the link chronos studio printed again.', 'Forget key'))) return;
    try {
      sessionStorage.removeItem('chronos-studio-token');
    } catch {
      /* nothing kept */
    }
    location.reload();
  };
  const resetAll = async () => {
    if (
      !(await dlg.confirm(
        'Reset the Studio’s settings?',
        'Theme, layout, query history and diagram positions in this browser go back to their defaults. The database is not touched.',
        'Reset settings',
        { danger: true },
      ))
    )
      return;
    try {
      for (const k of Object.keys(localStorage)) if (k.startsWith('chronos-studio')) localStorage.removeItem(k);
    } catch {
      /* private mode: nothing kept */
    }
    location.reload();
  };

  return (
    <div className="settings">
      <nav className="st-nav" aria-label="Settings sections">
        {SECTIONS.map((s) => (
          <button key={s} type="button" aria-current={at === s ? 'true' : undefined} onClick={() => go(s)}>
            {s}
          </button>
        ))}
      </nav>
      <div className="st-body" ref={body}>
        <header className="st-head">
          <h1>Settings</h1>
          <p className="muted">
            How the Studio looks and reads is kept in this browser. Nothing here changes the database except History, which sets how long it keeps its past (the integrity check only reads it).
          </p>
        </header>

        <section data-sec="Appearance">
          <h2 className="label">Appearance</h2>
          <Setting title="Theme" note="Match the system follows your computer's light or dark mode.">
            <Choice
              label="Theme"
              value={prefs.theme}
              onChange={set('theme')}
              options={[
                ['light', 'Light'],
                ['dark', 'Dark'],
                ['system', 'Match the system'],
              ]}
            />
          </Setting>
          <Setting title="Density" note="Row height in the data grid and lists. Compact fits about a quarter more rows.">
            <Choice
              label="Density"
              value={prefs.density}
              onChange={set('density')}
              options={[
                ['comfortable', 'Comfortable'],
                ['compact', 'Compact'],
              ]}
            />
          </Setting>
        </section>

        <section data-sec="Layout">
          <h2 className="label">Layout</h2>
          <Setting title="Sidebar" note={`Worlds, tables, the timeline and the outline. ${mod}B collapses or expands it anywhere.`}>
            <Choice
              label="Sidebar"
              value={prefs.explorer ? 'on' : 'off'}
              onChange={(v) => setPref('explorer', v === 'on')}
              options={[
                ['on', 'Shown'],
                ['off', 'Collapsed'],
              ]}
            />
          </Setting>
          <Setting title="Worldline" note="The strip of worlds above the views. Click a lane to switch world; drag across it to read the past.">
            <Choice
              label="Worldline"
              value={prefs.worldline ? 'on' : 'off'}
              onChange={(v) => setPref('worldline', v === 'on')}
              options={[
                ['on', 'Shown'],
                ['off', 'Hidden'],
              ]}
            />
          </Setting>
        </section>

        <section data-sec="Data and time">
          <h2 className="label">Data and time</h2>
          <Setting title="Times" note={<>How times read across the Studio. “As of” times you type without a zone are read as UTC, as in SQL.</>}>
            <Choice
              label="Times"
              value={prefs.time}
              onChange={set('time')}
              options={[
                ['local', zone],
                ['utc', 'UTC'],
              ]}
            />
          </Setting>
        </section>

        <section data-sec="Stored in this browser">
          <h2 className="label">Stored in this browser</h2>
          <Setting
            title="SQL history"
            note={history ? `${n(history)} recent ${history === 1 ? 'query' : 'queries'}, newest first, in the SQL tab's History.` : 'No queries saved yet.'}
          >
            <button
              className="btn"
              type="button"
              disabled={!history}
              onClick={() => {
                store.set('chronos-studio-history', '[]');
                setHistory(0);
                toast('Cleared the SQL history');
              }}
            >
              Clear history
            </button>
          </Setting>
          <Setting title="Diagram layout" note="Where you dragged the cards in the Diagram, and the tables you hid.">
            <button
              className="btn"
              type="button"
              onClick={() => {
                store.set(`chronos-studio-diagram:${app.folder}`, '{}');
                toast('Reset the diagram layout');
              }}
            >
              Reset layout
            </button>
          </Setting>
          <Setting title="All Studio settings" note="Everything above goes back to its default.">
            <button className="btn danger" type="button" onClick={resetAll}>
              Reset settings
            </button>
          </Setting>
        </section>

        <section data-sec="Keyboard">
          <h2 className="label">Keyboard</h2>
          <div className="keys">
            {SHORTCUTS.map(([k, v]) => (
              <div className="key-row" key={k}>
                <span>{v}</span>
                <span className="kbd">{k}</span>
              </div>
            ))}
          </div>
        </section>

        <section data-sec="Session">
          <h2 className="label">Session</h2>
          {app.user ? (
            <>
              <Setting
                title="Signed in"
                note={
                  app.user.superuser
                    ? 'A superuser role: the Studio may do everything the database’s owner may.'
                    : 'Not a superuser: the Studio reads, and SQL runs with this role’s privileges. Worlds, agents, history and imports need a superuser role.'
                }
              >
                <span className="mono muted">{app.user.name}</span>
              </Setting>
              <Setting title="Sign out" note="Ends this session on the server. A session also ends after 30 minutes unused, or 12 hours.">
                <button className="btn" type="button" onClick={() => signOut().then(() => location.reload())}>
                  Sign out
                </button>
              </Setting>
            </>
          ) : (
            <>
              <Setting
                title="Access"
                note="The Studio listens on 127.0.0.1 only, and this page talks to nothing else. Every call carries this run's key, which lives in this tab and dies when chronos studio stops."
              >
                <span className="mono muted">key …{token ? token.slice(-6) : 'none'}</span>
              </Setting>
              <Setting title="Forget the key" note="Locks this tab: open the printed link again to come back.">
                <button className="btn" type="button" onClick={forget}>
                  Forget key
                </button>
              </Setting>
            </>
          )}
        </section>

        <section data-sec="History">
          <h2 className="label">History</h2>
          <RetentionSection />
        </section>

        <section data-sec="Database">
          <h2 className="label">Database</h2>
          <div className="facts">
            {(
              [
                ['Folder', status?.folder || app.folder || '…'],
                ['Chronos', status?.version || '…'],
                ['Encrypted', status ? (status.encrypted ? 'Yes' : 'No') : '…'],
                ['Storage', status ? (status.remote ? `Remote: ${status.remote}` : 'This computer') : '…'],
                ['Worlds', n(app.worlds.length)],
                ['Tables in this world', n(app.tables.length)],
              ] as [string, string][]
            ).map(([k, v]) => (
              <div key={k}>
                <span className="muted">{k}</span>
                <span className="mono">{v}</span>
              </div>
            ))}
          </div>
          {app.admin && (
            <Setting title="Integrity check" note="Reads every page and log record to check they're intact. Safe while agents work: it only reads.">
              <button className="btn" type="button" disabled={check.state === 'running'} onClick={verify}>
                {check.state === 'running' ? 'Checking…' : 'Check integrity'}
              </button>
            </Setting>
          )}
          {check.state !== 'idle' && (
            <p className={'check-out ' + check.state} role="status">
              {check.text}
            </p>
          )}
        </section>
      </div>
    </div>
  );
}
