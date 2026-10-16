// The inspector beside the grid: a row's fields (label over value, JSON pretty-printed), editing
// and deleting it, adding a row, or the table's schema when a column header is picked.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api, sql, type Row } from './api';
import { useApp } from './context';
import { Icon } from './icons';
import type { Column } from './Grid';
import { copyText, ErrorBox, toast, useDialogs, VecStrip } from './ui';
import { isVector, norm } from './vectors';
import { lit, qid, qtable, splitName, text } from './util';
import { castType, type TableInfo } from './views/Data';

export type Inspect = { mode: 'row'; row: Row; edit?: boolean } | { mode: 'insert' } | { mode: 'schema'; col: string };
type Write = (what: string, run: (world: string) => Promise<unknown>) => Promise<unknown>;

interface Props {
  inspect: Inspect;
  table: string;
  info: TableInfo;
  columns: Column[];
  asOf: string;
  /** why its rows can't be changed here (a view), if they can't */
  readOnly?: string;
  live: boolean;
  onEdit: (row: Row) => void;
  onClose: () => void;
  write: Write;
}

export function Inspector(p: Props) {
  const box = useRef<HTMLElement>(null);
  useEffect(() => {
    const key = (e: KeyboardEvent) => e.key === 'Escape' && !document.querySelector('.overlay, .pop') && p.onClose();
    addEventListener('keydown', key);
    return () => removeEventListener('keydown', key);
  }, [p.onClose]);
  useEffect(() => {
    if (p.inspect.mode !== 'schema') box.current?.querySelector<HTMLElement>('textarea, input')?.focus();
  }, []);
  const title =
    p.inspect.mode === 'row' ? `${p.table} ${rowId(p.info.pk, p.inspect.row) ?? ''}` : p.inspect.mode === 'insert' ? `New row in ${p.table}` : p.table;
  return (
    <aside className="inspector" ref={box} aria-label="Inspector">
      <div className="ih">
        <span className="label">{p.inspect.mode === 'schema' ? 'Schema' : p.inspect.mode === 'insert' ? 'Add' : p.inspect.edit ? 'Edit' : 'Row'}</span>
        <span className="ident" title={title}>
          {title}
        </span>
        <span className="spacer" />
        {p.live && p.inspect.mode !== 'schema' && !p.asOf && <span className="live">Live data</span>}
        <button className="btn ghost icon" type="button" aria-label="Close the inspector (Esc)" title="Close (Esc)" onClick={p.onClose}>
          ×
        </button>
      </div>
      {p.inspect.mode === 'schema' ? (
        <SchemaPanel table={p.table} info={p.info} col={p.inspect.col} />
      ) : p.inspect.mode === 'insert' ? (
        <InsertPanel {...p} />
      ) : (
        <RowPanel {...p} row={p.inspect.row} edit={!!p.inspect.edit} />
      )}
    </aside>
  );
}

/** A row's id as the JSON ops (put, delete) name it: its key's value, or (a,b) for several columns. */
export function rowId(pk: string[], row: Row): string | null {
  const vals = pk.map((c) => row[c]);
  if (!vals.length || vals.some((v) => v === undefined || v === null)) return null;
  if (vals.length === 1) return typeof vals[0] === 'object' ? JSON.stringify(vals[0]) : String(vals[0]);
  const field = (v: unknown) => {
    const s = typeof v === 'boolean' ? (v ? 't' : 'f') : typeof v === 'object' ? JSON.stringify(v) : String(v);
    return s === '' || /[\s"\\(),]/.test(s) ? '"' + s.replace(/\\/g, '\\\\').replace(/"/g, '""') + '"' : s;
  };
  return '(' + vals.map(field).join(',') + ')';
}

const NUMERIC = /^(smallint|integer|bigint|real|double precision|numeric|decimal|int2|int4|int8|float4|float8)/;
const editText = (v: unknown) => (v === null || v === undefined ? '' : typeof v === 'object' ? JSON.stringify(v, null, 2) : String(v));
function parseField(type: string, s: string, orig: unknown): unknown {
  if (/json/.test(type) || (!type && typeof orig === 'object' && orig !== null)) return JSON.parse(s);
  if (NUMERIC.test(type) && /^-?\d+(\.\d+)?([eE][-+]?\d+)?$/.test(s.trim())) return Number(s);
  if (type === 'boolean') return /^(t|true|1|yes|on)$/i.test(s.trim());
  return s;
}

function RowPanel(p: Props & { row: Row; edit: boolean }) {
  const dlg = useDialogs();
  const app = useApp();
  const { row, info } = p;
  const id = rowId(info.pk, row);
  const readonly = !!p.asOf || !!p.readOnly || id === null;
  const [vals, setVals] = useState<Record<string, string>>(() => Object.fromEntries(p.columns.map((c) => [c.name, editText(row[c.name])])));
  const [nulls, setNulls] = useState<Record<string, boolean>>(() => Object.fromEntries(p.columns.map((c) => [c.name, row[c.name] === null])));
  const [json, setJson] = useState(() => {
    const rec = { ...row };
    delete rec.id;
    return JSON.stringify(rec, null, 2);
  });
  const save = () => {
    let record: Record<string, unknown>;
    try {
      record = info.json
        ? JSON.parse(json)
        : Object.fromEntries(p.columns.map((c) => [c.name, nulls[c.name] ? null : parseField(c.type || '', vals[c.name], row[c.name])]));
    } catch (e) {
      return toast('Not valid JSON: ' + (e as Error).message);
    }
    p.write(`change row ${id} of ${p.table}`, (world) => api('put', { branch: world, table: p.table, id, record }));
  };
  const del = async () => {
    if (!(await dlg.confirm('Delete this row?', `Row ${id} of ${p.table} will be deleted.`, 'Delete row', { danger: true }))) return;
    p.write(`delete row ${id} of ${p.table}`, (world) => api('delete', { branch: world, table: p.table, id }));
  };
  const note = p.readOnly
    ? p.readOnly
    : id === null
      ? 'This table has no primary key: change its rows with SQL.'
      : p.asOf
        ? `As of ${p.asOf}: read only.`
        : null;
  return (
    <>
      <div className="ib">
        {note && <p className="note">{note}</p>}
        {!p.edit ? (
          p.columns.map((c) => {
            const v = row[c.name];
            const obj = typeof v === 'object' && v !== null;
            return (
              <div className="kv" key={c.name}>
                <div className="k">
                  <span>{c.name}</span>
                  {c.type && <span className="t">{c.type}</span>}
                  {c.pk && <span className="t">key</span>}
                  <button className="btn ghost small copy" type="button" aria-label={`Copy ${c.name}`} onClick={() => copyText(text(v))}>
                    Copy
                  </button>
                </div>
                {isVector(v) ? (
                  <div className="v vecbox">
                    <VecStrip v={v} bins={64} h={34} dims={false} />
                    <span className="vec-stats">
                      {v.length} dimensions · length {norm(v).toFixed(3)} · range {v.reduce((m, x) => Math.min(m, x), Infinity).toFixed(3)} to{' '}
                      {v.reduce((m, x) => Math.max(m, x), -Infinity).toFixed(3)}
                    </span>
                    {id !== null && (
                      <button
                        className="btn small accent"
                        type="button"
                        onClick={() => {
                          app.setSearch({ table: p.table, field: c.name, id });
                          app.show('search');
                        }}
                      >
                        <Icon name="search" size={13} /> Find similar rows
                      </button>
                    )}
                  </div>
                ) : (
                  <div className={'v' + (obj ? ' json' : '') + (v === null || v === undefined ? ' v-null' : '')}>
                    {obj ? JSON.stringify(v, null, 2) : text(v)}
                  </div>
                )}
              </div>
            );
          })
        ) : info.json ? (
          <label className="field">
            <span>Fields (a JSON object; the id stays {id})</span>
            <textarea rows={16} spellCheck={false} value={json} onChange={(e) => setJson(e.target.value)} />
          </label>
        ) : (
          p.columns.map((c) => {
            const fixed = !!c.pk;
            const v = row[c.name];
            return (
              <div className="kv" key={c.name}>
                <div className="k">
                  <label htmlFor={'f-' + c.name}>{c.name}</label>
                  {c.type && <span className="t">{c.type}</span>}
                  {fixed && <span className="t">key · fixed</span>}
                  <span className="spacer" />
                  {!fixed && (
                    <label className="check">
                      <input type="checkbox" checked={nulls[c.name]} onChange={(e) => setNulls({ ...nulls, [c.name]: e.target.checked })} /> null
                    </label>
                  )}
                </div>
                <textarea
                  id={'f-' + c.name}
                  rows={typeof v === 'object' && v !== null ? 5 : 1}
                  readOnly={fixed}
                  disabled={nulls[c.name]}
                  spellCheck={false}
                  value={vals[c.name]}
                  onChange={(e) => setVals({ ...vals, [c.name]: e.target.value })}
                />
              </div>
            );
          })
        )}
      </div>
      <div className="if">
        {p.edit ? (
          <>
            <button className="btn primary" type="button" onClick={save}>
              Save row
            </button>
            <button className="btn ghost" type="button" onClick={p.onClose}>
              Cancel
            </button>
          </>
        ) : (
          <>
            {!readonly && (
              <button className="btn" type="button" title="Edit (E)" onClick={() => p.onEdit(row)}>
                Edit <span className="kbd">E</span>
              </button>
            )}
            <button className="btn ghost" type="button" onClick={() => copyText(JSON.stringify(row))}>
              Copy row
            </button>
            <span className="spacer" />
            {!readonly && (
              <button className="btn ghost danger" type="button" onClick={del}>
                Delete
              </button>
            )}
          </>
        )}
      </div>
    </>
  );
}

function InsertPanel(p: Props) {
  const { info } = p;
  const [vals, setVals] = useState<Record<string, string>>({});
  const [id, setId] = useState('');
  const [json, setJson] = useState('{\n  \n}');
  const save = () => {
    if (info.json) {
      let record: unknown;
      try {
        record = JSON.parse(json);
      } catch (e) {
        return toast('Not valid JSON: ' + (e as Error).message);
      }
      if (!id.trim()) return toast('Give the row an id');
      return p.write(`add a row to ${p.table}`, (world) => api('put', { branch: world, table: p.table, id: id.trim(), record }));
    }
    const set = info.cols.filter((c) => (vals[c.column_name] ?? '') !== '');
    const stmt = set.length
      ? `insert into ${qtable(p.table)} (${set.map((c) => qid(c.column_name)).join(', ')}) values (${set.map((c) => `CAST(${lit(vals[c.column_name])} AS ${castType(c)})`).join(', ')})`
      : `insert into ${qtable(p.table)} default values`;
    return p.write(`add a row to ${p.table}`, (world) => sql(stmt, world));
  };
  return (
    <>
      <div className="ib">
        <p className="note">{info.json ? 'A JSON table: any fields.' : 'Leave a field empty for its default, or null.'}</p>
        {info.json ? (
          <>
            <label className="field">
              <span>id</span>
              <input type="text" value={id} onChange={(e) => setId(e.target.value)} />
            </label>
            <label className="field">
              <span>Fields (a JSON object)</span>
              <textarea rows={10} spellCheck={false} value={json} onChange={(e) => setJson(e.target.value)} />
            </label>
          </>
        ) : (
          info.cols.map((c) => (
            <div className="kv" key={c.column_name}>
              <div className="k">
                <label htmlFor={'n-' + c.column_name}>{c.column_name}</label>
                <span className="t">{castType(c)}</span>
                {info.pk.includes(c.column_name) && <span className="t">key</span>}
              </div>
              <textarea
                id={'n-' + c.column_name}
                rows={1}
                spellCheck={false}
                placeholder={c.column_default ? 'default: ' + c.column_default : c.is_nullable === 'YES' ? 'null' : 'required'}
                value={vals[c.column_name] ?? ''}
                onChange={(e) => setVals({ ...vals, [c.column_name]: e.target.value })}
              />
            </div>
          ))
        )}
      </div>
      <div className="if">
        <button className="btn primary" type="button" onClick={save}>
          Add row
        </button>
        <button className="btn ghost" type="button" onClick={p.onClose}>
          Cancel
        </button>
      </div>
    </>
  );
}

type Settled = PromiseSettledResult<Row[]>;
function SchemaPanel({ table, info, col }: { table: string; info: TableInfo; col: string }) {
  const { world } = useApp();
  const [parts, setParts] = useState<Settled[] | null>(null);
  useEffect(() => {
    if (info.json) return;
    const [s, t] = splitName(table);
    const q = (x: string) => sql(x, world).then((r) => r[0].rows);
    Promise.allSettled([
      q(
        `select tc.constraint_name, tc.constraint_type, k.column_name from information_schema.table_constraints tc left join information_schema.key_column_usage k on k.constraint_schema = tc.constraint_schema and k.constraint_name = tc.constraint_name where tc.table_schema = ${lit(s)} and tc.table_name = ${lit(t)} order by tc.constraint_name, k.ordinal_position`,
      ),
      q(
        `select rc.constraint_name, ccu.table_name as ref_table, ccu.column_name as ref_column, rc.delete_rule from information_schema.referential_constraints rc join information_schema.constraint_column_usage ccu on ccu.constraint_schema = rc.constraint_schema and ccu.constraint_name = rc.constraint_name where rc.constraint_schema = ${lit(s)}`,
      ),
      q(`select indexname, indexdef from pg_indexes where schemaname = ${lit(s)} and tablename = ${lit(t)} order by indexname`),
      q(`select constraint_name, check_clause from information_schema.check_constraints where constraint_schema = ${lit(s)}`),
      q(`select table_schema, table_name from information_schema.views where table_schema not in ('information_schema', 'pg_catalog') order by table_name`),
    ]).then(setParts);
  }, [table, world, info.json]);
  const sec = (title: string, body: ReactNode) => (
    <div className="sec" key={title}>
      <h3 className="label">{title}</h3>
      {body}
    </div>
  );
  const cols = (
    <div>
      {info.cols.map((c) => (
        <div key={c.column_name} className={'schema-col' + (c.column_name === col ? ' hl' : '')}>
          <span className="n">{c.column_name}</span>
          <span className="ty">{castType(c)}</span>
          <span className="d">
            {[
              info.pk.includes(c.column_name) && !info.json && 'primary key',
              c.is_nullable === 'NO' && 'not null',
              c.column_default && `default ${c.column_default}`,
            ]
              .filter(Boolean)
              .join(' · ') || '\u00a0'}
          </span>
        </div>
      ))}
    </div>
  );
  if (info.json)
    return (
      <div className="ib">
        <p className="note">
          A JSON table (made by writing rows, no <code>CREATE TABLE</code>): SQL sees its key as the text column <code>id</code>, plus every field any row has.
        </p>
        {sec('Fields', cols)}
      </div>
    );
  const group = new Map<string, { type: string; cols: string[] }>();
  const [cons, fks, idx, checks, views] = parts || [];
  if (cons?.status === 'fulfilled')
    for (const r of cons.value) {
      const name = String(r.constraint_name);
      const g = group.get(name) || { type: String(r.constraint_type), cols: [] };
      group.set(name, g);
      if (r.column_name) g.cols.push(String(r.column_name));
    }
  const of = (ty: string) => [...group].filter(([, g]) => g.type === ty);
  const lines = (items: [string, string][]) =>
    items.length ? (
      items.map(([a, b]) => (
        <div className="item-line" key={a}>
          {a} <span className="d">{b}</span>
        </div>
      ))
    ) : (
      <span className="note">None</span>
    );
  const settled = (r: Settled | undefined, f: (rows: Row[]) => ReactNode) =>
    !r ? <span className="note">Loading…</span> : r.status === 'fulfilled' ? f(r.value) : <ErrorBox error={r.reason} />;
  const fk = new Set(of('FOREIGN KEY').map(([k]) => k));
  return (
    <div className="ib">
      {sec('Columns', cols)}
      {sec(
        'Unique',
        settled(cons, () => lines(of('UNIQUE').map(([k, g]) => [g.cols.join(', '), k]))),
      )}
      {sec(
        'Foreign keys',
        settled(fks, (rows) => {
          const out = new Map<string, { refs: string[]; t: string; del: string }>();
          for (const r of rows.filter((x) => fk.has(String(x.constraint_name)))) {
            const k = String(r.constraint_name);
            const e = out.get(k) || { refs: [], t: String(r.ref_table), del: String(r.delete_rule ?? '') };
            out.set(k, e);
            e.refs.push(String(r.ref_column));
          }
          return lines([...out].map(([k, e]) => [`${group.get(k)!.cols.join(', ')} → ${e.t}(${e.refs.join(', ')})`, `on delete ${e.del.toLowerCase()}`]));
        }),
      )}
      {sec(
        'Checks',
        settled(checks, (rows) => {
          const clause = new Map(rows.map((r) => [String(r.constraint_name), String(r.check_clause ?? '')]));
          return lines(of('CHECK').map(([k]) => [clause.get(k) || k, k]));
        }),
      )}
      {sec(
        'Indexes',
        settled(idx, (rows) =>
          lines(
            rows.map((r) => [String(r.indexname), String(r.indexdef).replace(/^CREATE (UNIQUE )?INDEX \S+ ON \S+ USING /, (_, u) => (u ? 'unique ' : ''))]),
          ),
        ),
      )}
      {sec(
        'Views in this world',
        settled(views, (rows) => lines(rows.map((r) => [(r.table_schema === 'public' ? '' : r.table_schema + '.') + String(r.table_name), '']))),
      )}
    </div>
  );
}
