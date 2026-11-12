import { t as tl } from '../lib/i18n';
// Automation in the UI: the right-click menu every automatable control shares, creating an
// automation clip, and the live (automated) value a control shows while the song plays.
import { createMemo, createRoot, createSignal } from 'solid-js';
import type { AutoPoint, AutoTarget } from '../api/types';
import { createAutomationOps, denormalize, targetKey, targetLabel, valueAt } from '../lib/automation';
import { fmtPos } from '../lib/format';
import { addFrameTask } from '../ui/frameLoop';
import { commit } from './edit';
import { allocId, S } from './store';
import { curBeat } from './telemetry';
import { openMenu, say, setU, U } from './ui';

const [beatSig, setBeatSig] = createSignal(0);
type Span = { start: number; end: number; offset: number; points: AutoPoint[] };
/**
 * Automation clips per target key, in clip order (the last one covering the beat wins). Built once
 * per edit; a knob then looks up only its own few clips instead of walking every clip each frame
 * (65 mixer strips × 1400 clips held playback at 20 fps).
 */
const index = createRoot(() => createMemo(() => {
  const autos = new Map((S.project.automations ?? []).map(a => [a.id, a]));
  const m = new Map<string, Span[]>();
  for (const c of S.project.clips) {
    if (c.kind !== 'auto' || c.muted) continue;
    const a = autos.get(c.ref); if (!a) continue;
    const k = targetKey(a.target);
    let l = m.get(k); if (!l) m.set(k, l = []);
    l.push({ start: c.start, end: c.start + c.len, offset: c.offset, points: a.points });
  }
  return m;
}));
const keys = () => index();
const valueFor = (t: AutoTarget, beat: number) => {
  let out: number | null = null;
  for (const s of index().get(targetKey(t)) ?? []) {
    if (beat < s.start || beat >= s.end) continue;
    const v = valueAt(s.points, beat - s.start + s.offset);
    if (v != null) out = v;
  }
  return out;
};

// While playing, publish the playhead ~30×/s — only controls that are automated subscribe to it.
let lastSet = 0;
addFrameTask(now => {
  if (!U.playing || !keys().size) return;
  if (now - lastSet < 30) return;
  lastSet = now; setBeatSig(curBeat());
});

/** Is this control driven by an automation clip? */
export const isAutomated = (t: AutoTarget) => keys().has(targetKey(t));

/**
 * The value (plain units) a control should display: the automated value at the playhead while
 * playing (or at the start marker when stopped), or null when not automated.
 */
export function autoDisplay(t: AutoTarget): number | null {
  if (!isAutomated(t)) return null;
  const beat = U.playing ? beatSig() : U.start;
  const v = valueFor(t, beat);
  return v == null ? null : denormalize(S.project, S.catalog, t, v);
}

/** Create an automation clip for `target` at the start marker (or over the loop), 4 bars long.
 * `current` is a hosted plugin parameter's live normalized value (the project does not know it);
 * `paramLabel` names the parameter ("Decay") for the clip and mixer lane. */
export function automate(target: AutoTarget, color = '#e0a44a', opts?: { current?: number; paramLabel?: string }) {
  const lp = S.project.loop;
  const start = lp ? lp.a : Math.max(0, U.start), len = lp ? lp.b - lp.a : 16;
  const name = targetLabel(S.project, S.catalog, target, opts?.paramLabel);
  const r = createAutomationOps(S.project, S.catalog, target, name, color, start, len, allocId, opts?.current);
  commit(r.ops, tl('a.au.label'));
  setU('sel', [r.clipId]);
  say(tl('a.au.created', { name, n: r.track + 1, pos: fmtPos(start) }));
}

/**
 * The right-click menu of a control that cannot be automated (a clip's fades, a channel's root
 * key): the same menu, with "Automate" shown disabled and why.
 */
export function controlMenu(x: number, y: number, label: string, reset: () => void, why: string) {
  openMenu(x, y, [
    { t: 'title', label },
    { t: 'item', label: tl('a.au.automate'), disabled: true, title: why, act: () => {} },
    { t: 'sep' },
    { t: 'item', label: tl('a.au.reset'), kbd: tl('a.pl.dblClick'), act: reset },
  ], 140);
}

/** The shared right-click menu of an automatable control. `opts` carries a hosted
 * plugin parameter's live value and name (see `automate`). */
export function paramMenu(x: number, y: number, target: AutoTarget, reset: (() => void) | null, color?: string, opts?: { current?: number; paramLabel?: string }) {
  const auto = isAutomated(target);
  const clips = () => S.project.clips.filter(c => c.kind === 'auto' && S.project.automations?.find(a => a.id === c.ref && targetKey(a.target) === targetKey(target)));
  openMenu(x, y, [
    { t: 'title', label: targetLabel(S.project, S.catalog, target, opts?.paramLabel) },
    { t: 'item', label: tl('a.au.automate'), kbd: S.project.loop ? tl('a.au.loop') : tl('a.au.bars4'), act: () => automate(target, color, opts) },
    ...(auto ? [{ t: 'item' as const, label: tl('a.au.selClips'), act: () => setU('sel', clips().map(c => c.id)) }] : []),
    ...(reset ? [{ t: 'sep' as const }, { t: 'item' as const, label: tl('a.au.reset'), kbd: tl('a.pl.dblClick'), act: reset }] : []),
  ], 140);
}
