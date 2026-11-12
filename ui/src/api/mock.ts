// In-browser fake engine, used when the page is not running inside Tauri.
// Holds the demo project, applies ops with undo/gestures, plays a fake transport and
// streams binary telemetry frames (encoded exactly like the real ones).
import type { Backend, EventMap, OsDropEvent } from './backend';
import { encodeFrame, SCOPE_LEN, SPEC_BANDS, type EncFx } from './telemetry';
import { BUILTIN, emptyProject, mockCatalog, sketchProject } from './mockData';
import { FACTORY_FILES, FACTORY_KIT, factoryFile, LIB_SCHEME, libPath } from '../lib/factory';

/** Reverb mix (%) the engine computes for the research plate at a -18 dB send (autovocal::reverb_mix_for_send). */
const MOCK_REVERB_MIX = 37.2;
import { applyAll } from '../lib/applyOps';
import { landTakes, takeEnd } from '../lib/comp';
import { satDriveDb } from '../lib/saturator';
import { eqCurve } from '../lib/eq';
import { clamp, dbg } from '../lib/format';
import { t } from '../lib/i18n';
import { midiToHz, noteAt, scaleMask, simpleTarget, snapNote } from '../lib/pitch';
import { newFx } from '../lib/presets';
import { synthVocal, vocalAt, vocalPeaks, type VocalSample } from './mockVocal';
import type {
  AudioConfig, AudioStatus, LatencyReport, AnyBuiltinKind, AutoVocalPlan, AutoVocalStyle, Catalog, Clip, ExportResult, F0ModelStatus, Fx, Id, KeysStatus, LibFile, LibraryList, MidiDevice, MidiStatus, MusicAnalysis, Op, OpenResult, PitchAnalysis, PitchTarget, Project, RefTune, SaveResult, Source, StateDto, TransportDto, Vst3Info, Vst3Param,
} from './types';
import { exportRange, exportSeconds } from '../lib/export';
import { MockUpdates } from './mockUpdates';

const TC = ['#4f8ef7', '#5aa9e6', '#69c3d6', '#47b8a8', '#5fbf8a', '#7c8cf8', '#e0a44a', '#9a8cf0', '#d9678f', '#8b93a1'];
const BAND_F = new Float32Array(SPEC_BANDS).map((_, k) => 20 * Math.pow(1000, k / (SPEC_BANDS - 1)));
const delay = (ms = 0) => new Promise<void>(r => setTimeout(r, ms));
const hash = (s: string) => { let h = 2166136261; for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619); return h >>> 0; };

function rng(seed: number) {
  let s = seed || 7;
  return () => ((s = Math.imul(s, 16807) % 2147483647) & 0x7fffffff) / 2147483647;
}

/** Synthetic waveform peaks: a loop of transients with decays, 100 [min,max] pairs per second. */
function synthPeaks(seed: number, dur: number, bpm: number): Float32Array {
  const n = Math.max(1, Math.round(dur * 100)), out = new Float32Array(n * 2), r = rng(seed);
  const style = seed % 3, spb = 60 / bpm;
  for (let i = 0; i < n; i++) {
    const t = i / 100, ph = (t % (spb / (style === 1 ? 2 : 1))) / spb;
    const env = style === 0 ? Math.exp(-ph * 9) * 0.85 + 0.05 : style === 1 ? Math.exp(-ph * 4) * 0.6 + 0.2 : 0.45 + 0.25 * Math.sin(t * 2.1) * Math.sin(t * 0.7);
    const a = clamp(env * (0.75 + 0.25 * r()), 0, 1);
    out[2 * i] = -a * (0.85 + 0.15 * r()); out[2 * i + 1] = a;
  }
  return out;
}

/** Peaks of a one-shot (a standard-library sample): a hit that decays over its length. */
function oneShotPeaks(seed: number, dur: number): Float32Array {
  const n = Math.max(1, Math.round(dur * 100)), out = new Float32Array(n * 2), r = rng(seed);
  for (let i = 0; i < n; i++) {
    const a = clamp(Math.exp(-(i / n) * 5) * (0.8 + 0.2 * r()), 0, 1);
    out[2 * i] = -a * (0.85 + 0.15 * r()); out[2 * i + 1] = a;
  }
  return out;
}

interface MockSource extends Source { seed: number; vocal?: boolean; trimFrom?: { id: Id; offSec: number } }
const isVocalName = (n: string) => /vocal|vox|wokal|śpiew/i.test(n);
const VOCAL_DUR = 14;

/** Per-effect DSP state of the fake processors (so meters move like the engine's). */
interface FxSim { t: number; corr: number; center: number; held: number; last: number; m: number[]; gr: number; dyn: Float32Array }
const newSim = (): FxSim => ({ t: 0, corr: 0, center: 0, held: 0, last: 0, m: [0, 0, 0, 0, 0, 0], gr: 0, dyn: new Float32Array(32) });

/** Spectrum (dBFS per band) of a sung vowel at `midi` with level `lv`, plus sibilant noise `sib`. */
function vocalSpec(s: VocalSample, out: Float32Array) {
  const pw = new Float64Array(SPEC_BANDS);
  const lvDb = s.level > 1e-4 ? 20 * Math.log10(s.level) : -120;
  const idx = (f: number) => Math.round(Math.log(f / 20) / Math.log(1000) * (SPEC_BANDS - 1));
  if (s.midi != null && s.level > 0.01) {
    const f0 = midiToHz(s.midi);
    for (let h = 1; h * f0 < 12000; h++) {
      const f = h * f0, oct = Math.log2(f / 500);
      // formants around 700 Hz and 2.6 kHz on a −9 dB/oct tilt
      const env = -9 * Math.max(0, oct) + 9 * Math.exp(-((Math.log2(f / 700)) ** 2) * 3) + 7 * Math.exp(-((Math.log2(f / 2600)) ** 2) * 4);
      const k = idx(f); if (k < 0 || k >= SPEC_BANDS) continue;
      pw[k] += Math.pow(10, (lvDb - 14 + env) / 10);
    }
  }
  for (let k = 0; k < SPEC_BANDS; k++) {
    const f = BAND_F[k];
    const hiss = s.sib > 0 ? Math.pow(10, (-24 + 6 * s.sib - 18 * Math.max(0, Math.log2(4500 / f)) ** 2 * (f < 4500 ? 1 : 0) - 3 * Math.max(0, Math.log2(f / 9000))) / 10) : 0;
    const floor = Math.pow(10, (-92 - 3 * Math.log2(f / 1000)) / 10);
    out[k] = 10 * Math.log10(pw[k] + hiss + floor) + (Math.random() - 0.5) * 2.5;
  }
}

export class MockBackend implements Backend {
  readonly isMock = true;
  /** No window chrome in a browser; `?chrome` previews the custom title bar with inert buttons. */
  readonly win = typeof location !== 'undefined' && new URLSearchParams(location.search).has('chrome')
    ? { minimize: async () => {}, toggleMaximize: async () => {}, close: async () => {}, isMaximized: async () => false, startDragging: async () => {}, onResized: async () => () => {} }
    : null;
  private cat: Catalog = mockCatalog();
  // perf harness hook: perf/ui_bench.mjs injects a generated long project here (see perf/README)
  private p: Project = (typeof window !== 'undefined' && (window as unknown as { __waveFixture?: Project }).__waveFixture) || sketchProject(this.cat);
  private revision = 1;
  private savedRev = 1;
  private path: string | null = null;
  private undoStack: Project[] = [];
  private redoStack: Project[] = [];
  private gesture: Project | null = null;
  private tr: TransportDto = { playing: false, rec: false, monitor: false, metro: false, countIn: true, startBeat: 0 };
  private listeners = new Map<string, Set<(p: unknown) => void>>();
  private teleCb: ((b: ArrayBuffer) => void) | null = null;
  private watched: Id[] = [];
  private seq = 0;
  private playT0 = 0; private playFrom = 0;
  private rec: null | { track: number; clipStart: number; t0: number; peaks: number[]; sent: number; lap: [number, number] | null; pass: number } = null;
  private audition: Record<string, Id> = {};
  private preview: null | { t0: number; dur: number; off: number } = null;
  /** Pattern loop preview (engine `pattern_play`) and recent auditions (insert id → time) for the meters. */
  private patPlay: null | { id: Id; t0: number } = null;
  private flash = new Map<Id, number>();
  private srcMeta = new Map<Id, MockSource>();
  private takeN = 0;
  private kits = [
    { id: FACTORY_KIT, name: 'Standard library', description: 'Drums, percussion and tonal one-shots that ship with WAVE', bytes: FACTORY_FILES.reduce((n, f) => n + f.bytes, 0), count: FACTORY_FILES.length, installed: true },
    { id: 'wave-drums', name: 'WAVE Drums', description: 'Stopy, werble, hi-haty i pętle perkusyjne', bytes: 86e6, count: 240, installed: false },
    { id: 'wave-keys', name: 'WAVE Keys & Pads', description: 'Akordy, pady i tekstury', bytes: 142e6, count: 96, installed: true },
    { id: 'wave-fx', name: 'WAVE FX', description: 'Uplightery, impakty, szumy', bytes: 38e6, count: 64, installed: false },
  ];
  private folders = ['C:\\Users\\michal\\Music\\Samples'];
  private vstParams = new Map<Id, Vst3Param[]>();
  private audio: AudioStatus = {
    host: 'wasapi', output: 'Głośniki (Realtek High Definition Audio)', input: 'Mikrofon (USB Audio)', sampleRate: null, bufferSize: null, inputChannel: null,
    running: true, actualRate: 48000, actualBuffer: 480, latencyMs: 20, inputChannels: 2, error: null, note: t('d.mock.noaudio'), asioCompiled: false,
    recOffsetMs: 0, inputUnderruns: 0, inputOverruns: 0, inputDriftPpm: 0, restarts: 0,
  };

  private midiPorts: MidiDevice[] = [{ id: 'mock-keys', name: 'Mock Keystation 61' }, { id: 'mock-pads', name: 'Mock Pad Controller' }];
  private midi: MidiStatus = { connected: null, selected: null, error: null, lastNote: null, noteCount: 0, target: null };

  constructor() {
    setInterval(() => this.tick(), 1000 / 60);
    (window as unknown as { __waveMock: MockBackend }).__waveMock = this;
  }

  // ---------- state ----------
  private dto(): StateDto {
    return structuredClone({ project: this.p, revision: this.revision, canUndo: this.undoStack.length > 0, canRedo: this.redoStack.length > 0,
      transport: this.tr, path: this.path, dirty: this.revision !== this.savedRev, audition: this.liveAudition() });
  }
  private emit<K extends keyof EventMap>(ev: K, payload: EventMap[K]) {
    const ls = this.listeners.get(ev); if (!ls) return;
    setTimeout(() => ls.forEach(cb => cb(payload)), 0);
  }
  private changed() { this.revision++; this.emit('state', this.dto()); }
  private pushUndo(prev: Project) { this.undoStack.push(prev); if (this.undoStack.length > 300) this.undoStack.shift(); this.redoStack = []; }

  async getState() { return this.dto(); }
  async applyOps(ops: Op[], opts?: { label?: string; transient?: boolean }) {
    await delay();
    // like the engine: a plugin automation target must name a parameter the hosted
    // plugin lists, else it would be a dead envelope
    for (const op of ops) {
      if (op.op === 'putAutomation' && op.automation.target.kind === 'plugin') {
        const { fxId, paramId } = op.automation.target;
        const fx = this.p.inserts.flatMap(m => m.fx).find(f => f.id === fxId);
        if (fx?.type !== 'vst3' || !this.vstParams.get(fxId)?.some(q => q.id === paramId))
          throw new Error(`INVALID_TARGET: effect ${fxId} has no VST3 parameter ${paramId} to automate`);
      }
    }
    const prev = this.p;
    this.p = applyAll(this.p, ops); // throws (rejects) on an invalid batch
    // like the engine: no state event per transient batch of a gesture (endGesture reports it)
    if (opts?.transient && this.gesture) this.revision++;
    else { this.pushUndo(prev); this.changed(); }
    return { revision: this.revision };
  }
  async beginGesture() { this.gesture = structuredClone(this.p); }
  async endGesture() {
    const g = this.gesture; this.gesture = null;
    if (g && JSON.stringify(g) !== JSON.stringify(this.p)) {
      this.pushUndo(g);
      // pitch edits are rendered offline at the end of a gesture: the state event comes after the render
      const tuned = this.p.clips.some(c => { const o = g.clips.find(x => x.id === c.id); return JSON.stringify(o?.tune ?? null) !== JSON.stringify(c.tune ?? null); });
      if (tuned) { this.revision++; this.renderReady = performance.now() + 380; setTimeout(() => this.emit('state', this.dto()), 380); } else this.changed();
    }
  }
  async undo() {
    const prev = this.undoStack.pop();
    if (prev) { this.redoStack.push(this.p); this.p = prev; this.changed(); }
    return this.dto();
  }
  async redo() {
    const next = this.redoStack.pop();
    if (next) { this.undoStack.push(this.p); this.p = next; this.changed(); }
    return this.dto();
  }

  // ---------- transport ----------
  private spb() { return 60 / this.p.bpm; }
  private songEnd() { return Math.ceil(Math.max(4, ...this.p.clips.map(c => c.start + c.len)) / 4) * 4; }
  /** Playhead without loop wrapping (beats since start, negative during count-in). */
  private rawBeat(): number { return this.playFrom + (performance.now() - this.playT0) / 1000 / this.spb(); }
  private beatNow(): number {
    if (!this.tr.playing) return this.tr.startBeat;
    let b = this.rawBeat();
    if (this.rec) {
      // loop recording cycles over the loop, like the engine (one take per pass)
      const lap = this.rec.lap;
      if (lap && b >= lap[1]) b = lap[0] + ((b - lap[1]) % (lap[1] - lap[0]));
      return b;
    }
    const lp = this.p.loop, a = lp ? lp.a : 0, e = lp ? lp.b : this.songEnd();
    if (b >= e && e > a) { b = a + ((b - a) % (e - a)); }
    return b;
  }
  async transportPlay(fromBeat: number) {
    if (this.tr.playing) this.finishRec();
    this.patPlay = null; // the song and the pattern preview are exclusive (like the engine)
    const pre = this.tr.rec && this.tr.countIn ? 4 : 0;
    const lp = this.p.loop;
    if (this.tr.rec && lp && lp.b > lp.a && (fromBeat < lp.a || fromBeat >= lp.b)) fromBeat = lp.a;
    this.tr = { ...this.tr, playing: true, startBeat: fromBeat };
    this.playFrom = fromBeat - pre; this.playT0 = performance.now();
    if (this.tr.rec) this.startRec(fromBeat);
    this.changedTransport();
  }
  async transportStop() {
    this.finishRec();
    this.tr = { ...this.tr, playing: false };
    this.changedTransport();
  }
  async transportSet(patch: Partial<TransportDto>) {
    const wasRec = this.tr.rec;
    this.tr = { ...this.tr, ...patch };
    if (wasRec && !this.tr.rec) this.finishRec();
    if (!wasRec && this.tr.rec && this.tr.playing) this.startRec(this.beatNow());
    this.changedTransport();
    return { ...this.tr };
  }
  private changedTransport() { this.emit('state', this.dto()); }
  private startRec(clipStart: number) {
    // like the engine: without an armed track nothing is recorded
    const t = this.p.tracks.findIndex(x => x.arm);
    if (t < 0) return;
    const lp = this.p.loop;
    const lap: [number, number] | null = lp && lp.b > lp.a && clipStart >= lp.a && clipStart < lp.b ? [lp.a, lp.b] : null;
    this.rec = { track: t, clipStart: Math.max(0, clipStart), t0: 0, peaks: [], sent: 0, lap, pass: 0 };
  }
  /** Peak-pair index where each pass after the first begins (100 pairs per second). */
  private passMarks(r: NonNullable<MockBackend['rec']>, total: number): number[] {
    if (!r.lap) return [];
    const spb = this.spb(), first = (r.lap[1] - r.clipStart) * spb * 100, lapLen = (r.lap[1] - r.lap[0]) * spb * 100, out: number[] = [];
    for (let m = first; m < total; m += lapLen) out.push(Math.round(m));
    return out;
  }
  private finishRec() {
    const r = this.rec; this.rec = null;
    if (!r) return;
    const total = r.peaks.length / 2, spb = this.spb();
    const bounds = [0, ...this.passMarks(r, total), total];
    const passes: [Float32Array, number][] = [];
    for (let k = 0; k < bounds.length - 1; k++) {
      const n = bounds[k + 1] - bounds[k];
      const cutShort = k > 0 && k === bounds.length - 2;
      if (n < (cutShort ? 100 * spb : 10)) continue; // a pass cut off right after the wrap is not a take
      passes.push([Float32Array.from(r.peaks.slice(bounds[k] * 2, bounds[k + 1] * 2)), k === 0 ? r.clipStart : r.lap![0]]);
    }
    if (!passes.length) return;
    const prev = structuredClone(this.p);
    const landed: [Id, number, number][] = [];
    for (const [pk, start] of passes) {
      const id = this.p.nextId++, dur = pk.length / 2 / 100;
      const src: MockSource = { id, name: 'Take ' + (++this.takeN), color: '#d9678f', path: 'recordings/take' + this.takeN + '.wav', dur, sr: 48000, ch: 1, peakDb: -6, seed: id };
      this.srcMeta.set(id, src);
      this.recPeaks.set(id, pk);
      this.p.sources.push(strip(src));
      landed.push([id, start, dur / spb]);
    }
    landTakes(this.p, r.track, landed, () => this.p.nextId++);
    this.pushUndo(prev); this.changed();
  }
  private recPeaks = new Map<Id, Float32Array>();

  // ---------- telemetry ----------
  async subscribeTelemetry(cb: (b: ArrayBuffer) => void) { this.teleCb = cb; }
  async watchFx(ids: Id[]) { this.watched = ids.slice(0, 8); }

  private tick() {
    if (!this.teleCb) return;
    const seq = ++this.seq, beat = this.beatNow(), playing = this.tr.playing, t = performance.now() / 1000;
    const spb = this.spb(), frac = ((beat % 1) + 1) % 1;
    // meters: inserts whose tracks have a live clip under the playhead
    const live = new Map<Id, number>();
    if (playing && beat >= 0) for (const c of this.p.clips) {
      if (c.muted || beat < c.start || beat >= c.start + c.len) continue;
      const tr = this.p.tracks[c.track]; if (!tr || tr.muted) continue;
      const lvl = (c.kind === 'pat' ? 0.25 + 0.55 * Math.exp(-frac * 5) : 0.45 + 0.2 * Math.sin(t * 7)) * 2 * tr.vol ** 3 * dbg(c.gain);
      live.set(tr.insert, Math.max(live.get(tr.insert) ?? 0, lvl));
    }
    // pattern loop preview and auditions (engine: rt.rs schedule_pattern / Cmd::Audition)
    let patBeat = -1;
    if (this.patPlay) {
      const pt = this.p.patterns.find(x => x.id === this.patPlay!.id);
      if (!pt) this.patPlay = null;
      else {
        patBeat = ((performance.now() - this.patPlay.t0) / 1000 / spb) % pt.len;
        const ins = this.patHome(pt.id) ?? 0, pf = ((patBeat % 1) + 1) % 1;
        live.set(ins, Math.max(live.get(ins) ?? 0, 0.25 + 0.55 * Math.exp(-pf * 5)));
      }
    }
    for (const [id, at] of this.flash) {
      const age = (performance.now() - at) / 1000;
      if (age > 0.6) { this.flash.delete(id); continue; }
      live.set(id, Math.max(live.get(id) ?? 0, 0.7 * Math.exp(-age * 9)));
    }
    let master = 0;
    const lvOf = new Map<Id, number>();
    for (const m of this.p.inserts) if (m.id !== 0) {
      const lv = (live.get(m.id) ?? 0) * (m.muted ? 0 : 2 * m.vol ** 3) * (0.9 + 0.1 * Math.random());
      lvOf.set(m.id, lv); master += lv * 0.6;
    }
    const ins = this.p.inserts.map(m => {
      const lv = m.id === 0 ? Math.min(1.2, master * (m.muted ? 0 : 2 * m.vol ** 3)) : lvOf.get(m.id) ?? 0;
      return { id: m.id, l: lv * Math.min(1, 1 - m.pan), r: lv * Math.min(1, 1 + m.pan) };
    });
    const insLvl = new Map(ins.map(e => [e.id, Math.max(e.l, e.r)]));
    const fx: EncFx[] = [];
    if (seq % 2 === 0) for (const id of this.watched) {
      const found = this.findFx(id); if (!found) continue;
      fx.push(this.fakeAnalysis(found.fx, found.ins, playing ? (insLvl.get(found.ins) ?? 0) + 0.05 : 0, t, beat, playing));
    }
    let rec = null;
    if (this.rec && playing && this.rawBeat() >= this.rec.clipStart) {
      const r = this.rec;
      if (!r.t0) r.t0 = performance.now();
      const want = Math.floor((performance.now() - r.t0) / 10);
      while (r.peaks.length / 2 < want) { const a = 0.15 + 0.6 * Math.abs(Math.sin(r.peaks.length / 37)) * Math.random(); r.peaks.push(-a, a); }
      // the live clip restarts every loop pass, like the engine's telemetry
      const marks = this.passMarks(r, r.peaks.length / 2), pass = marks.length, p0 = pass ? marks[pass - 1] : 0;
      if (pass !== r.pass) { r.pass = pass; r.sent = p0; }
      const off = r.sent - p0; r.sent = r.peaks.length / 2;
      rec = { track: r.track, clipStart: pass ? r.lap![0] : r.clipStart, seconds: (r.peaks.length / 2 - p0) / 100, peakOffset: off, peaks: r.peaks.slice((off + p0) * 2) };
    }
    let previewPos = -1;
    if (this.preview) {
      previewPos = this.preview.off + (performance.now() - this.preview.t0) / 1000;
      if (previewPos > this.preview.off + this.preview.dur) { this.preview = null; previewPos = -1; }
    }
    const inLevel = this.tr.rec ? 0.2 + 0.5 * Math.abs(Math.sin(t * 3)) * Math.random() : 0;
    void spb;
    this.teleCb(encodeFrame({ seq, beat, playing, recording: !!this.rec && this.rawBeat() >= (this.rec?.clipStart ?? 0), previewing: !!this.preview,
      inLevel, previewPos, cpu: playing ? 0.12 + 0.03 * Math.random() : 0.02, ins, fx, rec, patBeat }));
  }

  private findFx(id: Id): { fx: Fx; ins: Id } | null {
    for (const m of this.p.inserts) { const f = m.fx.find(x => x.id === id); if (f) return { fx: f, ins: m.id }; }
    return null;
  }

  private sims = new Map<Id, FxSim>();
  /** The vocal heard on insert `ins` now: a vocal clip under the playhead, or (for vocal processors) the looping demo take. */
  private vocalFor(fx: Fx, ins: Id, beat: number, t: number, playing: boolean): VocalSample | null {
    if (!playing) return null;
    for (const c of this.p.clips) {
      if (c.kind !== 'audio' || c.muted || beat < c.start || beat >= c.start + c.len) continue;
      const tr = this.p.tracks[c.track]; if (!tr || tr.insert !== ins || tr.muted) continue;
      const m = this.srcMeta.get(c.ref); if (!m?.vocal) continue;
      return vocalAt(synthVocal(m.seed, m.dur), (c.offset + beat - c.start) * this.spb());
    }
    return fx.type === 'tune' || fx.type === 'voice' || fx.type === 'deess' || fx.type === 'fet' ? vocalAt(synthVocal(4242, VOCAL_DUR), t) : null;
  }

  private fakeAnalysis(fx: Fx, ins: Id, level: number, t: number, beat: number, playing: boolean): EncFx {
    const specIn = new Float32Array(SPEC_BANDS), specOut = new Float32Array(SPEC_BANDS);
    const scopeIn = new Float32Array(SCOPE_LEN), scopeOut = new Float32Array(SCOPE_LEN);
    const meters = new Float32Array(32);
    let sim = this.sims.get(fx.id); if (!sim) { sim = newSim(); this.sims.set(fx.id, sim); }
    const dt = sim.t ? clamp(t - sim.t, 0, 0.25) : 1 / 30; sim.t = t;
    const voc = this.vocalFor(fx, ins, beat, t, playing);
    if (voc) { voc.level *= 0.4; level = voc.level; }
    const lvDb = level > 1e-4 ? 20 * Math.log10(level) : -120;
    const pulse = voc ? 1 - voc.level : Math.exp(-(((beat % 1) + 1) % 1) * 4);
    if (voc) vocalSpec(voc, specIn);
    else {
      for (let k = 0; k < SPEC_BANDS; k++) {
        const f = BAND_F[k], oct = Math.log2(f / 1000);
        const shape = -4.5 * oct - 6 * Math.max(0, oct - 2.5) ** 2 + 10 * Math.exp(-((Math.log2(f / 70)) ** 2) * 2) * pulse;
        specIn[k] = level > 0 ? lvDb - 28 + shape + (Math.random() - 0.5) * 5 : -120;
      }
    }
    const pr = fx.params;
    let gr = 0, out = level;
    if (fx.type === 'eq' && fx.bands) {
      // dynamic bands: the band's level vs its threshold, 6 dB soft knee, full range 12 dB over (dsp.rs)
      fx.bands.forEach((b, k) => {
        if (!b.on || !b.dynamic) { sim!.dyn[k] = 0; return; }
        const bi = clamp(Math.round(Math.log(b.f / 20) / Math.log(1000) * (SPEC_BANDS - 1)), 0, SPEC_BANDS - 1);
        let e = 0; for (let j = Math.max(0, bi - 3); j <= Math.min(SPEC_BANDS - 1, bi + 3); j++) e += Math.pow(10, specIn[j] / 10);
        const lvl = 10 * Math.log10(e + 1e-12) + 6;
        const target = b.dynamic.range * clamp((lvl - b.dynamic.thr + 3) / 15, 0, 1);
        const tau = Math.abs(target) > Math.abs(sim!.dyn[k]) ? 0.01 : 0.08; // fast in, slower out
        sim!.dyn[k] += (target - sim!.dyn[k]) * (1 - Math.exp(-dt / tau));
        meters[k] = sim!.dyn[k];
      });
      const c = eqCurve(fx.bands, pr.out ?? 0, BAND_F, new Float32Array(SPEC_BANDS), 'mid', sim.dyn);
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + (fx.bypass ? 0 : c[k]);
    } else if (fx.type === 'tune') {
      this.simTune(fx, sim, voc, dt);
      for (let i = 0; i < 6; i++) meters[i] = sim.m[i];
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k];
    } else if (fx.type === 'voice') {
      // vocal.rs `Voice`: [input Hz, output Hz, confidence, applied shift st]
      const mode = Math.round(pr.mode ?? 0), pitch = pr.pitch ?? 0, drive = pr.drive ?? 0;
      if (voc && voc.midi != null && voc.level > 0.02) {
        const m = voc.midi, shift = clamp(mode === 1 ? Math.round(m) - m + pitch : mode === 2 ? 60 + Math.round(pitch) - m : pitch, -24, 24);
        meters[0] = midiToHz(m); meters[1] = midiToHz(m + shift); meters[2] = 0.88 + Math.random() * 0.1; meters[3] = shift;
      } else meters[2] = 0.15 + Math.random() * 0.2;
      const fShift = (pr.formant ?? 0) + ((pr.link ?? 0) >= 0.5 ? meters[3] : 0);
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[clamp(Math.round(k - fShift / 12 * (SPEC_BANDS / 10)), 0, SPEC_BANDS - 1)] + drive * 0.15 * (k > 60 ? 2 : 1) + (pr.out ?? 0);
      out = level * dbg(pr.out ?? 0);
    } else if (fx.type === 'fet') {
      const inDb = level > 1e-5 ? 20 * Math.log10(level) + (pr.input ?? 8) : -120;
      const ri = Math.round(pr.ratio ?? 0), abi = ri >= 4, ratio = [4, 8, 12, 20, 20][clamp(ri, 0, 4)];
      const over = inDb + 10, knee = abi ? 2 : ratio <= 4 ? 8 : 4, slope = 1 / ratio - 1;
      let target = over <= -knee / 2 ? 0 : over >= knee / 2 ? slope * over : slope * (over + knee / 2) ** 2 / (2 * knee);
      if (abi && over > 0) target -= 4 * Math.min(1, over / 6) * (1 - Math.min(1, over / 30));
      const rel = (pr.release ?? 300) / 1000 * (abi ? 1.4 : 1);
      sim.gr = target < sim.gr ? target : target + (sim.gr - target) * Math.exp(-dt / rel);
      gr = sim.gr;
      const outDb = inDb + gr + (pr.output ?? 0);
      meters[0] = gr; meters[1] = Math.max(-120, inDb); meters[2] = Math.max(-120, outDb);
      out = dbg(outDb);
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + (pr.input ?? 8) + gr + (pr.output ?? 0);
    } else if (fx.type === 'denoise') {
      // noise floor ~ −62 dB tilted toward the lows, per-band reduction where the input is near it
      const red = pr.reduction ?? 12;
      let tot = 0;
      for (let b = 0; b < 16; b++) {
        const f = 50 * Math.pow(320, (b + 0.5) / 16), nz = -58 - 10 * Math.log10(f / 50);
        let near = 0, c = 0;
        for (let k = 0; k < SPEC_BANDS; k++) if (BAND_F[k] >= 50 * Math.pow(320, b / 16) && BAND_F[k] < 50 * Math.pow(320, (b + 1) / 16)) { near += specIn[k]; c++; }
        const lv = c ? near / c : -120, snr = lv - nz;
        const gdb = -Math.min(red, Math.max(0, 10 - snr) * red / 10);
        meters[b] = nz; meters[16 + b] = gdb; tot += gdb;
      }
      for (let k = 0; k < SPEC_BANDS; k++) { const b = Math.max(0, Math.min(15, Math.floor(Math.log(BAND_F[k] / 50) / Math.log(320) * 16))); specOut[k] = specIn[k] + meters[16 + b]; }
      gr = tot / 16; out = level * dbg(gr * 0.3);
    } else if (fx.type === 'deess') {
      const sib = voc?.sib ?? 0, lv = voc?.level ?? 0;
      const hiDb = lv > 1e-4 ? 20 * Math.log10(lv) + (sib > 0 ? 3 + 4 * sib : -26 + (Math.random() - 0.5) * 3) : -120;
      const over = Math.max(0, hiDb - (pr.thr ?? -32));
      const target = -Math.min(over * 0.8 * sib, pr.range ?? 8);
      sim.gr = target < sim.gr ? target + (sim.gr - target) * Math.exp(-dt / 0.004) : target + (sim.gr - target) * Math.exp(-dt / 0.06);
      gr = sim.gr;
      meters[0] = gr; meters[1] = Math.max(-120, hiDb); meters[2] = sib; meters[3] = pr.thr ?? -32;
      out = level * dbg(gr * (pr.mode ? 1 : 0.85));
      const fc = pr.freq ?? 6000;
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + (pr.mode || BAND_F[k] >= fc ? gr : 0);
    } else if (fx.type === 'comp' || fx.type === 'limit') {
      const inDb = lvDb + (fx.type === 'limit' ? pr.gain : 0), thr = fx.type === 'limit' ? pr.ceil : pr.thr;
      const over = inDb - thr;
      gr = level > 0 && over > 0 ? -(fx.type === 'limit' ? over : over - over / pr.ratio) : 0;
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + gr + (pr.gain ?? 0);
      out = level * dbg(gr);
    } else if (fx.type === 'reverb') {
      const hi = pr.hi ?? 8000, lo = pr.lo ?? 120, wet = 20 * Math.log10(Math.max(1e-3, (pr.mix ?? 25) / 100));
      const sustain = 6 * Math.log10(1 + (pr.size ?? 2));
      for (let k = 0; k < SPEC_BANDS; k++) {
        const f = BAND_F[k], r = (f / lo) ** 2;
        specOut[k] = level > 0 ? specIn[k] - 6 * (1 - pulse) + sustain + wet - 10 * Math.log10(1 + (f / hi) ** 2) + 20 * Math.log10(r / Math.sqrt(1 + r * r)) : -120;
      }
    } else if (fx.type === 'sat') {
      // harmonics grow with the drive; auto gain keeps the level (dsp/saturator.rs)
      const dd = satDriveDb(pr);
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + (pr.auto ?? 1 ? 0 : dd * 0.5) + (k > 60 ? dd * 0.3 : 0);
    } else {
      for (let k = 0; k < SPEC_BANDS; k++) specOut[k] = specIn[k] + 20 * Math.log10(1 + (pr.mix ?? 25) / 100);
    }
    const drive = fx.type === 'sat' ? dbg(satDriveDb(pr)) * 0.6 : 1, f0 = 2 + 3 * Math.sin(t * 0.3);
    for (let i = 0; i < SCOPE_LEN; i++) {
      const x = level * (Math.sin(i / SCOPE_LEN * Math.PI * 2 * f0 + t * 9) * 0.8 + (Math.random() - 0.5) * 0.12);
      scopeIn[i] = x;
      scopeOut[i] = fx.type === 'sat' ? Math.tanh(x * drive) / ((pr.auto ?? 1) ? drive : 0.6) * dbg(pr.out ?? 0) : x * dbg(gr);
    }
    return { id: fx.id, inPeak: level, outPeak: out, grDb: gr, meters, specIn, specOut, scopeIn, scopeOut };
  }

  /** vocal.rs `Tune::process`, stepped every 4 ms hop. */
  private simTune(fx: Fx, s: FxSim, voc: VocalSample | null, dt: number) {
    const p = fx.params, key = Math.round(p.key ?? 0), mask = scaleMask(p.scale ?? 0);
    const vib = (p.vibrato ?? 0) / 100, flex = (p.flex ?? 0) / 100, hum = (p.humanize ?? 0) / 100, tr = p.transpose ?? 0;
    const hop = 0.004, steps = Math.max(1, Math.round(dt / hop));
    for (let i = 0; i < steps; i++) {
      if (voc && voc.midi != null && voc.level > 0.02) {
        const m = voc.midi + (Math.random() - 0.5) * 0.02;
        if (s.center === 0 || Math.abs(m - s.center) > 1.5) s.center = m;
        s.center += (m - s.center) * Math.min(1, hop / 0.15);
        const fol = s.center * vib + m * (1 - vib);
        const note = snapNote(fol, key, mask);
        if (note !== s.last) { s.last = note; s.held = 0; } else s.held += hop;
        const err = note - fol, win = 1 - 0.9 * flex;
        const want = flex > 0 && Math.abs(err) > win * 0.5 ? 0 : err;
        let tau = (p.retune ?? 20) / 1000;
        if (hum > 0 && s.held > 0.25) tau += hum * 0.4 * Math.min(1, (s.held - 0.25) / 0.5);
        const k = tau <= 0.0005 ? 1 : 1 - Math.exp(-hop / tau);
        s.corr += (want - s.corr) * k;
        s.m = [midiToHz(m), midiToHz(note + tr), midiToHz(m + s.corr + tr), 0.88 + Math.random() * 0.1, (m - note) * 100, s.corr * 100];
      } else {
        s.corr *= 0.9; s.center = 0;
        s.m = [0, 0, 0, 0.15 + Math.random() * 0.2, 0, 0];
      }
    }
  }

  // ---------- catalog / audio files ----------
  async fxCatalog() { await delay(); return structuredClone(this.cat); }
  async importAudio(a: { paths: string[]; track?: number; beat?: number }) {
    await delay(30);
    const prev = structuredClone(this.p);
    const sourceIds: Id[] = [], clipIds: Id[] = [];
    a.paths.forEach((path, k) => {
      // a file of the sample library is one source however often it is used (engine.rs `import_audio`)
      const lib = path.startsWith(LIB_SCHEME), had = lib ? this.p.sources.find(s => s.path === path) : undefined;
      const fac = factoryFile(path);
      const name = fac?.name ?? (path.split(/[\\/]/).pop() ?? path).replace(/\.[^.]+$/, '');
      const h = hash(path), id = had?.id ?? this.p.nextId++;
      const vocal = isVocalName(name);
      const mins = /(\d+)\s*min/i.exec(name); // e.g. "Long take 4min.wav" (dev/perf checks)
      const dur = had?.dur ?? fac?.dur ?? (mins ? +mins[1] * 60 : vocal ? VOCAL_DUR : /loop/i.test(name) ? 4 * 4 * this.spb() : 0.6 + (h % 700) / 100);
      if (!had) {
        const src: MockSource = fac
          ? { id, name, color: fac.color, path, dur, sr: 44100, ch: fac.ch, peakDb: -1, seed: h }
          : { id, name, color: vocal ? '#d9678f' : TC[(this.p.sources.length + 3) % TC.length], path, dur, sr: 48000, ch: vocal ? 1 : 2, peakDb: -1 - (h % 60) / 10, seed: h, vocal };
        this.srcMeta.set(id, src); this.p.sources.push(strip(src));
      }
      sourceIds.push(id);
      if (a.track != null && a.beat != null) {
        const tr = clamp(a.track + k, 0, this.p.tracks.length - 1), cid = this.p.nextId++;
        const len = Math.min(dur / this.spb(), 512 - a.beat);
        this.p.clips.push(mkClip(cid, tr, a.beat, len, 'audio', id)); clipIds.push(cid);
      }
    });
    if (clipIds.length) this.pushUndo(prev);
    this.changed();
    return { sourceIds, clipIds };
  }
  async pickAudioFiles(): Promise<string[] | null> {
    return new Promise(res => {
      const inp = document.createElement('input');
      inp.type = 'file'; inp.accept = 'audio/*'; inp.multiple = true;
      inp.onchange = () => res(inp.files && inp.files.length ? [...inp.files].map(f => f.name) : null);
      inp.click();
    });
  }
  async sourcePeaks(id: Id, norm?: boolean, rev?: boolean) {
    await delay(5);
    const s = this.srcMeta.get(id) ?? this.p.sources.find(x => x.id === id);
    if (!s) throw new Error('no source ' + id);
    let pk: Float32Array;
    const meta = this.srcMeta.get(id);
    if (this.recPeaks.has(id)) pk = this.recPeaks.get(id)!.slice();
    else if (meta?.trimFrom) {
      const full = await this.sourcePeaks(meta.trimFrom.id);
      const a = Math.round(meta.trimFrom.offSec * 100) * 2;
      pk = full.slice(a, a + Math.round(s.dur * 100) * 2);
    } else if (meta?.vocal) pk = vocalPeaks(synthVocal(meta.seed, s.dur));
    else if (s.path.startsWith(LIB_SCHEME)) pk = oneShotPeaks(meta?.seed ?? id, s.dur);
    else pk = synthPeaks(meta?.seed ?? id, s.dur, 128);
    if (rev) { const n = pk.length / 2, o = new Float32Array(pk.length); for (let i = 0; i < n; i++) { o[2 * i] = pk[2 * (n - 1 - i)]; o[2 * i + 1] = pk[2 * (n - 1 - i) + 1]; } pk = o; }
    if (norm) { let m = 0; for (const v of pk) m = Math.max(m, Math.abs(v)); const g = m > 1e-6 ? dbg(-0.3) / m : 1; for (let i = 0; i < pk.length; i++) pk[i] *= g; }
    return pk;
  }
  private renderReady = 0;
  async clipPeaks(clipId: Id) {
    await delay(Math.max(0, this.renderReady - performance.now())); // the render of the last gesture
    const c = this.p.clips.find(x => x.id === clipId); if (!c || c.kind !== 'audio') throw new Error('no such clip');
    const tune = structuredClone(c.tune ?? null);
    const pk = await this.sourcePeaks(c.ref, c.norm, c.rev);
    await delay(20);
    if (tune?.notes.length) {
      const n = pk.length / 2;
      for (let i = 0; i < n; i++) {
        const j = noteAt(tune.notes, i / 100); if (j < 0) continue;
        const nt = tune.notes[j], edge = clamp(Math.min(i / 100 - nt.start, nt.end - i / 100) / 0.01, 0, 1);
        const g = 1 + (dbg(nt.gain) - 1) * edge;
        pk[2 * i] = clamp(pk[2 * i] * g, -1, 0); pk[2 * i + 1] = clamp(pk[2 * i + 1] * g, 0, 1);
      }
    }
    return pk;
  }
  async pitchAnalyze(clipId: Id): Promise<PitchAnalysis> {
    await delay(120); // the engine analyses on a worker thread; keep the loading state visible
    const c = this.p.clips.find(x => x.id === clipId);
    if (!c) throw new Error('no such clip');
    if (c.kind !== 'audio') throw new Error('only audio clips have pitch');
    const s = this.srcMeta.get(c.ref) ?? this.p.sources.find(x => x.id === c.ref) as MockSource | undefined;
    if (!s) throw new Error('audio not loaded');
    const v = synthVocal(s.seed ?? s.id, s.dur);
    return structuredClone({ rate: v.rate, curve: v.curve, levels: v.levels, notes: v.notes, confidence: v.curve.map(x => (x == null ? 0 : 0.9)), backend: this.f0Backend });
  }
  async pitchTarget(clipId: Id, reference?: RefTune): Promise<PitchTarget> {
    const c = this.p.clips.find(x => x.id === clipId);
    if (!c || c.kind !== 'audio') throw new Error('only audio clips have pitch');
    const p = reference ?? c.tune?.reference;
    if (!p) throw new Error('this clip has no reference');
    const take = await this.pitchAnalyze(clipId);
    const rs = this.srcMeta.get(p.source) ?? this.p.sources.find(x => x.id === p.source) as MockSource | undefined;
    if (!rs) throw new Error(`no reference source ${p.source}`);
    const rv = synthVocal(rs.seed ?? rs.id, rs.dur);
    const notes = c.tune?.notes.length ? c.tune.notes : take.notes;
    return simpleTarget(take, { rate: rv.rate, curve: rv.curve, levels: rv.levels, notes: rv.notes }, notes, p);
  }
  private f0Backend = 'yin';
  async f0ModelStatus(): Promise<F0ModelStatus> {
    return {
      id: 'rmvpe', name: 'RMVPE vocal pitch model', bytes: 361_688_443, installed: this.f0Backend === 'rmvpe', backend: this.f0Backend,
      licenseNote: 'RMVPE weights from the RVC project (lj1995/VoiceConversionWebUI). The RVC code is MIT and the RMVPE reference code Apache-2.0, but the weights\' licence file restricts use to research. Downloading is your decision; without the model WAVE uses its built-in YIN detector.',
      licenseUrl: 'https://huggingface.co/lj1995/VoiceConversionWebUI/blob/main/%E4%BD%BF%E7%94%A8%E9%9C%80%E9%81%B5%E5%AE%88%E7%9A%84%E5%8D%8F%E8%AE%AE-LICENSE.txt',
      path: 'C:\\Users\\michal\\AppData\\Local\\WAVE\\Models\\rmvpe.onnx', error: null,
    };
  }
  async f0ModelInstall(): Promise<F0ModelStatus> {
    const total = 361_688_443;
    for (let k = 1; k <= 20; k++) { await delay(50); this.emit('model-progress', { id: 'rmvpe', done: Math.round(total * k / 20), total }); }
    this.f0Backend = 'rmvpe';
    return this.f0ModelStatus();
  }
  async musicAnalyze(a?: { clipId?: Id; excludeFx?: Id }): Promise<MusicAnalysis> {
    await delay(80);
    if (a?.clipId != null) {
      const c = this.p.clips.find(x => x.id === a.clipId);
      if (!c) throw new Error('no such clip');
      if (c.kind !== 'audio') throw new Error('only audio clips have tempo and key');
      const s = this.p.sources.find(x => x.id === c.ref);
      const bpm = Math.round(this.p.bpm);
      // C minor profile (demo song is in C minor at 128 BPM)
      const root = (s ? s.id % 2 === 0 ? 0 : 0 : 0), minor = true;
      const chroma = [1, 0.08, 0.42, 0.78, 0.09, 0.51, 0.07, 0.89, 0.36, 0.12, 0.44, 0.19];
      return {
        tempo: {
          bpm,
          confidence: 0.86,
          candidates: [
            { bpm, score: 1 },
            { bpm: bpm / 2, score: 0.58 },
            { bpm: bpm * 2, score: 0.44 },
          ],
          firstBeat: 0,
        },
        key: {
          root, minor, confidence: 0.84,
          candidates: [
            { root, minor: true, score: 0.91 },
            { root: (root + 3) % 12, minor: false, score: 0.78 },
            { root: (root + 7) % 12, minor: true, score: 0.62 },
          ],
          chroma,
          tuningCents: 0,
        },
      };
    }
    const exIns = a?.excludeFx != null ? this.p.inserts.find(ins => ins.fx.some(f => f.id === a.excludeFx))?.id : undefined;
    const anySolo = this.p.tracks.some(t => t.solo);
    const active = this.p.clips.filter(c => {
      if (c.kind !== 'audio' || c.muted) return false;
      const tr = this.p.tracks[c.track];
      return tr && !tr.muted && (!anySolo || tr.solo);
    });
    const nonEx = exIns != null ? active.filter(c => this.p.tracks[c.track]?.insert !== exIns) : active;
    const chosen = nonEx.length ? nonEx : active;
    if (!chosen.length) return { tempo: null, key: null };
    return this.musicAnalyze({ clipId: chosen[0].id });
  }
  /** Same contract as the engine's `auto_vocal_plan`, with fixed measurements of a typical take
   *  (the real calibration renders the audio; the mock has no audio to render). */
  async autoVocalPlan(a: { clipId: Id; beatClipId?: Id; style?: AutoVocalStyle; bakeRider?: boolean }): Promise<AutoVocalPlan> {
    await delay(300);
    const c = this.p.clips.find(x => x.id === a.clipId);
    if (!c) throw new Error('no such clip');
    if (c.kind !== 'audio') throw new Error('only audio clips have pitch');
    const style = a.style ?? 'natural';
    const music = a.beatClipId != null ? await this.musicAnalyze({ clipId: a.beatClipId }) : await this.musicAnalyze();
    const cat = mockCatalog();
    const params = (t: AnyBuiltinKind, kv: Record<string, number>) =>
      ({ ...Object.fromEntries(cat.types[t]!.params.map(d => [d.id, d.def])), ...kv });
    const hard = style === 'hard';
    const key = music.key;
    const names = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];
    const keyName = key ? `${names[key.root]} ${key.minor ? 'minor' : 'major'}` : '';
    const tune = key
      ? params('tune', { key: key.root, scale: key.minor ? 2 : 1, retune: hard ? 0 : 40, humanize: hard ? 0 : 30, flex: hard ? 0 : 40, vibrato: hard ? 20 : 60 })
      : params('tune', {});
    const chain: AutoVocalPlan['chain'] = [
      { type: 'tune', params: tune, mix: 1, enabled: !!key, note: key ? `${keyName} from the beat (confidence ${key.confidence.toFixed(2)})` : 'no key detected in the beat: pitch correction left off' },
      { type: 'rider', params: params('rider', { amount: 50, phrase: 20, up: 5, down: 6, speed: hard ? 1 : 0, auto: 0, target: -24, floor: -70, gate: -48, look: 1, out: 0 }), mix: 1, enabled: true, note: 'target -24.0 dB, noise floor -70.0 dB; gain p5/p50/p95 -2.1/0.3/2.4 dB' },
      { type: 'fet', params: params('fet', { input: 12, output: -12, ratio: 0, attack: 800, release: 60, mix: 100 }), mix: 1, enabled: true, note: 'fast 4:1 peak stage, 4.8 dB on the loudest 1 %' },
      { type: 'comp', params: params('comp', { thr: -32, ratio: 2.5, att: 10, rel: 250, gain: 0, det: 1 }), mix: 1, enabled: true, note: 'slow 2.5:1 stage, 4.1 dB mean reduction' },
      { type: 'deess', params: params('deess', { thr: -9, range: 12, freq: 5000, mode: 0, look: 5, listen: 0, detect: 1 }), mix: 1, enabled: true, note: 'sibilance -4.0 → -5.1 dB (pro median -6.0)' },
      { type: 'reverb', params: params('reverb', { size: 1.2, pre: 30, lo: 300, hi: 7000, width: 100, mix: MOCK_REVERB_MIX, tone: 0, space: 0, duck: 0, bass: 1 }), mix: 1, enabled: true, note: 'small plate, T60 1.2 s, -18 dB send' },
      { type: 'limit', params: params('limit', { gain: 0, ceil: -1, rel: 80, auto: 0, look: 5, link: 100, out: 0 }), mix: 1, enabled: true, note: 'true-peak ceiling -1 dBTP' },
    ];
    const ri = chain.findIndex(p => p.type === 'rider');
    const riderBake = a.bakeRider ? chain.splice(ri, 1)[0] : null;
    return {
      version: 1, style, riderBake,
      analysis: {
        durationS: c.len * 60 / this.p.bpm, sampleRate: 48000, peakDbfs: -3.2, truePeakDbtp: -3.0, lufs: -21.5,
        wordLevelDb: -24, noiseFloorDb: -70, sungShare: 0.62, sibilanceDb: -4,
        key: key ? { root: key.root, minor: key.minor, name: keyName, confidence: key.confidence } : null,
        scaleShare: key ? 0.86 : null, pitchOffCents: key ? 18 : null, beatLufs: a.beatClipId != null ? -11 : null,
      },
      chain,
      trackGainDb: a.beatClipId != null ? 6.4 : null,
      stages: { riderGainDb: [-2.1, 0.3, 2.4], fetP99GrDb: [4.8, 4.8], compMeanGrDb: [4.1, 4.1], sibilanceAfterDb: [-5.1, -5.1], iqr400Db: [5.6, 2.3], lufsAfter: -23.6, truePeakAfterDbtp: -6.2 },
      warnings: [],
    };
  }
  private liveAudition(): Record<string, Id> {
    const out: Record<string, Id> = {};
    for (const [k, t] of Object.entries(this.audition)) if (this.p.comps?.some(c => String(c.id) === k && c.takes.some(x => x.id === t))) out[k] = t;
    return out;
  }
  async compAudition(compId: Id, takeId: Id | null) {
    const cp = this.p.comps?.find(c => c.id === compId);
    if (!cp) throw new Error('no comp ' + compId);
    if (takeId != null && !cp.takes.some(t => t.id === takeId)) throw new Error(`comp ${compId} has no take ${takeId}`);
    if (takeId == null) delete this.audition[compId]; else this.audition[compId] = takeId;
    this.emit('state', this.dto());
  }
  async flattenComp(clipId: Id) {
    const c = this.p.clips.find(x => x.id === clipId);
    if (!c || c.kind !== 'comp') throw new Error('only comp clips can be flattened');
    const cp = this.p.comps?.find(x => x.id === c.ref); if (!cp) throw new Error("the clip's comp is missing");
    const spb = this.spb(), n = Math.round(c.len * spb * 100), out = new Float32Array(n * 2);
    const pk = new Map<Id, Float32Array>();
    for (const t of cp.takes) pk.set(t.id, await this.sourcePeaks(t.ref));
    for (let i = 0; i < n; i++) {
      const cb = c.offset + i / 100 / spb;
      const r = cp.regions.find(x => cb >= x.from && cb < x.to); if (!r) continue;
      const t = cp.takes.find(x => x.id === r.take)!, src = pk.get(t.id)!;
      if (cb >= takeEnd(t)) continue;
      const j = Math.floor((cb - t.start + t.offset) * spb * 100);
      if (j >= 0 && j * 2 + 1 < src.length) { out[2 * i] = src[2 * j]; out[2 * i + 1] = src[2 * j + 1]; }
    }
    const prev = structuredClone(this.p), id = this.p.nextId++;
    const src: MockSource = { id, name: cp.name + ' (flat)', color: '#e05263', path: 'recordings/' + cp.name + ' flat.wav', dur: n / 100, sr: 48000, ch: 1, peakDb: -3, seed: id };
    this.srcMeta.set(id, src); this.recPeaks.set(id, out); this.p.sources.push(strip(src));
    Object.assign(this.p.clips.find(x => x.id === clipId)!, { kind: 'audio', ref: id, offset: 0, norm: false, rev: false, tune: null });
    if (!this.p.clips.some(x => x.kind === 'comp' && x.ref === cp.id)) { this.p.comps = this.p.comps!.filter(x => x.id !== cp.id); delete this.audition[cp.id]; }
    this.pushUndo(prev); this.changed();
    return { sourceId: id };
  }
  /** The mock has no audio to render: the clip moves to a copy of its region (clip gain applied), as one undo step. */
  async bakeRider(a: { clipId: Id; params: Record<string, number> }) {
    await delay(200);
    const c = this.p.clips.find(x => x.id === a.clipId);
    if (!c || c.kind !== 'audio') throw new Error('only audio clips can be baked');
    const s = this.srcMeta.get(c.ref); if (!s) throw new Error('audio not loaded');
    const prev = structuredClone(this.p), id = this.p.nextId++;
    const spb = this.spb(), a0 = c.offset * spb, dur = Math.max(0, Math.min(s.dur, (c.offset + c.len) * spb) - a0);
    const src: MockSource = { ...s, id, name: s.name + ' (rider)', path: s.path.replace(/\.wav$/i, '') + ' rider.wav', dur, trimFrom: undefined };
    this.srcMeta.set(id, src);
    const pk = await this.sourcePeaks(c.ref, c.norm, c.rev);
    const g = dbg(c.gain);
    this.recPeaks.set(id, pk.slice(Math.floor(a0 * 100) * 2, Math.ceil((a0 + dur) * 100) * 2).map(v => v * g));
    this.p.sources.push(strip(src));
    Object.assign(c, { ref: id, offset: 0, gain: 0, norm: false, rev: false, tune: null });
    this.pushUndo(prev); this.changed();
    return { sourceId: id, clipId: c.id };
  }
  async trimClip(clipId: Id) {
    const c = this.p.clips.find(x => x.id === clipId); if (!c) throw new Error('no clip');
    const s = this.srcMeta.get(c.ref); if (!s) throw new Error('no source');
    const prev = structuredClone(this.p), spb = this.spb(), id = this.p.nextId++;
    const ns: MockSource = { ...s, id, dur: c.len * spb, path: s.path.replace(/(\.\w+)?$/, '-trim.wav'), trimFrom: { id: s.id, offSec: c.offset * spb } };
    this.srcMeta.set(id, ns); this.p.sources.push(strip(ns));
    Object.assign(this.p.clips.find(x => x.id === clipId)!, { ref: id, offset: 0, norm: false, rev: false });
    this.pushUndo(prev); this.changed();
    return { sourceId: id };
  }
  async previewClip(clipId: Id) {
    const c = this.p.clips.find(x => x.id === clipId); if (!c) return;
    this.preview = { t0: performance.now(), dur: c.len * this.spb(), off: c.offset * this.spb() };
  }
  async previewFile(path: string) { this.preview = { t0: performance.now(), dur: 0.6 + (hash(path) % 400) / 100, off: 0 }; }
  async previewStop() { this.preview = null; }
  /** Insert of the track of the pattern's first clip (rt.rs `PatRt.home`). */
  private patHome(id: Id): Id | null {
    const c = this.p.clips.filter(x => x.kind === 'pat' && x.ref === id).sort((a, b) => a.start - b.start)[0];
    return c ? this.p.tracks[c.track]?.insert ?? null : null;
  }
  async noteAudition(a: { ch: Id; key?: number; vel?: number; pattern?: Id }) {
    const c = this.p.channels?.find(x => x.id === a.ch); if (!c) throw new Error('no channel ' + a.ch);
    const heard = this.p.patterns.find(pt => pt.notes.some(n => n.ch === c.id));
    const ins = c.insert ?? (a.pattern != null ? this.patHome(a.pattern) : null) ?? (heard ? this.patHome(heard.id) : null) ?? 0;
    this.flash.set(ins, performance.now());
  }
  async typingNote(a: { ch: Id; key: number; vel?: number; on: boolean }) { if (a.on) await this.noteAudition({ ch: a.ch, key: a.key, vel: a.vel }); }
  async patternPlay(id: Id | null) {
    if (id != null && !this.p.patterns.some(x => x.id === id)) throw new Error('no pattern ' + id);
    if (id != null && this.tr.playing) { this.finishRec(); this.tr = { ...this.tr, playing: false }; this.changedTransport(); }
    this.patPlay = id == null ? null : { id, t0: performance.now() };
  }

  // ---------- audio device ----------
  async audioHosts() { return [{ id: 'wasapi', name: 'WASAPI' }, { id: 'asio', name: 'ASIO' }]; }
  async audioDevices(host: string) {
    return host === 'asio'
      ? { outputs: ['Focusrite USB ASIO'], inputs: ['Focusrite USB ASIO'] }
      : { outputs: ['Głośniki (Realtek High Definition Audio)', 'Słuchawki (USB Audio)'], inputs: ['Mikrofon (USB Audio)', 'Line In (Realtek)'] };
  }
  async audioGetConfig() { return { ...this.audio }; }
  async audioSetConfig(cfg: AudioConfig) {
    await delay(80);
    // like the app: a config without recOffsetMs keeps the saved calibration
    cfg = { ...cfg, recOffsetMs: cfg.recOffsetMs ?? this.audio.recOffsetMs ?? 0 };
    const asioErr = cfg.host === 'asio' && !this.audio.asioCompiled;
    this.audio = { ...this.audio, ...cfg, running: !asioErr, error: asioErr ? t('d.mock.noasio') : null,
      actualRate: cfg.sampleRate ?? 48000, actualBuffer: cfg.bufferSize ?? 480, latencyMs: Math.round(((cfg.bufferSize ?? 480) * 2 / (cfg.sampleRate ?? 48000)) * 10000) / 10 };
    this.emit('audio-status', { ...this.audio });
    return { ...this.audio };
  }

  /** Dev tools / tests: the device stops by itself and the audio thread reopens it, as in the app. */
  audioDeviceLost(why = 'The requested device is no longer available') {
    this.audio = { ...this.audio, running: true, error: null, restarts: this.audio.restarts + 1, note: `Audio restarted by itself: ${why}.` };
    this.emit('audio-status', { ...this.audio });
  }

  async audioMeasureLatency(): Promise<LatencyReport> {
    if (this.tr.playing) throw new Error('Stop playback before measuring the latency.');
    await delay(5200);
    const reportedMs = 21.3, roundTripMs = 25.8;
    return { roundTripMs, reportedMs, suggestedOffsetMs: Math.round((roundTripMs - reportedMs) * 100) / 100, confidence: 0.93 };
  }

  // ---------- MIDI keyboard ----------
  async midiListDevices() { return this.midiPorts.map(d => ({ ...d })); }
  async midiSelectDevice(id: string | null) {
    const d = id == null ? null : this.midiPorts.find(x => x.id === id) ?? null;
    if (id != null && !d) this.midi = { ...this.midi, connected: null, selected: { id, name: id }, error: `${id} is not connected` };
    else this.midi = { ...this.midi, connected: d && { ...d }, selected: d && { ...d }, error: null };
    this.emit('midi-status', { ...this.midi });
    return { ...this.midi };
  }
  async midiStatus() { return { ...this.midi }; }
  async midiSetTarget(channel: Id | null) {
    this.midi = { ...this.midi, target: channel };
    this.emit('midi-status', { ...this.midi });
    return { ...this.midi };
  }

  // ---------- library ----------
  async libraryList(): Promise<LibraryList> { await delay(); return { kits: this.kits.map(k => ({ ...k })), folders: [...this.folders] }; }
  async libraryInstall(id: string) {
    const kit = this.kits.find(k => k.id === id); if (!kit) throw new Error('no kit');
    let done = 0; const total = kit.bytes;
    const iv = setInterval(() => {
      done = Math.min(total, done + total / 25);
      this.emit('library-progress', { id, done, total });
      if (done >= total) { clearInterval(iv); kit.installed = true; }
    }, 60);
  }
  /** The browser preview has no recordings to load: the piano is always "ready" (and silent, like every mock instrument). */
  async keysStatus(): Promise<KeysStatus> { await delay(); return { state: 'ready', error: null, kit: 'salamander-piano', installed: true }; }
  async libraryFiles(q: { kit?: string; folder?: string }): Promise<LibFile[]> {
    await delay(10);
    // the standard library is listed by its portable paths (the app lists where it is installed; importing either gives `lib:` sources)
    if (q.kit === FACTORY_KIT) return FACTORY_FILES.map(f => ({ name: f.name, path: libPath(f.file), group: f.group, root: f.root }));
    const root = q.kit ? 'C:\\WAVE\\Library\\' + q.kit : q.folder ?? '';
    const groups: Record<string, string[]> = q.kit === 'wave-keys'
      ? { Pady: ['Warm Pad C', 'Glass Pad Am', 'Choir Pad F'], Akordy: ['Rhodes Chord 1', 'Rhodes Chord 2', 'Piano Stab'], Pętle: ['Keys Loop 120', 'Pad Loop Am-F'] }
      : q.kit === 'wave-fx' ? { Uplightery: ['Riser 8 bar', 'Riser 4 bar'], Impakty: ['Impact Deep', 'Impact Metal'], Szumy: ['White Sweep', 'Vinyl Crackle'] }
      : q.kit ? { Kick: ['Kick 808', 'Kick Punch', 'Kick Deep'], Snare: ['Snare Tight', 'Snare Room', 'Clap 909'], Hats: ['Hat Closed', 'Hat Open', 'Ride'], Loops: ['Drum Loop 128', 'Perc Loop 128', 'Top Loop'] }
      : { '': ['vox_take1', 'vox_take2', 'guitar_di', 'bass_di', 'fx_loop'] };
    return Object.entries(groups).flatMap(([group, names]) => names.map(n => ({ name: n + '.wav', path: root + '\\' + (group ? group + '\\' : '') + n + '.wav', group })));
  }
  async libraryAddFolder() { this.folders.push('D:\\Sample packs\\Pack ' + (this.folders.length + 1)); return [...this.folders]; }
  async libraryRemoveFolder(path: string) { this.folders = this.folders.filter(f => f !== path); return [...this.folders]; }

  // ---------- project files ----------
  private resetTo(p: Project, path: string | null) {
    this.patPlay = null;
    // ids start over in a new project: the audio of the previous one must not answer to them
    this.srcMeta.clear(); this.recPeaks.clear();
    this.p = p; this.undoStack = []; this.redoStack = []; this.path = path;
    this.tr = { ...this.tr, playing: false }; this.revision++; this.savedRev = this.revision;
    this.emit('state', this.dto());
  }
  async projectNew() { this.resetTo(emptyProject(), null); this.recovery = null; return this.dto(); }
  /** Tests: the next native file dialog is dismissed, as when the user presses Cancel. */
  dismissNextDialog = false;
  private dialog(): boolean { const ok = !this.dismissNextDialog; this.dismissNextDialog = false; return ok; }
  async projectOpen(path?: string): Promise<OpenResult> {
    if (path == null && !this.dialog()) return { cancelled: true };
    this.resetTo(sketchProject(this.cat), path ?? 'C:\\Users\\michal\\Music\\Demo.wave');
    this.recovery = null;
    return { state: this.dto(), warnings: [] };
  }
  async projectSave(saveAs?: boolean): Promise<SaveResult> {
    if (!this.path || saveAs) {
      if (!this.dialog()) return { cancelled: true };
      this.path = 'C:\\Users\\michal\\Music\\Projekt ' + (1 + (this.revision % 9)) + '.wave';
    }
    this.savedRev = this.revision; this.recovery = null; this.emit('state', this.dto());
    return { path: this.path };
  }

  // ---------- crash recovery (app/src/recovery.rs) ----------
  /** This session's copy of a project that was never saved. */
  private recovery: { project: Project; savedMs: number } | null = null;
  /** Copies left by sessions that "crashed" (see `simulateCrash`). */
  private orphans = new Map<string, { project: Project; savedMs: number }>();
  async recoveryWrite() {
    if (this.path || this.revision === this.savedRev) { this.recovery = null; return false; }
    this.recovery = { project: JSON.parse(JSON.stringify(this.p)), savedMs: Date.now() };
    return true;
  }
  async recoveryList() {
    return [...this.orphans].map(([id, o]) => ({ id, savedMs: o.savedMs })).sort((a, b) => b.savedMs - a.savedMs);
  }
  async recoveryRestore(id: string) {
    const o = this.orphans.get(id);
    if (!o) throw new Error(`no recovered project \`${id}\``);
    this.orphans.delete(id);
    this.resetTo(o.project, null);
    this.revision++; // unsaved changes: Save asks where to put it
    this.recovery = o;
    this.emit('state', this.dto());
    return { state: this.dto(), warnings: [] };
  }
  async recoveryDiscard(id: string) {
    if (!this.orphans.delete(id)) throw new Error(`no recovered project \`${id}\``);
  }
  /** Tests: the app dies without a normal exit and starts again with an empty project. */
  simulateCrash() {
    if (this.recovery) this.orphans.set(`${1000 + this.orphans.size}-${this.recovery.savedMs}`, this.recovery);
    this.recovery = null;
    this.resetTo(emptyProject(), null);
  }

  private exportAbort = false;
  async exportAudio(opts: { useLoop: boolean; tail: number; sampleRate: number; format?: 'wav' | 'mp3'; mp3Kbps?: number }): Promise<ExportResult> {
    this.exportAbort = false;
    const { from, to } = exportRange(this.p, opts.useLoop);
    const secs = exportSeconds(from, to, this.p.bpm, opts.tail);
    // Simulate a short offline bounce so the dialog's busy/cancel path is exercisable.
    const steps = 8;
    for (let i = 0; i < steps; i++) {
      await delay(40);
      if (this.exportAbort) return { cancelled: true };
    }
    const name = (this.path ? this.path.replace(/\.wave$/i, '') : 'C:\\Users\\michal\\Music\\Song') + (opts.format === 'mp3' ? '.mp3' : '.wav');
    const silent = this.p.clips.length === 0 || this.p.tracks.every(t => t.muted);
    return {
      out: name,
      fromBeat: from,
      toBeat: to,
      tail: opts.tail,
      stats: {
        seconds: secs,
        sampleRate: opts.sampleRate,
        peakDbfs: silent ? -180 : -3.2,
        rmsDbfs: silent ? -180 : -14.5,
        crestDb: silent ? 0 : 11.3,
        clippedSamples: 0,
        silent,
      },
    };
  }
  async exportCancel() { this.exportAbort = true; }

  // ---------- VST3 ----------
  async vst3Scan(rescan: boolean): Promise<Vst3Info[]> {
    await delay(rescan ? 600 : 20);
    const mk = (name: string, vendor: string, category: string) => ({ name, vendor, category, path: 'C:\\Program Files\\Common Files\\VST3\\' + name + '.vst3', cid: hash(name).toString(16).padStart(32, '0') });
    return [mk('Pro-Q 3', 'FabFilter', 'Fx|EQ'), mk('Pro-C 2', 'FabFilter', 'Fx|Dynamics'), mk('Valhalla VintageVerb', 'Valhalla DSP', 'Fx|Reverb'),
      mk('Decapitator', 'Soundtoys', 'Fx|Distortion'), mk('EchoBoy', 'Soundtoys', 'Fx|Delay'), mk('OTT', 'Xfer Records', 'Fx|Dynamics'), mk('Serum FX', 'Xfer Records', 'Fx')];
  }
  async vst3Editor() {}
  async vst3Params(fxId: Id): Promise<Vst3Param[]> {
    if (!this.vstParams.has(fxId)) {
      const names = ['Input', 'Output', 'Mix', 'Drive', 'Tone', 'Attack', 'Release', 'Threshold', 'Ratio', 'Width', 'Low', 'High'];
      this.vstParams.set(fxId, names.map((name, i) => { const v = ((hash(name + fxId) % 100) / 100); return { id: i, name, value: v, def: 0.5, display: (v * 100).toFixed(0) + ' %' }; }));
    }
    return structuredClone(this.vstParams.get(fxId)!);
  }
  async vst3SetParam(fxId: Id, id: number, value: number) {
    const p = this.vstParams.get(fxId)?.find(x => x.id === id);
    if (p) { p.value = value; p.display = (value * 100).toFixed(0) + ' %'; }
  }

  private readonly upd = new MockUpdates((done, total) => this.emit('update-progress', { done, total }));
  appInfo() { return this.upd.appInfo(); }
  updateCheck() { return this.upd.updateCheck(); }
  updateDownload() { return this.upd.updateDownload(); }
  updateInstall() { return this.upd.updateInstall(); }
  updateSetAuto(on: boolean) { return this.upd.updateSetAuto(on); }
  updateMarkSeen(version: string) { return this.upd.updateMarkSeen(version); }
  openReleasePage(version?: string | null) { return this.upd.openReleasePage(version); }
  async cliStatus() { return { installed: false, command: 'wave', path: null, note: t('d.mock.nocli') }; }

  async on<K extends keyof EventMap>(ev: K, cb: (p: EventMap[K]) => void) {
    let s = this.listeners.get(ev); if (!s) this.listeners.set(ev, s = new Set());
    const f = cb as (p: unknown) => void; s.add(f);
    return () => { s!.delete(f); };
  }
  async onOsDrop(cb: (e: OsDropEvent) => void) {
    const over = (e: DragEvent) => { if (e.dataTransfer && [...e.dataTransfer.types].includes('Files')) { e.preventDefault(); cb({ type: 'over', paths: [], x: e.clientX, y: e.clientY }); } };
    const leave = (e: DragEvent) => { if (!e.relatedTarget) cb({ type: 'leave', paths: [], x: 0, y: 0 }); };
    const drop = (e: DragEvent) => {
      e.preventDefault();
      const files = [...(e.dataTransfer?.files ?? [])].filter(f => f.type.startsWith('audio') || /\.(wav|mp3|ogg|flac|m4a|aif+)$/i.test(f.name));
      cb({ type: 'drop', paths: files.map(f => f.name), x: e.clientX, y: e.clientY });
    };
    window.addEventListener('dragover', over); window.addEventListener('dragleave', leave); window.addEventListener('drop', drop);
    return () => { window.removeEventListener('dragover', over); window.removeEventListener('dragleave', leave); window.removeEventListener('drop', drop); };
  }
  setTitle(title: string) { document.title = title; }

  /** Dev helper: a sung vocal take on the Vocal track and a vocal chain on its insert. */
  async addDemoVocal() {
    const r = await this.importAudio({ paths: ['Vocal take 1.wav'], track: this.vocalTrack(), beat: 0 });
    const ins = this.p.inserts.find(m => m.name === 'Vocal') ?? this.p.inserts[6];
    const mk = (type: Fx['type']) => newFx(this.cat, this.p.nextId++, type as never, this.p.bpm);
    const eq = ins.fx.find(f => f.type === 'eq');
    if (eq?.bands) {
      eq.bands.push({ on: true, type: 'peak', f: 5200, g: 0, q: 2.2, slope: 12, place: 'stereo', dynamic: { range: -6, thr: -40 } });
      eq.bands.push({ on: true, type: 'highshelf', f: 9000, g: 2.5, q: 0.71, slope: 12, place: 'side' });
    }
    const fx = ins.fx.filter(f => f.type !== 'comp');
    ins.fx = [mk('tune'), ...fx.filter(f => f.type === 'eq'), mk('deess'), mk('fet'), ...fx.filter(f => f.type !== 'eq')];
    this.changed();
    return r;
  }

  /**
   * Dev helper: five sung takes of a 4-bar phrase on the Vocal track, as if loop-recorded
   * over bars 9–12 (what `finishRec` produces), so take lanes can be tried without a mic.
   */
  async addDemoTakes(track = this.vocalTrack(), from = 32, bars = 4) {
    const prev = structuredClone(this.p), spb = this.spb(), len = bars * 4, dur = len * spb, landed: [Id, number, number][] = [];
    for (let k = 0; k < 5; k++) {
      const id = this.p.nextId++;
      const src: MockSource = { id, name: 'Take ' + (++this.takeN), color: '#d9678f', path: 'recordings/take' + this.takeN + '.wav', dur, sr: 48000, ch: 1, peakDb: -6, seed: 7100 + k * 13, vocal: true };
      this.srcMeta.set(id, src); this.p.sources.push(strip(src));
      landed.push([id, from, len]);
    }
    landTakes(this.p, Math.min(track, this.p.tracks.length - 1), landed, () => this.p.nextId++);
    this.pushUndo(prev); this.changed();
  }

  /** Dev helper: two audio loops on the vocal track (free of pattern clips in every starting project). */
  async addDemoAudio() {
    await this.importAudio({ paths: ['Pad Loop Am-F.wav'], track: this.vocalTrack(), beat: 32 });
    await this.importAudio({ paths: ['Perc Loop.wav'], track: this.vocalTrack(), beat: 64 });
  }
  /** Index of the track named "Vocal" (the armed one in the demo), else the sixth. */
  private vocalTrack() { const i = this.p.tracks.findIndex(t => t.name === 'Vocal'); return i >= 0 ? i : Math.min(5, this.p.tracks.length - 1); }
}

function strip(s: MockSource): Source {
  const { seed: _s, trimFrom: _t, ...rest } = s; void _s; void _t;
  return rest;
}
function mkClip(id: Id, track: number, start: number, len: number, kind: Clip['kind'], ref: Id): Clip {
  return { id, track, start, len, offset: 0, kind, ref, muted: false, gain: 0, fadeIn: 0, fadeOut: 0, pan: 0, norm: false, rev: false };
}
void BUILTIN;
