import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show, type JSX } from 'solid-js';
import { be } from '../api/backend';
import type { AutoTarget, Fx, FxKind, Id, ParamDef, Vst3Param } from '../api/types';
import { clamp } from '../lib/format';
import { lang, t, tName, type TranslationKey } from '../lib/i18n';
import { NOTE_NAMES } from '../lib/pitch';
import { paramDefs, pFmt, pNorm, presetInfo, presetList, pVal } from '../lib/presets';
import * as A from '../state/actions';
import { autoDisplay, isAutomated, paramMenu } from '../state/automation';
import { commit, Gesture, wheelEdit } from '../state/edit';
import { findFx, insName, plain, S } from '../state/store';
import { onFxFrame, tele } from '../state/telemetry';
import { openMenu, say, U, type PlugWin } from '../state/ui';
import { dblCheck, track } from '../ui/drag';
import { addFrameTask, fitCanvas, fonts } from '../ui/frameLoop';
import { FXMETA, FxTile } from '../ui/icons';
import { Knob, MiniKnob, Segmented, type KnobSize } from '../ui/Knob';
import { theme } from '../ui/theme';
import type { Viz } from './fx/common';
import { deessDb, DeessViz, deessFx, deessGeom, deessXf, deessY } from './fx/deess';
import { DenoiseViz } from './fx/denoise';
import { DynViz } from './fx/dyn';
import { EqViz } from './fx/eq';
import { EqStrips } from './fx/eqStrips';
import { EqPanel, eqHover, eqLeave, eqPointerDown, eqWheel } from './fx/eqEdit';
import { fetModes, FetViz } from './fx/fet';
import { defaultView, ReverbViz, views3d } from './fx/reverb';
import { DelayViz, SatViz } from './fx/satDelay';
import { TuneViz } from './fx/tune';
import { RiderViz } from './fx/rider';
import { halfHit, HalfViz, halfXf } from './fx/half';
import { onVoicePad, padParams, padToVoice, VoiceViz } from './fx/voice';


function makeViz(f: Fx): Viz | null {
  switch (f.type) {
    case 'eq': return new EqViz();
    case 'comp': case 'limit': return new DynViz();
    case 'sat': return new SatViz();
    case 'delay': return new DelayViz();
    case 'reverb': return new ReverbViz(f.id);
    case 'tune': return new TuneViz();
    case 'fet': return new FetViz(f.id);
    case 'deess': return new DeessViz();
    case 'denoise': return new DenoiseViz();
    case 'voice': return new VoiceViz();
    case 'rider': return new RiderViz();
    case 'half': return new HalfViz();
    default: return null;
  }
}
const VIZ_H: Partial<Record<Fx['type'], number>> = { eq: 300, reverb: 260, tune: 280, fet: 250, sat: 240, deess: 230, denoise: 240, voice: 250, rider: 220, half: 250 };
/** Default window widths per type (actions.openPlug). */
export const WIN_W: Partial<Record<Fx['type'], number>> = { eq: 780, reverb: 640, vst3: 520, tune: 800, fet: 700, deess: 680, denoise: 640, voice: 700, half: 760 };

const HINTS: Partial<Record<Fx['type'], TranslationKey>> = {
  eq: 'c.fx.hint.eq',
  reverb: 'c.fx.hint.reverb',
  deess: 'c.fx.hint.deess',
  fet: 'c.fx.hint.fet',
  denoise: 'c.fx.hint.denoise',
  tune: 'c.fx.hint.tune',
  half: 'c.fx.hint.half',
  voice: 'c.fx.hint.voice',
};

/** Move a fixed window by its header. */
export function moveWindow(e: PointerEvent, get: () => { x: number; y: number; w: number }, set: (x: number, y: number) => void) {
  if (e.button !== 0 || (e.target as HTMLElement).closest('button,select,input,.miniknob')) return;
  e.preventDefault();
  const sx = e.clientX, sy = e.clientY, o = { ...get() };
  track(ev => set(clamp(o.x + ev.clientX - sx, 80 - o.w, window.innerWidth - 80), clamp(o.y + ev.clientY - sy, 0, window.innerHeight - 40)), null, 'grabbing');
}

export function FxWindow(props: { pw: PlugWin; z: number }) {
  const ff = () => findFx(props.pw.fxId);
  const setPos = (x: number, y: number) => A.movePlug(props.pw.fxId, x, y);
  return (
    <Show when={ff()}>
      {got => {
        const fx = () => got().f, m = () => got().m;
        const isVst = () => fx().type === 'vst3';
        const info = () => presetInfo(S.catalog, fx(), S.project.bpm);
        return (
          <div class="win fxwin" style={{ left: props.pw.x + 'px', top: props.pw.y + 'px', width: props.pw.w + 'px', 'z-index': props.z }} data-fxwin={props.pw.fxId}
            onPointerDown={() => A.plugFront(props.pw.fxId)}>
            <div class="winhd" onPointerDown={e => moveWindow(e, () => props.pw, setPos)}>
              <button class="pwr" classList={{ off: fx().bypass }} title={fx().bypass ? t('c.fx.bypassOn') : t('c.fx.bypassOff')}
                onPointerDown={e => { e.stopPropagation(); A.toggleBypass(fx().id); }} />
              <FxTile type={fx().type} size={20} icon={13} />
              <span class="title">{A.fxName(fx())}</span>
              <Show when={fx().type === 'comp' || fx().type === 'fet'}>
                <Segmented options={[t('c.fx.compClean'), 'FET']} small value={() => (fx().type === 'fet' ? 1 : 0)} color={FXMETA[fx().type].color}
                  pick={i => A.swapFxType(fx().id, i ? 'fet' : 'comp')} title={t('c.fx.compTypeTip')} />
              </Show>
              <span style={{ width: '6px', height: '6px', flex: 'none', 'border-radius': '2px', background: m().color }} />
              <span class="sub">{insName(m())}</span>
              <Show when={fx().bypass}><span class="bypasstag">BYPASS</span></Show>
              <div style={{ flex: 1 }} />
              {/* effects with their own wet/dry knob don't need a second one in the header */}
              <Show when={!OWN_MIX.has(fx().type) || (fx().mix ?? 1) !== 1}><MixControl fx={fx()} /></Show>
              <Show when={!isVst() && presetList(S.catalog, fx().type).length}>
                <div class="presetbar">
                  <button class="arrow" title={t('c.fx.presetPrev')} onClick={() => A.stepPreset(fx().id, -1)}>‹</button>
                  <button class="name" title={t('c.fx.presets')} classList={{ mod: info().mod }} data-preset-name
                    onClick={e => { const r = (e.currentTarget as HTMLElement).getBoundingClientRect(); openPresetMenu(fx().id, r.left, r.bottom + 4); }}>
                    <span class="ell" style={{ flex: 1, 'min-width': 0 }}>{tName(info().name)}{info().mod ? ' *' : ''}</span>
                    <svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style={{ flex: 'none' }}><path d="M6 9l6 6 6-6" /></svg>
                  </button>
                  <button class="arrow" title={t('c.fx.presetNext')} onClick={() => A.stepPreset(fx().id, 1)}>›</button>
                </div>
              </Show>
              <button class="closebtn" title={t('c.fx.close')} onClick={() => A.closePlug(fx().id)}>×</button>
            </div>
            <Show when={!isVst()} fallback={<Vst3Body fx={fx()} />}>
              <Show when={fx().type === 'eq'} fallback={<VizCanvas fxId={props.pw.fxId} pw={props.pw} height={VIZ_H[fx().type] ?? 190} />}>
                {/* EQ: band bank beside the graph, like FL Parametric EQ 2 */}
                <div class="eqmain">
                  <div class="eqgraph"><VizCanvas fxId={props.pw.fxId} pw={props.pw} height={VIZ_H.eq!} /></div>
                  <EqStrips fx={fx()} />
                </div>
              </Show>
              <Body fx={fx()} />
            </Show>
          </div>
        );
      }}
    </Show>
  );
}

/** Dry/wet of the effect (fx.mix), automatable. */
const OWN_MIX = new Set<string>(['reverb', 'delay', 'sat', 'fet', 'half']);
function MixControl(props: { fx: Fx }) {
  const target = () => ({ kind: 'mix' as const, fxId: props.fx.id });
  const val = () => autoDisplay(target()) ?? props.fx.mix ?? 1;
  let g: Gesture | null = null;
  const op = (v: number) => A.fxOp({ ...plain(findFx(props.fx.id)!.f), mix: Math.round(clamp(v, 0, 1) * 1000) / 1000 });
  return (
    <div class="mixctl" title={t('c.fx.mixTip')}>
      <span class="lab">MIX</span>
      <MiniKnob key={'mix' + props.fx.id} title={t('c.fx.mixKnobTip')} t={val} automated={() => isAutomated(target())}
        onStart={() => { g = new Gesture('Mix'); }} onChange={t => { g?.update([op(t)]); say(A.fxName(props.fx) + ' · Mix ' + Math.round(t * 100) + '%'); }}
        onEnd={() => { g?.end(); g = null; }} onReset={() => commit([op(1)], 'Mix')}
        onWheel={t => wheelEdit('mix' + props.fx.id, 'Mix', [op(t)])}
        onMenu={(x, y) => paramMenu(x, y, target(), () => commit([op(1)], 'Mix'), FXMETA[props.fx.type].color)} />
      <span class="v">{Math.round(val() * 100)}%</span>
    </div>
  );
}

function openPresetMenu(fxId: Id, x: number, y: number) {
  const ff = findFx(fxId); if (!ff || !S.catalog) return;
  const info = presetInfo(S.catalog, ff.f, S.project.bpm), list = presetList(S.catalog, ff.f.type);
  openMenu(x, y, [{ t: 'title', label: t('c.fx.presetsOf', { name: A.fxName(ff.f) }) },
    ...list.map((pr, i) => ({ t: 'item' as const, label: tName(pr.name), kbd: i === info.idx ? (info.mod ? t('c.fx.modified') : '●') : '', act: () => A.loadPreset(fxId, i) }))], 40 + list.length * 26);
}

// ---- parameter controls ----
function setParam(fxId: Id, id: string, v: number) {
  const f = findFx(fxId)?.f; if (!f) return null;
  return A.fxOp({ ...plain(f), params: { ...plain(f.params), [id]: v } });
}
const defOf = (fx: Fx, id: string) => paramDefs(S.catalog, fx.type).find(p => p.id === id);

function ParamKnob(props: { fxId: Id; p: ParamDef; ends?: [string, string]; color?: string; size?: KnobSize }) {
  const fx = () => findFx(props.fxId)?.f;
  const target = () => ({ kind: 'fx' as const, fxId: props.fxId, param: props.p.id });
  const val = () => autoDisplay(target()) ?? fx()?.params[props.p.id] ?? props.p.def;
  const p = props.p;
  let g: Gesture | null = null;
  const reset = () => { const op = setParam(props.fxId, p.id, p.def); if (op) commit([op], tName(p.label)); };
  return (
    <Knob key={props.fxId + p.id} label={tName(p.label)} t={() => clamp(pNorm(p, val()), 0, 1)} ends={props.ends} color={props.color} size={props.size}
      automated={() => isAutomated(target())}
      zero={() => (p.min < 0 && p.max > 0 && !p.log ? pNorm(p, 0) : 0)} text={() => pFmt(p, val())}
      onStart={() => { g = new Gesture(tName(p.label)); }}
      onChange={t => { const v = pVal(p, t); const op = setParam(props.fxId, p.id, v); if (op) g?.update([op]); say(A.fxName(fx()!) + ' · ' + tName(p.label) + ' ' + pFmt(p, v)); }}
      onEnd={() => { g?.end(); g = null; }}
      onReset={reset}
      onMenu={(x, y) => paramMenu(x, y, target(), reset, props.color)}
      onWheel={t => { const v = pVal(p, t); const op = setParam(props.fxId, p.id, v); if (op) wheelEdit('fx' + props.fxId + p.id, tName(p.label), [op]); say(A.fxName(fx()!) + ' · ' + tName(p.label) + ' ' + pFmt(p, v)); }} />
  );
}

/** A choice parameter as segmented buttons (one click = one undo step). */
function ParamChoice(props: { fxId: Id; p: ParamDef; danger?: number; color?: string; labels?: string[]; small?: boolean; hideLabel?: boolean }) {
  const fx = () => findFx(props.fxId)?.f;
  const target = () => ({ kind: 'fx' as const, fxId: props.fxId, param: props.p.id });
  const val = () => autoDisplay(target()) ?? fx()?.params[props.p.id] ?? props.p.def;
  const pick = (i: number) => {
    if (Math.round(val()) === i) return;
    const op = setParam(props.fxId, props.p.id, i); if (!op) return;
    commit([op], tName(props.p.label));
    say(A.fxName(fx()!) + ' · ' + tName(props.p.label) + ': ' + tName(props.p.options![i]));
  };
  return <Segmented label={props.hideLabel ? undefined : tName(props.p.label)} options={props.labels ?? props.p.options!.map(tName)} value={val} pick={pick} danger={props.danger} color={props.color} small={props.small}
    onMenu={(x, y) => paramMenu(x, y, target(), () => pick(props.p.def), props.color)} />;
}

/** One catalog parameter, as a knob (size tier) or segmented buttons — tinted with the effect's colour. */
function Ctl(props: { fx: Fx; id: string; ends?: [string, string]; danger?: number; labels?: string[]; small?: boolean; hideLabel?: boolean; size?: KnobSize }) {
  const d = () => defOf(props.fx, props.id);
  const color = () => FXMETA[props.fx.type].color;
  return (
    <Show when={d()}>
      {p => p().options
        ? <ParamChoice fxId={props.fx.id} p={p()} danger={props.danger} color={color()} labels={props.labels} small={props.small} hideLabel={props.hideLabel} />
        : <ParamKnob fxId={props.fx.id} p={p()} ends={props.ends} size={props.size} color={props.size === 'lg' ? color() : undefined} />}
    </Show>
  );
}

function Hint(props: { fx: Fx }) { return <span class="hint">{HINTS[props.fx.type] ? t(HINTS[props.fx.type]!) : ''}</span>; }
/** Layout entries starting with `c.` are translation keys, anything else ("C3", "2×") is shown as is. */
const L = (s: string) => (s.startsWith('c.') ? t(s as TranslationKey) : s);

// ---- shared layout system: every built-in effect is described as titled sections of controls.
// Size tiers: `lg` for the one or two controls that define the effect, `md` standard, `sm` trims.
// Choices render as segmented buttons; `stack` puts items in a column; `x` is a custom widget.
type Item = { id: string; size?: KnobSize; ends?: [string, string] | ((fx: Fx) => [string, string] | undefined); labels?: string[]; danger?: number; small?: boolean }
  | { stack: Item[] } | { x: 'fetMeter' };
/** Titles, labels and ends: keys starting with `c.` (see `L`). `adv`: shown only after the window's "Advanced" toggle (keeps the first view simple). */
type Section = { title: string; items: Item[]; adv?: boolean };
const [advOpen, setAdvOpen] = createSignal<Record<number, boolean>>({});
const LAYOUT: Partial<Record<FxKind, Section[]>> = {
  comp: [
    { title: 'c.fx.sec.compression', items: [{ id: 'thr', size: 'lg' }, { id: 'ratio', size: 'lg' }] },
    { title: 'c.fx.sec.time', items: [{ id: 'att' }, { id: 'rel' }] },
    { title: 'c.fx.sec.output', items: [{ id: 'gain' }] },
  ],
  fet: [
    { title: 'c.fx.sec.input', items: [{ id: 'input', size: 'lg' }] },
    { title: 'c.fx.sec.character', items: [{ stack: [{ id: 'ratio', danger: 4 }, { x: 'fetMeter' }] }] },
    { title: 'c.fx.sec.time', items: [{ id: 'attack' }, { id: 'release' }] },
    { title: 'c.fx.sec.output', items: [{ id: 'output', size: 'lg' }, { id: 'mix', size: 'sm' }] },
  ],
  limit: [
    { title: 'c.fx.sec.volume', items: [{ id: 'gain', size: 'lg' }] },
    { title: 'c.fx.sec.ceiling', items: [{ id: 'ceil' }] },
    { title: 'c.fx.sec.time', adv: true, items: [{ id: 'rel' }, { stack: [{ id: 'auto', small: true }] }, { id: 'look', size: 'sm' }] },
    { title: 'c.fx.sec.output', adv: true, items: [{ id: 'link', size: 'sm' }, { id: 'out', size: 'sm' }] },
  ],
  sat: [
    { title: 'c.fx.sec.character', items: [{ stack: [{ id: 'style', small: true }] }] },
    { title: 'c.fx.sec.drive', items: [{ id: 'drive', size: 'lg' }, { stack: [{ id: 'punish', labels: ['c.fx.lab.normal', 'Punish +20'], small: true, danger: 1 }] }] },
    { title: 'c.fx.sec.tone', items: [{ id: 'lowcut' }, { id: 'thump', size: 'sm' }, { id: 'tone', ends: ['c.fx.end.dark', 'c.fx.end.bright'] }, { id: 'highcut' }, { stack: [{ id: 'steep', labels: ['c.fx.lab.gentle', 'c.fx.lab.steep'], small: true }] }] },
    { title: 'c.fx.sec.output', items: [{ stack: [{ id: 'auto', labels: ['c.fx.lab.manual', 'Auto'], small: true }] }, { id: 'mix' }, { id: 'out' }] },
    { title: 'c.fx.sec.quality', adv: true, items: [{ stack: [{ id: 'hq', labels: ['2×', '4× HQ'], small: true }] }] },
  ],
  delay: [
    { title: 'c.fx.sec.echo', items: [{ id: 'time', size: 'lg' }, { id: 'fb' }] },
    { title: 'c.fx.sec.output', items: [{ id: 'mix' }] },
  ],
  reverb: [
    { title: 'c.fx.sec.space', items: [{ stack: [{ id: 'space', small: true }] }, { id: 'size', size: 'lg' }, { id: 'pre' }] },
    { title: 'c.fx.sec.tone', items: [{ id: 'tone', ends: ['c.fx.end.dark', 'c.fx.end.bright'] }] },
    { title: 'c.fx.sec.amount', items: [{ id: 'mix', size: 'lg' }] },
    { title: 'c.fx.sec.filters', adv: true, items: [{ id: 'lo', size: 'sm' }, { id: 'hi', size: 'sm' }, { id: 'bass', size: 'sm' }] },
    { title: 'c.fx.sec.character', adv: true, items: [{ id: 'early', size: 'sm' }, { id: 'width', size: 'sm' }, { id: 'mod', size: 'sm' }, { id: 'duck', size: 'sm' }] },
  ],
  rider: [
    { title: 'c.fx.sec.leveling', items: [{ id: 'amount', size: 'lg' }, { id: 'phrase' }, { stack: [{ id: 'speed', labels: ['c.fx.lab.sung', 'Rap'], small: true }] }] },
    { title: 'c.fx.sec.range', items: [{ id: 'up' }, { id: 'down' }] },
    { title: 'c.fx.sec.lookahead', items: [{ stack: [{ id: 'look', labels: ['c.fx.lab.lookShort', 'c.fx.lab.lookLong', 'c.fx.lab.live'], small: true }] }] },
    { title: 'c.fx.sec.target', adv: true, items: [{ stack: [{ id: 'auto', labels: ['c.fx.lab.manualAlt', 'Auto'], small: true }] }, { id: 'target', size: 'sm' }, { id: 'floor', size: 'sm' }, { id: 'gate', size: 'sm' }] },
    { title: 'c.fx.sec.output', adv: true, items: [{ id: 'out', size: 'sm' }] },
  ],
  deess: [
    { title: 'c.fx.sec.detection', items: [{ id: 'thr', size: 'lg' }, { id: 'freq' }, { stack: [{ id: 'mode', small: true }] }] },
    { title: 'c.fx.sec.reduction', items: [{ id: 'range' }, { id: 'look', size: 'sm' }] },
    { title: 'c.fx.sec.listen', items: [{ stack: [{ id: 'listen', labels: ['Off', 'c.fx.lab.onlyS'], small: true }] }] },
  ],
  denoise: [
    { title: 'c.fx.sec.reduction', items: [{ id: 'reduction', size: 'lg' }, { id: 'sens' }, { id: 'smooth' }] },
    { title: 'c.fx.sec.noiseProfile', items: [{ stack: [{ id: 'learn', small: true }, { id: 'listen', small: true }] }] },
  ],
  voice: [
    { title: 'c.fx.sec.mode', items: [{ stack: [{ id: 'mode', small: true }, { id: 'link', small: true }] }] },
    { title: 'c.fx.sec.voice', items: [{ id: 'pitch', size: 'lg', ends: fx => (fx.params.mode === 2 ? ['C3', 'C5'] : undefined) }, { id: 'formant', size: 'lg', ends: ['c.fx.end.deeper', 'c.fx.end.brighter'] }] },
    { title: 'c.fx.sec.output', items: [{ id: 'drive' }, { id: 'out' }] },
  ],
  tune: [
    { title: 'c.fx.sec.correction', items: [{ id: 'retune', size: 'lg', ends: ['robot', 'natural'] }, { id: 'humanize' }, { id: 'flex' }, { id: 'vibrato' }] },
    { title: 'c.fx.sec.shift', items: [{ id: 'transpose', size: 'sm' }, { id: 'formant', size: 'sm' }] },
  ],
  eq: [{ title: 'c.fx.sec.output', items: [{ id: 'out', size: 'sm' }] }],
  half: [
    { title: 'c.fx.sec.slowdown', items: [{ stack: [{ id: 'speed', labels: ['c.fx.lab.halfFifth', 'c.fx.lab.halfOctave', 'c.fx.lab.halfTwoOct'], small: true }, { id: 'loop', small: true }] }] },
    { title: 'c.fx.sec.transitions', items: [{ id: 'smooth', size: 'lg', ends: ['c.fx.end.percussive', 'c.fx.end.swell'] }] },
    { title: 'c.fx.sec.band', items: [{ id: 'lo', size: 'sm' }, { id: 'hi', size: 'sm' }, { stack: [{ id: 'solo', labels: ['c.fx.lab.fullBand', 'c.fx.lab.bandSolo'], small: true }] }] },
    { title: 'c.fx.sec.output', items: [{ id: 'mix' }] },
  ],
};

function LayoutItem(props: { fx: Fx; it: Item }) {
  const it = props.it;
  if ('stack' in it) return <div class="stack">{it.stack.map(x => <LayoutItem fx={props.fx} it={x} />)}</div>;
  if ('x' in it) {
    const mode = () => fetModes.get(props.fx.id);
    return <Segmented label={t('c.fx.meter')} small options={['GR', 'OUT']} value={() => (mode() === 'gr' ? 0 : 1)} pick={i => fetModes.set(props.fx.id, i ? 'out' : 'gr')} title={t('c.fx.meterTip')} />;
  }
  const ends = () => { const e = typeof it.ends === 'function' ? it.ends(props.fx) : it.ends; return e ? [L(e[0]), L(e[1])] as [string, string] : undefined; };
  return <Ctl fx={props.fx} id={it.id} size={it.size} ends={ends()} labels={it.labels?.map(L)} danger={it.danger} small={it.small} />;
}

/** Sections of an effect's controls; params the layout doesn't place go to an "Other" section. */
function Sections(props: { fx: Fx }) {
  const secs = () => {
    const lay = LAYOUT[props.fx.type];
    const defs = paramDefs(S.catalog, props.fx.type);
    if (!lay) return [{ title: 'c.fx.sec.params', items: defs.map(p => ({ id: p.id })) }] as Section[];
    const placed = new Set<string>();
    const walk = (xs: Item[]) => xs.forEach(x => ('stack' in x ? walk(x.stack) : 'id' in x ? placed.add(x.id) : null));
    lay.forEach(s => walk(s.items));
    const known = new Set(defs.map(p => p.id));
    const rest = defs.filter(p => !placed.has(p.id) && !(props.fx.type === 'tune' && ['key', 'scale', 'voice'].includes(p.id)));
    // drop layout items the engine does not define (older catalog), then sections left empty
    const keep = (xs: Item[]): Item[] => xs.flatMap((x): Item[] => {
      if ('stack' in x) { const k = keep(x.stack); return k.length ? [{ stack: k }] : []; }
      return 'id' in x && !known.has(x.id) ? [] : [x];
    });
    const all = lay.map(s => ({ ...s, items: keep(s.items) })).filter(s => s.items.length);
    return rest.length ? [...all, { title: 'c.fx.sec.other', adv: true, items: rest.map(p => ({ id: p.id })) }] : all;
  };
  const hasAdv = () => secs().some(s => s.adv);
  const adv = () => !!advOpen()[props.fx.id];
  const shown = () => secs().filter(s => !s.adv || adv());
  return (
    <div class="fxsecs" style={{ '--fxc': FXMETA[props.fx.type].color }}>
      <For each={shown()}>
        {(sec, i) => (
          <>
            <Show when={i() > 0}><div class="vline" /></Show>
            <div class="fxsec">
              <div class="fxsec-h">{L(sec.title)}</div>
              <div class="fxsec-b"><For each={sec.items}>{it => <LayoutItem fx={props.fx} it={it} />}</For></div>
            </div>
          </>
        )}
      </For>
      <div style={{ flex: 1 }} />
      <Show when={hasAdv()}>
        <button class="smallbtn advbtn" classList={{ on: adv() }} title={t('c.fx.advTip')}
          onClick={() => setAdvOpen(m => ({ ...m, [props.fx.id]: !adv() }))}>{adv() ? t('c.fx.less') : t('c.fx.advanced')}</button>
      </Show>
      <Hint fx={props.fx} />
    </div>
  );
}

function Body(props: { fx: Fx }): JSX.Element {
  const t = () => props.fx.type;
  return (
    <>
      <Show when={t() === 'eq'}><EqPanel fx={props.fx} /></Show>
      <Show when={t() === 'tune'}><TuneHeader fx={props.fx} /></Show>
      <Show when={t() !== 'eq'}><Sections fx={props.fx} /></Show>
    </>
  );
}

/** Pitch Correct: key / scale / voice / auto-key row above the knobs. */
function TuneHeader(props: { fx: Fx }) {
  const color = FXMETA.tune.color;
  return (
    <div class="ctlrow">
      <Show when={defOf(props.fx, 'key')}>{p => <ParamChoice fxId={props.fx.id} p={p()} color={color} labels={NOTE_NAMES} small />}</Show>
      <Show when={defOf(props.fx, 'scale')}>{p => <ParamChoice fxId={props.fx.id} p={p()} color={color} small />}</Show>
      <Show when={defOf(props.fx, 'voice')}>{p => <ParamChoice fxId={props.fx.id} p={p()} color={color} labels={[t('c.fx.voice.auto'), t('c.fx.voice.bass'), t('c.fx.voice.male'), t('c.fx.voice.female'), t('c.fx.voice.soprano')]} small />}</Show>
      <div class="segp">
        <span class="lab">AUTO-KEY</span>
        <button class="smallbtn" style={{ height: '24px', padding: '0 10px' }} data-detect-key
          title={t('c.fx.detectKeyTip')}
          onClick={() => A.openMusicAnalysis({ fxId: props.fx.id })}>
          {t('c.fx.detectKey')}
        </button>
      </div>
    </div>
  );
}

/** The window's canvas: one Viz drawn from the shared frame loop, only when something changed. */
function VizCanvas(props: { fxId: Id; pw: PlugWin; height: number }) {
  let cv!: HTMLCanvasElement;
  const ff = () => findFx(props.fxId);
  const viz = makeViz(ff()!.f);
  const height = createMemo(() => props.height);
  const pw = createMemo(() => props.pw);
  let version = 0, drawnVersion = -1, lastW = 0, fontEpoch = -1, themeEpoch = -1;
  createEffect(() => { // anything param-ish changes → redraw
    const f = ff()?.f; if (!f) return;
    f.bypass; f.mix; for (const k in f.params) void f.params[k];
    f.bands?.forEach(b => { b.on; b.type; b.f; b.g; b.q; b.slope; b.place; b.dynamic?.range; b.dynamic?.thr; });
    f.bands?.length;
    U.eqSel[props.fxId]; U.fetMode[props.fxId]; S.project.bpm; lang();
    version++;
  });
  onMount(() => {
    const off = viz?.ingest ? onFxFrame(props.fxId, (a, at) => viz.ingest!(a, at)) : null;
    onCleanup(() => off?.());
    const stop = addFrameTask(now => {
      const f = ff()?.f; if (!f || !viz) return;
      const w = pw();
      if (w.x > window.innerWidth || w.y > window.innerHeight || w.x + w.w < 0 || document.hidden) return;
      const raw = tele.fx.get(props.fxId) ?? null;
      const a = raw && tele.frame.seq - raw.seq < 30 ? raw : null;
      const changed = viz.update(f, a, now);
      const W = cv.clientWidth, H = height();
      if (!W) return;
      if (!changed && version === drawnVersion && W === lastW && fontEpoch === fonts.epoch && themeEpoch === theme.epoch) return;
      drawnVersion = version; lastW = W; fontEpoch = fonts.epoch; themeEpoch = theme.epoch;
      fitCanvas(cv, W, H);
      const g = cv.getContext('2d')!;
      const dpr = window.devicePixelRatio || 1;
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      viz.draw(g, W, H, f, a, { selBand: U.eqSel[props.fxId] ?? null, fxId: props.fxId, now });
    });
    onCleanup(stop);
  });

  const rel = (e: PointerEvent | WheelEvent) => { const r = cv.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top, W: r.width, H: r.height }; };
  const down = (e: PointerEvent) => {
    const got = ff(); if (!got || !viz) return;
    if (got.f.type === 'eq') eqPointerDown(e, cv, props.fxId, viz as EqViz);
    else if (got.f.type === 'reverb') verbDown(e);
    else if (got.f.type === 'deess') deessDown(e, got.f);
    else if (got.f.type === 'voice') voiceDown(e);
    else if (got.f.type === 'half') halfDown(e, got.f);
    else if (got.f.type === 'fet' && e.button === 0) {
      const m = fetModes.get(props.fxId) === 'out' ? 'gr' : 'out';
      fetModes.set(props.fxId, m); version++; say(m === 'gr' ? t('c.fx.meterGr') : t('c.fx.meterOut'));
    }
  };
  const move = (e: PointerEvent) => {
    const got = ff(); if (!got || !viz || e.buttons) return;
    if (got.f.type === 'eq') eqHover(e, cv, props.fxId, viz as EqViz);
    else if (got.f.type === 'deess') {
      const d = viz as DeessViz, { x, y, W, H } = rel(e), G = deessGeom(W, H);
      const onThr = x < G.hx + G.hw && Math.abs(y - deessY(got.f.params.thr ?? -32, H)) < 6;
      const onF = x >= G.sx && Math.abs(x - deessFx(got.f.params.freq ?? 6000, W, H)) < 8;
      if (onThr !== d.hoverThr || onF !== d.hoverFreq) { d.hoverThr = onThr; d.hoverFreq = onF; version++; }
      cv.style.cursor = onThr ? 'ns-resize' : onF ? 'ew-resize' : 'default';
    } else if (got.f.type === 'half') {
      const v = viz as HalfViz, { x, y, W, H } = rel(e), on = halfHit(got.f, x, y, W, H);
      if (on !== v.hover) { v.hover = on; version++; }
      cv.style.cursor = on ? 'ew-resize' : 'default';
    } else if (got.f.type === 'voice') {
      const v = viz as VoiceViz, { x, y, W, H } = rel(e), on = onVoicePad(x, y, W, H);
      if (on !== v.hover) { v.hover = on; version++; }
      cv.style.cursor = on ? 'crosshair' : 'default';
    }
  };
  const leave = () => {
    const got = ff(); if (!got || !viz) return;
    if (got.f.type === 'eq') eqLeave(viz as EqViz);
    if (got.f.type === 'deess') { const d = viz as DeessViz; d.hoverThr = d.hoverFreq = false; version++; }
    if (got.f.type === 'voice') { (viz as VoiceViz).hover = false; version++; }
    if (got.f.type === 'half') { (viz as HalfViz).hover = null; version++; }
  };
  /** Pad: click / drag sets Pitch and Formant together (one undo step), double click resets both. */
  const voiceDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    const { x, y, W, H } = rel(e);
    if (!onVoicePad(x, y, W, H)) return;
    e.preventDefault();
    const put = (pitch: number, formant: number) => {
      const f = findFx(props.fxId)?.f; if (!f) return null;
      return A.fxOp({ ...plain(f), params: { ...plain(f.params), pitch, formant } });
    };
    const tell = () => { const p = findFx(props.fxId)?.f.params; if (p) say('Voice · Pitch ' + (p.pitch ?? 0).toFixed(1) + ' st · Formant ' + (p.formant ?? 0).toFixed(1) + ' st'); };
    if (dblCheck('voice' + props.fxId)) { const op = put(0, 0); if (op) commit([op], 'Pitch / Formant'); tell(); return; }
    const g = new Gesture('Pitch / Formant');
    const r = cv.getBoundingClientRect();
    const at = (cx: number, cy: number) => {
      const f = findFx(props.fxId)?.f; if (!f) return;
      const v = padToVoice(cx - r.left, cy - r.top, r.width, r.height), q = padParams(f, v.pitch, v.formant);
      const op = put(q.pitch, q.formant); if (op) g.update([op]);
      tell();
    };
    at(e.clientX, e.clientY);
    track(ev => at(ev.clientX, ev.clientY), () => g.end(), 'grabbing');
  };
  /** Half-Speed band strip: drag an edge (double-click = that edge back to off). */
  const halfDown = (e: PointerEvent, f: Fx) => {
    if (e.button !== 0) return;
    const { x, y, W, H } = rel(e), which = halfHit(f, x, y, W, H); if (!which) return;
    const pd = defOf(f, which); if (!pd) return;
    e.preventDefault();
    const name = t(which === 'lo' ? 'c.fx.half.edgeLo' : 'c.fx.half.edgeHi');
    if (dblCheck('half' + which + props.fxId)) { const op = setParam(props.fxId, which, pd.def); if (op) commit([op], name); return; }
    const g = new Gesture(name);
    const r = cv.getBoundingClientRect();
    const at = (cx: number) => {
      const cur = findFx(props.fxId)?.f; if (!cur) return;
      let v = halfXf(cx - r.left, r.width);
      // keep the edges apart (≥ 1/6 octave)
      if (which === 'lo') v = Math.min(v, (cur.params.hi ?? 20000) / 1.12); else v = Math.max(v, (cur.params.lo ?? 20) * 1.12);
      const op = setParam(props.fxId, which, Math.round(clamp(v, pd.min, pd.max))); if (op) g.update([op]);
      const nv = findFx(props.fxId)?.f.params[which] ?? v;
      say(name + ': ' + (nv >= 1000 ? (nv / 1000).toFixed(2) + ' kHz' : Math.round(nv) + ' Hz'));
    };
    at(e.clientX);
    track(ev => at(ev.clientX), () => g.end(), 'ew-resize');
  };
  const deessDown = (e: PointerEvent, f: Fx) => {
    if (e.button !== 0) return;
    const { x, y, W, H } = rel(e), G = deessGeom(W, H);
    const thrP = defOf(f, 'thr'), frP = defOf(f, 'freq'); if (!thrP || !frP) return;
    const onThr = x < G.hx + G.hw && Math.abs(y - deessY(f.params.thr ?? -32, H)) < 6;
    const onF = x >= G.sx - 4;
    if (!onThr && !onF) return;
    e.preventDefault();
    if (onThr && dblCheck('thr' + props.fxId)) { const op = setParam(props.fxId, 'thr', thrP.def); if (op) commit([op], t('c.fx.thr')); return; }
    const g = new Gesture(onThr ? t('c.fx.thr') : t('c.fx.freq'));
    const r = cv.getBoundingClientRect();
    track(ev => {
      const op = onThr
        ? setParam(props.fxId, 'thr', Math.round(clamp(deessDb(ev.clientY - r.top, H), thrP.min, thrP.max)))
        : setParam(props.fxId, 'freq', Math.round(clamp(deessXf(ev.clientX - r.left, W, H), frP.min, frP.max)));
      if (op) g.update([op]);
      const nf = findFx(props.fxId)?.f;
      say(onThr ? t('c.fx.sayThr', { v: Math.round(nf?.params.thr ?? 0) }) : t('c.fx.sayBand', { v: Math.round(nf?.params.freq ?? 0) }));
    }, () => g.end(), onThr ? 'ns-resize' : 'ew-resize');
    if (onF && !onThr) { const op = setParam(props.fxId, 'freq', Math.round(clamp(deessXf(x, W, H), frP.min, frP.max))); if (op) g.update([op]); }
  };
  const verbDown = (e: PointerEvent) => {
    if (e.button !== 0) return; e.preventDefault();
    let v = views3d.get(props.fxId); if (!v) { v = defaultView(); views3d.set(props.fxId, v); }
    const bump = () => { (viz as ReverbViz).viewVersion++; };
    if (dblCheck('verb' + props.fxId)) { Object.assign(v, defaultView()); bump(); say(t('c.fx.view3dReset')); return; }
    let lx = e.clientX, ly = e.clientY;
    track(ev => { v!.yaw = clamp(v!.yaw + (ev.clientX - lx) * 0.008, -1.2, 1.2); v!.pitch = clamp(v!.pitch + (ev.clientY - ly) * 0.006, 0.1, 1.3); lx = ev.clientX; ly = ev.clientY; bump(); }, null, 'grabbing');
  };
  const wheel = (e: WheelEvent) => {
    const got = ff(); if (!got || !viz) return;
    if (got.f.type === 'eq') eqWheel(e, props.fxId, viz as EqViz);
  };
  return <canvas ref={cv} class="vizcv" data-viz={props.fxId} style={{ height: props.height + 'px', cursor: viz?.cursor ?? 'default' }}
    onPointerDown={down} onPointerMove={move} onPointerLeave={leave} onWheel={wheel} onContextMenu={e => e.preventDefault()} />;
}

function Vst3Body(props: { fx: Fx }) {
  const [params, setParams] = createSignal<Vst3Param[]>([]);
  const [err, setErr] = createSignal<string | null>(null);
  const [guiOpen, setGuiOpen] = createSignal(false);
  // plugin editor windows exist on Windows only; elsewhere this list is the plugin's UI
  const [editors, setEditors] = createSignal(true);
  const [q, setQ] = createSignal('');
  let refresh = 0;
  const load = () => be().vst3Params(props.fx.id).then(p => { setParams(p); setErr(null); }).catch(e => setErr(String(e)));
  onMount(() => { load(); be().appInfo().then(i => setEditors(i.pluginEditors)).catch(() => {}); });
  onCleanup(() => { if (guiOpen()) void be().vst3Editor(props.fx.id, false).catch(() => {}); });
  const set = (id: number, v: number) => {
    setParams(ps => ps.map(p => p.id === id ? { ...p, value: v } : p));
    void be().vst3SetParam(props.fx.id, id, v).catch(e => say('VST3: ' + String(e)));
    clearTimeout(refresh); refresh = window.setTimeout(load, 200);
  };
  const shown = () => { const s = q().toLowerCase(); return params().filter(p => !s || p.name.toLowerCase().includes(s)); };
  return (
    <div style={{ padding: '12px 14px', display: 'flex', 'flex-direction': 'column', gap: '10px' }}>
      <div class="row" style={{ gap: '8px' }}>
        <Show when={editors()} fallback={<span class="note" style={{ flex: 'none' }} title={t('c.fx.guiNone')}>{t('c.fx.guiNoneShort')}</span>}>
          <button class="smallbtn primary" style={{ height: '26px', padding: '0 12px' }} onClick={() => {
            const open = !guiOpen();
            be().vst3Editor(props.fx.id, open).then(() => setGuiOpen(open)).catch(e => say(t('c.fx.guiFail', { e: String(e) })));
          }}>{guiOpen() ? t('c.fx.guiClose') : t('c.fx.guiOpen')}</button>
        </Show>
        <span class="note ell" style={{ flex: 1 }}>{props.fx.plugin?.vendor} · {props.fx.plugin?.path}</span>
      </div>
      <input class="textin" placeholder={t('c.fx.searchParam')} value={q()} onInput={e => setQ(e.currentTarget.value)} onKeyDown={e => e.stopPropagation()} />
      <Show when={err()}><span class="note err">{err()}</span></Show>
      <div style={{ 'max-height': '320px', overflow: 'auto', 'padding-right': '4px' }}>
        <For each={shown()} fallback={<span class="note">{t('c.fx.noParams')}</span>}>
          {p => {
            const target: AutoTarget = { kind: 'plugin', fxId: props.fx.id, paramId: p.id };
            return (
              <div class="vparam" onContextMenu={e => { e.preventDefault(); e.stopPropagation();
                paramMenu(e.clientX, e.clientY, target, () => set(p.id, p.def), undefined, { current: p.value, paramLabel: p.name });
              }}>
                <span class="ell" style={{ font: '12px var(--sans)', color: 'var(--fg)' }} title={p.name}>{p.name}</span>
                <input type="range" min="0" max="1" step="0.001" value={p.value} onInput={e => set(p.id, +e.currentTarget.value)}
                  onDblClick={() => set(p.id, p.def)} />
                <span class="mono ell" style={{ 'font-size': '10px', color: 'var(--fg2)', 'text-align': 'right' }} title={isAutomated(target) ? t('d.ui.knob.auto') : p.display}>
                  <Show when={isAutomated(target)}>● </Show>{p.display}
                </span>
              </div>
            );
          }}
        </For>
      </div>
    </div>
  );
}
