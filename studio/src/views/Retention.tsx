// Settings' History section: how long the database keeps its history (SHOW and ALTER SYSTEM SET
// history_retention), what that buys and costs, and the disk it takes now (SHOW DISK).
import { useEffect, useState } from 'react';
import { sql } from '../api';
import { toast, useDialogs } from '../ui';
import { bytes, lit } from '../util';
import './screens.css';

const CHOICES: [string, string][] = [
  ['1 hour', '1 hour'],
  ['1 day', '1 day'],
  ['7 days', '7 days'],
  ['30 days', '30 days'],
  ['90 days', '90 days'],
];

const UNITS: Record<string, number> = { s: 1, sec: 1, second: 1, min: 60, minute: 60, h: 3600, hour: 3600, d: 86400, day: 86400, w: 604800, week: 604800, mon: 2592000, month: 2592000 };

/** An interval as SHOW prints it (`01:00:00`, `30 days`, `1 day 12:00:00`) or as typed (`12 hours`, `2 weeks`) in
 * ms, as ALTER SYSTEM reads it (a month is 30 days); null if it isn't one. */
export function intervalMs(v: string): number | null {
  const t = v.trim().toLowerCase();
  if (t === '0') return 0;
  let secs = 0;
  let any = false;
  const h = t.match(/(\d+):(\d\d)(?::(\d\d(?:\.\d+)?))?/);
  if (h) ((secs += Number(h[1]) * 3600 + Number(h[2]) * 60 + Number(h[3] || 0)), (any = true));
  for (const m of t.replace(h?.[0] || '\u0000', ' ').matchAll(/(\d+(?:\.\d+)?)\s*([a-z]+)/g)) {
    const u = UNITS[m[2].replace(/s$/, '')] ?? UNITS[m[2]];
    if (u == null) return null;
    secs += Number(m[1]) * u;
    any = true;
  }
  return any ? secs * 1000 : null;
}

/** An interval as people say it: `1 hour`, `30 days`, `1 day 12 hours`. */
function spoken(v: string): string {
  const ms = intervalMs(v);
  if (ms == null) return v;
  if (ms === 0) return 'none';
  const days = Math.floor(ms / 86_400_000);
  const hours = Math.floor((ms % 86_400_000) / 3_600_000);
  const mins = Math.floor((ms % 3_600_000) / 60_000);
  const part = (x: number, u: string) => (x ? `${x} ${u}${x === 1 ? '' : 's'}` : '');
  return [part(days, 'day'), part(hours, 'hour'), part(mins, 'minute')].filter(Boolean).join(' ') || `${ms / 1000} s`;
}

export function RetentionSection() {
  const dlg = useDialogs();
  const [now, setNow] = useState<string | null>(null);
  const [disk, setDisk] = useState<Record<string, number> | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const load = () => {
    sql('show history_retention', 'main').then(
      (r) => setNow(String(r[0]?.rows[0]?.history_retention ?? '')),
      (e) => setError((e as Error).message),
    );
    sql('show disk', 'main').then(
      (r) => setDisk(Object.fromEntries((r[0]?.rows || []).map((x) => [String(x.what), Number(x.bytes)]))),
      () => setDisk({}),
    );
  };
  useEffect(load, []);
  const change = async (to: string) => {
    if (!to || to === now) return;
    const was = now ? intervalMs(now) : null;
    const will = intervalMs(to);
    // a shorter window lets go of history past it at the next cleanup: no undo
    if (was != null && will != null && will < was) {
      const ok = await dlg.confirm(
        `Keep history for ${spoken(to)}?`,
        `History older than ${spoken(to)} goes at the next cleanup: time travel, UNDO AGENT and UNDO MERGE, and the audit trail can’t reach past it from then on. This can’t be undone.`,
        'Shorten history',
        { danger: true },
      );
      if (!ok) return;
    }
    setBusy(true);
    try {
      await sql(`alter system set history_retention = ${lit(to.trim())}`, 'main');
      toast(`History is kept for ${spoken(to)}`);
      load();
    } catch (e) {
      toast((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const other = async () => {
    const v = await dlg.prompt('Keep history for', 'An interval, like 12 hours, 14 days or 0 for none', now || '1 hour', 'Set retention');
    if (v) change(v);
  };
  const known = now != null && CHOICES.some(([v]) => intervalMs(v) === intervalMs(now));
  return (
    <>
      <div className="setting">
        <div className="st-text">
          <div className="st-title">History retention</div>
          <div className="st-note">
            How far back the database keeps its past: time travel (<code>AS OF</code>, the worldline), <code>UNDO AGENT</code> and <code>UNDO MERGE</code>,
            restoring a world to a moment, and the audit trail all reach back this far. The cost is disk: old pages and the log are kept for the whole window,
            so a busy database grows with it. A new database keeps 1 hour; one made before Chronos 0.2.0 keeps the 30 days it had.
          </div>
        </div>
        <div className="st-control">
          {error ? (
            <span className="note">{error}</span>
          ) : now == null ? (
            <span className="note">Loading…</span>
          ) : (
            <select
              aria-label="History retention"
              disabled={busy}
              value={known ? CHOICES.find(([v]) => intervalMs(v) === intervalMs(now))![0] : 'now'}
              onChange={(e) => (e.target.value === 'other' ? other() : change(e.target.value))}
            >
              {!known && <option value="now">{spoken(now)}</option>}
              {CHOICES.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
              <option value="other">Other…</option>
            </select>
          )}
        </div>
      </div>
      {disk && Object.keys(disk).length > 0 && (
        <div className="disk" aria-label="Disk use">
          {(
            [
              ['live', 'Live data', 'pages the worlds use now'],
              ['history', 'History', 'pages kept only for the retention window'],
              ['log', 'Log', 'the write-ahead log, and its history'],
              ['reclaimable', 'Reclaimable', 'pages nothing needs: the next cleanup frees them'],
            ] as const
          ).map(([k, label, title]) => (
            <div className="card" key={k} title={title}>
              <div className="k">{label}</div>
              <div className="v">{bytes(disk[k])}</div>
            </div>
          ))}
        </div>
      )}
    </>
  );
}
