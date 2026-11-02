// The database: version, size, worlds, encryption, remote storage, and its metrics.
import { useEffect, useState } from 'react';
import { api, sql, type SqlResult } from '../api';
import { useApp } from '../context';
import { ErrorBox } from '../ui';
import { bytes, n, text } from '../util';

interface Status {
  version: string;
  encrypted: boolean;
  remote: string | null;
  counters: Record<string, number>;
}

export function StatusView() {
  const { world } = useApp();
  const [s, setS] = useState<Status | null>(null);
  const [metrics, setMetrics] = useState<SqlResult | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [mError, setMError] = useState<unknown>(null);
  useEffect(() => {
    api<Status>('status').then(setS, setError);
    sql('show metrics', world).then((r) => setMetrics(r[0]), setMError);
  }, [world]);
  const c = s?.counters || {};
  const up = c.uptime_seconds;
  const cards: [string, string, string?][] = s
    ? [
        ['Worlds', n(c.worlds)],
        ['Version', s.version],
        ['On disk', bytes((c.pages_bytes || 0) + (c.log_bytes || 0) + (c.history_bytes || 0)), 'pages, log and history'],
        ['Pages', bytes(c.pages_bytes)],
        ['Log', bytes(c.log_bytes)],
        ['History', bytes(c.history_bytes)],
        ['Encrypted', s.encrypted ? 'Yes' : 'No'],
        ['Storage', s.remote ? 'Remote' : 'Local', s.remote || undefined],
        ['Open for', up != null ? `${Math.floor(up / 3600)}h ${Math.floor((up % 3600) / 60)}m` : '?'],
        ['Rows written', n(c.rows_written)],
        ['Errors', n(c.errors)],
        ['Conflicts', n(c.conflicts)],
      ]
    : [];
  return (
    <div className="scroll">
      <div className="pad stack">
        {error ? <ErrorBox error={error} /> : null}
        {s && (
          <div className="sec">
            <h3 className="label">Database</h3>
            <div className="cards">
              {cards.map(([k, v, title], i) => (
                <div className={i ? 'card' : 'card lead'} key={k} title={title}>
                  <div className="k">{k}</div>
                  <div className="v">{v}</div>
                </div>
              ))}
            </div>
            {s.remote && (
              <p className="note">
                Backed by <code>{s.remote}</code>
              </p>
            )}
          </div>
        )}
        <div className="sec">
          <h3 className="label">Operations since the database opened</h3>
          {mError ? (
            <ErrorBox error={mError} />
          ) : metrics ? (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    {metrics.columns.map((x) => (
                      <th key={x} className={x === 'name' || x === 'metric' ? '' : 'num'}>
                        {x}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {metrics.rows.map((r, i) => (
                    <tr key={i}>
                      {metrics.columns.map((x) => (
                        <td key={x} className={typeof r[x] === 'number' ? 'num' : 'mono'}>
                          {r[x] == null ? '' : text(r[x])}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <p className="note">Loading…</p>
          )}
        </div>
      </div>
    </div>
  );
}
