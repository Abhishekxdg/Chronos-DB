// A table's rows: one toolbar row (filter, sort, as of, + row), the virtualized grid, the status
// line, and the inspector (a row's fields, editing, the table's schema). Writes on main ask first,
// offering to fork and edit in a new world instead.
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { api, sql, type Row } from '../api';
import { useApp, type App } from '../context';
import { Grid, PAGE, type Column, type Sort } from '../Grid';
import { Inspector, rowId, type Inspect } from '../Inspector';
import { Empty, ErrorBox, Popover, StatusSlot, toast, type Dialogs, useDialogs } from '../ui';
import { Icon } from '../icons';
import { importRows } from './Import';
import { downloadCsv, historyStart, likeEsc, lit, momentOf, n, outsideHistory, qid, qtable, span, splitName, suggestName, when } from '../util';

export interface ColumnInfo {
  column_name: string;
  data_type: string;
  udt_name: string;
  is_nullable: string;
  column_default: string | null;
}
export interface TableInfo {
  cols: ColumnInfo[];
  pk: string[];
  /** made by writing JSON rows: no schema */
  json: boolean;
  types: Record<string, string>;
}

/** A column's type as a CAST names it. */
export const castType = (c: ColumnInfo) =>
  c.data_type === 'USER-DEFINED' ? c.udt_name : c.data_type === 'ARRAY' ? c.udt_name.replace(/^_/, '') + '[]' : c.data_type;

/** A table's columns and key. `json`: made by writing JSON rows (no schema; its key is `id`). */
export async function tableInfo(table: string, world: string, json: boolean): Promise<TableInfo> {
  const [s, t] = splitName(table);
  const [cols, pk] = await sql(
    `select column_name, data_type, udt_name, is_nullable, column_default from information_schema.columns where table_schema = ${lit(s)} and table_name = ${lit(t)} order by ordinal_position;
     select k.column_name from information_schema.table_constraints tc join information_schema.key_column_usage k on k.constraint_schema = tc.constraint_schema and k.constraint_name = tc.constraint_name where tc.table_schema = ${lit(s)} and tc.table_name = ${lit(t)} and tc.constraint_type = 'PRIMARY KEY' order by k.ordinal_position`,
    world,
  );
  const c = cols.rows as unknown as ColumnInfo[];
  const types: Record<string, string> = {};
  for (const x of c) types[x.column_name] = castType(x);
  return { cols: c, pk: json ? ['id'] : pk.rows.map((r) => String(r.column_name)), json, types };
}

interface Filter {
  col: string;
  op: 'eq' | 'contains' | 'gte' | 'lte' | 'null' | 'notnull';
  val: string;
}
const OPS: [Filter['op'], string][] = [
  ['contains', 'contains'],
  ['eq', 'equals'],
  ['gte', 'at least'],
  ['lte', 'at most'],
  ['null', 'is empty'],
  ['notnull', 'is not empty'],
];
const OP_LABEL = { eq: '=', contains: '∋', gte: '≥', lte: '≤', null: 'is empty', notnull: 'is not empty' };

function queries(table: string, info: TableInfo, filters: Filter[], sort: Sort | null, asOf: string) {
  const where = filters.map((f) => {
    const c = qid(f.col);
    const type = info.types[f.col];
    const val = (v: string) => (type ? `CAST(${lit(v)} AS ${type})` : /^-?\d+(\.\d+)?$/.test(v) ? v : lit(v));
    switch (f.op) {
      case 'eq':
        return `CAST(${c} AS text) = ${lit(f.val)}`;
      case 'contains':
        return `CAST(${c} AS text) ILIKE ${lit('%' + likeEsc(f.val) + '%')}`;
      case 'gte':
        return `${c} >= ${val(f.val)}`;
      case 'lte':
        return `${c} <= ${val(f.val)}`;
      case 'null':
        return `${c} IS NULL`;
      case 'notnull':
        return `${c} IS NOT NULL`;
    }
  });
  const from = qtable(table) + (asOf ? ` AS OF ${lit(asOf)}` : '');
  const w = where.length ? ' WHERE ' + where.join(' AND ') : '';
  let order = '';
  if (sort) {
    const tie = info.pk.filter((c) => c !== sort.col && (info.json || info.types[c])).map((c) => qid(c) + ' ASC');
    order = ' ORDER BY ' + [`${qid(sort.col)} ${sort.dir === 'asc' ? 'ASC' : 'DESC'}`, ...tie].join(', ');
  }
  return {
    count: `select count(*) as n from ${from}${w}`,
    page: (p: number) => `select * from ${from}${w}${order} limit ${PAGE} offset ${p * PAGE}`,
    all: (limit: number) => `select * from ${from}${w}${order} limit ${limit}`,
  };
}

export function DataView() {
  const app = useApp();
  const dlg = useDialogs();
  const { world, table } = app;
  const json = app.tables.some((t) => t.name === table && !t.schema);
  // a view or materialized view: its rows are read, never written here
  const view = app.tables.find((t) => t.name === table)?.kind;
  const [info, setInfo] = useState<TableInfo | null>(null);
  const [filters, setFilters] = useState<Filter[]>([]);
  // undefined: the default, a SQL table in key order (without ORDER BY rows come in the key's text order)
  const [picked, setSort] = useState<Sort | null | undefined>(undefined);
  const sort = picked !== undefined ? picked : info && !info.json && info.pk.length ? { col: info.pk[0], dir: 'asc' as const } : null;
  const [asOf, setAsOf] = useState(app.asOf);
  const [res, setRes] = useState<{ total: number; first: Row[]; columns: Column[]; ms: number } | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [inspect, setInspect] = useState<Inspect | null>(null);
  const [selRow, setSelRow] = useState<Row | undefined>(undefined);
  const [range, setRange] = useState<[number, number]>([0, 0]);
  useEffect(() => app.setAsOf(''), []); // taken: History's "View as of" applies once

  useEffect(() => {
    if (!table) return;
    tableInfo(table, world, json).then(setInfo, setError);
  }, [table, world, json]);
  const sortKey = JSON.stringify(sort);
  const q = useMemo(() => (info && table ? queries(table, info, filters, sort, asOf) : null), [info, table, filters, sortKey, asOf]);
  useEffect(() => {
    if (!q || !info) return;
    let live = true;
    setRes(null);
    setError(null);
    const t0 = performance.now();
    sql(q.count + ';\n' + q.page(0), world).then(
      ([c, rows]) => {
        if (!live) return;
        const names = rows.columns.length ? rows.columns : info.cols.map((x) => x.column_name);
        setRes({
          total: Number(c.rows[0].n),
          first: rows.rows,
          columns: names.map((name) => ({ name, type: info.types[name] || '', pk: info.pk.includes(name) })),
          ms: performance.now() - t0,
        });
      },
      (e) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [q, info, world]);
  // an edit in the grid: one UPDATE ... RETURNING (a JSON table: put), and the grid shows the stored row
  const canEdit = (col: string) => !asOf && !view && app.admin && !!info && info.pk.length > 0 && !info.pk.includes(col);
  const onCellEdit = async (row: Row, col: string, s: string): Promise<Row | null> => {
    if (!info || !table) return null;
    const id = rowId(info.pk, row);
    let stored: Row | null = null;
    const ran = await runWrite(
      app,
      dlg,
      `change ${col} of row ${id} in ${table}`,
      async (w) => {
        if (info.json) {
          const orig = row[col];
          const v =
            typeof orig === 'number' && s.trim() !== '' && !isNaN(Number(s))
              ? Number(s)
              : typeof orig === 'boolean'
                ? /^(t|true|1|yes)$/i.test(s.trim())
                : orig !== null && typeof orig === 'object'
                  ? JSON.parse(s)
                  : s;
          const record: Row = { ...row, [col]: v };
          delete record.id;
          await api('put', { branch: w, table, id, record });
          stored = { ...row, [col]: v };
        } else {
          const type = info.types[col];
          const val = s === '' && !/char|text/.test(type) ? 'NULL' : `CAST(${lit(s)} AS ${type})`;
          const where = info.pk.map((c) => `CAST(${qid(c)} AS text) = ${lit(String(row[c]))}`).join(' AND ');
          const [r] = await sql(`update ${qtable(table)} set ${qid(col)} = ${val} where ${where} returning *`, w);
          if (!r.rows.length) throw new Error(`Row ${id} is gone: it was changed or deleted since the grid loaded`);
          stored = r.rows[0];
        }
      },
      { reload: false },
    );
    // in a new world (forked from main to make the edit) the view reloads there on its own
    return ran === world ? stored : null;
  };
  const EXPORT_MAX = 100_000;
  const [exporting, setExporting] = useState(false);
  const exportCsv = async () => {
    if (!q || !res || !table) return;
    setExporting(true);
    try {
      const [r] = await sql(q.all(EXPORT_MAX), world);
      downloadCsv(table.replace(/\W+/g, '-'), r.columns.length ? r.columns : res.columns.map((c) => c.name), r.rows);
      toast(res.total > EXPORT_MAX ? `Exported the first ${n(EXPORT_MAX)} of ${n(res.total)} rows` : `Exported ${n(r.rows.length)} rows`);
    } catch (e) {
      toast((e as Error).message);
    } finally {
      setExporting(false);
    }
  };
  const fetchPage = useCallback((p: number) => sql(q!.page(p), world).then((r) => r[0].rows), [q, world]);

  // the explorer's outline picked a column: show it in the schema
  useEffect(() => {
    const f = (e: Event) => setInspect({ mode: 'schema', col: (e as CustomEvent<string>).detail });
    addEventListener('studio:schema', f);
    return () => removeEventListener('studio:schema', f);
  }, []);

  // E edits the selected (or open) row
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (e.key !== 'e' || e.metaKey || e.ctrlKey || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName) || document.querySelector('.overlay')) return;
      const row = inspect?.mode === 'row' ? inspect.row : selRow;
      if (row && !asOf && !view && app.admin) setInspect({ mode: 'row', row, edit: true });
    };
    addEventListener('keydown', key);
    return () => removeEventListener('keydown', key);
  }, [inspect, selRow, asOf, view, app.admin]);

  if (!table && !app.tablesReady)
    return (
      <div className="pane">
        <div className="grid-wrap">
          <Skeleton />
        </div>
      </div>
    );
  if (!table)
    return (
      <Empty>
        {app.tables.length ? 'Pick a table on the left.' : 'No tables in this world yet. Create one in the SQL tab, or write JSON rows through the API.'}
      </Empty>
    );
  const write = (what: string, run: (world: string) => Promise<unknown>) => runWrite(app, dlg, what, run);
  return (
    <>
      <div className="pane">
        <Toolbar
          table={table}
          total={res?.total ?? null}
          json={!!info?.json}
          view={view}
          columns={res?.columns.map((c) => c.name) || []}
          filters={filters}
          setFilters={setFilters}
          sort={sort}
          sorted={!!picked}
          setSort={setSort}
          asOf={asOf}
          setAsOf={setAsOf}
          onInsert={() => setInspect({ mode: 'insert' })}
          onExport={exportCsv}
          onImport={() => info && table && importRows(app, dlg, table, info)}
          exporting={exporting}
          live={world === 'main'}
        />
        <div className="grid-wrap">
          {error ? (
            <div className="pad">
              {asOf && (error as { detail?: { code?: string } })?.detail?.code === '42P01' ? (
                <div className="empty-state">
                  <strong>
                    {table} didn’t exist yet at {when(asOf)}
                  </strong>
                  <span className="muted">It was created after that moment. Pick a later time, or go back to now.</span>
                  <button className="btn" type="button" onClick={() => setAsOf('')}>
                    Back to now
                  </button>
                </div>
              ) : (
                <ErrorBox error={error} />
              )}
            </div>
          ) : !res ? (
            <Skeleton />
          ) : !res.total ? (
            <Empty>
              {filters.length
                ? 'No rows match these filters.'
                : asOf
                  ? `The table had no rows at ${asOf}.`
                  : view
                    ? `The ${view} has no rows.`
                    : 'No rows yet. Add one with + Row, or run SQL.'}
            </Empty>
          ) : (
            <Grid
              key={JSON.stringify([filters, sort, asOf])}
              columns={res.columns}
              total={res.total}
              first={res.first}
              fetchPage={fetchPage}
              sort={sort}
              onSort={(col) => setSort(!sort || sort.col !== col ? { col, dir: 'asc' } : sort.dir === 'asc' ? { col, dir: 'desc' } : null)}
              onHeader={(col) => setInspect({ mode: 'schema', col })}
              headerSel={inspect?.mode === 'schema' ? inspect.col : null}
              onPick={(r) => {
                setSelRow(r);
                if (r && inspect?.mode === 'row' && !inspect.edit) setInspect({ mode: 'row', row: r });
              }}
              onOpen={(row) => setInspect({ mode: 'row', row })}
              canEdit={canEdit}
              onCellEdit={onCellEdit}
              onRange={(a, b) => setRange([a, b])}
            />
          )}
        </div>
        <StatusSlot>
          {res ? (
            <>
              <span>{Math.round(res.ms)} ms</span>
              <span>{res.total ? `rows ${n(range[0])}–${n(range[1])} of ${n(res.total)}` : '0 rows'}</span>
              {asOf && <span>as of {when(asOf)} · read only</span>}
              {view && !asOf && <span>{view} · read only</span>}
              {!app.admin && !view && !asOf && <span>read only here · change rows with SQL</span>}
              <span className="spacer" />
              <span className="hint faint">Double-click a cell to edit · Enter opens the row · / filters</span>
            </>
          ) : (
            <span>Loading…</span>
          )}
        </StatusSlot>
      </div>
      {inspect && info && (
        <Inspector
          key={inspect.mode === 'row' ? JSON.stringify(inspect.row).slice(0, 200) + inspect.edit : inspect.mode}
          inspect={inspect}
          table={table}
          info={info}
          columns={res?.columns || []}
          asOf={asOf}
          readOnly={view ? `A ${view}: read only. Change the tables it reads, or its definition with SQL.` : !app.admin ? ROLE_READ_ONLY : undefined}
          live={world === 'main'}
          onEdit={(row) => setInspect({ mode: 'row', row, edit: true })}
          onClose={() => setInspect(null)}
          write={write}
        />
      )}
    </>
  );
}

/** Why rows are read only here for a role that isn't a superuser (chronos studio --login). */
const ROLE_READ_ONLY = 'Signed in as a role that isn’t a superuser: rows are read only here. Change them with SQL, as its privileges allow.';

const SKEL = { '--cols': '44px repeat(5, 160px)' } as React.CSSProperties;
function Skeleton() {
  return (
    <div className="grid" aria-busy="true" aria-label="Loading rows">
      <div className="head" style={SKEL} />
      <div className="body">
        {Array.from({ length: 12 }, (_, i) => (
          <div key={i} className="row" style={{ ...SKEL, top: i * 32 }}>
            {Array.from({ length: 6 }, (_, j) => (
              <div key={j} className="cell">
                <span className="skel" />
              </div>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

function Toolbar(p: {
  table: string;
  total: number | null;
  json: boolean;
  view?: string;
  columns: string[];
  filters: Filter[];
  setFilters: (f: Filter[]) => void;
  sort: Sort | null;
  sorted: boolean;
  setSort: (s: Sort | null | undefined) => void;
  asOf: string;
  setAsOf: (at: string) => void;
  onInsert: () => void;
  onExport: () => void;
  onImport: () => void;
  exporting: boolean;
  live: boolean;
}) {
  const app = useApp();
  const [open, setOpen] = useState<'filter' | 'sort' | 'asof' | null>(null);
  const [col, setCol] = useState('');
  const [op, setOp] = useState<Filter['op']>('contains');
  const [val, setVal] = useState('');
  const [at, setAt] = useState(p.asOf);
  const valRef = useRef<HTMLInputElement>(null);
  const c = col || p.columns[0] || '';
  useEffect(() => {
    const f = () => setOpen('filter');
    addEventListener('studio:filter', f);
    return () => removeEventListener('studio:filter', f);
  }, []);
  useEffect(() => {
    if (open === 'filter') valRef.current?.focus();
  }, [open]);
  const needsValue = op !== 'null' && op !== 'notnull';
  const add = () => {
    if (!c || (needsValue && val === '')) return valRef.current?.focus();
    p.setFilters([...p.filters, { col: c, op, val }]);
    setVal('');
    setOpen(null);
  };
  const toggle = (k: typeof open) => (o: boolean) => setOpen(o ? k : null);
  // how far back history goes, and why a moment typed can't be read (a moment it can't parse: the server says)
  const h = app.history;
  const outside = at.trim() ? outsideHistory(h, momentOf(at)) : null;
  const readAsOf = () => !outside && (p.setAsOf(at.trim()), setOpen(null));
  return (
    <div className="toolbar">
      <span className="ident">{p.table}</span>
      {p.total != null && <span className="rows">{n(p.total)} rows</span>}
      {p.json && (
        <span className="tag" title="Made by writing JSON rows: every field any row has is a column">
          JSON
        </span>
      )}
      {p.view && (
        <span className="tag" title={`A ${p.view}: its rows are read only here`}>
          {p.view.toUpperCase()}
        </span>
      )}
      <Popover
        label={
          <>
            <Icon name="search" size={14} /> Filter
          </>
        }
        title="Filter the rows (/)"
        cls={'btn' + (p.filters.length ? ' on' : '')}
        open={open === 'filter'}
        setOpen={toggle('filter')}
      >
        <div className="row2">
          <select aria-label="Column" value={c} onChange={(e) => setCol(e.target.value)}>
            {p.columns.map((x) => (
              <option key={x} value={x}>
                {x}
              </option>
            ))}
          </select>
          <select aria-label="Condition" value={op} onChange={(e) => setOp(e.target.value as Filter['op'])}>
            {OPS.map(([k, t]) => (
              <option key={k} value={k}>
                {t}
              </option>
            ))}
          </select>
        </div>
        {needsValue && (
          <input
            ref={valRef}
            type="text"
            placeholder="Value"
            aria-label="Value"
            value={val}
            onChange={(e) => setVal(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && add()}
          />
        )}
        <div className="row2">
          <button className="btn primary" type="button" onClick={add}>
            Add filter
          </button>
          {p.filters.length > 0 && (
            <button className="btn ghost" type="button" onClick={() => (p.setFilters([]), setOpen(null))}>
              Clear all
            </button>
          )}
        </div>
      </Popover>
      <Popover
        label={
          <>
            <Icon name="sort" size={14} /> Sort
          </>
        }
        title="Sort the rows"
        cls={'btn' + (p.sorted ? ' on' : '')}
        open={open === 'sort'}
        setOpen={toggle('sort')}
      >
        <select
          aria-label="Sort by"
          value={p.sort?.col ?? ''}
          onChange={(e) => p.setSort(e.target.value ? { col: e.target.value, dir: p.sort?.dir ?? 'asc' } : null)}
        >
          <option value="">Stored order</option>
          {p.columns.map((x) => (
            <option key={x} value={x}>
              {x}
            </option>
          ))}
        </select>
        <div className="choice" role="group" aria-label="Direction">
          {(['asc', 'desc'] as const).map((d) => (
            <button key={d} type="button" aria-pressed={p.sort?.dir === d} disabled={!p.sort} onClick={() => p.sort && p.setSort({ col: p.sort.col, dir: d })}>
              {d === 'asc' ? 'Ascending' : 'Descending'}
            </button>
          ))}
        </div>
        <button className="btn ghost" type="button" onClick={() => (p.setSort(undefined), setOpen(null))}>
          Reset to key order
        </button>
      </Popover>
      <Popover
        label={
          <>
            <Icon name="clock" size={14} /> As of
          </>
        }
        title="Read the table as it was at a moment"
        cls={'btn' + (p.asOf ? ' on' : '')}
        open={open === 'asof'}
        setOpen={toggle('asof')}
      >
        <label className="field">
          <span>A time, or a time ago</span>
          <input
            type="text"
            placeholder="-1 hour, or 2026-09-20 10:00"
            value={at}
            onChange={(e) => setAt(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && readAsOf()}
          />
        </label>
        <p className="note">
          Times without a zone are UTC. Past tables are read only.
          {h &&
            (h.from === null
              ? ` No history kept yet (history_retention: ${span(h.retention)}).`
              : ` History goes back to ${historyStart(h.from)} (history_retention: ${span(h.retention)}).`)}
        </p>
        {outside && <p className="err small">{outside}</p>}
        <div className="row2">
          <button className="btn primary" type="button" disabled={!!outside} title={outside || undefined} onClick={readAsOf}>
            Read as of
          </button>
          {p.asOf && (
            <button className="btn ghost" type="button" onClick={() => (setAt(''), p.setAsOf(''), setOpen(null))}>
              Back to now
            </button>
          )}
        </div>
      </Popover>
      {p.filters.map((f, i) => (
        <span className="chip" key={i} title={`${f.col} ${OP_LABEL[f.op]} ${f.val}`}>
          <span>{`${f.col} ${OP_LABEL[f.op]}${f.op === 'null' || f.op === 'notnull' ? '' : ' ' + f.val}`}</span>
          <button type="button" aria-label={`Remove the filter on ${f.col}`} onClick={() => p.setFilters(p.filters.filter((_, j) => j !== i))}>
            ×
          </button>
        </span>
      ))}
      {p.asOf && (
        <span className="chip time">
          <span>as of {when(p.asOf)}</span>
          <button type="button" aria-label="Back to now" onClick={() => (setAt(''), p.setAsOf(''))}>
            ×
          </button>
        </span>
      )}
      <span className="spacer" />
      <button
        className="btn ghost"
        type="button"
        disabled={!!p.asOf || !!p.view || !app.admin}
        title={!app.admin ? ROLE_READ_ONLY : p.view ? `A ${p.view} is read only` : p.asOf ? 'Past tables are read only' : 'Add rows from a CSV, JSON or NDJSON file'}
        onClick={p.onImport}
      >
        <Icon name="upload" size={14} /> Import
      </button>
      <button
        className="btn ghost"
        type="button"
        disabled={!p.total || p.exporting}
        title="Download these rows (filters, sort and as of applied) as CSV"
        onClick={p.onExport}
      >
        <Icon name="download" size={14} /> {p.exporting ? 'Exporting…' : 'Export CSV'}
      </button>
      <button
        className={'btn' + (p.live ? ' live-edit' : '')}
        type="button"
        disabled={!!p.asOf || !!p.view || !app.admin}
        title={!app.admin ? ROLE_READ_ONLY : p.view ? `A ${p.view} is read only` : p.asOf ? 'Past tables are read only' : p.live ? 'Add a row to main (live data)' : 'Add a row'}
        onClick={p.onInsert}
      >
        + Row
      </button>
    </div>
  );
}

/** Runs a write in the active world. On main it asks first: the Chronos way is to fork, edit there, review and merge. */
/** Runs a write in the active world (on main it asks first). Returns the world it ran in, or false.
 *  `reload: false` keeps the view as it is (an edit in the grid updates its own row). */
export async function runWrite(app: App, dlg: Dialogs, what: string, run: (world: string) => Promise<unknown>, opts: { reload?: boolean } = {}) {
  let world = app.world;
  if (world === 'main' && !app.mainOk) {
    const allow = { current: false };
    const choice = await dlg.modal<'main' | 'fork'>(
      'Change live data?',
      <>
        <p>
          You’re about to {what} in <strong>main</strong>, the live data.
        </p>
        <p className="note">Safer: make the change in a new world, review it in Changes, then merge it into main.</p>
        <label className="check note">
          <input type="checkbox" onChange={(e) => (allow.current = e.target.checked)} /> Don’t ask again in this tab
        </label>
      </>,
      [
        { label: 'Cancel', value: () => null },
        { label: 'Change main', cls: 'danger', value: () => 'main' },
        { label: 'Edit in a new world', cls: 'primary', value: () => 'fork' },
      ],
    );
    if (!choice) return false;
    if (choice === 'fork') {
      const name = await app.forkFrom('main', suggestName());
      if (!name) return false;
      world = name;
    } else if (allow.current) app.setMainOk(true);
  }
  try {
    await run(world);
  } catch (e) {
    toast((e as Error).message);
    return false;
  }
  toast(`Saved in ${world}`);
  app.reloadWorlds();
  if (opts.reload !== false) {
    app.reloadTables();
    app.refresh();
  }
  return world;
}
