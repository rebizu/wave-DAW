import { createEffect, createMemo, createSignal, For, Match, onCleanup, onMount, Show, Switch } from 'solid-js';
import { createStore } from 'solid-js/store';
import { be } from '../api/backend';
import type { Channel, ClipKind, Id, Kit, LibFile, LibraryProgress, Pattern, Source } from '../api/types';
import { clamp, fmtBytes, baseName } from '../lib/format';
import { t } from '../lib/i18n';
import { rackSampleDrop } from '../lib/rack';
import { snapFloor } from '../lib/snap';
import * as A from '../state/actions';
import { getPeaks, peakRange, peaksVersion } from '../state/peaks';
import { S } from '../state/store';
import { openMenu, say, setU, U } from '../state/ui';
import { dblCheck, track } from '../ui/drag';
import { fitCanvas } from '../ui/frameLoop';
import { dropClipAt, gridPoint } from './Playlist';
import { patternPreview } from './playlistDraw';
import { addSampler, addSamplerFiles, chanById, channels, newPattern, openPattern, replaceWithFile, replaceWithSource, showPatternWindow, type SampleFile } from '../state/pattern';
import { libGroup, STD_KIT } from '../state/library';
import { browserSources, FACTORY_FILES } from '../lib/factory';
import { flatten, groupFiles, groupOpen, GHEAD_H, matches, ROW_H, searchTerms, stepIndex, windowRange, type FlatItem } from '../lib/libraryView';
import { noteName } from '../lib/pitch';
import { libFileGlyph, libGroupGlyph, type MenuGlyph } from '../ui/menuIcons';
import { patternItemMenu } from './patternShared';
import { guessIcon, InstIcon, patternIcon } from '../ui/instIcons';

/**
 * Drag something from the browser with a ghost label; `onDrop` gets the release point. `over`
 * names what a drop at a point would do (the ghost shows it instead of the name), or null.
 */
function dragGhost(e: PointerEvent, name: string, color: string, onClick: () => void, onDrop: (x: number, y: number, alt: boolean) => void, over?: (x: number, y: number) => string | null) {
  const sx = e.clientX, sy = e.clientY; let drag = false;
  track(ev => {
    if (!drag && Math.hypot(ev.clientX - sx, ev.clientY - sy) > 4) drag = true;
    if (drag) setU('ghost', { x: ev.clientX + 12, y: ev.clientY + 8, name: over?.(ev.clientX, ev.clientY) ?? name, color });
  }, ev => {
    if (over) markRow();
    setU('ghost', null);
    if (ev.type !== 'pointerup') return;
    if (!drag) return onClick();
    onDrop(ev.clientX, ev.clientY, ev.altKey);
  }, 'grabbing');
}

/**
 * Press on a Project-tab item: it becomes the playlist brush; a click (no drag) runs `onClick`.
 * Dragged, a pattern lands in the Playlist; audio lands there or, over the Channel Rack, becomes a sampler.
 */
function pick(kind: ClipKind, ref: Id, name: string, color: string, e: PointerEvent, onClick: () => void = () => {}) {
  if (e.button !== 0) return;
  e.preventDefault();
  setU('cur', { kind, ref, clipId: null });
  say(t('d.br.picked', { name }));
  const src = kind === 'audio' ? S.project.sources.find(s => s.id === ref) : undefined;
  if (src) return dragGhost(e, name, color, onClick, (x, y, alt) => dropSource(src, x, y, alt), sampleOver);
  dragGhost(e, name, color, onClick, (x, y, alt) => { dropClipAt(x, y, kind, ref, alt); });
}

/** The preview key of a project source (library previews use the file path). */
const srcKey = (id: Id) => 'src:' + id;
/** Double-click on a project source: hear it alone; again while it plays stops it. */
function previewSource(s: Source) {
  if (U.previewing && U.previewPath === srcKey(s.id)) { void be().previewStop(); setU('previewPath', null); return; }
  setU('previewPath', srcKey(s.id));
  be().previewSource(s.id).catch(e => { setU('previewPath', null); say(t('d.br.preverr', { err: String(e) })); });
  say(t('d.br.prev', { name: s.name }));
}

export async function importFiles(paths: string[], at?: { track: number; beat: number }) {
  if (!paths.length) return;
  try {
    const r = await be().importAudio({ paths, track: at?.track, beat: at?.beat });
    if (r.sourceIds.length) setU('cur', { kind: 'audio', ref: r.sourceIds[r.sourceIds.length - 1], clipId: null });
    if (r.clipIds.length) setU('sel', r.clipIds);
    say(t('d.br.loaded', { names: paths.map(baseName).join(', ') }));
  } catch (err) { say(t('d.br.loadfail', { err: String(err) })); }
}

/** Import at a client point if it is over the playlist grid. */
export function importAt(paths: string[], x: number, y: number, alt = false) {
  const p = gridPoint(x, y);
  // a MIDI file opens the import dialog (its pattern goes where it was dropped); audio is loaded
  const midi = paths.find(f => /\.midi?$/i.test(f));
  if (midi) {
    void A.midiImport({ path: midi, ...(p.inside && p.track >= 0 ? { track: p.track, beat: Math.max(0, snapFloor(p.beat, A.snapUnit(alt))) } : {}) });
    paths = paths.filter(f => !/\.midi?$/i.test(f));
    if (!paths.length) return;
  }
  if (!p.inside || p.track < 0) return importFiles(paths);
  return importFiles(paths, { track: p.track, beat: Math.max(0, snapFloor(p.beat, A.snapUnit(alt))) });
}

function Thumb(props: { src: Source }) {
  let cv!: HTMLCanvasElement;
  createEffect(() => {
    peaksVersion();
    const pk = getPeaks(props.src.id); if (!pk || !cv) return;
    const W = cv.clientWidth || 180, H = 14, dpr = window.devicePixelRatio || 1;
    fitCanvas(cv, W, H);
    const g = cv.getContext('2d')!; g.setTransform(dpr, 0, 0, dpr, 0, 0); g.clearRect(0, 0, W, H);
    const bins = pk.length / 2, mid = H / 2;
    g.globalAlpha = 0.7; g.fillStyle = props.src.color; g.beginPath();
    for (let x = 0; x < W; x++) {
      const a = Math.floor(x / W * bins), b = Math.max(a + 1, Math.floor((x + 1) / W * bins));
      const [mn, mx] = peakRange(pk, a, b);
      g.rect(x, mid - mx * mid, 1, Math.max(0.6, (mx - mn) * mid));
    }
    g.fill();
  });
  return <canvas ref={cv} style={{ display: 'block', width: '100%', height: '14px' }} />;
}

const PREVIEW_H = 18;
/** One scratch canvas draws every preview; the pixels go to an <img>. A canvas element per
 *  pattern would be a compositor layer each (80+ in a song), and every layer-tree update of the
 *  page then costs ~10 ms more: a dropped frame while dragging a clip. Images paint into their
 *  parent's layer. */
let scratch: HTMLCanvasElement | null = null;
/**
 * A pattern's notes in miniature, laid out like its clips in the Playlist (drum channels one
 * row each, melodic parts by key), one pass across the full width. Empty patterns draw a
 * faint baseline. Redraws when the notes, the channels or the panel width change.
 */
function PatPreview(props: { pat: Pattern; chans: Map<Id, Channel> }) {
  let el!: HTMLImageElement;
  const [w, setW] = createSignal(0);
  onMount(() => {
    const ro = new ResizeObserver(() => setW(Math.round(el.clientWidth)));
    ro.observe(el);
    onCleanup(() => ro.disconnect());
  });
  const src = createMemo(() => {
    const W = w(), p = props.pat;
    if (!W) return '';
    const r = patternPreview(p, props.chans, PREVIEW_H); // reads every note: tracks edits
    const dpr = window.devicePixelRatio || 1;
    const cv = (scratch ??= document.createElement('canvas'));
    fitCanvas(cv, W, PREVIEW_H);
    const g = cv.getContext('2d')!;
    g.setTransform(1, 0, 0, 1, 0, 0);
    g.clearRect(0, 0, cv.width, cv.height);
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.fillStyle = p.color;
    if (!r.length) {
      g.globalAlpha = 0.25; g.fillRect(0, PREVIEW_H / 2, W, 1);
    } else {
      const k = W / Math.max(p.len, 1e-3);
      // bar lines, so a 4-bar pattern reads as 4 bars
      g.globalAlpha = 0.18;
      for (let b = 4; b < p.len; b += 4) g.fillRect(Math.round(b * k), 0, 1, PREVIEW_H);
      g.globalAlpha = 0.9;
      for (let i = 0; i < r.length; i += 4) g.fillRect(r[i] * k, r[i + 1], Math.max(1.5, r[i + 2] * k - 0.5), r[i + 3]);
    }
    g.globalAlpha = 1;
    return cv.toDataURL();
  });
  return <img ref={el} class="pprev" src={src() || undefined} alt="" draggable={false} />;
}

function PatternsTab() {
  const chanMap = createMemo(() => new Map((S.project.channels ?? []).map(c => [c.id, c])));
  return (
    <div class="scroll">
      <div class="bhead">
        <span>{t('d.br.patCount', { n: S.project.patterns.length })}</span>
        <button class="linkbtn" title={t('d.br.newpat.tip')} onClick={() => { newPattern(); showPatternWindow(); }}>{t('d.br.new')}</button>
      </div>
      <div class="plist">
        <For each={S.project.patterns}>
          {p => (
            <div class="pitem" classList={{ on: U.cur?.kind === 'pat' && U.cur.ref === p.id }} title={t('d.br.pat.tip')} style={{ '--c': p.color }}
              onContextMenu={e => { e.preventDefault(); patternItemMenu(p, e.clientX, e.clientY); }}
              onPointerDown={e => pick('pat', p.id, p.name, p.color, e, () => openPattern(p.id))}>
              <div class="prow">
                <Show when={patternIcon(p, chanMap())} fallback={<span style={{ width: '8px', height: '8px', 'border-radius': '2px', flex: 'none', background: p.color }} />}>
                  {id => <span class="picon" style={{ color: p.color }}><InstIcon id={id()} size={12} /></span>}
                </Show>
                <span class="nm">{p.name}</span>
                <span class="meta">{t('d.br.bars', { n: p.len / 4 })}</span>
              </div>
              <PatPreview pat={p} chans={chanMap()} />
            </div>
          )}
        </For>
      </div>
    </div>
  );
}

function AudioTab() {
  const list = createMemo(() => browserSources(S.project));
  const importBtn = async () => {
    try { const paths = await be().pickAudioFiles(); if (paths) await importFiles(paths); }
    catch (e) { say(t('d.br.noimport', { err: String(e) })); }
  };
  return (
    <div class="scroll">
      <div class="bhead">
        <span>{t('d.br.audioCount', { n: list().length })}</span>
        <button class="linkbtn" title={t('d.br.import.tip')} onClick={importBtn}>{t('d.br.import')}</button>
      </div>
      <Show when={list().length} fallback={<div class="bnote">{t('d.br.audioEmpty')}</div>}>
        <div style={{ display: 'flex', 'flex-direction': 'column', padding: '4px 0' }}>
          <For each={list()}>
            {s => (
              <div class="sitem" classList={{ on: U.cur?.kind === 'audio' && U.cur.ref === s.id, playing: U.previewing && U.previewPath === srcKey(s.id) }} title={t('d.br.src.tip')}
                onPointerDown={e => {
                  if (e.button === 0 && dblCheck('sitem' + s.id)) { e.preventDefault(); return previewSource(s); }
                  pick('audio', s.id, s.name, s.color, e);
                }}>
                <div class="row" style={{ gap: '8px' }}>
                  {/* the icon its clips show in the Playlist (guessed from the name), else a sample's */}
                  <span class="picon" style={{ color: s.color }}><InstIcon id={guessIcon(s.name) ?? 'sample'} size={12} /></span>
                  <span class="ell" style={{ flex: 1, font: '500 12px var(--sans)' }}>{s.name}</span>
                  <span class="mono" style={{ 'font-size': '10px', color: 'var(--fg4)' }}>{s.dur.toFixed(2)}s</span>
                </div>
                <Thumb src={s} />
              </div>
            )}
          </For>
        </div>
        <div class="bnote">{t('d.br.dragnote')}</div>
      </Show>
    </div>
  );
}

// ---------- library ----------
const STD = 'k:' + STD_KIT;
/**
 * `files` by list key (`k:<kit>`, `f:<folder>`); `open` = kits and folders shown open; `gopen` =
 * groups the user opened or closed while browsing, `sopen` = the same during the current search
 * (forgotten when the search text changes); `sel` = the row the keyboard is on.
 */
const [lib, setLib] = createStore<{
  kits: Kit[]; folders: string[]; open: Record<string, boolean>; files: Record<string, LibFile[] | undefined>; errs: Record<string, string | undefined>;
  gopen: Record<string, boolean | undefined>; sopen: Record<string, boolean | undefined>; q: string; sel: string | null;
  progress: Record<string, LibraryProgress | undefined>; loaded: boolean; error: string | null;
  /** the list was cut off at the folder walk's cap (the note under the list says so) */
  trunc: Record<string, boolean | undefined>;
}>({ kits: [], folders: [], open: {}, files: {}, errs: {}, gopen: {}, sopen: {}, q: '', sel: null, progress: {}, loaded: false, error: null, trunc: {} });
const terms = createMemo(() => searchTerms(lib.q));
const searching = () => terms().length > 0;
/** Whole sections (STANDARD LIBRARY, KITS, FOLDERS) the user closed; in U so it survives a restart. A search opens everything it matched, like the groups inside a list. */
const secOpen = (s: 'std' | 'kits' | 'folders') => searching() || !U.browserSec[s];
const toggleSec = (s: 'std' | 'kits' | 'folders') => { if (!searching()) setU('browserSec', s, U.browserSec[s] ? undefined : true); };
const GROUP_COLOR = new Map(FACTORY_FILES.map(f => [f.group, f.color]));
let listEl: HTMLDivElement | undefined, searchEl: HTMLInputElement | undefined;

const listQuery = (key: string) => (key.startsWith('k:') ? { kit: key.slice(2) } : { folder: key.slice(2) });
const loading = new Map<string, Promise<void>>();
/** Loads a list once; a failure is kept (`errs`) and shown with a way to try again. */
function ensure(key: string): Promise<void> {
  if (lib.files[key]) return Promise.resolve();
  let p = loading.get(key);
  if (!p) {
    setLib('errs', key, undefined);
    p = be().libraryFiles(listQuery(key)).then(
      r => { setLib('files', key, r.files); setLib('trunc', key, r.truncated || undefined); },
      e => setLib('errs', key, String(e)),
    ).finally(() => loading.delete(key));
    loading.set(key, p);
  }
  return p;
}
async function refreshLib() {
  try { const l = await be().libraryList(); setLib({ kits: l.kits, folders: l.folders, loaded: true, error: null }); if (searching()) loadAll(); }
  catch (e) { setLib({ loaded: true, error: String(e) }); }
}
function toggleOpen(key: string) {
  const open = !lib.open[key];
  setLib('open', key, open);
  if (open) void ensure(key);
}
/** A search looks in everything that is installed, opened before or not. */
function loadAll() {
  for (const k of lib.kits) if (k.installed) void ensure('k:' + k.id);
  for (const f of lib.folders) void ensure('f:' + f);
}
function setQuery(q: string) {
  setLib({ q, sopen: {} });
  if (searching()) loadAll();
}
const matchCount = (key: string) => { const tm = terms(); return (lib.files[key] ?? []).reduce((n, f) => n + (matches(f, tm, libGroup) ? 1 : 0), 0); };
/**
 * The flat row model of every mounted list (`k:<kit>`, `f:<folder>`, the standard library's
 * key), each list's way to scroll a row into its drawn window, and the walk the keyboard
 * follows: every drawn file of the library, in row order. `LibraryTab` fills `walk` from
 * these; a list reports its model through `flats` and nudges `walkTick` when it changes.
 */
const flats: Record<string, FlatItem[]> = {};
const reveals: Record<string, (path: string) => void> = {};
const [walkTick, bumpWalk] = createSignal(0, { equals: false });
let walk: { f: LibFile; k: string }[] = [];
const revealAt = (i: number) => { const r = walk[i]; if (r) reveals[r.k]?.(r.f.path); };
/** Bumped by the shared scroll container, its content resizing (a group or section opening elsewhere, a load finishing), and viewport resize: every drawn window follows. */
const [layoutTick, bumpLayout] = createSignal(0, { equals: false });

function startPreview(f: LibFile) {
  setU('previewPath', f.path);
  be().previewFile(f.path).catch(e => say(t('d.br.preverr', { err: String(e) })));
  say(t('d.br.prev', { name: f.name }));
}
function previewFile(f: LibFile) {
  if (U.previewing && U.previewPath === f.path) { void be().previewStop(); setU('previewPath', null); return; }
  startPreview(f);
}

/** ↑ ↓ walk the drawn rows (open groups, the search's matches) and play each; Enter makes a sampler channel. Works past the window: the walk follows the flat models, the lists scroll what is picked. */
function onListKey(e: KeyboardEvent) {
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    const cur = walk.findIndex(r => r.f.path === lib.sel);
    e.preventDefault(); e.stopPropagation();
    if (e.key === 'ArrowUp' && cur <= 0) { searchEl?.focus(); return; }
    const i = stepIndex(walk.length, cur, e.key === 'ArrowDown' ? 1 : -1);
    if (i < 0) return;
    const r = walk[i];
    setLib('sel', r.f.path);
    revealAt(i);
    startPreview(r.f);
  } else if (e.key === 'Enter' && lib.sel) {
    const r = walk.find(x => x.f.path === lib.sel);
    if (!r) return;
    e.preventDefault(); e.stopPropagation();
    void addSamplerFiles([r.f]);
  }
}

type SampleDrop =
  | { kind: 'replace'; channel: Channel; row?: HTMLElement }
  | { kind: 'before' | 'after'; channel: Channel; row: HTMLElement }
  | { kind: 'append' | 'import' };

function fileTarget(x: number, y: number): SampleDrop {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  const w = el?.closest<HTMLElement>('[data-channel-window]'), win = w ? chanById(Number(w.dataset.channelWindow)) : undefined;
  if (win?.kind === 'sample') return { kind: 'replace', channel: win };
  const row = el?.closest<HTMLElement>('.rackwin [data-ch]');
  const channel = row ? chanById(Number(row.dataset.ch)) : undefined;
  if (channel && row) {
    const { top, height } = row.getBoundingClientRect();
    return { kind: rackSampleDrop(y - top, height), channel, row };
  }
  return { kind: w || el?.closest('.rackwin') ? 'append' : 'import' };
}
let marked: HTMLElement | undefined;
function markRow(row?: HTMLElement, kind?: 'replace' | 'before' | 'after') {
  if (marked === row && marked?.dataset.sampleDrop === kind) return;
  if (marked) delete marked.dataset.sampleDrop;
  marked = row;
  if (row && kind) row.dataset.sampleDrop = kind;
}
/**
 * What a sample dragged to `x, y` would do (marking the Channel Rack row it would land on): the
 * ghost's label, or null when the point is not the rack (the Playlist / the project take it).
 */
function sampleOver(x: number, y: number): string | null {
  const d = fileTarget(x, y);
  if ('row' in d && d.row) markRow(d.row, d.kind);
  else markRow();
  switch (d.kind) {
    case 'replace': return t('d.br.dropReplace', { name: d.channel.name });
    case 'before': return t('d.br.dropBefore', { name: d.channel.name });
    case 'after': return t('d.br.dropAfter', { name: d.channel.name });
    case 'append': return t('d.br.dropNewCh');
    case 'import': return null;
  }
}
/** Where a channel dropped before / after `d.channel` goes, looked up when it lands (the rack may change while files load). */
const dropIndex = (d: { kind: 'before' | 'after'; channel: Channel }) => () => {
  const index = channels().findIndex(c => c.id === d.channel.id);
  return index < 0 ? undefined : index + (d.kind === 'after' ? 1 : 0);
};
/**
 * Drop audio files (library files, or files dragged in from the system) at a point: onto the
 * Channel Rack they become sampler channels (FL) — a row's edge inserts there, its middle replaces
 * the sample (the other files follow it as new channels); elsewhere they go to the Playlist / the project.
 */
function dropFiles(files: SampleFile[], x: number, y: number, alt: boolean) {
  const d = fileTarget(x, y);
  const audio = files.filter(f => !/\.midi?$/i.test(f.path));
  // a MIDI file is never a sample: the import dialog handles it (and whatever came with it) as before
  if (d.kind === 'import' || audio.length < files.length) return void importAt(files.map(f => f.path), x, y, alt);
  if (d.kind === 'replace') {
    const [first, ...rest] = audio;
    void replaceWithFile(d.channel, first).then(() => {
      if (rest.length) return addSamplerFiles(rest, dropIndex({ kind: 'after', channel: d.channel }));
    });
    return;
  }
  if (d.kind === 'before' || d.kind === 'after') return void addSamplerFiles(audio, dropIndex(d));
  void addSamplerFiles(audio);
}
/** Drop a project source (Browser ▸ Project ▸ Audio) at a point: a sampler on the rack, else an audio clip in the Playlist. */
function dropSource(src: Source, x: number, y: number, alt: boolean) {
  const d = fileTarget(x, y);
  if (d.kind === 'replace') return replaceWithSource(d.channel, src.id);
  if (d.kind === 'before' || d.kind === 'after') return addSampler(src.id, undefined, dropIndex(d)());
  if (d.kind === 'append') return addSampler(src.id);
  dropClipAt(x, y, 'audio', src.id, alt);
}

/**
 * Files dragged in from the system (Explorer, a web browser): while they hover, the rack row they
 * would land on is marked and a label says what the drop does; over anything else the Playlist's
 * drop hint shows. Returns whether the rack is the target.
 */
export function osDragOver(x: number, y: number): boolean {
  const label = sampleOver(x, y);
  setU('ghost', label ? { x: x + 12, y: y + 8, name: label, color: '#47b8a8' } : null);
  return label != null;
}
export function osDragEnd() { markRow(); setU('ghost', null); }
export function osDrop(paths: string[], x: number, y: number, alt: boolean) {
  osDragEnd();
  dropFiles(paths.map(path => ({ path })), x, y, alt);
}

const Chev = (props: { open: boolean }) => (
  <svg class="chev" classList={{ open: props.open }} width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 5l7 7-7 7" /></svg>
);
const Glyph = (props: { g: MenuGlyph; size: number }) => (
  <svg width={props.size} height={props.size} viewBox={props.g.vb} fill="currentColor" aria-hidden="true"><path fill-rule={props.g.eo ? 'evenodd' : 'nonzero'} d={props.g.d} /></svg>
);

function FileRow(props: { f: LibFile; sub: boolean }) {
  const f = props.f, color = GROUP_COLOR.get(f.group);
  return (
    <div class="fitem" data-path={f.path} classList={{ playing: U.previewing && U.previewPath === f.path, sel: lib.sel === f.path, sub: props.sub }} title={t('d.br.file.tip')}
      onContextMenu={e => { e.preventDefault(); setLib('sel', f.path); openMenu(e.clientX, e.clientY, [{ t: 'title', label: f.name }, { t: 'item', label: t('d.br.addsampler'), title: t('d.br.addsampler.tip'), act: () => void addSamplerFiles([f]) }], 90); }}
      onPointerDown={e => {
        if (e.button !== 0) return;
        e.preventDefault(); setLib('sel', f.path); listEl?.focus({ preventScroll: true });
        dragGhost(e, f.name, color ?? '#47b8a8', () => previewFile(f), (x, y, alt) => dropFiles([f], x, y, alt), sampleOver);
      }}>
      <span class="fico" style={{ color: color ?? 'var(--fg4)' }}><Glyph g={libFileGlyph(f)} size={11} /></span>
      <span class="ell nm">{f.name.replace(/\.[^.]+$/, '')}</span>
      <Show when={f.pitched && f.root != null}><span class="fkey" title={t('d.br.key.tip')}>{noteName(f.root!)}</span></Show>
    </div>
  );
}

function GroupHead(props: { k: string; g: { name: string; total: number }; open: boolean }) {
  const key = () => props.k + '|' + props.g.name;
  const choices = () => (searching() ? 'sopen' : 'gopen');
  return (
    <div class="grow" role="button" aria-expanded={props.open} title={t('d.br.group.tip', { n: props.g.total })} style={{ '--c': GROUP_COLOR.get(props.g.name) ?? 'var(--fg3)' }}
      onClick={() => setLib(choices(), key(), !props.open)}>
      <Chev open={props.open} />
      <span class="gico"><Glyph g={libGroupGlyph(props.g.name)} size={11} /></span>
      <span class="ell nm">{libGroup(props.g.name)}</span>
      <span class="cnt">{props.g.total}</span>
    </div>
  );
}

function FileList(props: { k: string }) {
  const all = () => lib.files[props.k];
  const groups = createMemo(() => groupFiles(all() ?? [], terms(), libGroup));
  const choices = () => (searching() ? 'sopen' : 'gopen');
  // every row of the list with its offset (virtualized: the block is this tall, only the
  // viewport's rows are built)
  const flat = createMemo(() => flatten(groups(), g => groupOpen(lib[choices()][props.k + '|' + g], searching(), all()!.length)));
  const totalH = () => { const f = flat(); return f.length ? f[f.length - 1].y + (f[f.length - 1].kind === 'ghead' ? GHEAD_H : ROW_H) : 0; };
  let el: HTMLDivElement | undefined;
  const [win, setWin] = createSignal<{ f: FlatItem[]; i0: number; i1: number } | null>(null, { equals: (a, b) => !!a && !!b && a.f === b.f && a.i0 === b.i0 && a.i1 === b.i1 });
  /** The rows the browser viewport shows right now; scroll and resize run this again. */
  const recompute = () => {
    const f = flat();
    if (!el || !listEl || !f.length) return setWin(null);
    layoutTick();
    // the viewport as a range of the block's own coordinates: its top is `off` above the
    // block's first row (negative when the list is scrolled deep), so it spans `clientHeight`
    const off = el.getBoundingClientRect().top - listEl.getBoundingClientRect().top;
    const [i0, i1] = windowRange(f, -off, listEl.clientHeight - off);
    setWin({ f, i0, i1 });
  };
  createEffect(recompute);
  onMount(() => {
    reveals[props.k] = path => {
      const f = flat(), i = f.findIndex(it => it.kind === 'frow' && it.f?.path === path);
      if (i < 0 || !el || !listEl) return;
      const y = el.getBoundingClientRect().top - listEl.getBoundingClientRect().top + listEl.scrollTop + f[i].y;
      listEl.scrollTop = clamp(y - 48, 0, listEl.scrollHeight - listEl.clientHeight);
    };
    onCleanup(() => delete reveals[props.k]);
  });
  // the keyboard walk reads the model; a fresh list or a new open/search state rebuilds it
  createEffect(() => { flats[props.k] = flat(); bumpWalk(1); });
  onCleanup(() => { delete flats[props.k]; bumpWalk(1); }); // a list unmounting (its section closed) must retire its rows from the walk, not linger in it
  const slice = () => { const w = win(); return w ? w.f.slice(w.i0, w.i1 + 1) : []; };
  return (
    <div class="flist" ref={el}>
      <Show when={all()} fallback={
        <Show when={lib.errs[props.k]} fallback={<div class="fnote">{t('d.br.loading')}</div>}>
          {err => <div class="fnote err">{t('d.br.liberr', { err: err() })} <button class="linkbtn" onClick={() => void ensure(props.k)}>{t('d.br.retry')}</button></div>}
        </Show>
      }>
        <Show when={groups().length} fallback={<div class="fnote">{searching() ? t('d.br.nomatch', { q: lib.q.trim() }) : t('d.br.nofiles')}</div>}>
          <div style={{ position: 'relative', height: totalH() + 'px' }}>
            <Show when={win()}>
              {w => (
                <div style={{ position: 'absolute', top: w().f[w().i0]?.y + 'px', left: 0, right: 0 }}>
                  <For each={slice()}>
                    {it => it.kind === 'ghead'
                      ? <GroupHead k={props.k} g={it.g!} open={!!it.open} />
                      : <FileRow f={it.f!} sub={!!it.sub} />}
                  </For>
                </div>
              )}
            </Show>
          </div>
          <Show when={lib.trunc[props.k]}><div class="fnote">{t('d.br.truncated')}</div></Show>
        </Show>
      </Show>
    </div>
  );
}

function LibraryTab() {
  let contentEl: HTMLDivElement | undefined;
  onMount(() => {
    if (!lib.loaded) void refreshLib();
    void ensure(STD);
    let off: (() => void) | null = null;
    be().on('library-progress', p => {
      setLib('progress', p.id, p);
      if (p.error) say(t('d.br.instfail', { id: p.id, err: p.error }));
      else if (p.done >= p.total) { setLib('progress', p.id, undefined); void refreshLib(); say(t('d.br.installed', { id: p.id })); }
    }).then(u => { off = u; });
    const onScroll = () => bumpLayout(1);
    listEl?.addEventListener('scroll', onScroll, { passive: true });
    // the content is observed too: a group or section opening ABOVE a list shifts it without a
    // scroll event, and every drawn window has to re-measure when that happens
    const ro = new ResizeObserver(onScroll);
    if (listEl) ro.observe(listEl);
    if (contentEl) ro.observe(contentEl);
    onCleanup(() => { listEl?.removeEventListener('scroll', onScroll); ro.disconnect(); off?.(); });
  });
  // The keyboard walk: every drawn file in row order — the standard library, then the open
  // kits, then the open folders. `walkTick` covers a list's model changing (load, open, search).
  createEffect(() => {
    walkTick();
    const out: { f: LibFile; k: string }[] = [];
    const push = (k: string) => { for (const it of flats[k] ?? []) if (it.kind === 'frow' && it.f) out.push({ f: it.f, k }); };
    if (stdShown()) push(STD);
    for (const k of kits()) if (k.installed && isOpen('k:' + k.id)) push('k:' + k.id);
    for (const f of folders()) if (isOpen('f:' + f)) push('f:' + f);
    walk = out;
  });
  const install = (k: Kit) => {
    setLib('progress', k.id, { id: k.id, done: 0, total: k.bytes });
    be().libraryInstall(k.id).catch(e => { setLib('progress', k.id, undefined); say(t('d.br.insterr', { err: String(e) })); });
  };
  const addFolder = async () => { try { setLib('folders', await be().libraryAddFolder()); if (searching()) loadAll(); } catch (e) { say(String(e)); } };
  const removeFolder = async (p: string) => { try { setLib('folders', await be().libraryRemoveFolder(p)); setLib('files', 'f:' + p, undefined); setLib('trunc', 'f:' + p, undefined); } catch (e) { say(String(e)); } };
  // browsing: every kit and folder; searching: the installed ones the search found something in
  const kits = () => lib.kits.filter(k => k.id !== STD_KIT && (!searching() || (k.installed && matchCount('k:' + k.id) > 0)));
  const folders = () => lib.folders.filter(f => !searching() || matchCount('f:' + f) > 0);
  const stdShown = () => !searching() || !lib.files[STD] || matchCount(STD) > 0;
  const nothing = () => searching() && !!lib.files[STD] && !stdShown() && !kits().length && !folders().length;
  // a folder cut off at the walk's cap hides itself from a search whose matches its retained files
  // don't show — say so, or "no matches" would claim a search the browser never completed
  const searchTruncated = () => searching() && lib.folders.some(f => lib.trunc['f:' + f] && matchCount('f:' + f) === 0);
  const isOpen = (key: string) => searching() || !!lib.open[key];
  return (
    <>
      <div class="bsearch" title={t('d.br.search.tip')}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="M20 20l-3.6-3.6" /></svg>
        <input ref={searchEl} type="text" spellcheck={false} autocomplete="off" value={lib.q} placeholder={t('d.br.search')} aria-label={t('d.br.search')}
          onInput={e => setQuery(e.currentTarget.value)}
          onKeyDown={e => {
            if (e.key === 'Escape') { e.stopPropagation(); if (lib.q) setQuery(''); else e.currentTarget.blur(); }
            else if (e.key === 'ArrowDown') { e.preventDefault(); setLib('sel', null); listEl?.focus({ preventScroll: true }); onListKey(e); }
          }} />
        <Show when={lib.q}><button class="xbtn" title={t('d.br.search.clear')} aria-label={t('d.br.search.clear')} onClick={() => { setQuery(''); searchEl?.focus(); }}>×</button></Show>
      </div>
      <div class="scroll" ref={listEl} tabIndex={0} onKeyDown={onListKey}>
        <div ref={contentEl}>
        <Show when={lib.error}><div class="bnote" style={{ color: 'var(--danger)' }}>{lib.error}</div></Show>
        <Show when={nothing()}><div class="bnote">{t('d.br.nomatch', { q: lib.q.trim() })}</div></Show>
        <Show when={searchTruncated()}><div class="bnote">{t('d.br.searchtrunc')}</div></Show>
        <Show when={stdShown()}>
          <div class="bhead">
            <button type="button" class="btog" aria-expanded={secOpen('std')} title={t('d.br.sec.tip')} onClick={() => toggleSec('std')} onKeyDown={e => e.stopPropagation()}>
              <Chev open={secOpen('std')} />
              <span class="ell" style={{ 'text-transform': 'uppercase' }}>{t('d.br.stdlib')}</span>
              <Show when={lib.files[STD]}>{f => <span class="hmeta">{searching() ? `${matchCount(STD)} / ${f().length}` : f().length}</span>}</Show>
            </button>
          </div>
          <Show when={secOpen('std')}>
            <div class="flist"><FileList k={STD} /></div>
          </Show>
        </Show>
        <Show when={kits().length}>
          <div class="bhead top">
            <button type="button" class="btog" aria-expanded={secOpen('kits')} title={t('d.br.sec.tip')} onClick={() => toggleSec('kits')} onKeyDown={e => e.stopPropagation()}>
              <Chev open={secOpen('kits')} />
              <span class="ell">{t('d.br.kits')}</span>
            </button>
          </div>
          <Show when={secOpen('kits')}>
          <For each={kits()}>
            {k => {
              const prog = () => lib.progress[k.id], key = 'k:' + k.id;
              return (
                <div class="kit">
                  <div class="top" classList={{ can: k.installed && !searching() }} role={k.installed ? 'button' : undefined} aria-expanded={k.installed ? isOpen(key) : undefined} onClick={() => k.installed && !searching() && toggleOpen(key)}>
                    <Show when={k.installed} fallback={<span style={{ width: '12px', flex: 'none' }} />}><Chev open={isOpen(key)} /></Show>
                    <span class="nm ell">{k.name}</span>
                    <Show when={!k.installed && !prog()}><button class="smallbtn primary" onClick={e => { e.stopPropagation(); install(k); }}>{t('d.br.install')}</button></Show>
                  </div>
                  <Show when={!searching()}>
                    <span class="desc" style={{ 'padding-left': '20px' }}>{k.description}</span>
                    <span class="meta" style={{ 'padding-left': '20px' }}>{t('d.br.kitmeta', { n: k.count, size: fmtBytes(k.bytes) }) + (k.installed ? t('d.br.kitinst') : '')}</span>
                  </Show>
                  <Show when={prog()}>
                    {p => (
                      <div style={{ 'padding-left': '20px', display: 'flex', 'flex-direction': 'column', gap: '3px' }}>
                        <div class="prog"><div style={{ width: clamp(p().total ? p().done / p().total : 0, 0, 1) * 100 + '%' }} /></div>
                        <span class="meta">{p().error ? p().error : t('d.br.downloading', { n: Math.round(p().total ? p().done / p().total * 100 : 0) })}</span>
                      </div>
                    )}
                  </Show>
                  <Show when={k.installed && isOpen(key)}><div class="flist in"><FileList k={key} /></div></Show>
                </div>
              );
            }}
          </For>
          </Show>
        </Show>
        <Show when={!searching() || folders().length}>
          <div class="bhead top">
            <button type="button" class="btog" aria-expanded={secOpen('folders')} title={t('d.br.sec.tip')} onClick={() => toggleSec('folders')} onKeyDown={e => e.stopPropagation()}>
              <Chev open={secOpen('folders')} />
              <span class="ell">{t('d.br.folders')}</span>
            </button>
            <button class="linkbtn" onClick={e => { e.stopPropagation(); void addFolder(); }} title={t('d.br.addfolder.tip')}>{t('d.br.add')}</button>
          </div>
          <Show when={secOpen('folders')}>
          <For each={folders()} fallback={<div class="bnote">{t('d.br.addown')}</div>}>
            {f => (
              <>
                <div class="folder" role="button" aria-expanded={isOpen('f:' + f)} onClick={() => !searching() && toggleOpen('f:' + f)} title={f}>
                  <Chev open={isOpen('f:' + f)} />
                  <span class="ell" style={{ flex: 1 }}>{baseName(f) || f}</span>
                  <button class="xbtn" title={t('d.br.rmfolder')} onClick={e => { e.stopPropagation(); void removeFolder(f); }}>×</button>
                </div>
                <Show when={isOpen('f:' + f)}><div class="flist"><FileList k={'f:' + f} /></div></Show>
              </>
            )}
          </For>
          </Show>
        </Show>
        <div class="bnote">{t('d.br.libnote')}</div>
        </div>
      </div>
    </>
  );
}

const BROWSER_TABS = ['patterns', 'audio', 'library'] as const;

export function Browser() {
  const split = (e: PointerEvent) => {
    if (e.button !== 0) return; e.preventDefault(); e.stopPropagation();
    const sx = e.clientX, w0 = U.pickW;
    track(ev => setU('pickW', clamp(w0 + ev.clientX - sx, 160, 460)), null, 'col-resize');
  };
  return (
    <>
      <div class="browser" style={{ width: U.pickW + 'px' }}>
        <div class="btabs">
          <For each={BROWSER_TABS}>{k => <button classList={{ on: U.browserTab === k }} onClick={() => setU('browserTab', k)}>{t(`d.br.tab.${k}`)}</button>}</For>
        </div>
        <Switch fallback={<LibraryTab />}>
          <Match when={U.browserTab === 'patterns'}><PatternsTab /></Match>
          <Match when={U.browserTab === 'audio'}><AudioTab /></Match>
        </Switch>
      </div>
      <div class="vsplit" onPointerDown={split} onDblClick={() => setU('pickOpen', false)} title={t('d.br.resize')} />
    </>
  );
}
