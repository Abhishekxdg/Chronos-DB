// Agents (capabilities, quotas; create, alter, disable, drop) and the audit log. A new agent's
// token is shown once.
import { useEffect, useState } from 'react';
import { api, type Agent } from '../api';
import { useApp } from '../context';
import { copyText, Empty, ErrorBox, toast, type Dialogs, useDialogs } from '../ui';
import { fmtTime, n } from '../util';

const CAPS = ['read', 'fork', 'write_own', 'write', 'write_main', 'merge_own', 'merge', 'restore', 'admin'];
const QUOTAS: [string, string][] = [
  ['max_worlds', 'Max worlds'],
  ['writes_per_minute', 'Writes per minute'],
  ['max_changes', 'Max changes per world'],
  ['world_ttl', 'World lifetime (ms)'],
  ['max_query_ms', 'Max query time (ms)'],
  ['max_concurrent', 'Max statements at once'],
  ['max_memory_mb', 'Max query memory (MB)'],
];

export function AgentsView() {
  const app = useApp();
  const dlg = useDialogs();
  const [agents, setAgents] = useState<Agent[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  useEffect(() => {
    api<{ agents: Agent[] }>('agents').then((r) => setAgents(r.agents), setError);
  }, []);
  const alter = async (name: string, spec: object, done: string) => {
    try {
      await api('alter_agent', { ...spec, name });
      toast(done);
      app.refresh();
    } catch (e) {
      toast((e as Error).message);
    }
  };
  const drop = async (name: string) => {
    if (!(await dlg.confirm(`Drop agent ${name}?`, 'Its token stops working at once. The worlds it made stay.', 'Drop agent', { danger: true, typed: name }))) return;
    try {
      await api('drop_agent', { name });
      toast(`Dropped ${name}`);
      app.refresh();
    } catch (e) {
      toast((e as Error).message);
    }
  };
  const form = async (a: Agent | null) => {
    const spec = await agentForm(dlg, a);
    if (!spec) return;
    if (a) return alter(a.name, spec, `Saved ${a.name}`);
    try {
      const r = await api<{ agent: Agent; token: string }>('create_agent', spec);
      await dlg.modal(
        `Agent ${r.agent.name} created`,
        <>
          <p>Its token, shown only this once. Give it to the agent as Authorization: Bearer &lt;token&gt;.</p>
          <div className="secret">{r.token}</div>
        </>,
        [
          { label: 'Copy token', value: () => (copyText(r.token), undefined) },
          { label: 'Done', cls: 'primary', value: () => true },
        ],
      );
      app.refresh();
    } catch (e) {
      toast((e as Error).message);
    }
  };
  return (
    <div className="scroll">
      <div className="pad stack">
        <div className="head-row">
          <h2>Agents</h2>
          <span className="muted">each has a token and only the rights you give it</span>
          <span className="spacer" />
          <button className="btn primary" type="button" onClick={() => form(null)}>
            New agent
          </button>
        </div>
        {error ? (
          <ErrorBox error={error} />
        ) : !agents ? (
          <p className="note">Loading…</p>
        ) : !agents.length ? (
          <Empty>No agents yet. Create one to give an AI agent its own token, rights and quotas.</Empty>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Can</th>
                  <th>Quotas</th>
                  <th>Created</th>
                  <th>Status</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {agents.map((a) => (
                  <tr key={a.name}>
                    <td className="mono">{a.name}</td>
                    <td className="mono">{a.can.join(', ')}</td>
                    <td className="muted">
                      {QUOTAS.filter(([k]) => a[k] != null && a[k] !== 0)
                        .map(([k, l]) => `${l.toLowerCase()} ${n(a[k])}`)
                        .join(' · ') || 'none'}
                    </td>
                    <td className="mono nowrap">{fmtTime(a.created)}</td>
                    <td>
                      <span className={'tag ' + (a.disabled ? 'bad' : 'ok')}>{a.disabled ? 'disabled' : 'active'}</span>
                    </td>
                    <td className="nowrap">
                      <button className="btn small ghost" type="button" onClick={() => form(a)}>
                        Edit
                      </button>
                      <button
                        className="btn small ghost"
                        type="button"
                        onClick={() => alter(a.name, { disabled: !a.disabled }, `${a.disabled ? 'Enabled' : 'Disabled'} ${a.name}`)}
                      >
                        {a.disabled ? 'Enable' : 'Disable'}
                      </button>
                      <button className="btn small ghost danger" type="button" onClick={() => drop(a.name)}>
                        Drop
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

interface AuditEvent {
  at: number;
  agent: string;
  world: string;
  action: string;
  rows: number;
}

export function AuditView() {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [who, setWho] = useState('');
  const [audit, setAudit] = useState<AuditEvent[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  useEffect(() => {
    api<{ agents: Agent[] }>('agents').then((r) => setAgents(r.agents), () => {});
  }, []);
  useEffect(() => {
    setAudit(null);
    api<{ events: AuditEvent[] }>('audit', { agent: who || null, limit: 200 }).then((r) => setAudit(r.events), setError);
  }, [who]);
  return (
    <div className="scroll">
      <div className="pad stack">
        <div className="head-row">
          <h2>Audit log</h2>
          <span className="muted">what agents did, newest first</span>
          <span className="spacer" />
          <select aria-label="Agent" value={who} onChange={(e) => setWho(e.target.value)}>
            <option value="">All agents</option>
            {agents.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
          </select>
        </div>
        {error ? (
          <ErrorBox error={error} />
        ) : !audit ? (
          <p className="note">Loading…</p>
        ) : !audit.length ? (
          <Empty>Nothing yet: every write, fork and merge an agent makes shows here.</Empty>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Agent</th>
                  <th>World</th>
                  <th>Action</th>
                  <th className="num">Rows</th>
                </tr>
              </thead>
              <tbody>
                {audit.map((e, i) => (
                  <tr key={i}>
                    <td className="mono nowrap">{fmtTime(e.at)}</td>
                    <td className="mono">{e.agent}</td>
                    <td className="mono">{e.world}</td>
                    <td>{e.action}</td>
                    <td className="num">{n(e.rows)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

/** The create/edit form: a name, capabilities and quotas. Resolves to the request, or null. */
function agentForm(dlg: Dialogs, a: Agent | null) {
  const f = {
    name: a?.name ?? '',
    can: new Set(a ? a.can : ['read', 'fork', 'write_own']),
    nums: Object.fromEntries(QUOTAS.map(([k]) => [k, a?.[k] ? String(a[k]) : ''])), // 0: no limit
  };
  return dlg.modal<Record<string, unknown>>(
    a ? `Edit agent ${a.name}` : 'New agent',
    <>
      <label className="field">
        <span>Name</span>
        <input type="text" defaultValue={f.name} disabled={!!a} autoComplete="off" spellCheck={false} onChange={(e) => (f.name = e.target.value)} />
      </label>
      <fieldset className="field" style={{ border: 0, padding: 0, margin: 0 }}>
        <legend className="note">What it may do</legend>
        <div className="row2" style={{ display: 'flex', flexWrap: 'wrap', gap: '6px 14px', marginTop: 4 }}>
          {CAPS.map((c) => (
            <label key={c} className="check mono">
              <input type="checkbox" defaultChecked={f.can.has(c)} onChange={(e) => (e.target.checked ? f.can.add(c) : f.can.delete(c))} /> {c}
            </label>
          ))}
        </div>
      </fieldset>
      <div className="cards">
        {QUOTAS.map(([k, l]) => (
          <label className="field" key={k}>
            <span>{l}</span>
            <input type="text" inputMode="numeric" defaultValue={f.nums[k]} placeholder="no limit" onChange={(e) => (f.nums[k] = e.target.value)} />
          </label>
        ))}
      </div>
    </>,
    [
      { label: 'Cancel', value: () => null },
      {
        label: a ? 'Save agent' : 'Create agent',
        cls: 'primary',
        value: () => {
          if (!f.name.trim()) return (toast('Give the agent a name'), undefined);
          const s: Record<string, unknown> = { name: f.name.trim(), can: CAPS.filter((c) => f.can.has(c)) };
          for (const [k, l] of QUOTAS) {
            const t = f.nums[k].trim();
            if (t === '') continue;
            if (!/^\d+$/.test(t)) return (toast(`${l} must be a whole number`), undefined);
            s[k] = Number(t);
          }
          return s;
        },
      },
    ],
    true,
  );
}
