// Where the decision happens: what the active world changed since its fork (a per-table summary,
// then the paged diff with only the changed columns, before → after), a dry-run preview with
// conflicts explained and settled one by one, then Merge into <parent>, Discard, or Undo merge.
import { useEffect, useState } from 'react';
import { api, type ApiError, type Change, type Reader, type Row } from '../api';
import { useApp, worldOf, type App } from '../context';
import { Empty, ErrorBox, toast, type Dialogs, useDialogs } from '../ui';
import { n, plural, text } from '../util';

const PAGE = 50;

interface Planned {
  key: string;
  outcome: string;
  detail: string;
  ours: Row | null;
  theirs: Row | null;
  explain?: string;
}
interface Plan {
  rows: Planned[];
  rows_total: number;
  conflicts: number;
  blocked: string | null;
  outcomes: Record<string, number>;
}
type Pick = 'ours' | 'theirs';

export function ChangesView() {
  const app = useApp();
  const dlg = useDialogs();
  const w = worldOf(app, app.world);
  const [counts, setCounts] = useState<{ total: number; tables: Record<string, number>; readers?: Reader[]; coverage?: string[] } | null>(null);
  const [changes, setChanges] = useState<Change[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(false);
  const [plan, setPlan] = useState<Plan | null>(null);
  const [picks, setPicks] = useState<Record<string, Pick>>({});
  const [busy, setBusy] = useState(false);
  const page = async (after: string | null) => {
    setLoading(true);
    try {
      const r = await api<{ changes: Change[]; next: string | null }>('diff', { branch: app.world, limit: PAGE, after });
      setChanges((c) => [...(after ? c : []), ...r.changes]);
      setNext(r.next);
    } catch (e) {
      setError(e);
    }
    setLoading(false);
  };
  useEffect(() => {
    if (!w?.parent) return;
    api('diff', { branch: app.world, count: true, readers: true }).then(setCounts, setError);
    page(null);
  }, []);

  const m = app.lastMerge;
  const undo = m && (
    <div className="banner">
      <span>
        Merged <code>{m.world}</code> into <code>{m.into}</code>: {plural(m.rows, 'change')}.
      </span>
      <button className="btn small" type="button" onClick={() => undoMerge(app, dlg)}>
        Undo merge
      </button>
    </div>
  );
  if (!w?.parent)
    return (
      <div className="scroll">
        <div className="pad stack">
          {undo}
          <Empty>main is where changes land: it has nothing to merge. Fork a world, change it, then review its changes here.</Empty>
        </div>
      </div>
    );
  const parent = w.parent;
  const preview = async () => {
    setBusy(true);
    try {
      const p = await api<Plan>('merge', { branch: app.world, dry_run: true });
      setPlan(p);
      setPicks({});
    } catch (e) {
      toast((e as Error).message);
    }
    setBusy(false);
  };
  const merge = async () => {
    const unsettled = plan ? plan.rows.filter((r) => r.outcome === 'conflict' && !picks[r.key]).length : 0;
    if (unsettled) return toast(`Choose a side for ${plural(unsettled, 'conflict')} first`);
    const live = parent === 'main' ? ' (the live data)' : '';
    const ok = await dlg.confirm(
      `Merge into ${parent}?`,
      `${counts ? plural(counts.total, 'change') : 'The changes'} from ${app.world} go into ${parent}${live}, and ${app.world} closes. You can undo the merge afterwards.`,
      `Merge into ${parent}`,
    );
    if (!ok) return;
    setBusy(true);
    try {
      const r = await api<{ merged: number }>('merge', { branch: app.world, confirm: true, picks });
      await app.drawBack(app.world);
      const rows = counts?.total ?? r.merged; // what the user reviewed (the merge also counts index entries)
      app.setLastMerge({ world: app.world, into: parent, rows });
      toast(`Merged ${plural(rows, 'change')} into ${parent}`);
      await app.reloadWorlds();
      await app.setWorld(parent);
      app.show('changes');
    } catch (err) {
      const e = err as ApiError;
      setBusy(false);
      if (e.status === 409 && e.detail?.conflicts) {
        toast(`${plural(e.detail.conflicts_total ?? e.detail.conflicts.length, 'row')} changed on both sides: choose a side for each`);
        preview();
      } else toast(e.message);
    }
  };
  const tables = Object.entries(counts?.tables || {});
  const conflicts = plan?.rows.filter((r) => r.outcome === 'conflict') || [];
  const pickAll = (s: Pick) => setPicks(Object.fromEntries(conflicts.map((c) => [c.key, s])));
  return (
    <div className="pane">
      <div className="scroll">
        <div className="pad stack">
          {undo}
          <div className="head-row">
            <h2 className="ident">{app.world}</h2>
            <span className="muted">
              since its fork from <code>{parent}</code>
            </span>
          </div>
          {error ? <ErrorBox error={error} /> : null}
          <div className="summary">
            {!counts ? (
              <span className="note">Counting…</span>
            ) : !counts.total ? (
              <span className="muted">No changes yet. Edit a row or run SQL in this world.</span>
            ) : (
              <>
                <span className="muted">{plural(counts.total, 'change')}:</span>
                {tables.map(([t, k]) => (
                  <span className="t" key={t}>
                    {t} <b>{n(k)}</b>
                  </span>
                ))}
              </>
            )}
          </div>
          {counts && counts.total > 0 && <Readers readers={counts.readers || []} coverage={counts.coverage || []} />}
          {plan && (
            <div className="plan">
              <div className="head-row" style={{ margin: 0 }}>
                <h3 className="label">Merge preview</h3>
                <div className="outcomes">
                  {Object.entries(plan.outcomes).map(([k, c]) => (
                    <span key={k} className={k === 'conflict' ? 'bad' : ''}>
                      {k} <b>{n(c)}</b>
                    </span>
                  ))}
                </div>
                <span className="spacer" />
                <button className="btn ghost small" type="button" onClick={() => setPlan(null)}>
                  Close preview
                </button>
              </div>
              {plan.blocked && <div className="err">This merge can’t go through: {plan.blocked}</div>}
              {!plan.conflicts && !plan.blocked && <p className="note">No conflicts: every change applies cleanly.</p>}
              {plan.conflicts > 0 && (
                <>
                  <div className="head-row" style={{ margin: 0 }}>
                    <span className="note">
                      {plural(plan.conflicts, 'row')} changed in both {app.world} and {parent} since the fork. Keep one side for each.
                    </span>
                    <span className="spacer" />
                    <button className="btn small" type="button" onClick={() => pickAll('theirs')}>
                      Keep all theirs
                    </button>
                    <button className="btn small" type="button" onClick={() => pickAll('ours')}>
                      Keep all ours
                    </button>
                  </div>
                  <div className="changes">
                    {conflicts.map((c) => (
                      <div className="change" key={c.key}>
                        <div className="key">
                          <span className="id">{c.key}</span>
                          <span className="kind del">conflict</span>
                          <div className="choice" role="group" aria-label={`Keep which side of ${c.key}`}>
                            <button
                              type="button"
                              aria-pressed={picks[c.key] === 'theirs'}
                              title={`Keep ${parent}'s row`}
                              onClick={() => setPicks({ ...picks, [c.key]: 'theirs' })}
                            >
                              Keep theirs
                            </button>
                            <button
                              type="button"
                              aria-pressed={picks[c.key] === 'ours'}
                              title={`Keep ${app.world}'s row`}
                              onClick={() => setPicks({ ...picks, [c.key]: 'ours' })}
                            >
                              Keep ours
                            </button>
                          </div>
                        </div>
                        <Cols before={c.theirs} after={c.ours} labels={[parent, app.world]} />
                        {(c.detail || c.explain) && <div className="explain">{c.explain || c.detail}</div>}
                      </div>
                    ))}
                  </div>
                  {plan.rows_total > plan.rows.length && <p className="note">Showing the first {plan.rows.length} rows of the plan, conflicts first.</p>}
                </>
              )}
            </div>
          )}
          {changes.length > 0 && (
            <div className="changes">
              {changes.map((c) => (
                <ChangeRow key={c.key} c={c} />
              ))}
            </div>
          )}
          {loading ? (
            <p className="note">Loading…</p>
          ) : (
            next && (
              <div>
                <button className="btn" type="button" onClick={() => page(next)}>
                  Load {PAGE} more
                </button>
              </div>
            )
          )}
        </div>
      </div>
      <div className="footbar">
        <button className="btn primary" type="button" disabled={busy || !counts?.total || !!plan?.blocked} onClick={merge}>
          Merge into {parent}
        </button>
        <button className="btn" type="button" disabled={busy || !counts?.total} onClick={preview}>
          Preview merge
        </button>
        <span className="spacer" />
        <button className="btn ghost danger" type="button" disabled={busy} onClick={() => discard(app, dlg, app.world)}>
          Discard world
        </button>
      </div>
    </div>
  );
}

/** What reads the columns this world changed, by table, critical readers first, and what that can't see. */
function Readers({ readers, coverage }: { readers: Reader[]; coverage: string[] }) {
  const tables = [...new Set(readers.map((r) => r.table))];
  return (
    <div className="readers">
      <h3 className="label">What reads these changes</h3>
      {!readers.length && <p className="note">Nothing inside the database reads the changed columns.</p>}
      {tables.map((t) => (
        <div key={t}>
          <div className="ident">{t}</div>
          <ul>
            {readers
              .filter((r) => r.table === t)
              .map((r) => (
                <li key={`${r.kind}/${r.reader}`}>
                  {r.critical && <span className="kind bad">critical</span>} <span className="kind">{r.kind}</span> <code>{r.reader}</code>
                  <span className="muted"> reads {r.columns.join(', ')}</span>
                  {r.detail && <div className="note">{r.detail}</div>}
                </li>
              ))}
          </ul>
        </div>
      ))}
      <ul className="note">
        {coverage.map((c) => (
          <li key={c}>{c}</li>
        ))}
      </ul>
    </div>
  );
}

/** Only what changed: each changed column, before → after. */
function Cols({ before, after, changed, labels }: { before: Row | null; after: Row | null; changed?: string[] | null; labels?: [string, string] }) {
  const keys = changed?.length
    ? changed
    : [...new Set([...Object.keys(before || {}), ...Object.keys(after || {})])].filter((k) => JSON.stringify(before?.[k]) !== JSON.stringify(after?.[k]));
  return (
    <div className="cols">
      {labels && (
        <>
          <span />
          <span className="note">
            {labels[0]} → {labels[1]}
          </span>
        </>
      )}
      {keys.map((k) => (
        <Pair key={k} k={k} before={before?.[k]} after={after?.[k]} />
      ))}
    </div>
  );
}
const Pair = ({ k, before, after }: { k: string; before: unknown; after: unknown }) => (
  <>
    <span className="c">{k}</span>
    <span className="v">
      <span className="old">{text(before, 400)}</span>
      <span className="arrow">→</span>
      <span className="new">{text(after, 400)}</span>
    </span>
  </>
);

function ChangeRow({ c }: { c: Change }) {
  const [cls, label] = !c.before ? ['add', 'added'] : !c.after ? ['del', 'removed'] : ['chg', 'changed'];
  const row = (c.after || c.before) as Row;
  const [open, setOpen] = useState(false);
  const fields = Object.entries(row).filter(([, v]) => v !== null && v !== '');
  return (
    <div className="change">
      <div className="key">
        <span className="id" title={c.key}>
          {c.key}
        </span>
        <span className={'kind ' + cls}>{label}</span>
      </div>
      {cls === 'chg' ? (
        <Cols before={c.before} after={c.after} changed={c.columns} />
      ) : open ? (
        <div className="cols">
          {Object.entries(row).map(([k, v]) => (
            <span key={k} style={{ display: 'contents' }}>
              <span className="c">{k}</span>
              <span className={'v ' + (cls === 'add' ? 'new add' : 'old')}>{text(v, 400)}</span>
            </span>
          ))}
          <span />
          <button className="btn small ghost more" type="button" onClick={() => setOpen(false)}>
            Show less
          </button>
        </div>
      ) : (
        // a whole row came or went: a line of its first fields, the rest one click away
        <div className={'row-sum ' + cls}>
          {fields.slice(0, 4).map(([k, v]) => (
            <span className="f" key={k}>
              <span className="c">{k}</span> {text(v, 60)}
            </span>
          ))}
          {fields.length > 4 && (
            <button className="btn small ghost more" type="button" onClick={() => setOpen(true)}>
              +{fields.length - 4} fields
            </button>
          )}
        </div>
      )}
    </div>
  );
}

async function undoMerge(app: App, dlg: Dialogs) {
  const m = app.lastMerge!;
  if (
    !(await dlg.confirm(
      'Undo the merge?',
      `Every row the merge of ${m.world} changed in ${m.into} goes back. Refused if any of them changed again since.`,
      'Undo merge',
    ))
  )
    return;
  try {
    const r = await api<{ undone: number }>('undo_merge', { branch: m.world });
    app.setLastMerge(null);
    toast(`Undid the merge: ${plural(r.undone, 'row')} put back in ${m.into}`);
    await app.reloadWorlds();
    await app.reloadTables();
    app.refresh();
  } catch (e) {
    toast((e as Error).message);
  }
}

export async function discard(app: App, dlg: Dialogs, world: string) {
  const w = worldOf(app, world);
  const kids = app.worlds.filter((x) => x.parent === world || x.parent === w?.id);
  const also = kids.length ? ` Its ${plural(kids.length, 'fork')} (${kids.map((k) => k.name).join(', ')}) go too.` : '';
  if (!(await dlg.confirm(`Discard ${world}?`, `Its changes are thrown away for good.${also}`, 'Discard world', { danger: true, typed: world }))) return;
  try {
    await api('discard', { branch: world, cascade: kids.length > 0 });
    toast(`Discarded ${world}`);
    const parent = w?.parent && worldOf(app, w.parent) ? w.parent : 'main';
    await app.reloadWorlds();
    await app.setWorld(parent);
  } catch (e) {
    toast((e as Error).message);
  }
}
