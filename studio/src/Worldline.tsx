// The worldline: every world drawn as a lane branching off its parent at the moment it was
// forked, running to now, like a subway map of the database. Click a lane to switch to it;
// drag across the strip to scrub back in time, and let go to read the tables as they were.
// Time runs piecewise: each fork gets an even share of the width, so a busy hour and a quiet
// month read equally well. Merges into main (from its history) rejoin it: a world that's gone
// since is drawn as a faint lane from its fork to its merge. Moments before the history kept
// (history_retention) are greyed: time travel can't read them.
import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from 'react';
import { api, type World } from './api';
import { useApp } from './context';
import { fmtTime, hueOf, n, plural, span as lasting } from './util';

const PAD_L = 20;
const PAD_R = 188; // room for the active world's label at the end of its lane
const TOP = 14; // main's lane
const MERGES = 10; // the latest merges into main drawn

interface Merge {
  name: string;
  forkAt: number | null;
  mergeAt: number;
  rows: number;
}

export function Worldline() {
  const app = useApp();
  const box = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(800);
  const [scrub, setScrub] = useState<{ x: number; t: number } | null>(null);
  const [hover, setHover] = useState<string | null>(null);
  // merges into main, from its history: `forked X` then `merged X`
  const [merges, setMerges] = useState<Merge[]>([]);
  const worldKey = app.worlds.map((x) => x.name + x.version).join(',');
  useEffect(() => {
    let live = true;
    api<{ events: { at: number; event: string; rows: number }[] }>('history', { branch: 'main', limit: 400 }).then(
      (r) => {
        const forkAt = new Map<string, number>();
        const out: Merge[] = [];
        for (const e of [...r.events].sort((a, b) => a.at - b.at)) {
          const m = /^(forked|merged) (.+)$/.exec(e.event);
          if (m?.[1] === 'forked') forkAt.set(m[2], e.at);
          else if (m?.[1] === 'merged') out.push({ name: m[2], forkAt: forkAt.get(m[2]) ?? null, mergeAt: e.at, rows: e.rows });
        }
        if (live) setMerges(out.slice(-MERGES));
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, [worldKey]);
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const worlds = [...app.worlds].sort((a, b) => a.created - b.created);
  const main = worlds.find((x) => x.name === 'main');
  const forks = worlds.filter((x) => x !== main);
  if (!main) return <div className="worldline" ref={box} />;

  const now = Date.now();
  const alive = new Set(worlds.map((x) => x.name));
  const ghosts = merges.filter((m) => !alive.has(m.name) && m.forkAt !== null);
  const stops = [...new Set([main.created, ...forks.map((x) => x.created), ...ghosts.map((g) => g.forkAt!), ...merges.map((m) => m.mergeAt), now])]
    .filter((v) => v > 0)
    .sort((a, b) => a - b);
  const span = Math.max(1, w - PAD_L - PAD_R);
  const step = span / Math.max(1, stops.length - 1);
  const x = (t: number) => {
    if (t <= stops[0]) return PAD_L;
    for (let i = 1; i < stops.length; i++) if (t <= stops[i]) return PAD_L + step * (i - 1 + (t - stops[i - 1]) / Math.max(1, stops[i] - stops[i - 1]));
    return PAD_L + span;
  };
  const t = (px: number) => {
    const f = Math.max(0, Math.min(stops.length - 1, (px - PAD_L) / step));
    const i = Math.min(stops.length - 2, Math.floor(f));
    return stops.length < 2 ? now : stops[i] + (f - i) * (stops[i + 1] - stops[i]);
  };
  // lanes close up as worlds multiply, so the strip stays under ~80px
  const gap = Math.max(3, Math.min(8, 44 / Math.max(1, forks.length + ghosts.length)));
  const lane = new Map<string, number>([[main.name, TOP]]);
  forks.forEach((f, i) => lane.set(f.name, TOP + 14 + i * gap));
  const ghostY = (i: number) => TOP + 14 + (forks.length + i) * gap;
  const H = Math.max(46, TOP + 14 + Math.max(0, forks.length + ghosts.length - 1) * gap + 14);
  const names = new Set(worlds.map((x) => x.name));
  const ids = new Map(worlds.map((x) => [x.id, x.name]));
  const parentOf = (f: World) => (f.parent == null ? 'main' : names.has(f.parent) ? f.parent : (ids.get(f.parent) ?? 'main'));
  const xNow = x(now);
  // the moments time travel can read: from the oldest history kept (none kept yet: none before now)
  const h = app.history;
  const kept = h ? (h.from ?? now) : null;
  const xKept = kept === null ? PAD_L : Math.max(PAD_L, Math.min(xNow, x(kept)));
  const keptNote = h ? `history kept ${lasting(h.retention)}` : '';

  const path = (f: World) => {
    const x0 = x(f.created);
    const yp = lane.get(parentOf(f)) ?? TOP;
    const y = lane.get(f.name)!;
    const r = Math.min(14, Math.max(6, (y - yp) * 0.6));
    return `M${x0} ${yp}C${x0 + r} ${yp} ${x0} ${y} ${x0 + r} ${y}H${xNow}`;
  };
  const active = app.worlds.find((x) => x.name === app.world);
  const pick = (name: string) => name !== app.world && app.setWorld(name);

  const startScrub = (e: React.PointerEvent<SVGSVGElement>) => {
    if (e.button !== 0 || (e.target as Element).closest('.lane')) return;
    const svg = e.currentTarget;
    const left = svg.getBoundingClientRect().left;
    const at = (cx: number) => {
      const px = Math.max(PAD_L, Math.min(xNow, cx - left));
      return { x: px, t: t(px) };
    };
    svg.setPointerCapture(e.pointerId);
    setScrub(at(e.clientX));
    let cur = at(e.clientX);
    const move = (ev: PointerEvent) => setScrub((cur = at(ev.clientX)));
    const up = () => {
      svg.removeEventListener('pointermove', move);
      svg.removeEventListener('pointerup', up);
      setScrub(null);
      // the last few pixels mean "now"
      if (xNow - cur.x > 4) app.timeTravel(new Date(cur.t).toISOString());
    };
    svg.addEventListener('pointermove', move);
    svg.addEventListener('pointerup', up);
  };

  const label = (f: World) => {
    const y = lane.get(f.name)!;
    return (
      <g className="wl-label" transform={`translate(${xNow + 10} ${y})`} style={{ '--hue': hueOf(f) } as CSSProperties}>
        <text className="nm" dy="0.35em">
          {f.name}
        </text>
        {f.changes ? (
          <text className="ch" dy="0.35em" dx={f.name.length * 7.4 + 8}>
            {n(f.changes)} {f.changes === 1 ? 'change' : 'changes'}
          </text>
        ) : null}
      </g>
    );
  };
  const shownLabel = forks.find((f) => f.name === hover) || (active && active !== main ? active : undefined);

  return (
    <div className="worldline" ref={box} aria-label="Worlds over time">
      <svg width={w} height={H} onPointerDown={startScrub} role="img">
        <title>Worlds over time: click a lane to switch, drag to read the past</title>
        {/* before the history kept: greyed, unreadable */}
        {xKept > PAD_L && (
          <rect className="wl-gone" x={PAD_L} y={2} width={xKept - PAD_L} height={H - 4}>
            <title>
              {kept === now
                ? `No history kept yet (history_retention is ${lasting(h!.retention)})`
                : `Before ${fmtTime(kept)}: history_retention is ${lasting(h!.retention)}, so time travel can't read this far back`}
            </title>
          </rect>
        )}
        {/* main: the live data, from its start to now */}
        <g className={'lane main' + (app.world === 'main' ? ' on' : '')} style={{ '--hue': hueOf(main) } as CSSProperties} onClick={() => pick('main')}>
          <line className="hit" x1={PAD_L} x2={xNow} y1={TOP} y2={TOP} />
          <line className="ln" pathLength={1} x1={PAD_L} x2={xNow} y1={TOP} y2={TOP} />
          <circle className="end" cx={xNow} cy={TOP} r="3.5" />
          <title>main · the live data · since {fmtTime(main.created)}</title>
        </g>
        {ghosts.map((g, i) => {
          const [x0, x1, y] = [x(g.forkAt!), x(g.mergeAt), ghostY(i)];
          const r = Math.min(10, Math.max(4, (y - TOP) * 0.6), Math.max(2, (x1 - x0) / 3));
          return (
            <g key={'g' + g.name + g.mergeAt} className="ghost">
              <path d={`M${x0} ${TOP}C${x0 + r} ${TOP} ${x0} ${y} ${x0 + r} ${y}H${x1 - r}C${x1} ${y} ${x1 - r} ${TOP} ${x1} ${TOP}`} />
              <title>
                {g.name} · forked {fmtTime(g.forkAt)}, merged {plural(g.rows, 'row')} into main {fmtTime(g.mergeAt)}
              </title>
            </g>
          );
        })}
        {merges.map((m) => {
          const xm = x(m.mergeAt);
          const y = lane.get(m.name);
          const w0 = worlds.find((x) => x.name === m.name);
          return (
            <g key={'m' + m.name + m.mergeAt} className="merge" style={{ '--hue': w0 ? hueOf(w0) : 'var(--faint)' } as CSSProperties}>
              {y !== undefined && <path d={`M${xm} ${y}C${xm} ${y - 6} ${xm} ${TOP + 6} ${xm} ${TOP}`} />}
              <circle cx={xm} cy={TOP} r="3.5" />
              <title>
                {m.name} merged {plural(m.rows, 'row')} into main · {fmtTime(m.mergeAt)}
              </title>
            </g>
          );
        })}
        {forks.map((f) => (
          <g
            key={f.name}
            className={'lane' + (f.name === app.world ? ' on' : '') + (f.name === hover ? ' hover' : '')}
            style={{ '--hue': hueOf(f) } as CSSProperties}
            onClick={() => pick(f.name)}
            onMouseEnter={() => setHover(f.name)}
            onMouseLeave={() => setHover(null)}
          >
            <path className="hit" d={path(f)} />
            <path className="ln" pathLength={1} d={path(f)} />
            <circle className="fork" cx={x(f.created)} cy={lane.get(parentOf(f))} r="2.5" />
            <circle className={'end' + (f.changes ? ' dirty' : '')} cx={xNow} cy={lane.get(f.name)} r={f.changes ? 3.5 : 2.5} />
            <title>
              {f.name} · forked from {parentOf(f)} {fmtTime(f.created)}
              {f.owner ? ` · by ${f.owner}` : ''}
              {f.changes != null ? ` · ${plural(f.changes, 'change')}` : ''}
            </title>
          </g>
        ))}
        {(app.world === 'main' || !shownLabel) && !hover && (
          <g className="wl-label" transform={`translate(${xNow + 10} ${TOP})`} style={{ '--hue': hueOf(main) } as CSSProperties}>
            <text className="nm" dy="0.35em">
              main
            </text>
            <text className="ch" dy="0.35em" dx="42">
              {forks.length ? plural(forks.length, 'world') + ' forked' : 'live data'}
            </text>
          </g>
        )}
        {shownLabel && label(shownLabel)}
        {(main.created > 0 || keptNote) && (
          <text className="wl-axis" x={PAD_L} y={H - 5}>
            {[main.created > 0 ? `since ${fmtTime(main.created).slice(0, 10)}` : '', keptNote].filter(Boolean).join(' · ')}
          </text>
        )}
        <text className="wl-axis" x={xNow + 10} y={H - 5}>
          now · drag left to read the past
        </text>
        {scrub && (
          <g className={'scrub' + (kept !== null && scrub.t < kept ? ' gone' : '')}>
            <line x1={scrub.x} x2={scrub.x} y1={2} y2={H - 2} />
            <rect x={scrub.x + (scrub.x + 160 > w ? -156 : 6)} y={H / 2 - 10} width={150} height={20} rx={6} />
            <text x={scrub.x + (scrub.x + 160 > w ? -148 : 14)} y={H / 2} dy="0.35em">
              {kept !== null && scrub.t < kept ? 'before the history kept' : `as of ${fmtTime(scrub.t).slice(5)}`}
            </text>
          </g>
        )}
      </svg>
      {!forks.length && <span className="wl-hint">Only main so far: fork a world to try changes without touching live data</span>}
    </div>
  );
}
