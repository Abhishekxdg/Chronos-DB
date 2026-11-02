// Dialogs, popovers, the toast and copying: shared by every view.
import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { useApp } from './context';
import { fold } from './vectors';

// ---------- toast ----------
let showToast: (msg: string) => void = () => {};
export const toast = (msg: string) => showToast(msg);

export function Toast() {
  const [msg, setMsg] = useState('');
  const [on, setOn] = useState(false);
  const timer = useRef<number>(0);
  useEffect(() => {
    showToast = (m) => {
      setMsg(m);
      setOn(true);
      clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setOn(false), 2600);
    };
  }, []);
  return (
    <div className={'toast' + (on ? ' show' : '')} role="status" aria-live="polite">
      {msg}
    </div>
  );
}

export function copyText(s: string) {
  if (!navigator.clipboard) return toast('Copying needs a secure page: select the text instead');
  navigator.clipboard.writeText(s).then(
    () => toast('Copied'),
    () => toast('The browser refused to copy'),
  );
}

// ---------- dialogs ----------
export interface Button<T> {
  label: string;
  cls?: string;
  /** what the dialog resolves to (null: cancelled); undefined keeps it open (a check failed) */
  value: () => T | null | undefined;
}
interface Open {
  title: string;
  body: ReactNode;
  buttons: Button<unknown>[];
  wide?: boolean;
  resolve: (v: unknown) => void;
}
export interface Dialogs {
  modal<T>(title: string, body: ReactNode, buttons: Button<T>[], wide?: boolean): Promise<T | null>;
  confirm(title: string, message: ReactNode, ok: string, opts?: { danger?: boolean; typed?: string | null }): Promise<boolean>;
  prompt(title: string, label: string, value: string, ok: string, hint?: string): Promise<string | null>;
}
const Ctx = createContext<Dialogs | null>(null);
export const useDialogs = () => useContext(Ctx)!;

export function DialogHost({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState<Open | null>(null);
  const modal = <T,>(title: string, body: ReactNode, buttons: Button<T>[], wide?: boolean) =>
    new Promise<T | null>((resolve) => setOpen({ title, body, buttons, wide, resolve: resolve as (v: unknown) => void }));
  const api: Dialogs = {
    modal,
    confirm(title, message, ok, opts = {}) {
      const typed = opts.typed;
      const input = { current: '' };
      return modal<boolean>(
        title,
        <>
          <p>{message}</p>
          {typed && (
            <label className="field">
              <span>
                Type <code>{typed}</code> to confirm
              </span>
              <input type="text" autoComplete="off" spellCheck={false} onChange={(e) => (input.current = e.target.value)} />
            </label>
          )}
        </>,
        [
          { label: 'Cancel', value: () => false },
          {
            label: ok,
            cls: opts.danger ? 'danger solid' : 'primary',
            value: () => (typed && input.current !== typed ? (toast(`Type ${typed} to confirm`), undefined) : true),
          },
        ],
      ).then((v) => v === true);
    },
    prompt(title, label, value, ok, hint) {
      const input = { current: value };
      return modal<string>(
        title,
        <>
          <label className="field">
            <span>{label}</span>
            <input
              type="text"
              defaultValue={value}
              autoComplete="off"
              spellCheck={false}
              onChange={(e) => (input.current = e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') (e.currentTarget.closest('.dialog')?.querySelector('.primary') as HTMLElement | null)?.click();
              }}
            />
          </label>
          {hint && <p className="note">{hint}</p>}
        </>,
        [
          { label: 'Cancel', value: () => null },
          { label: ok, cls: 'primary', value: () => input.current.trim() || undefined },
        ],
      );
    },
  };
  return (
    <Ctx.Provider value={api}>
      {children}
      {open && <Dialog open={open} close={(v) => (setOpen(null), open.resolve(v))} />}
    </Ctx.Provider>
  );
}

function Dialog({ open, close }: { open: Open; close: (v: unknown) => void }) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    const f = box.current?.querySelector<HTMLElement>('input, textarea, select') || box.current?.querySelector<HTMLElement>('.df button:last-child');
    f?.focus();
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        close(null);
      }
    };
    document.addEventListener('keydown', key, true);
    return () => {
      document.removeEventListener('keydown', key, true);
      prev?.focus?.();
    };
  }, []);
  return (
    <div className="overlay center" onMouseDown={(e) => e.target === e.currentTarget && close(null)}>
      <div className={'dialog' + (open.wide ? ' wide' : '')} role="dialog" aria-modal="true" aria-label={open.title} ref={box}>
        <div className="dh">{open.title}</div>
        <div className="db">{open.body}</div>
        <div className="df">
          {open.buttons.map((b) => (
            <button
              key={b.label}
              type="button"
              className={'btn ' + (b.cls || '')}
              onClick={() => {
                const v = b.value();
                if (v !== undefined) close(v);
              }}
            >
              {b.label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

// ---------- popover: a button and a panel that closes on Esc or a click outside ----------
export function Popover(p: {
  label: ReactNode;
  title?: string;
  cls?: string;
  right?: boolean;
  menu?: boolean;
  open: boolean;
  setOpen: (o: boolean) => void;
  children: ReactNode;
}) {
  const { open, setOpen } = p;
  const wrap = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => !wrap.current?.contains(e.target as Node) && setOpen(false);
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        setOpen(false);
        (wrap.current?.querySelector('button') as HTMLElement | null)?.focus();
      }
    };
    document.addEventListener('mousedown', down);
    document.addEventListener('keydown', key, true);
    return () => {
      document.removeEventListener('mousedown', down);
      document.removeEventListener('keydown', key, true);
    };
  }, [open, setOpen]);
  return (
    <span className="pop-wrap" ref={wrap}>
      <button type="button" className={p.cls ?? 'btn'} title={p.title} aria-expanded={open} aria-haspopup="true" onClick={() => setOpen(!open)}>
        {p.label}
      </button>
      {open && <div className={'pop' + (p.right ? ' right' : '') + (p.menu ? ' menu' : '')}>{p.children}</div>}
    </span>
  );
}

export const Empty = ({ children }: { children: ReactNode }) => <div className="pad muted">{children}</div>;
export function ErrorBox({ error }: { error: unknown }) {
  const d = (error as { detail?: { code?: string; message?: string } })?.detail;
  if (d && typeof d === 'object' && d.code)
    return (
      <div className="err">
        <span className="code">SQLSTATE {d.code}</span>
        {d.message}
      </div>
    );
  return <div className="err">{error instanceof Error ? error.message : String(error)}</div>;
}

/** A plain table of text cells; columns in `num` are numbers, right-aligned. */
export function Table({ heads, rows, num }: { heads: string[]; rows: (string | number | null | undefined)[][]; num?: number[] }) {
  if (!rows.length) return <p className="note">None</p>;
  return (
    <div className="tbl-wrap">
      <table className="tbl">
        <thead>
          <tr>
            {heads.map((h) => (
              <th key={h}>{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              {r.map((c, j) => (
                <td key={j} className={num?.includes(j) ? 'num' : 'mono'}>
                  {c == null ? '' : String(c)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** The view's own status line, shown in the status bar at the foot of the window. */
export function StatusSlot({ children }: { children: ReactNode }) {
  const slot = useApp().statusSlot;
  return slot ? createPortal(children, slot) : null;
}

/** A vector at a glance: its numbers folded into bars (accent above zero, orange below), and its size. */
export function VecStrip({ v, bins = 32, h = 14, dims = true }: { v: number[]; bins?: number; h?: number; dims?: boolean }) {
  const f = fold(v, bins);
  const max = Math.max(1e-9, ...f.map(Math.abs));
  const w = 3;
  return (
    <span className="vec" title={`vector of ${v.length} numbers`}>
      <svg width={f.length * w} height={h} viewBox={`0 0 ${f.length * w} ${h}`} aria-hidden="true">
        <line x1="0" x2={f.length * w} y1={h / 2} y2={h / 2} className="axis" />
        {f.map((x, i) => {
          const len = Math.max(0.75, (Math.abs(x) / max) * (h / 2 - 0.5));
          return <rect key={i} x={i * w} y={x >= 0 ? h / 2 - len : h / 2} width={w - 1} height={len} className={x >= 0 ? 'pos' : 'neg'} />;
        })}
      </svg>
      {dims && <span className="d">{v.length}d</span>}
    </span>
  );
}
