// The data grid: only the visible rows are rendered, and rows arrive a page at a time, so a table
// of any size scrolls smoothly without the browser ever holding all of it.
import { useEffect, useLayoutEffect, useReducer, useRef, useState } from 'react';
import type { Row } from './api';
import { copyText, toast, VecStrip } from './ui';
import { isVector } from './vectors';
import { typeMark } from './icons';
import { rowHeight, text, valClass } from './util';

export const PAGE = 200; // rows per request
const HEAD = 44;
const MAX_H = 15e6; // px: taller scroll areas break in some browsers, so very long tables are scaled
const KEEP = 40; // pages kept in memory

export interface Column {
  name: string;
  type?: string;
  pk?: boolean;
}
export interface Sort {
  col: string;
  dir: 'asc' | 'desc';
}

interface Props {
  columns: Column[];
  total: number;
  first?: Row[];
  fetchPage?: (page: number) => Promise<Row[]>;
  sort?: Sort | null;
  onSort?: (col: string) => void;
  /** a header was picked (the inspector shows the schema) */
  onHeader?: (col: string) => void;
  headerSel?: string | null;
  /** the selected row changed (J/K, arrows, a click) */
  onPick?: (row: Row | undefined, i: number) => void;
  /** open the row (Enter, a double click, its number) */
  onOpen?: (row: Row, i: number) => void;
  /** whether a column's cells can be edited in place (double-click, or F2) */
  canEdit?: (col: string) => boolean;
  /** saves an edit; resolves to the row as stored, or null (cancelled, failed, or saved elsewhere) */
  onCellEdit?: (row: Row, col: string, text: string) => Promise<Row | null>;
  onRange?: (first: number, last: number) => void;
}

/** Remount (change its `key`) when the query changes: pages are cached per grid. */
export function Grid(p: Props) {
  const [ROW] = useState(rowHeight);
  const { columns, total } = p;
  const el = useRef<HTMLDivElement>(null);
  const pages = useRef(new Map<number, Row[] | 'loading'>(p.first ? [[0, p.first]] : []));
  const [, redraw] = useReducer((x: number) => x + 1, 0);
  const [view, setView] = useState({ top: 0, height: 600 });
  const [sel, setSel] = useState<{ i: number; c: number } | null>(null);
  const [editing, setEditing] = useState<{ i: number; c: number; text: string; saving?: boolean } | null>(null);
  const alive = useRef(true);
  const timer = useRef(0);

  const measure = () => {
    const e = el.current;
    if (e) setView({ top: e.scrollTop, height: e.clientHeight });
  };
  useLayoutEffect(() => {
    measure();
    const ro = new ResizeObserver(measure);
    if (el.current) ro.observe(el.current);
    return () => {
      ro.disconnect();
      alive.current = false;
      clearTimeout(timer.current);
    };
  }, []);

  const full = total * ROW;
  const H = Math.min(full, MAX_H);
  const vh = Math.max(0, view.height - HEAD);
  const k = full > H && H > vh ? (full - vh) / (H - vh) : 1;
  const vtop = view.top * k;
  const firstRow = Math.max(0, Math.floor(vtop / ROW) - 4);
  const lastRow = Math.min(total, Math.ceil((vtop + vh) / ROW) + 4);
  const row = (i: number) => {
    const pg = pages.current.get(Math.floor(i / PAGE));
    return Array.isArray(pg) ? pg[i % PAGE] : undefined;
  };
  const visible = [Math.min(total, Math.floor(vtop / ROW) + 1), Math.min(total, Math.floor((vtop + vh) / ROW))];
  useEffect(() => p.onRange?.(visible[0], visible[1]), [visible[0], visible[1]]); // eslint-disable-line

  // fetch the pages in view once scrolling pauses, not on every frame of a fast scroll
  useEffect(() => {
    if (!p.fetchPage || !total) return;
    clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      const a = Math.floor(firstRow / PAGE);
      const b = Math.floor(Math.max(firstRow, lastRow - 1) / PAGE);
      for (let pg = a; pg <= b; pg++) {
        if (pages.current.has(pg)) continue;
        pages.current.set(pg, 'loading');
        p.fetchPage!(pg).then(
          (rows) => {
            if (!alive.current) return;
            const m = pages.current;
            m.set(pg, rows);
            if (m.size > KEEP) {
              const far = [...m.keys()].sort((x, y) => Math.abs(y - pg) - Math.abs(x - pg));
              for (const q of far.slice(0, m.size - KEEP)) m.delete(q);
            }
            redraw();
          },
          (e: Error) => {
            pages.current.delete(pg);
            toast(e.message);
          },
        );
      }
    }, 60);
  }, [firstRow, lastRow, p.fetchPage, total]);

  const pick = (s: { i: number; c: number }) => {
    setSel(s);
    p.onPick?.(row(s.i), s.i);
  };
  const editable = (c: number) => !!p.onCellEdit && !!p.canEdit?.(columns[c].name);
  const startEdit = (i: number, c: number) => {
    const r = row(i);
    if (!r || !editable(c)) return false;
    const v = r[columns[c].name];
    setEditing({ i, c, text: v === null || v === undefined ? '' : typeof v === 'object' ? JSON.stringify(v) : String(v) });
    return true;
  };
  const saving = useRef(false); // a dialog taking focus mid-save blurs the input: save once
  const commit = async () => {
    if (!editing || saving.current) return;
    const r = row(editing.i);
    const col = columns[editing.c].name;
    const before = r?.[col];
    const was = before === null || before === undefined ? '' : typeof before === 'object' ? JSON.stringify(before) : String(before);
    if (!r || editing.text === was) return setEditing(null);
    setEditing({ ...editing, saving: true });
    saving.current = true;
    const stored = await p.onCellEdit!(r, col, editing.text).finally(() => (saving.current = false));
    if (!alive.current) return;
    if (stored) {
      const pg = pages.current.get(Math.floor(editing.i / PAGE));
      if (Array.isArray(pg)) pg[editing.i % PAGE] = stored;
      p.onPick?.(stored, editing.i);
    }
    setEditing(null);
    el.current?.focus();
  };
  const key = (e: React.KeyboardEvent) => {
    if (e.target !== el.current) return;
    if (e.key === 'F2' && sel) {
      e.preventDefault();
      startEdit(sel.i, sel.c);
      return;
    }
    const cur = sel ?? { i: -1, c: 0 };
    const mv = (
      { ArrowDown: [1, 0], j: [1, 0], ArrowUp: [-1, 0], k: [-1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1], PageDown: [10, 0], PageUp: [-10, 0] } as Record<
        string,
        number[]
      >
    )[e.key];
    if (mv && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault();
      const s = { i: Math.max(0, Math.min(total - 1, cur.i + mv[0])), c: Math.max(0, Math.min(columns.length - 1, cur.c + mv[1])) };
      pick(s);
      const box = el.current;
      if (box && full <= MAX_H) {
        const top = s.i * ROW;
        if (top < box.scrollTop) box.scrollTop = top;
        else if (top + ROW > box.scrollTop + vh) box.scrollTop = top + ROW - vh;
      }
    } else if (e.key === 'Enter' && sel) {
      const r = row(sel.i);
      if (r) p.onOpen?.(r, sel.i);
    } else if ((e.metaKey || e.ctrlKey) && e.key === 'c' && sel && !String(getSelection())) {
      const r = row(sel.i);
      if (r) {
        e.preventDefault();
        copyText(text(r[columns[sel.c].name]));
      }
    }
  };

  const [widths] = useState(() =>
    columns.map((c) => {
      if ((p.first || []).slice(0, 5).some((r) => isVector(r[c.name]))) return 168;
      const data = Math.max(0, ...(p.first || []).slice(0, 100).map((r) => text(r[c.name], 48).length));
      return Math.round(Math.min(360, Math.max(84, Math.max(c.name.length * 7.6, ((c.type?.length || 0) + 4) * 6.4) + 56, data * 7.4 + 24)));
    }),
  );
  const nw = Math.max(44, String(total).length * 7.5 + 20);
  const cols = [nw + 'px', ...widths.map((w) => w + 'px')].join(' ');
  const rows = [];
  for (let i = firstRow; i < lastRow; i++) {
    const r = row(i);
    const top = k === 1 ? i * ROW : view.top + (i * ROW - vtop);
    rows.push(
      <div
        key={i}
        className={'row' + (sel?.i === i ? ' picked' : '')}
        role="row"
        aria-rowindex={i + 2}
        style={{ top }}
        onDoubleClick={() => r && p.onOpen?.(r, i)}
      >
        <div className="cell rn" title="Open this row" onClick={() => r && (pick({ i, c: sel?.c ?? 0 }), p.onOpen?.(r, i))}>
          {i + 1}
        </div>
        {columns.map((c, ci) => {
          if (!r)
            return (
              <div key={c.name} className="cell">
                <span className="skel" />
              </div>
            );
          const v = r[c.name];
          const t = isVector(v) ? '' : text(v, 300);
          return (
            <div
              key={c.name}
              role="gridcell"
              className={'cell ' + valClass(v, c.type) + (sel?.i === i && sel.c === ci ? ' on' : '')}
              title={
                isVector(v)
                  ? `${v.length} numbers: ${v
                      .slice(0, 8)
                      .map((x) => +x.toFixed(4))
                      .join(', ')}…`
                  : t.length > 24
                    ? text(v, 2000)
                    : undefined
              }
              onClick={() => pick({ i, c: ci })}
              onDoubleClick={(e) => {
                if (startEdit(i, ci)) e.stopPropagation();
              }}
            >
              {editing?.i === i && editing.c === ci ? (
                <input
                  className="cell-edit"
                  autoFocus
                  aria-label={`Edit ${c.name}`}
                  value={editing.text}
                  disabled={editing.saving}
                  spellCheck={false}
                  onChange={(e) => setEditing({ ...editing, text: e.target.value })}
                  onKeyDown={(e) => {
                    e.stopPropagation();
                    if (e.key === 'Enter') commit();
                    else if (e.key === 'Escape') (setEditing(null), el.current?.focus());
                  }}
                  onBlur={() => commit()}
                />
              ) : v === null || v === undefined ? (
                <span className="null">null</span>
              ) : isVector(v) ? (
                <VecStrip v={v} />
              ) : typeof v === 'boolean' ? (
                <span className={'bool ' + v}>{t}</span>
              ) : (
                t
              )}
            </div>
          );
        })}
      </div>,
    );
  }
  return (
    <div
      className="grid"
      ref={el}
      tabIndex={0}
      role="grid"
      aria-rowcount={total + 1}
      aria-label="Rows"
      style={{ '--cols': cols } as React.CSSProperties}
      onScroll={measure}
      onKeyDown={key}
    >
      <div className="head" role="row">
        <div className="rn">#</div>
        {columns.map((c) => {
          const s = p.sort?.col === c.name ? p.sort.dir : null;
          return (
            <div
              key={c.name}
              role="columnheader"
              aria-sort={s === 'asc' ? 'ascending' : s === 'desc' ? 'descending' : undefined}
              className={p.headerSel === c.name ? 'sel' : ''}
              title={`${c.name}${c.type ? ' · ' + c.type : ''}${c.pk ? ' · key' : ''}${p.onHeader ? '\nClick for its schema' : ''}`}
              onClick={() => p.onHeader?.(c.name)}
            >
              <span className="hn">
                <span className={'nm' + (c.pk ? ' key' : '')}>{c.name}</span>
                {c.type && (
                  <span className="t">
                    <span className="tm">{c.pk ? 'key' : typeMark(c.type)}</span> {c.type}
                  </span>
                )}
              </span>
              {p.onSort && (
                <button
                  type="button"
                  className={'sort' + (s ? ' on' : '')}
                  aria-label={`Sort by ${c.name}`}
                  title="Sort"
                  onClick={(e) => {
                    e.stopPropagation();
                    p.onSort!(c.name);
                  }}
                >
                  {s === 'asc' ? '▲' : s === 'desc' ? '▼' : '⇅'}
                </button>
              )}
            </div>
          );
        })}
      </div>
      <div className="body" style={{ height: H }}>
        {rows}
      </div>
    </div>
  );
}
