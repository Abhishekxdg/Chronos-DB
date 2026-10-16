// A world's details and metadata, in a dialog from the breadcrumb's world menu.
import { api, type World } from '../api';
import type { App } from '../context';
import { toast, type Dialogs } from '../ui';
import { ago, fmtTime, n } from '../util';

export async function worldDetails(app: App, dlg: Dialogs, w: World) {
  const meta = { current: JSON.stringify(w.meta ?? {}, null, 2) };
  const pairs: [string, unknown][] = [
    ['id', w.id],
    ['forked from', w.parent ?? 'none: main is the root'],
    ['created', fmtTime(w.created)],
    ['changes', w.changes != null ? n(w.changes) : ''],
    ['writes since fork', n(w.version)],
    ['owner', w.owner],
    ['expires', w.expires ? fmtTime(w.expires) : 'never'],
    ['last active', w.active ? `${fmtTime(w.active)} (${ago(w.active)})` : ''],
    ['pinned', w.pinned ? 'yes' : 'no'],
    ['flagged', w.flagged ? 'yes: open during a crash, so its merge must be confirmed' : 'no'],
    ['checkpoints', w.checkpoints.map((c) => `${c.name} (${fmtTime(c.at)})`).join(', ')],
  ];
  const save = await dlg.modal<unknown>(
    w.name,
    <>
      <table className="tbl">
        <tbody>
          {pairs.map(([k, v]) => (
            <tr key={k}>
              <td className="muted nowrap">{k}</td>
              <td className="mono">{v == null || v === '' ? '—' : String(v)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <label className="field">
        <span>Metadata (a JSON object)</span>
        <textarea rows={6} defaultValue={meta.current} spellCheck={false} readOnly={!app.admin} onChange={(e) => (meta.current = e.target.value)} />
      </label>
    </>,
    // (a role that isn't a superuser reads it: set_meta needs one)
    !app.admin ? [{ label: 'Close', value: () => null }] : [
      { label: 'Close', value: () => null },
      {
        label: 'Save metadata',
        cls: 'primary',
        value: () => {
          try {
            return JSON.parse(meta.current);
          } catch (e) {
            toast('The metadata isn’t valid JSON: ' + (e as Error).message);
            return undefined;
          }
        },
      },
    ],
  );
  if (save == null) return;
  try {
    await api('set_meta', { branch: w.name, meta: save });
    toast(`Saved the metadata of ${w.name}`);
    await app.reloadWorlds();
  } catch (e) {
    toast((e as Error).message);
  }
}
