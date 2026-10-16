// The SQL console: runs in the active world, one result per statement, with timing, SQLSTATE on
// errors, EXPLAIN, and a history of recent queries (this browser's localStorage).
import { useRef, useState } from 'react';
import { sql, type ApiError, type SqlResult } from '../api';
import { useApp } from '../context';
import { Grid, PAGE } from '../Grid';
import { copyText, Empty, StatusSlot } from '../ui';
import { downloadCsv, mod, plural, store } from '../util';

const HISTORY = 'chronos-studio-history';
const history = (): string[] => {
  try {
    return JSON.parse(store.get(HISTORY) || '[]');
  } catch {
    return [];
  }
};

interface Ran {
  id: number;
  world: string;
  results?: SqlResult[];
  error?: ApiError;
  ms: number;
}
// kept across tab switches
let draft = 'select * from information_schema.tables;';
/** Puts `q` in the console, for the next time it opens. */
export const setDraft = (q: string) => {
  draft = q;
  last = null;
};
let last: Ran | null = null;

export function SqlView() {
  const app = useApp();
  const [text, setText] = useState(draft);
  const [ran, setRan] = useState<Ran | null>(last && last.world === app.world ? last : null);
  const [running, setRunning] = useState(false);
  const [showHist, setShowHist] = useState(false);
  const ta = useRef<HTMLTextAreaElement>(null);
  const hl = useRef<HTMLPreElement>(null);
  const run = async (explain: boolean) => {
    const q0 = text.trim();
    if (!q0) return;
    store.set(HISTORY, JSON.stringify([q0, ...history().filter((x) => x !== q0)].slice(0, 50)));
    const q = explain ? 'explain ' + q0.replace(/;\s*$/, '') : q0;
    setRunning(true);
    const t0 = performance.now();
    let r: Ran;
    try {
      r = { id: t0, world: app.world, results: await sql(q, app.world), ms: performance.now() - t0 };
    } catch (e) {
      r = { id: t0, world: app.world, error: e as ApiError, ms: performance.now() - t0 };
    }
    last = r;
    setRan(r);
    setRunning(false);
    app.reloadTables();
  };
  return (
    <div className="pane">
      <div className="sql-top">
        <div className="editor">
          <pre className="hl" aria-hidden="true" ref={hl}>
            {highlight(text)}
            {'\n'}
          </pre>
          <textarea
            ref={ta}
            autoFocus
            spellCheck={false}
            aria-label="SQL"
            placeholder="SQL: several statements are fine, separated by ;"
            value={text}
            onScroll={(e) => hl.current && ((hl.current.scrollTop = e.currentTarget.scrollTop), (hl.current.scrollLeft = e.currentTarget.scrollLeft))}
            onChange={(e) => setText((draft = e.target.value))}
            onKeyDown={(e) => {
              if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
                e.preventDefault();
                run(false);
              }
            }}
          />
        </div>
        <div className="bar">
          <button className="btn primary" type="button" disabled={running} title={`Run (${mod}Enter)`} onClick={() => run(false)}>
            {running ? 'Running…' : 'Run'} <span className="kbd">{mod}↵</span>
          </button>
          <button className="btn" type="button" title="Show the plan of one statement" onClick={() => run(true)}>
            Explain
          </button>
          <button className="btn" type="button" aria-expanded={showHist} onClick={() => setShowHist(!showHist)}>
            History
          </button>
          <span className="spacer" />
          <span className="muted">
            in <code>{app.world}</code>
          </span>
          {app.world === 'main' && <span className="live">Live data</span>}
        </div>
        {showHist && (
          <div className="hist">
            {history().length ? (
              history().map((x) => (
                <button
                  key={x}
                  type="button"
                  title={x}
                  onClick={() => {
                    setText((draft = x));
                    setShowHist(false);
                    ta.current?.focus();
                  }}
                >
                  {x.replace(/\s+/g, ' ')}
                </button>
              ))
            ) : (
              <div className="empty">No queries yet</div>
            )}
          </div>
        )}
      </div>
      {ran ? <Results key={ran.id} ran={ran} /> : <Empty>Results show here. Several statements run in order; each run is its own session.</Empty>}
    </div>
  );
}

// SQL colouring for the editor: comments, strings, numbers, keywords, quoted names
const KEYWORDS = new Set(
  'select from where and or not in is null as on join left right inner outer full cross group by order having limit offset insert into values update set delete create table index view drop alter add column primary key references foreign unique check default distinct union all except intersect with recursive case when then else end exists between like ilike returning begin commit rollback explain analyze asc desc true false cast over partition window filter using natural lateral if replace constraint materialized fork merge of show'.split(
    ' ',
  ),
);
const TOKEN = /(--[^\n]*|\/\*[\s\S]*?\*\/)|('(?:[^']|'')*'?)|("(?:[^"]|"")*"?)|(\b\d+(?:\.\d+)?\b)|([A-Za-z_][A-Za-z0-9_]*)/g;
export function highlight(src: string) {
  const out: React.ReactNode[] = [];
  let last = 0;
  for (const m of src.matchAll(TOKEN)) {
    const [tok, comment, str, quoted, num, word] = m;
    const cls = comment
      ? 'c'
      : str
        ? 's'
        : quoted
          ? 'q'
          : num
            ? 'n'
            : word && KEYWORDS.has(word.toLowerCase())
              ? 'k'
              : word && src[m.index! + tok.length] === '('
                ? 'f'
                : '';
    if (!cls) continue;
    if (m.index! > last) out.push(src.slice(last, m.index));
    out.push(
      <span key={m.index} className={'t-' + cls}>
        {tok}
      </span>,
    );
    last = m.index! + tok.length;
  }
  out.push(src.slice(last));
  return out;
}

function Results({ ran }: { ran: Ran }) {
  const res = ran.results || [];
  const [cur, setCur] = useState(res.length - 1);
  const ms = `${Math.round(ran.ms)} ms`;
  if (ran.error) {
    const d = ran.error.detail || {};
    return (
      <div className="pad">
        <div className="err">
          {d.code && <span className="code">SQLSTATE {d.code}</span>}
          {d.message || ran.error.message}
        </div>
        <p className="muted">{ms}</p>
      </div>
    );
  }
  const x = res[cur];
  if (!x) return <Empty>No statements</Empty>;
  const affected = /^(INSERT \d+ |UPDATE |DELETE |MERGE |COPY )(\d+)/.exec(x.command);
  return (
    <>
      {res.length > 1 && (
        <div className="result-tabs">
          {res.map((r, i) => (
            <button key={i} className={'btn small' + (i === cur ? ' on' : '')} type="button" onClick={() => setCur(i)}>
              {i + 1}. {r.command}
            </button>
          ))}
        </div>
      )}
      <div className="grid-wrap">
        {x.columns.length ? (
          x.rows.length ? (
            <Grid
              key={cur}
              columns={x.columns.map((name, j) => ({ name, type: x.types?.[j] }))}
              total={x.rows.length}
              first={x.rows.slice(0, PAGE)}
              fetchPage={(p) => Promise.resolve(x.rows.slice(p * PAGE, (p + 1) * PAGE))}
            />
          ) : (
            <Empty>No rows</Empty>
          )
        ) : (
          <div className="pad">
            <code>{x.command}</code>
          </div>
        )}
      </div>
      <StatusSlot>
        <span>{x.command}</span>
        {x.columns.length > 0 && <span>{plural(x.rows.length, 'row')}</span>}
        {affected && <span>{plural(Number(affected[2]), 'row')} affected</span>}
        <span>{ms}</span>
        {x.rows.length > 0 && (
          <>
            <button className="btn small ghost" type="button" onClick={() => copyText(JSON.stringify(x.rows))}>
              Copy as JSON
            </button>
            <button className="btn small ghost" type="button" onClick={() => downloadCsv('query', x.columns, x.rows)}>
              Download CSV
            </button>
          </>
        )}
      </StatusSlot>
    </>
  );
}
