// Search a table the way Chronos searches: words (BM25, typo tolerant), exact filters, and
// nearness to a row's vector, fused into one ranking by one `find` call. Beside the results,
// the table's vector space: up to 1,500 vectors on their two main axes, coloured by a field
// with few values. Clicking a point or a hit makes it the query row, to walk the neighbourhood.
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { api, sql, type Row } from '../api';
import { useApp } from '../context';
import { Icon } from '../icons';
import { ErrorBox, StatusSlot, VecStrip } from '../ui';
import { lit, n, plural, qid, qtable, text } from '../util';
import { isVector, project } from '../vectors';
import { rowId } from '../Inspector';
import { tableInfo } from './Data';

interface Hit {
  id: string;
  score: number;
  row: Row;
}
interface Found {
  total: number;
  hits: Hit[];
  ms: number;
  like: string | null;
}
interface Space {
  ids: string[];
  pts: [number, number][];
  rows: Row[];
  colorField: string | null;
  titleField: string | null;
  colors: string[];
}
interface Filter {
  f: string;
  v: string;
}
const HUES = ['var(--main)', 'var(--h0)', 'var(--h1)', 'var(--h2)', 'var(--h3)', 'var(--h4)', 'var(--warn)', 'var(--accent)'];
const PREFER_COLOR = /^(topic|category|type|kind|status|label|class|group|stage|source|industry)$/i;
const PREFER_TITLE = /^(title|name|subject|label|headline|summary)$/i;

const NUMERIC = /^(smallint|integer|bigint|real|double precision|numeric|decimal)/;
/** A filter value as `find` compares it (JSON), by the column's type: numbers and booleans as
 *  themselves, text as text. A JSON table has no types: numbers, booleans and null are guessed. */
function asJson(s: string, type: string | undefined): unknown {
  if (type) return NUMERIC.test(type) && s.trim() !== '' && !isNaN(Number(s)) ? Number(s) : type === 'boolean' ? /^(t|true|1|yes)$/i.test(s) : s;
  return /^-?\d+(\.\d+)?$/.test(s) ? Number(s) : s === 'true' ? true : s === 'false' ? false : s === 'null' ? null : s;
}

interface Info {
  table: string;
  pk: string[];
  cols: string[];
  types: Record<string, string>;
}

export function SearchView() {
  const app = useApp();
  const { world } = app;
  const start = app.search;
  const [vecTables, setVecTables] = useState<Map<string, string[]> | null>(null);
  const [table, setTable] = useState<string>(start?.table || app.table || '');
  const [info, setInfo] = useState<Info | null>(null);
  const [vfield, setVfield] = useState<string>(start?.field || '');
  const [words, setWords] = useState('');
  const [filters, setFilters] = useState<Filter[]>([]);
  const [fField, setFField] = useState('');
  const [fVal, setFVal] = useState('');
  const [like, setLike] = useState<string | null>(start?.id ?? null);
  const [found, setFound] = useState<Found | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [space, setSpace] = useState<Space | null>(null);
  const [spaceErr, setSpaceErr] = useState<unknown>(null);
  const [colorBy, setColorBy] = useState<string | null>(null);
  const seq = useRef(0); // the latest search: replies to older ones are dropped
  useEffect(() => app.setSearch(null), []); // taken: Find similar applies once
  const ready = info?.table === table ? info : null;
  const canWalk = !!ready?.pk.length && !!vfield;

  // which tables have vectors: pgvector columns of real tables (not views), and JSON tables' arrays
  useEffect(() => {
    let live = true;
    sql(
      `select c.table_schema, c.table_name, c.column_name from information_schema.columns c join information_schema.tables t on t.table_schema = c.table_schema and t.table_name = c.table_name where c.udt_name = 'vector' and t.table_type = 'BASE TABLE' order by c.table_name, c.ordinal_position`,
      world,
    ).then(
      async ([r]) => {
        const m = new Map<string, string[]>();
        for (const x of r.rows) {
          const t = x.table_schema === 'public' ? String(x.table_name) : `${x.table_schema}.${x.table_name}`;
          m.set(t, [...(m.get(t) || []), String(x.column_name)]);
        }
        for (const t of app.tables.filter((t) => !t.schema).slice(0, 20)) {
          const [one] = await sql(`select * from ${qtable(t.name)} limit 1`, world).catch(() => [null]);
          const f = one ? Object.keys(one.rows[0] || {}).filter((k) => isVector(one.rows[0][k])) : [];
          if (f.length) m.set(t.name, f);
        }
        if (!live) return;
        setVecTables(m);
        setTable((cur) => (cur && (m.has(cur) || start) ? cur : [...m.keys()][0] || cur || app.tables[0]?.name || ''));
      },
      () => live && setVecTables(new Map()),
    );
    return () => {
      live = false;
    };
  }, [world]); // eslint-disable-line

  // the table's key, columns and their types (a JSON table's columns from a row, if the catalog has none)
  useEffect(() => {
    if (!table) return;
    let live = true;
    const json = app.tables.some((t) => t.name === table && !t.schema);
    tableInfo(table, world, json)
      .then(async (i) => {
        let cols = i.cols.map((c) => c.column_name);
        if (!cols.length) {
          const [one] = await sql(`select * from ${qtable(table)} limit 1`, world);
          cols = one.columns.length ? one.columns : Object.keys(one.rows[0] || {});
        }
        if (!live) return;
        setInfo({ table, pk: i.pk, cols, types: i.types });
        setFField(cols.find((c) => !i.pk.includes(c) && i.types[c] !== 'vector') || cols[0] || '');
      })
      .catch((e) => live && setError(e));
    return () => {
      live = false;
    };
  }, [table, world]); // eslint-disable-line

  // the vector field: kept as given (Find similar) until the vector tables are known
  useEffect(() => {
    if (!vecTables) return;
    const v = vecTables.get(table) || [];
    setVfield((f) => (f && v.includes(f) ? f : v[0] || ''));
  }, [table, vecTables]);

  // the vector space: a sample of the table's vectors, projected
  useEffect(() => {
    setSpace(null);
    setSpaceErr(null);
    if (!ready || !vfield) return;
    let live = true;
    const small = ready.cols.filter((c) => c !== vfield && !ready.pk.includes(c) && (!ready.types[c] || /text|char/.test(ready.types[c]))).slice(0, 12);
    sql(`select * from ${qtable(table)} limit 1`, world)
      .then(async ([first]) => {
        const dims = isVector(first.rows[0]?.[vfield]) ? (first.rows[0][vfield] as number[]).length : 64;
        const cap = dims > 512 ? 600 : 1500; // big vectors: fewer of them, so the page stays quick
        const pick = [...new Set([...ready.pk, vfield, ...small])];
        const [r] = await sql(`select ${pick.map(qid).join(', ')} from ${qtable(table)} limit ${cap}`, world);
        const rows = r.rows.filter((x) => isVector(x[vfield]) && (x[vfield] as number[]).length === dims);
        const pts = project(rows.map((x) => x[vfield] as number[]));
        const distinct = (c: string) => new Set(rows.map((x) => x[c])).size;
        const colorable = small.filter((c) => distinct(c) >= 2 && distinct(c) <= HUES.length);
        const colorField = colorable.find((c) => PREFER_COLOR.test(c)) || colorable[0] || null;
        const titleField = small.find((c) => PREFER_TITLE.test(c)) || small.find((c) => distinct(c) > HUES.length) || null;
        if (!live) return;
        setColorBy(colorField);
        setSpace({ ids: rows.map((x, i) => rowId(ready.pk, x) ?? `#${i}`), pts, rows, colorField, titleField, colors: colorable });
      })
      .catch((e) => live && setSpaceErr(e));
    return () => {
      live = false;
    };
  }, [ready, vfield, world]); // eslint-disable-line

  const run = async (likeId = like, fs = filters) => {
    if (!table || !ready) return;
    const me = ++seq.current;
    setBusy(true);
    setError(null);
    const args: Record<string, unknown> = { branch: world, table, limit: 30 };
    if (words.trim()) args.text = words.trim();
    if (fs.length) args.where = Object.fromEntries(fs.map((f) => [f.f, asJson(f.v, ready.types[f.f])]));
    const t0 = performance.now();
    try {
      if (likeId && canWalk) {
        // the row's vector: from the map's sample, the hits, or (a one-column key) the table
        let vec: unknown = space?.rows[space.ids.indexOf(likeId)]?.[vfield] ?? found?.hits.find((h) => h.id === likeId)?.row[vfield];
        if (!isVector(vec) && ready.pk.length === 1)
          vec = (await sql(`select ${qid(vfield)} as v from ${qtable(table)} where CAST(${qid(ready.pk[0])} AS text) = ${lit(likeId)} limit 1`, world))[0]
            .rows[0]?.v;
        if (!isVector(vec)) throw new Error(`row ${likeId} has no vector in ${vfield}`);
        args.vector_field = vfield;
        args.vector = vec;
      }
      if (!args.text && !args.where && !args.vector) {
        if (me === seq.current) setFound(null);
        return;
      }
      const r = await api<{ total: number; hits: Hit[] }>('find', args);
      if (me === seq.current) setFound({ ...r, ms: performance.now() - t0, like: likeId });
    } catch (e) {
      if (me === seq.current) setError(e);
    } finally {
      if (me === seq.current) setBusy(false);
    }
  };
  // Find similar arrives with a row: search once the table's key and its vector field are known
  const ran = useRef(false);
  useEffect(() => {
    if (!ran.current && start?.id && ready && vecTables && vfield === start.field) {
      ran.current = true;
      run(start.id);
    }
  }, [ready, vecTables, vfield]); // eslint-disable-line
  const walk = (id: string) => {
    if (!canWalk) return;
    setLike(id);
    run(id);
  };
  const withTyped = () => (fField && fVal !== '' ? [...filters, { f: fField, v: fVal }] : filters);
  const addFilter = () => {
    const fs = withTyped();
    setFilters(fs);
    setFVal('');
    return fs;
  };
  const pickTable = (t: string) => {
    seq.current++; // a search still running belongs to the old table
    setTable(t);
    setFound(null);
    setLike(null);
    setFilters([]);
    setError(null);
  };

  const noVectors = vecTables && !vecTables.size;
  const title = space?.titleField;
  const colorOf = useMemo(() => {
    if (!space || !colorBy) return () => 'var(--accent)';
    const vals = [...new Set(space.rows.map((r) => String(r[colorBy])))].sort();
    return (v: unknown) => HUES[vals.indexOf(String(v))] ?? 'var(--faint)';
  }, [space, colorBy]);

  return (
    <div className="pane finder">
      <div className="sq">
        <label className="sq-part table">
          <span className="k">In</span>
          <select value={table} onChange={(e) => pickTable(e.target.value)} aria-label="Table">
            {app.tables.map((t) => (
              <option key={t.name} value={t.name}>
                {t.name}
                {vecTables?.has(t.name) ? '  · vectors' : ''}
              </option>
            ))}
          </select>
        </label>
        <label className="sq-part grow">
          <span className="k">Match</span>
          <input
            type="search"
            placeholder="words, typos forgiven"
            value={words}
            onChange={(e) => setWords(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && run(like, addFilter())}
          />
        </label>
        <div className="sq-part">
          <span className="k">Where</span>
          {filters.map((f, i) => (
            <span className="chip" key={i}>
              <span>
                {f.f} = {f.v}
              </span>
              <button type="button" aria-label={`Remove ${f.f} = ${f.v}`} onClick={() => setFilters(filters.filter((_, j) => j !== i))}>
                ×
              </button>
            </span>
          ))}
          <select value={fField} onChange={(e) => setFField(e.target.value)} aria-label="Filter field">
            {(ready?.cols || [])
              .filter((c) => c !== vfield)
              .map((c) => (
                <option key={c}>{c}</option>
              ))}
          </select>
          <input
            className="fv"
            type="text"
            placeholder="= value"
            value={fVal}
            onChange={(e) => setFVal(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && (e.preventDefault(), run(like, addFilter()))}
            aria-label="Filter value"
          />
        </div>
        {canWalk && (
          <div className="sq-part">
            <span className="k">Near</span>
            {(vecTables?.get(table)?.length ?? 0) > 1 ? (
              <select value={vfield} onChange={(e) => setVfield(e.target.value)} aria-label="Vector field">
                {vecTables!.get(table)!.map((f) => (
                  <option key={f}>{f}</option>
                ))}
              </select>
            ) : (
              <code>{vfield}</code>
            )}
            <span className="muted">like row</span>
            <input
              className="fv id"
              type="text"
              placeholder="pick on the map"
              value={like ?? ''}
              onChange={(e) => setLike(e.target.value || null)}
              onKeyDown={(e) => e.key === 'Enter' && run(like, addFilter())}
              aria-label="Row to search near"
            />
            {like && (
              <button className="btn ghost icon small" type="button" aria-label="Clear the row" onClick={() => setLike(null)}>
                <Icon name="x" size={13} />
              </button>
            )}
          </div>
        )}
        <button className="btn primary" type="button" disabled={busy || !table} onClick={() => run(like, addFilter())}>
          <Icon name="search" size={14} /> {busy ? 'Searching…' : 'Search'}
        </button>
      </div>

      <div className="sr-body">
        <section className="hits" aria-label="Results">
          {noVectors && !found && (
            <div className="empty-state">
              <strong>No table here has vectors yet</strong>
              <span className="muted">Words and filters work on any table. For vectors, store embeddings as a pgvector column or a JSON array of numbers:</span>
              <pre className="snippet">
                create table docs (id bigint primary key, title text, embedding vector(1536));{'\n'}select id, title from docs order by embedding {'<=>'} '[…]'
                limit 10;
              </pre>
            </div>
          )}
          {error ? (
            <div className="pad">
              <ErrorBox error={error} />
            </div>
          ) : !found ? (
            !noVectors && (
              <div className="empty-state">
                <strong>{vfield ? 'Pick a point on the map, or type words to match' : 'Type words to match, or add a filter'}</strong>
                <span className="muted">
                  {vfield
                    ? `Search ranks ${table}'s rows by how near their ${vfield} is to the row you pick, by the words, or both at once.`
                    : `Words are matched across every text field, with typos forgiven.`}
                </span>
              </div>
            )
          ) : !found.hits.length ? (
            <div className="empty-state">
              <strong>No rows match</strong>
              <span className="muted">
                {filters.length ? 'The filters leave nothing: remove one. ' : ''}
                {words ? 'No row has these words, even one letter off.' : ''}
              </span>
            </div>
          ) : (
            <ol className="hit-list">
              {found.hits.map((h, i) => {
                const max = found.hits[0].score || 1;
                const others = Object.entries(h.row).filter(
                  ([k, v]) => k !== title && !ready?.pk.includes(k) && k !== vfield && !isVector(v) && v !== null && String(v).length < 40,
                );
                return (
                  <li key={h.id} className={h.id === found.like ? 'self' : ''}>
                    <button type="button" className="hit" onClick={() => walk(h.id)} title={canWalk ? 'Search near this row' : undefined}>
                      <span className="rank">{i + 1}</span>
                      <span className="hit-main">
                        <span className="t">{title ? text(h.row[title], 120) : h.id}</span>
                        <span className="meta">
                          <span className="id">{h.id}</span>
                          {others.slice(0, 2).map(([k, v]) => (
                            <span key={k} className="f">
                              {colorBy === k && <span className="dot" style={{ background: colorOf(v) }} />}
                              {typeof v === 'number' ? `${k} ${n(v)}` : String(v)}
                            </span>
                          ))}
                        </span>
                      </span>
                      <span className="hit-side">
                        <span className="score">{h.score.toFixed(3)}</span>
                        <span className="bar">
                          <span style={{ width: `${Math.max(4, (h.score / max) * 100)}%` }} />
                        </span>
                        {vfield && isVector(h.row[vfield]) && <VecStrip v={h.row[vfield] as number[]} bins={24} h={12} dims={false} />}
                      </span>
                    </button>
                  </li>
                );
              })}
            </ol>
          )}
        </section>
        {vfield && (
          <section className="space" aria-label="Vector space">
            <div className="space-head">
              <span className="label">Vector space</span>
              <span className="muted">{space ? `${n(space.ids.length)} vectors of ${vfield}, on their two main axes` : spaceErr ? '' : 'Projecting…'}</span>
              <span className="spacer" />
              {space && space.colors.length > 0 && (
                <label className="color-by">
                  <span className="muted">Colour by</span>
                  <select value={colorBy ?? ''} onChange={(e) => setColorBy(e.target.value || null)}>
                    {space.colors.map((c) => (
                      <option key={c}>{c}</option>
                    ))}
                  </select>
                </label>
              )}
            </div>
            {spaceErr ? (
              <ErrorBox error={spaceErr} />
            ) : (
              space && <SpaceMap space={space} found={found} like={like} colorBy={colorBy} colorOf={colorOf} onPick={walk} />
            )}
          </section>
        )}
      </div>
      <StatusSlot>
        {found ? (
          <>
            <span>{Math.round(found.ms)} ms</span>
            <span>
              {plural(found.hits.length, 'hit')} of {n(found.total)} passing the filters
            </span>
            <span className="muted">{[words.trim() && 'words', filters.length && 'filters', found.like && 'vector'].filter(Boolean).join(' + ')}</span>
          </>
        ) : (
          <span>{vfield ? `${table} · ${vfield}` : table}</span>
        )}
        <span className="spacer" />
        <span className="hint faint">Click a point or a hit to search near it</span>
      </StatusSlot>
    </div>
  );
}

function SpaceMap({
  space,
  found,
  like,
  colorBy,
  colorOf,
  onPick,
}: {
  space: Space;
  found: Found | null;
  like: string | null;
  colorBy: string | null;
  colorOf: (v: unknown) => string;
  onPick: (id: string) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 600, h: 400 });
  const [hover, setHover] = useState<number | null>(null);
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setSize({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const P = 24;
  const xy = (i: number) => [P + space.pts[i][0] * (size.w - 2 * P), P + (1 - space.pts[i][1]) * (size.h - 2 * P)] as const;
  const hitRank = new Map((found?.hits || []).map((h, i) => [h.id, i]));
  const likeAt = like ? space.ids.indexOf(like) : -1;
  const legend = colorBy ? [...new Set(space.rows.map((r) => String(r[colorBy])))].sort() : [];
  const hv = hover != null ? space.rows[hover] : null;
  return (
    <div className="map" ref={box}>
      <svg width={size.w} height={size.h} onMouseLeave={() => setHover(null)}>
        {found && likeAt >= 0 && (
          // lines from the query row to its nearest hits
          <g className="spokes">
            {found.hits.slice(0, 10).map((h) => {
              const j = space.ids.indexOf(h.id);
              if (j < 0 || j === likeAt) return null;
              const [x1, y1] = xy(likeAt);
              const [x2, y2] = xy(j);
              return <line key={h.id} x1={x1} y1={y1} x2={x2} y2={y2} />;
            })}
          </g>
        )}
        <g className={'pts' + (found ? ' dim' : '')}>
          {space.pts.map((_, i) => {
            const [x, y] = xy(i);
            return (
              <circle
                key={i}
                cx={x}
                cy={y}
                r={hitRank.has(space.ids[i]) ? 4.5 : 3}
                className={hitRank.has(space.ids[i]) ? 'hit' : ''}
                style={{ '--c': colorBy ? colorOf(space.rows[i][colorBy]) : 'var(--accent)' } as CSSProperties}
                onMouseEnter={() => setHover(i)}
                onClick={() => onPick(space.ids[i])}
              />
            );
          })}
        </g>
        {likeAt >= 0 && (
          <g className="query" transform={`translate(${xy(likeAt)[0]} ${xy(likeAt)[1]})`}>
            <circle r="11" className="halo" />
            <rect x="-5" y="-5" width="10" height="10" transform="rotate(45)" />
          </g>
        )}
      </svg>
      {hv && hover != null && (
        <div className="map-tip" style={{ left: Math.min(size.w - 240, xy(hover)[0] + 12), top: Math.max(8, xy(hover)[1] - 12) }}>
          <strong>{space.titleField ? text(hv[space.titleField], 80) : space.ids[hover]}</strong>
          <span className="muted mono">
            {space.ids[hover]}
            {colorBy ? ` · ${String(hv[colorBy])}` : ''}
            {hitRank.has(space.ids[hover]) ? ` · hit ${hitRank.get(space.ids[hover])! + 1}` : ''}
          </span>
        </div>
      )}
      {legend.length > 0 && (
        <div className="legend">
          {legend.map((v) => (
            <span key={v}>
              <span className="dot" style={{ background: colorOf(v) }} />
              {v}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
