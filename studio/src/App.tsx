// The Studio's window: the title bar (menus, the search that opens the command palette, the
// active world and its one primary action), the icon rail of views, the explorer (the folder,
// worlds as lineage, tables opening to their columns, the timeline and the outline), the pill
// tabs over the canvas, the status bar, and the state they share. A world's hue marks it
// everywhere it appears; main's is amber, the live data.
import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from 'react';
import { api, errText, onConnection, onGate, onSignedOut, signIn, signOut, token, whoami, type ApiError, type HistoryWindow, type Table, type User, type Who, type World } from './api';
import { ADMIN_ONLY, AppCtx, lineage, TABS, worldOf, type App as AppState, type Merged, type Prefs, type Tab } from './context';
import { Icon, typeMark, type IconName } from './icons';
import { Palette, type Command } from './Palette';
import { Worldline } from './Worldline';
import { DialogHost, Popover, Toast, toast, useDialogs } from './ui';
import { ago, fmtTime, hueOf, mod, momentOf, n, outsideHistory, plural, reducedMotion, setTimeZone, store, suggestName } from './util';
import { AgentsView, AuditView } from './views/Agents';
import { ChangesView, discard } from './views/Changes';
import { castType, DataView, tableInfo, type TableInfo } from './views/Data';
import { DiagramView } from './views/Diagram';
import { HistoryView, saveCheckpoint } from './views/History';
import { PgImportView } from './views/PgImport';
import { SearchView } from './views/Search';
import { SessionsView } from './views/Sessions';
import { SettingsView, SHORTCUTS } from './views/Settings';
import { SimulateView } from './views/Simulate';
import { SqlView } from './views/Sql';
import { StatusView } from './views/Status';
import { worldDetails } from './views/World';

const PAGES: Partial<Record<Tab, string>> = {
  simulate: 'Simulations',
  agents: 'Agents',
  audit: 'Audit log',
  status: 'Status',
  settings: 'Settings',
  sessions: 'Sessions',
  pgimport: 'Import from Postgres',
};
const VIEWS = {
  data: DataView,
  diagram: DiagramView,
  sql: SqlView,
  search: SearchView,
  changes: ChangesView,
  history: HistoryView,
  agents: AgentsView,
  audit: AuditView,
  status: StatusView,
  settings: SettingsView,
  simulate: SimulateView,
  sessions: SessionsView,
  pgimport: PgImportView,
};
const ICONS: Record<Tab, IconName> = {
  data: 'table',
  diagram: 'diagram',
  sql: 'code',
  search: 'search',
  changes: 'diff',
  history: 'clock',
  agents: 'bot',
  audit: 'flag',
  status: 'pulse',
  settings: 'sliders',
  simulate: 'play',
  sessions: 'eye',
  pgimport: 'download',
};
const typing = (e: KeyboardEvent) => {
  const t = e.target as HTMLElement;
  return t.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(t.tagName);
};

export function App() {
  return (
    <DialogHost>
      <Studio />
      <Toast />
    </DialogHost>
  );
}

type Theme = Prefs['theme'];
type Menu = 'file' | 'view' | 'world' | 'help' | 'worldpill' | 'settings' | 'more' | 'user' | null;

function Studio() {
  const dlg = useDialogs();
  // preferences (Settings), in this browser; the older one-key settings are read once as defaults
  const [prefs, setPrefs] = useState<Prefs>(() => {
    let saved: Partial<Prefs> = {};
    try {
      saved = JSON.parse(store.get('chronos-studio-prefs') || '{}');
    } catch {
      /* a damaged value: defaults */
    }
    return {
      theme: (store.get('chronos-studio-theme') as Theme) || 'system',
      explorer: store.get('chronos-studio-explorer') !== 'hidden',
      worldline: store.get('chronos-studio-worldline') !== 'hidden',
      density: 'comfortable',
      time: 'local',
      ...saved,
    };
  });
  const setPref = <K extends keyof Prefs>(k: K, v: Prefs[K]) =>
    setPrefs((p) => {
      const next = { ...p, [k]: v };
      store.set('chronos-studio-prefs', JSON.stringify(next));
      return next;
    });
  const theme = prefs.theme;
  const setTheme = (t: Theme) => setPref('theme', t);
  useEffect(() => {
    if (theme === 'system') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = theme;
  }, [theme]);
  const dark = theme === 'dark' || (theme === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
  const [gate, setGate] = useState(!token);
  const [conn, setConn] = useState(true);
  useEffect(() => onGate(() => setGate(true)), []);
  useEffect(() => onConnection(setConn), []);
  // the key in the link, or (chronos studio --login) a role signed in; asked once, before anything else
  // without a key (with one the page starts at once, as it always has)
  const [who, setWho] = useState<Who | null>(null);
  useEffect(() => {
    whoami().then(setWho, () => setWho({ mode: 'key', user: null }));
    return onSignedOut(() => setWho((w) => w && { ...w, user: null }));
  }, []);
  const login = who?.mode === 'login';
  const user = (login && who?.user) || null;
  const admin = !login || !!user?.superuser;
  // nothing to show yet: the key's gate (shown once the mode is known), or the sign-in
  const locked = login ? !user : gate;
  const tabs = admin ? TABS : TABS.filter(([t]) => !ADMIN_ONLY.has(t));

  const [world, setWorldName] = useState('main');
  const [worlds, setWorlds] = useState<World[]>([]);
  const [tables, setTables] = useState<Table[] | null>(null);
  const [tablesError, setTablesError] = useState('');
  const [table, setTable] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>('data');
  const [asOf, setAsOf] = useState('');
  const [search, setSearch] = useState<AppState['search']>(null);
  const [lastMerge, setLastMerge] = useState<Merged | null>(null);
  const [mainOk, setMainOk] = useState(false);
  const [side, setSide] = useState(false); // the explorer, as a drawer on narrow screens
  const explorer = prefs.explorer;
  const setExplorer = (on: boolean) => setPref('explorer', on);
  const line = prefs.worldline;
  const setLine = (on: boolean) => setPref('worldline', on);
  const [bump, setBump] = useState(0); // remounts the view when data may have changed
  // density and time zone change how every view draws: set them, then redraw the view
  useEffect(() => {
    document.documentElement.dataset.density = prefs.density;
    setTimeZone(prefs.time === 'utc');
    setBump((b) => b + 1);
  }, [prefs.density, prefs.time]);
  const [palette, setPalette] = useState(false);
  const [merging, setMerging] = useState<string | null>(null);
  const [menu, setMenu] = useState<Menu>(null);
  const [sideW, setSideW] = useState(() => Number(store.get('chronos-studio-side')) || 264);
  const [statusSlot, setStatusSlot] = useState<HTMLElement | null>(null);
  const [about, setAbout] = useState<{ version: string; folder: string } | null>(null);
  const [history, setHistory] = useState<HistoryWindow | null>(null);
  const worldRef = useRef(world);

  const reloadWorlds = useCallback(async () => {
    // the history window moves on with checkpoints: read it again with the worlds
    api<{ history?: HistoryWindow }>('status').then((s) => setHistory(s.history ?? null), () => {});
    const r = await api<{ branches: World[] }>('worlds');
    setWorlds(r.branches);
    if (!r.branches.some((w) => w.name === worldRef.current)) {
      worldRef.current = 'main';
      setWorldName('main');
    }
  }, []);
  const reloadTables = useCallback(async () => {
    const w = worldRef.current;
    try {
      const r = await api<{ tables: Table[] }>('tables', { branch: w });
      if (w !== worldRef.current) return;
      setTables(r.tables);
      setTablesError('');
      setTable((t) => (t && r.tables.some((x) => x.name === t) ? t : (r.tables[0]?.name ?? null)));
    } catch (e) {
      setTables([]);
      setTablesError((e as Error).message);
    }
  }, []);
  const setWorld = useCallback(
    async (name: string) => {
      worldRef.current = name;
      setWorldName(name);
      setAsOf('');
      setSide(false);
      setTables(null);
      await reloadTables();
      setBump((b) => b + 1);
    },
    [reloadTables],
  );
  const forkFrom = useCallback(
    async (from: string, suggested?: string) => {
      const name = await dlg.prompt(
        `Fork a world from ${from}`,
        'Name of the new world',
        suggested || '',
        'Fork world',
        'A fork is instant: an isolated copy to change, review, then merge or discard.',
      );
      if (!name) return null;
      try {
        await api('fork', { from, name });
      } catch (e) {
        toast((e as Error).message);
        return null;
      }
      await reloadWorlds();
      await setWorld(name);
      toast(`Forked ${name} from ${from}`);
      return name;
    },
    [dlg, reloadWorlds, setWorld],
  );

  useEffect(() => {
    if (locked) return;
    reloadWorlds().then(reloadTables, () => {});
    api<{ version: string; folder?: string }>('status').then(
      (s) => {
        setAbout({ version: s.version, folder: s.folder || 'database' });
        document.title = `${s.folder || 'Chronos'} · Chronos Studio`;
      },
      () => {},
    );
  }, [locked, reloadWorlds, reloadTables]);

  const app: AppState = {
    world,
    worlds,
    tables: tables || [],
    tablesReady: tables !== null,
    table,
    tab,
    setWorld,
    pickTable: (name) => {
      setTable(name);
      setSide(false);
      if (tab !== 'data') setTab('data');
      setBump((b) => b + 1);
    },
    reloadWorlds,
    reloadTables,
    forkFrom,
    show: (t) => {
      if (!admin && ADMIN_ONLY.has(t)) return toast(`${PAGES[t] || TABS.find(([x]) => x === t)?.[1] || t} needs a superuser role`);
      setTab(t);
      setSide(false);
      setBump((b) => b + 1);
    },
    refresh: () => setBump((b) => b + 1),
    asOf,
    setAsOf,
    search,
    setSearch,
    timeTravel: (at) => {
      if (!table) return toast('Pick a table first: the moment applies to it');
      const why = outsideHistory(history, momentOf(at));
      if (why) return toast(why);
      setAsOf(at);
      setTab('data');
      setSide(false);
      setBump((b) => b + 1);
    },
    lastMerge,
    setLastMerge,
    drawBack: async (w) => {
      setMerging(w);
      await new Promise((r) => setTimeout(r, reducedMotion() ? 0 : 400));
      setMerging(null);
    },
    mainOk,
    setMainOk,
    prefs,
    setPref,
    statusSlot,
    folder: about?.folder || '',
    history,
    user,
    admin,
  };

  const w = worldOf(app, world);
  const hue = hueOf(w);
  const onMain = world === 'main';
  const changes = w?.changes ?? 0;
  const reloadAll = () => (reloadWorlds(), reloadTables(), app.refresh());
  // signing out ends the session on the server; the sign-in comes back
  const out = async () => {
    await signOut();
    setWho((x) => x && { ...x, user: null });
  };

  // ---------- commands and keys ----------
  const commands = useMemo<Command[]>(() => {
    const owner: Command[] = [
      { group: 'Go to', label: 'Simulations', run: () => app.show('simulate') },
      { group: 'Go to', label: 'Agents', run: () => app.show('agents') },
      { group: 'Go to', label: 'Audit log', run: () => app.show('audit') },
    ];
    const c: Command[] = [
      ...tabs.map(([t, label], i) => ({ group: 'Go to', label, hint: `${mod}${i + 1}`, run: () => app.show(t) })),
      ...(admin ? owner : []),
      { group: 'Go to', label: 'Status', run: () => app.show('status') },
      ...(admin
        ? [
            { group: 'Go to', label: 'Sessions and locks', run: () => app.show('sessions') },
            { group: 'Go to', label: 'Import from Postgres…', run: () => app.show('pgimport') },
            { group: 'World', label: `Fork a world from ${world}`, run: () => forkFrom(world, suggestName()) },
          ]
        : []),
      { group: 'World', label: `Details of ${world}`, run: () => w && worldDetails(app, dlg, w) },
    ];
    if (user) c.push({ group: 'Session', label: `Sign out ${user.name}`, run: () => out() });
    if (admin && !onMain && w?.parent) {
      c.push({ group: 'World', label: `Review changes in ${world}`, run: () => app.show('changes') });
      c.push({ group: 'World', label: `Merge ${world} into ${w.parent}`, run: () => app.show('changes') });
      c.push({ group: 'World', label: `Discard ${world}`, run: () => discard(app, dlg, world) });
    }
    c.push({ group: 'Settings', label: dark ? 'Switch to the light theme' : 'Switch to the dark theme', run: () => setTheme(dark ? 'light' : 'dark') });
    c.push({ group: 'Settings', label: explorer ? 'Hide the explorer' : 'Show the explorer', hint: `${mod}B`, run: () => setExplorer(!explorer) });
    c.push({ group: 'Settings', label: line ? 'Hide the worldline' : 'Show the worldline', run: () => setLine(!line) });
    c.push({ group: 'Settings', label: 'Open settings', hint: `${mod},`, run: () => app.show('settings') });
    c.push({ group: 'Settings', label: 'Keyboard shortcuts', hint: '?', run: () => shortcuts(dlg) });
    for (const x of worlds) c.push({ group: 'Switch world', label: x.name, run: () => setWorld(x.name) });
    for (const t of tables || []) c.push({ group: 'Open table', label: t.name, run: () => app.pickTable(t.name) });
    return c;
  }, [world, worlds, tables, dark, tab, explorer, line, user, admin]); // eslint-disable-line

  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      const m = e.metaKey || e.ctrlKey;
      if (m && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPalette((p) => !p);
      } else if (m && e.key === ',') {
        e.preventDefault();
        app.show('settings');
      } else if (m && e.key.toLowerCase() === 'b') {
        e.preventDefault();
        setExplorer(!explorer);
      } else if (m && /^[1-6]$/.test(e.key)) {
        e.preventDefault();
        if (tabs[Number(e.key) - 1]) app.show(tabs[Number(e.key) - 1][0]);
      } else if (!m && !typing(e) && !document.querySelector('.overlay')) {
        if (e.key === '/') {
          e.preventDefault();
          if (tab !== 'data') app.show('data');
          setTimeout(() => dispatchEvent(new Event('studio:filter')), 0);
        } else if (e.key === '?') shortcuts(dlg);
      }
    };
    addEventListener('keydown', key);
    return () => removeEventListener('keydown', key);
  });

  const startResize = (e: React.PointerEvent) => {
    const start = e.clientX;
    const w0 = sideW;
    const el = e.currentTarget as HTMLElement;
    el.classList.add('drag');
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => setSideW(Math.min(440, Math.max(200, w0 + ev.clientX - start)));
    const up = () => {
      el.classList.remove('drag');
      el.removeEventListener('pointermove', move);
      el.removeEventListener('pointerup', up);
      setSideW((x) => (store.set('chronos-studio-side', String(x)), x));
    };
    el.addEventListener('pointermove', move);
    el.addEventListener('pointerup', up);
  };

  // a desktop menu bar: once one menu is open, hovering another opens it instead
  const menuProps = (m: Exclude<Menu, null>) => ({
    open: menu === m,
    setOpen: (o: boolean) => setMenu(o ? m : null),
    menu: true,
  });
  const hoverMenu = (m: Menu) => () => menu && ['file', 'view', 'world', 'help'].includes(menu) && setMenu(m);
  const item = (label: ReactNode, run: () => void, hint?: string, cls = '') => (
    <button type="button" className={'menu-item ' + cls} onClick={() => (setMenu(null), run())}>
      {label}
      {hint && <span className="kbd">{hint}</span>}
    </button>
  );
  // (a role that isn't a superuser may look at a world, not fork, checkpoint, review or discard it)
  const worldMenu = admin ? (
    <>
      {item(`Fork from ${world}…`, () => forkFrom(world, suggestName()))}
      {item('Details and metadata', () => w && worldDetails(app, dlg, w))}
      {item('Save a checkpoint…', () => saveCheckpoint(app, dlg))}
      {!onMain && item(`Review changes${changes ? ` (${n(changes)})` : ''}`, () => app.show('changes'))}
      {!onMain && (
        <>
          <div className="menu-sep" />
          {item(`Discard ${world}`, () => discard(app, dlg, world), undefined, 'danger')}
        </>
      )}
    </>
  ) : (
    item('Details', () => w && worldDetails(app, dlg, w))
  );

  // without a key, the mode not known yet: nothing for the moment whoami takes (not the key's gate, nor the window)
  if (!who && !token) return <div className="signin" />;
  // chronos studio --login without a session: the sign-in, in place of the window
  if (login && !user) return <SignIn onIn={(u) => (setTab('data'), setWho({ mode: 'login', user: u }))} />;

  const View = VIEWS[tab];
  const chain = lineage(worlds, world);
  return (
    <AppCtx.Provider value={app}>
      <div
        className={'app' + (side ? ' side-open' : '') + (explorer ? '' : ' no-explorer')}
        style={{ '--side-w': sideW + 'px', '--hue': hue } as CSSProperties}
      >
        <header className="top">
          <button className="btn ghost icon menu-btn" type="button" aria-label="Show the explorer" onClick={() => setSide(!side)}>
            <Icon name="menu" />
          </button>
          <span className="logo" aria-label="Chronos Studio" title="Chronos Studio">
            <svg width="22" height="22" viewBox="0 0 24 24" aria-hidden="true">
              <rect x="3" y="3" width="15" height="4.5" rx="2.25" />
              <rect x="7" y="9.75" width="14" height="4.5" rx="2.25" />
              <rect x="3" y="16.5" width="10" height="4.5" rx="2.25" />
            </svg>
          </span>
          <nav className="menus" aria-label="Menus">
            <span onMouseEnter={hoverMenu('file')}>
              <Popover label="File" cls="menu-top" {...menuProps('file')}>
                {admin && item(`Fork a world from ${world}…`, () => forkFrom(world, suggestName()))}
                {item('New SQL query', () => app.show('sql'), `${mod}3`)}
                {admin && item('Import from Postgres…', () => app.show('pgimport'))}
                {item('Reload', reloadAll)}
                {item('Settings', () => app.show('settings'), `${mod},`)}
                <div className="menu-sep" />
                {admin && item('Simulations', () => app.show('simulate'))}
                {admin && item('Simulations', () => app.show('simulate'))}
                {admin && item('Agents', () => app.show('agents'))}
                {admin && item('Audit log', () => app.show('audit'))}
                {item('Status', () => app.show('status'))}
                {user && (
                  <>
                    <div className="menu-sep" />
                    {item(`Sign out ${user.name}`, out)}
                  </>
                )}
              </Popover>
            </span>
            <span onMouseEnter={hoverMenu('view')}>
              <Popover label="View" cls="menu-top" {...menuProps('view')}>
                {tabs.map(([t, label], i) => (
                  <span key={t}>{item(label, () => app.show(t), `${mod}${i + 1}`, t === tab ? 'active' : '')}</span>
                ))}
                <div className="menu-sep" />
                {admin && (
                  <>
                    {item('Sessions', () => app.show('sessions'), undefined, tab === 'sessions' ? 'active' : '')}
                    <div className="menu-sep" />
                  </>
                )}
                {item(explorer ? 'Hide the explorer' : 'Show the explorer', () => setExplorer(!explorer), `${mod}B`)}
                {item(line ? 'Hide the worldline' : 'Show the worldline', () => setLine(!line))}
                {item(dark ? 'Light theme' : 'Dark theme', () => setTheme(dark ? 'light' : 'dark'))}
                {item('Command palette', () => setPalette(true), `${mod}K`)}
              </Popover>
            </span>
            <span onMouseEnter={hoverMenu('world')}>
              <Popover label="World" cls="menu-top" {...menuProps('world')}>
                <div className="group label menu-head">Switch to</div>
                {worlds.slice(0, 12).map((x) => (
                  <span key={x.name}>
                    {item(
                      <>
                        <span className="dot" style={{ background: hueOf(x) }} />
                        {x.name}
                      </>,
                      () => setWorld(x.name),
                      x.name === world ? '✓' : undefined,
                      x.name === world ? 'active' : '',
                    )}
                  </span>
                ))}
                <div className="menu-sep" />
                {worldMenu}
              </Popover>
            </span>
            <span onMouseEnter={hoverMenu('help')}>
              <Popover label="Help" cls="menu-top" {...menuProps('help')}>
                {item('Keyboard shortcuts', () => shortcuts(dlg), '?')}
                <a
                  className="menu-item"
                  href="https://github.com/Abhishekxdg/chronosdb#readme"
                  target="_blank"
                  rel="noreferrer noopener"
                  onClick={() => setMenu(null)}
                >
                  Chronos documentation
                </a>
                <a
                  className="menu-item"
                  href="https://github.com/Abhishekxdg/chronosdb/issues/new/choose"
                  target="_blank"
                  rel="noreferrer noopener"
                  onClick={() => setMenu(null)}
                >
                  Report a problem
                </a>
                {about && <div className="menu-foot faint">Chronos {about.version}</div>}
              </Popover>
            </span>
          </nav>
          <button className="search" type="button" onClick={() => setPalette(true)} title={`Command palette (${mod}K)`}>
            <Icon name="search" size={14} />
            <span className="q">{about?.folder || 'Search'}</span>
            <span className="kbd">{mod}K</span>
          </button>
          <div className="top-right">
            {!conn && <span className="conn-off">Studio is unreachable</span>}
            <Popover
              label={
                <>
                  <span className="avatar" aria-hidden="true">
                    {world.slice(0, 1).toUpperCase()}
                  </span>
                  <span className="wname">{world}</span>
                  {onMain && <span className="live-tag">live</span>}
                  <Icon name="down" size={14} />
                </>
              }
              cls="world-pill"
              title={chain.map((x) => x.name).join(' › ') + ': world actions'}
              right
              {...menuProps('worldpill')}
            >
              {chain.length > 1 && <div className="menu-head faint mono">{chain.map((x) => x.name).join(' › ')}</div>}
              {worldMenu}
            </Popover>
            {!admin ? null : !onMain && changes > 0 ? (
              <button className="btn accent" type="button" onClick={() => app.show('changes')} title="Review and merge this world's changes">
                <Icon name="diff" />
                <span className="txt">Review</span>
                <span className="n">{n(changes)}</span>
              </button>
            ) : (
              <button
                className="btn outline"
                type="button"
                onClick={() => forkFrom(world, suggestName())}
                title={`Fork a world from ${world}: an isolated copy to change`}
              >
                <Icon name="fork" />
                <span className="txt">Fork</span>
              </button>
            )}
            {user && (
              <Popover
                label={
                  <>
                    <Icon name="user" size={14} />
                    <span className="uname">{user.name}</span>
                    {user.superuser && <span className="tag">superuser</span>}
                  </>
                }
                cls="btn ghost user-chip"
                title={`Signed in as ${user.name}`}
                right
                {...menuProps('user')}
              >
                <div className="menu-head faint">
                  Signed in as <span className="mono">{user.name}</span>
                  {user.superuser ? ', a superuser' : ': reads, and SQL with its privileges'}
                </div>
                {item('Sign out', out)}
              </Popover>
            )}
          </div>
        </header>

        <nav className="rail" aria-label="Views">
          <button
            type="button"
            className="rail-btn toggle"
            aria-pressed={explorer}
            aria-label={`${explorer ? 'Collapse' : 'Expand'} the sidebar (${mod}B)`}
            title={`${explorer ? 'Collapse' : 'Expand'} the sidebar (${mod}B)`}
            onClick={() => setExplorer(!explorer)}
          >
            <Icon name="panel" size={18} />
          </button>
          <span className="rail-sep" />
          {tabs.map(([t, label], i) => (
            <RailButton key={t} icon={ICONS[t]} label={`${label} (${mod}${i + 1})`} on={tab === t} onClick={() => app.show(t)} />
          ))}
          <span className="rail-sep" />
          {((admin ? ['simulate', 'agents', 'audit', 'status'] : ['status']) as Tab[]).map((t) => (
            <RailButton key={t} icon={ICONS[t]} label={PAGES[t]!} on={tab === t} onClick={() => app.show(t)} />
          ))}
          <span className="spacer" />
          <RailButton icon={dark ? 'sun' : 'moon'} label={dark ? 'Light theme' : 'Dark theme'} onClick={() => setTheme(dark ? 'light' : 'dark')} />
          <RailButton icon="sliders" label={`Settings (${mod},)`} on={tab === 'settings'} onClick={() => app.show('settings')} />
        </nav>

        <aside className="side" aria-label="Explorer">
          <Explorer app={app} tablesError={tablesError} merging={merging} onReload={reloadAll} onCollapse={() => (setExplorer(false), setSide(false))} />
          <div className="side-resize" onPointerDown={startResize} role="separator" aria-orientation="vertical" aria-label="Resize the explorer" />
        </aside>
        <div className="scrim" onClick={() => setSide(false)} />

        <main className="main">
          <div className="tabs">
            <div className="tray" role="tablist" aria-label="Views">
              {tabs.map(([t, label], i) => (
                <button
                  key={t}
                  role="tab"
                  type="button"
                  className="tab"
                  aria-selected={t === tab}
                  title={`${label} (${mod}${i + 1})`}
                  onClick={() => app.show(t)}
                >
                  <Icon name={ICONS[t]} size={14} />
                  <span>{t === 'data' && table ? table : label}</span>
                  {t === 'changes' && !onMain && changes > 0 && <span className="count">{n(changes)}</span>}
                </button>
              ))}
              {PAGES[tab] && (
                <span className="tab page" role="tab" aria-selected="true">
                  <Icon name={ICONS[tab]} size={14} />
                  <span>{PAGES[tab]}</span>
                  <button type="button" className="close" aria-label={`Close ${PAGES[tab]}`} title="Back to the data" onClick={() => app.show('data')}>
                    <Icon name="x" size={12} />
                  </button>
                </span>
              )}
            </div>
            <span className="spacer" />
            <Popover label={<Icon name="more" />} cls="btn ghost icon" title="More" right {...menuProps('more')}>
              {item('Refresh the view', reloadAll)}
              {admin && item('Agents', () => app.show('agents'))}
              {admin && item('Audit log', () => app.show('audit'))}
              {item('Status', () => app.show('status'))}
              {item('Settings', () => app.show('settings'), `${mod},`)}
              <div className="menu-sep" />
              {item('Keyboard shortcuts', () => shortcuts(dlg), '?')}
            </Popover>
          </div>
          {line && !locked && admin && !PAGES[tab] && <Worldline />}
          <div className={'canvas c-' + tab}>{locked ? <Gate /> : <View key={`${tab}/${world}/${table}/${bump}`} />}</div>
        </main>

        <footer className="statusbar">
          <span className="slot" ref={setStatusSlot} />
          <span className="sb-world">
            <span className="dot" style={{ background: hue }} />
            {world}
            {onMain && <span className="live">Live data</span>}
          </span>
          {about && <span className="sb-extra">Chronos {about.version}</span>}
          <span className={'sb-conn' + (conn ? '' : ' off')}>{conn ? 'Connected' : 'Unreachable'}</span>
        </footer>
      </div>
      {palette && <Palette commands={commands} onClose={() => setPalette(false)} />}
    </AppCtx.Provider>
  );
}

function RailButton({ icon, label, on, onClick }: { icon: IconName; label: string; on?: boolean; onClick: () => void }) {
  return (
    <button type="button" className="rail-btn" aria-label={label} aria-current={on ? 'page' : undefined} title={label} onClick={onClick}>
      <Icon name={icon} size={18} />
    </button>
  );
}

// ---------- the explorer ----------
function Explorer({
  app,
  tablesError,
  merging,
  onReload,
  onCollapse,
}: {
  app: AppState;
  tablesError: string;
  merging: string | null;
  onReload: () => void;
  onCollapse: () => void;
}) {
  const [shut, setShut] = useState<Record<string, boolean>>(() => {
    try {
      return { timeline: true, outline: true, ...JSON.parse(store.get('chronos-studio-sections') || '{}') };
    } catch {
      return { timeline: true, outline: true };
    }
  });
  const toggle = (k: string) =>
    setShut((prev) => {
      const next = { ...prev, [k]: !prev[k] };
      store.set('chronos-studio-sections', JSON.stringify(next));
      return next;
    });
  const [filter, setFilter] = useState('');
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [info, setInfo] = useState<Record<string, TableInfo | 'error'>>({});
  const { world, tables, table } = app;
  const load = (t: string) => {
    const k = world + '/' + t;
    if (info[k]) return;
    const json = tables.some((x) => x.name === t && !x.schema);
    tableInfo(t, world, json).then(
      (i) => setInfo((m) => ({ ...m, [k]: i })),
      () => setInfo((m) => ({ ...m, [k]: 'error' })),
    );
  };
  const expand = (t: string) => {
    const s = new Set(open);
    if (s.has(t)) s.delete(t);
    else (s.add(t), load(t));
    setOpen(s);
  };
  useEffect(() => {
    if (table && !shut.outline) load(table);
  }, [table, world, shut.outline]); // eslint-disable-line
  const shown = tables.filter((t) => !filter || t.name.toLowerCase().includes(filter.toLowerCase()));
  const outline = table ? info[world + '/' + table] : undefined;
  return (
    <div className="explorer">
      <div className="ex-head">
        <Icon name="database" size={15} />
        <span className="folder" title={app.folder}>
          {app.folder || 'database'}
        </span>
        <span className="spacer" />
        {app.admin && (
          <button
            className="btn ghost icon small"
            type="button"
            title={`Fork a world from ${world}`}
            aria-label={`Fork a world from ${world}`}
            onClick={() => app.forkFrom(world, suggestName())}
          >
            <Icon name="fork" size={15} />
          </button>
        )}
        <button className="btn ghost icon small" type="button" title="Reload" aria-label="Reload" onClick={onReload}>
          <Icon name="refresh" size={15} />
        </button>
        <button className="btn ghost icon small" type="button" title={`Collapse the sidebar (${mod}B)`} aria-label="Collapse the sidebar" onClick={onCollapse}>
          <Icon name="collapse" size={15} />
        </button>
      </div>

      <div className="ex-card grow">
        <Section title="Worlds" count={app.worlds.length} shut={!!shut.worlds} onToggle={() => toggle('worlds')}>
          <WorldTree worlds={app.worlds} active={world} merging={merging} onPick={app.setWorld} />
        </Section>
        <Section title="Tables" count={tables.length} shut={!!shut.tables} onToggle={() => toggle('tables')}>
          {tables.length > 8 && (
            <input
              className="side-filter"
              type="search"
              placeholder="Filter tables"
              aria-label="Filter tables"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          )}
          <ul className="list tree" role="tree">
            {!app.tablesReady ? (
              <li className="empty">Loading…</li>
            ) : tablesError ? (
              <li className="empty">{tablesError}</li>
            ) : !shown.length ? (
              <li className="empty">{tables.length ? 'No table matches' : 'No tables yet. Create one in SQL.'}</li>
            ) : (
              shown.map((t) => {
                const isOpen = open.has(t.name);
                const i = info[world + '/' + t.name];
                return (
                  <li key={t.name} role="treeitem" aria-expanded={isOpen} aria-selected={t.name === table}>
                    <div className={'item' + (t.name === table && app.tab === 'data' ? ' on' : '')}>
                      <button
                        type="button"
                        className="caret"
                        aria-label={(isOpen ? 'Hide' : 'Show') + ` the columns of ${t.name}`}
                        onClick={() => expand(t.name)}
                      >
                        <Icon name={isOpen ? 'down' : 'right'} size={13} />
                      </button>
                      <button
                        type="button"
                        className="pick"
                        title={t.kind ? `${t.name} · ${t.kind}, read only` : t.name}
                        onClick={() => app.pickTable(t.name)}
                      >
                        <Icon name={t.kind ? 'eye' : 'table'} size={14} />
                        <span className="name">{t.name}</span>
                        <span className="count">{t.rows == null ? '' : n(t.rows)}</span>
                      </button>
                    </div>
                    {isOpen && (
                      <ul className="cols-list" role="group">
                        {!i ? (
                          <li className="empty">Loading…</li>
                        ) : i === 'error' ? (
                          <li className="empty">Couldn’t read the columns</li>
                        ) : (
                          i.cols.map((c) => <ColumnLine key={c.column_name} name={c.column_name} type={castType(c)} pk={i.pk.includes(c.column_name)} />)
                        )}
                      </ul>
                    )}
                  </li>
                );
              })
            )}
          </ul>
        </Section>
      </div>

      {app.admin && (
        <div className="ex-card">
          <Section title="Timeline" shut={!!shut.timeline} onToggle={() => toggle('timeline')}>
            <Timeline world={world} onOpen={() => app.show('history')} />
          </Section>
        </div>
      )}
      <div className="ex-card">
        <Section title={table ? `Outline · ${table}` : 'Outline'} shut={!!shut.outline} onToggle={() => toggle('outline')}>
          <ul className="list cols-list flat">
            {!table ? (
              <li className="empty">Pick a table</li>
            ) : !outline ? (
              <li className="empty">Loading…</li>
            ) : outline === 'error' ? (
              <li className="empty">Couldn’t read the columns</li>
            ) : (
              outline.cols.map((c) => (
                <ColumnLine
                  key={c.column_name}
                  name={c.column_name}
                  type={castType(c)}
                  pk={outline.pk.includes(c.column_name)}
                  onClick={() => {
                    if (app.tab !== 'data') app.show('data');
                    setTimeout(() => dispatchEvent(new CustomEvent('studio:schema', { detail: c.column_name })), 50);
                  }}
                />
              ))
            )}
          </ul>
        </Section>
      </div>
    </div>
  );
}

function Section({ title, count, shut, onToggle, children }: { title: string; count?: number; shut: boolean; onToggle: () => void; children: ReactNode }) {
  return (
    <section className={'ex-sec' + (shut ? ' shut' : '')}>
      <button type="button" className="ex-sec-head" aria-expanded={!shut} onClick={onToggle}>
        <Icon name={shut ? 'right' : 'down'} size={13} />
        <span className="t">{title}</span>
        {count != null && <span className="c">{n(count)}</span>}
      </button>
      {!shut && <div className="ex-sec-body">{children}</div>}
    </section>
  );
}

function ColumnLine({ name, type, pk, onClick }: { name: string; type: string; pk: boolean; onClick?: () => void }) {
  const body = (
    <>
      <span className="tm" aria-hidden="true">
        {pk ? <Icon name="key" size={12} /> : typeMark(type)}
      </span>
      <span className="name">{name}</span>
      <span className="ty">{type}</span>
    </>
  );
  return (
    <li title={`${name} ${type}${pk ? ' · primary key' : ''}`}>
      {onClick ? (
        <button type="button" className="col-line" onClick={onClick}>
          {body}
        </button>
      ) : (
        <span className="col-line">{body}</span>
      )}
    </li>
  );
}

interface HistEvent {
  at: number;
  world: string;
  event: string;
  rows: number;
}
function Timeline({ world, onOpen }: { world: string; onOpen: () => void }) {
  const [events, setEvents] = useState<HistEvent[] | null>(null);
  useEffect(() => {
    let live = true;
    api<{ events: HistEvent[] }>('history', { branch: world, limit: 8 }).then(
      (r) => live && setEvents(r.events),
      () => live && setEvents([]),
    );
    return () => {
      live = false;
    };
  }, [world]);
  if (!events) return <p className="empty">Loading…</p>;
  if (!events.length) return <p className="empty">No history yet</p>;
  return (
    <ul className="list timeline">
      {events.map((e, i) => (
        <li key={i}>
          <button type="button" className="tl" title={`${fmtTime(e.at)} · ${e.world}`} onClick={onOpen}>
            <span className="ev">
              {e.event}
              {e.rows ? <span className="faint"> · {plural(e.rows, 'row')}</span> : null}
              {e.world !== world && <span className="faint"> · {e.world}</span>}
            </span>
            <span className="when">{ago(e.at)}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

function shortcuts(dlg: ReturnType<typeof useDialogs>) {
  const rows = SHORTCUTS;
  dlg.modal(
    'Keyboard shortcuts',
    <table className="tbl">
      <tbody>
        {rows.map(([k, v]) => (
          <tr key={k}>
            <td className="mono nowrap">{k}</td>
            <td>{v}</td>
          </tr>
        ))}
      </tbody>
    </table>,
    [{ label: 'Close', cls: 'primary', value: () => true }],
  );
}

/** chronos studio --login's sign-in: a database role and its password, as psql asks for them. The server's
 *  refusal is shown as it says it (one message for any wrong name or password). */
function SignIn({ onIn }: { onIn: (u: User) => void }) {
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name || !password) return setError('Give the role’s name and its password');
    setBusy(true);
    setError('');
    try {
      onIn(await signIn(name, password));
    } catch (err) {
      setError(errText((err as ApiError).detail ?? (err as Error).message));
      setPassword('');
      setBusy(false);
    }
  };
  return (
    <main className="signin">
      <form className="signin-card" onSubmit={submit} aria-label="Sign in">
        <span className="logo" aria-hidden="true">
          <svg width="28" height="28" viewBox="0 0 24 24">
            <rect x="3" y="3" width="15" height="4.5" rx="2.25" />
            <rect x="7" y="9.75" width="14" height="4.5" rx="2.25" />
            <rect x="3" y="16.5" width="10" height="4.5" rx="2.25" />
          </svg>
        </span>
        <h1>Sign in to Chronos Studio</h1>
        <p className="note">With a database role that may log in and has a password. The Studio acts with that role’s privileges.</p>
        <label className="field">
          <span>Role</span>
          <input type="text" autoComplete="username" autoCapitalize="none" spellCheck={false} autoFocus value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="field">
          <span>Password</span>
          <input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />
        </label>
        {error && (
          <p className="err" role="alert">
            {error}
          </p>
        )}
        <button className="btn primary" type="submit" disabled={busy}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>
      </form>
    </main>
  );
}

function Gate() {
  return (
    <div className="pad">
      <h2 className="ident">This page needs its session link</h2>
      <p className="muted">
        Open the link <code>chronos studio</code> printed in your terminal: it ends in <code>#t=…</code>, and each run makes a new one.
      </p>
    </div>
  );
}

function WorldTree({ worlds, active, merging, onPick }: { worlds: World[]; active: string; merging: string | null; onPick: (name: string) => void }) {
  const names = new Set(worlds.map((w) => w.name));
  const ids = new Map(worlds.map((w) => [w.id, w.name]));
  const parentOf = (w: World) => (w.parent == null ? null : names.has(w.parent) ? w.parent : (ids.get(w.parent) ?? null));
  const kids = new Map<string | null, World[]>();
  for (const w of worlds) {
    const p = parentOf(w);
    kids.set(p, [...(kids.get(p) || []), w]);
  }
  const rows: { w: World; depth: number }[] = [];
  const walk = (w: World, depth: number) => {
    rows.push({ w, depth });
    for (const c of (kids.get(w.name) || []).sort((a, b) => a.created - b.created)) walk(c, depth + 1);
  };
  worlds.filter((w) => parentOf(w) === null).forEach((w) => walk(w, 0));
  if (!rows.length) return <p className="empty">Loading…</p>;
  return (
    <ul className="list tree worlds" role="tree">
      {rows.map(({ w, depth }) => {
        const title = [
          w.name,
          `created ${fmtTime(w.created)}`,
          w.owner && `owner ${w.owner}`,
          w.expires && `expires ${fmtTime(w.expires)}`,
          w.flagged && 'open during a crash: its merge must be confirmed',
          w.changes != null && plural(w.changes, 'change'),
        ]
          .filter(Boolean)
          .join('\n');
        const style = { '--hue': hueOf(w), '--depth': depth, '--connector': depth ? 'block' : 'none' } as CSSProperties;
        return (
          <li key={w.name} role="treeitem" aria-level={depth + 1} aria-selected={w.name === active}>
            <button
              type="button"
              className={'item node' + (w.name === active ? ' on' : '') + (w.name === merging ? ' merging' : '')}
              style={style}
              title={title}
              onClick={() => onPick(w.name)}
            >
              <span className="stripe" aria-hidden="true" />
              <span className="name">{w.name}</span>
              {w.name === 'main' && <span className="live-tag">live</span>}
              {w.owner && <span className="owner">{w.owner}</span>}
              {w.flagged && <span className="tag bad">flagged</span>}
              {w.changes ? <span className="badge">{n(w.changes)}</span> : null}
            </button>
          </li>
        );
      })}
    </ul>
  );
}
