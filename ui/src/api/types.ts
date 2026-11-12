// TypeScript mirror of crates/engine/src/model.rs, ops.rs, catalog.rs and the DTOs in docs/architecture.md.
// JSON is camelCase; enum tags are lowercase.

export type Id = number;

/** Fader position that gives unity gain (gain = 2·t³). */
export const VOL_UNITY = 0.7937;
/** Beats shown on the playlist; the arrangement may not go past this. */
export const TOTAL_BEATS = 2048; // engine `model::TOTAL_BEATS`: 512 bars

export interface LoopRange { a: number; b: number }

export interface Track {
  id: Id; name: string; color: string;
  muted: boolean; solo: boolean; arm: boolean;
  vol: number; h: number; insert: Id;
  /** Instrument icon id (ui/instIcons INST_ICONS), absent = none. */
  icon?: string | null;
}

export type FxKind = 'eq' | 'comp' | 'sat' | 'delay' | 'reverb' | 'limit' | 'fet' | 'deess' | 'tune' | 'denoise' | 'voice' | 'rider' | 'half' | 'vst3';
/** Built-in kinds the effect picker lists (the views keep a label and blurb per kind). */
export type BuiltinKind = Exclude<FxKind, 'vst3'>;
/** Every built-in kind, including ones the picker does not list yet ('rider': added by "Auto wokal"). */
export type AnyBuiltinKind = Exclude<FxKind, 'vst3'>;
export type BandType = 'lowcut' | 'lowshelf' | 'peak' | 'highshelf' | 'highcut' | 'notch' | 'bandpass' | 'tilt';
/** Which part of the stereo image a band works on. */
export type Placement = 'stereo' | 'left' | 'right' | 'mid' | 'side';
/** Dynamic EQ: the band's gain moves by up to `range` dB as its own level goes over `thr` dBFS. */
export interface DynBand { range: number; thr: number }

/** Up to 24 bands (MAX_BANDS). `slope` (cuts only): 12/24/36/48 dB/oct. Missing fields take the engine defaults. */
export interface EqBand {
  on: boolean; type: BandType; f: number; g: number; q: number;
  slope?: number; place?: Placement; dynamic?: DynBand | null;
}
export const MAX_BANDS = 24;

export interface PluginRef {
  format: string; path: string; cid: string; name: string; vendor: string; state?: string;
}

export interface Fx {
  id: Id; type: FxKind; bypass: boolean;
  /** Dry/wet 0..1 (1 = fully processed; missing = 1). */
  mix?: number;
  preset?: number | null;
  params: Record<string, number>;
  bands?: EqBand[];
  plugin?: PluginRef;
}

export interface Insert {
  id: Id; name: string; color: string;
  vol: number; pan: number;
  /** Stereo width (M/S side gain): 0 mono, 1 unchanged (default when absent), 2 extra wide. */
  width?: number;
  muted: boolean; solo: boolean;
  fx: Fx[];
}

/** MIDI key at which a sampler plays its sample unpitched (C5 in FL Studio's naming, C4 here). */
export const SAMPLE_ROOT = 60;
/**
 * A synth channel's sound: parameter values by id (`Catalog.synth`; a missing id plays its default,
 * the engine rejects unknown ids). `preset`: the factory preset the values came from, for display
 * (null once a knob was changed, or for a blank patch). Parameter edits are `putChannel` with the
 * new `synth`; they take the engine's fast path (no timeline rebuild) and glide on held notes.
 */
export interface SynthState { preset: string | null; params: Record<string, number> }
/**
 * WAVE Keys (the sampled grand piano): `release` = how long a note rings after its key is let
 * go, in ms; `dynamics` 0..100 % = how much the velocity changes the level (the timbre always
 * follows the velocity). Mirrors crates/engine/src/keys.rs.
 */
export interface KeysState { release: number; dynamics: number }
export const KEYS_RELEASE = { min: 40, max: 3000, def: 350 } as const;
export const KEYS_DYNAMICS = { min: 0, max: 100, def: 100 } as const;
/** Where the piano's recordings are: `missing` = not on this computer yet (install library kit `kit`). */
export interface KeysStatus { state: 'idle' | 'missing' | 'loading' | 'ready' | 'error'; error: string | null; kit: string; installed: boolean }
/**
 * How a sampler channel plays its sample (crates/engine/src/model.rs `SamplerState`).
 * `start`/`end`: the part of the file that plays, fractions 0..1. `semi` (−24..24) and `fine`
 * (cents) transpose on top of the note. `gate` false = one-shot (the note's length is ignored),
 * true = the sample plays while the note is held, then fades over `release` ms. `attack`/`decay`
 * in ms, `sustain` 0..1 (0 = the decay is the whole sound). `cut` 0 = none, 1..8 = a cut group:
 * a new note silences every sounding note of the group's channels.
 */
export interface SamplerState {
  start: number; end: number; reverse: boolean; norm: boolean; semi: number; fine: number;
  gate: boolean; attack: number; decay: number; sustain: number; release: number; cut: number;
}
export const SAMPLER = {
  semi: { min: -24, max: 24 }, fine: { min: -100, max: 100 },
  attack: { min: 0, max: 2000 }, decay: { min: 0, max: 5000 }, release: { min: 1, max: 5000, def: 20 },
  cutGroups: 8, minSpan: 0.001,
} as const;
export const samplerDefault = (): SamplerState => ({ start: 0, end: 1, reverse: false, norm: false, semi: 0, fine: 0, gate: false, attack: 0, decay: 0, sustain: 1, release: SAMPLER.release.def, cut: 0 });
/** What a channel plays: a sample of a project source (the sampler), the built-in synth, or WAVE Keys. */
export type ChannelSrc = { kind: 'sample'; source: Id; sampler: SamplerState } | { kind: 'synth'; synth: SynthState } | { kind: 'keys'; keys: KeysState };
/**
 * A Channel Rack row, global to the project (every pattern can have notes for it).
 * `vol` fader position (0.7937 = 0 dB), `pan` −1..1, `insert` null = the clip's track insert,
 * `root` = the key a step plays (and the sampler's original pitch).
 */
export type Channel = {
  id: Id; name: string; color: string;
  vol: number; pan: number; mute: boolean; solo: boolean; insert: Id | null; root: number;
} & ChannelSrc;
/**
 * A pattern note: channel `ch`, `step`/`len` in 16ths, `p` MIDI key (absent = the channel's root),
 * `v` velocity 0..1 (absent = 1).
 */
export interface Note { ch: Id; step: number; len: number; p?: number; v?: number }
/** `len` in beats; `swing` 0..1 (absent = 0). Notes at or past `len` are kept but hidden and silent. */
/**
 * `len` (beats) is derived by the engine: the end of the last note rounded up to a bar, at least
 * `minLen` (whole bars the user keeps on purpose, ≥ 1 bar). A `len` sent in `putPattern` is ignored.
 */
export interface Pattern { id: Id; name: string; color: string; len: number; minLen?: number; notes: Note[]; swing?: number }

export interface Source {
  id: Id; name: string; color: string; path: string;
  dur: number; sr: number; ch: number; peakDb: number;
}

/** `comp`: plays a take group's comp (`ref` = comp id, `offset` in comp beats). */
export type ClipKind = 'pat' | 'audio' | 'auto' | 'comp';

/** One recorded pass. `start`/`len` in comp beats, `offset` beats into the source. Takes are never cut. */
export interface Take { id: Id; name: string; ref: Id; start: number; len: number; offset: number; color?: string | null }
/** `take` plays over [from, to) (comp beats). `xfade`: crossfade at `from`, seconds (comp default when absent). */
export interface CompRegion { take: Id; from: number; to: number; xfade?: number | null }
/**
 * A take group and its comp. Comp beat = clip.offset + (timeline beat − clip.start).
 * `regions` sorted, non-overlapping, gaps silent. `xfade`: default boundary crossfade, seconds.
 */
export interface Comp { id: Id; name: string; takes: Take[]; regions: CompRegion[]; xfade: number }
/** Default comp crossfade (s) and the choices offered on a boundary. */
export const DEFAULT_XFADE = 0.01;
export const MAX_XFADE = 0.5;
export interface Clip {
  id: Id; track: number; start: number; len: number; offset: number;
  kind: ClipKind; ref: Id; muted: boolean; gain: number;
  fadeIn: number; fadeOut: number; pan: number; norm: boolean; rev: boolean;
  /** Note-level pitch edits (the pitch editor); rendered offline by the engine. */
  tune?: ClipTune | null;
}

/** Times in seconds of the source; `shift`/`formant` in cents, `drift` 0..1, `gain` dB. */
export interface NoteEdit { start: number; end: number; shift: number; drift: number; formant: number; gain: number }
/** `pencil`: hand-drawn `[seconds, midi]` points, sorted; gaps > 30 ms end a run.
 *  `reference`: Tune to Reference — the base pitch comes from another performance; notes and pencil apply on top. */
export interface ClipTune { notes: NoteEdit[]; pencil?: [number, number][]; reference?: RefTune | null }
/** Tune to Reference: `source` = the reference audio; correction/vibrato/influence 0..1;
 *  `octave` in semitones (multiple of 12), null = automatic; key 0..11 and scale 0..4 for the non-reference share. */
export interface RefTune { source: Id; correction: number; vibrato: number; influence: number; octave?: number | null; key?: number; scale?: number }

/** `pitch_analyze`: pitch every 1/rate s (null = unvoiced), levels 0..1, segmented notes (seconds of the source).
 *  `confidence` 0..1 per frame; `backend` = the detector ("rmvpe" or "yin"). */
export interface NoteBlob { start: number; end: number; pitch: number; level: number }
export interface PitchAnalysis { rate: number; curve: (number | null)[]; levels: number[]; notes: NoteBlob[]; confidence?: number[]; backend?: string }

/** `pitch_target`: per analysis frame from `start` (null = unvoiced/none). `targetF0` is the base pitch the
 *  engine renders; where `alignmentConfidence` is ~0 it equals the take's own pitch. `notes` are the
 *  reference's notes on the take's time axis. `reason` is set when the reference was rejected. */
export interface PitchTarget {
  rate: number; start: number;
  sourceF0: (number | null)[]; referenceF0: (number | null)[]; targetF0: (number | null)[];
  alignmentConfidence: number[]; refTime: (number | null)[];
  octave: number; notes: NoteBlob[]; score: number; reason: string | null;
}
/** The optional RMVPE pitch model (`f0_model_status`). */
export interface F0ModelStatus {
  id: string; name: string; bytes: number; installed: boolean; backend: string;
  licenseNote: string; licenseUrl: string; path: string; error: string | null;
}
/** An update found on GitHub Releases (`update_check`). `notes`: the release notes (Markdown), `date`: `YYYY-MM-DD`. */
export interface UpdateInfo { version: string; notes: string | null; date: string | null }
/** `app_info`: the running version, the version whose notes were last shown (null on a fresh install), the auto-check switch. */
/** `pluginEditors`: VST3 plugins can open their own editor windows (Windows only for now). */
export interface AppInfo { version: string; seenVersion: string | null; autoUpdate: boolean; pluginEditors: boolean }
export interface UpdateProgress { done: number; total: number }
export interface ModelProgress { id: string; done: number; total: number; error?: string }

export interface TempoCandidate { bpm: number; score: number }
export interface TempoEstimate { bpm: number; confidence: number; candidates: TempoCandidate[]; firstBeat: number }
export interface KeyCandidate { root: number; minor: boolean; score: number }
export interface KeyEstimate {
  root: number; minor: boolean; confidence: number;
  candidates: KeyCandidate[]; chroma: number[]; tuningCents: number;
}
export interface MusicAnalysis { tempo: TempoEstimate | null; key: KeyEstimate | null }

/** `auto_vocal_plan` (crates/engine/src/autovocal.rs): `natural` = sung, gentle tune; `hard` = rap windows, fast retune. */
export type AutoVocalStyle = 'natural' | 'hard';
/** One insert effect of the planned chain, in chain order. Apply as `PutFx` with these params and `mix`. */
export interface PlannedFx {
  type: AnyBuiltinKind;
  /** Full parameter set (catalog ids). */
  params: Record<string, number>;
  /** Effect dry/wet (`Fx.mix`, 0..1). */
  mix: number;
  /** false: proposed but should be added bypassed (e.g. tune without a detected key). */
  enabled: boolean;
  /** One line on why (tooltip). */
  note: string;
}
export interface AutoVocalAnalysis {
  durationS: number; sampleRate: number; peakDbfs: number; truePeakDbtp: number; lufs: number;
  /** Typical sung word level (K-weighted, 200 ms) = the rider's target. */
  wordLevelDb: number; noiseFloorDb: number;
  /** Share of the take that is confident vocal, 0..1. */
  sungShare: number;
  /** p95 high/low band ratio (≥ 5 kHz vs below), dB. */
  sibilanceDb: number;
  key: { root: number; minor: boolean; name: string; confidence: number } | null;
  /** Share of voiced pitch on the beat's scale (< 0.75 → tune snaps chromatically). */
  scaleShare: number | null;
  pitchOffCents: number | null;
  beatLufs: number | null;
}
export interface AutoVocalStages {
  /** Rider gain on confident frames, p5 / p50 / p95 dB. */
  riderGainDb: [number, number, number];
  /** [research target, engine] pairs. */
  fetP99GrDb: [number, number]; compMeanGrDb: [number, number]; sibilanceAfterDb: [number, number];
  /** 400 ms loudness IQR before → after the dynamics, dB. */
  iqr400Db: [number, number];
  lufsAfter: number; truePeakAfterDbtp: number;
}
export interface AutoVocalPlan {
  version: number;
  style: AutoVocalStyle;
  analysis: AutoVocalAnalysis;
  /** Insert chain, first = first in the chain: tune, rider, fet, comp, deess, reverb, limit (no rider when baked). */
  chain: PlannedFx[];
  /** Suggested vocal level change (dB) to sit 1.2 LU under the beat; null without a beat clip. */
  trackGainDb: number | null;
  stages: AutoVocalStages;
  warnings: string[];
  /** With `autoVocalPlan({ bakeRider: true })`: the rider stage, taken out of `chain`, to render into the clip
   *  with `bakeRider({ clipId, params: riderBake.params })` before the chain goes on the insert. Else null. */
  riderBake: PlannedFx | null;
}

/** What an automation moves. `param`: catalog param id, or `band:<i>:f|g|q` of an EQ. */
export type AutoTarget =
  | { kind: 'fx'; fxId: Id; param: string }
  | { kind: 'mix'; fxId: Id }
  | { kind: 'insertVol'; id: Id }
  | { kind: 'insertPan'; id: Id }
  | { kind: 'trackVol'; id: Id }
  /** A Channel Rack channel's fader position and pan. */
  | { kind: 'chanVol'; id: Id }
  | { kind: 'chanPan'; id: Id }
  /** A synth channel's parameter (`Catalog.synth` id). */
  | { kind: 'synth'; chanId: Id; param: string }
  /** A WAVE Keys channel's release (ms, log) or dynamics (%). */
  | { kind: 'keys'; chanId: Id; param: 'release' | 'dynamics' }
  /** A hosted VST3 parameter (`paramId` as `vst3Params` reports it); values are the plugin's normalized 0..1. */
  | { kind: 'plugin'; fxId: Id; paramId: number }
  /** An audio clip's own gain (−24..+12 dB, `CLIP_GAIN_DB`) and pan. */
  | { kind: 'clipGain'; id: Id }
  | { kind: 'clipPan'; id: Id };
/** The span of an audio clip's gain automation, dB (the sample editor's VOL knob; rt.rs `CLIP_GAIN_DB`). */
export const CLIP_GAIN_DB = { min: -24, max: 12 } as const;
/** `t` beats from the start of the automation, `v` normalised 0..1. */
/** How a segment travels from the previous point to this one (FL's curve types). */
export type CurveShape = 'curve' | 'double' | 'hold' | 'stairs';
/** `tension` (−1..1, 0 = straight) and `shape` (default `curve`) shape the segment that ends here. */
export interface AutoPoint { t: number; v: number; tension?: number; shape?: CurveShape }
export interface Automation { id: Id; name: string; color: string; target: AutoTarget; points: AutoPoint[] }

export interface Project {
  version: number; bpm: number;
  tracks: Track[]; inserts: Insert[]; patterns: Pattern[]; sources: Source[]; clips: Clip[];
  loop: LoopRange | null; nextId: Id;
  /** Automation envelopes; played by clips of kind `auto` (`ref` = automation id). */
  automations?: Automation[];
  /** Take groups with their comps; played by clips of kind `comp`. */
  comps?: Comp[];
  /** Channel Rack rows shared by every pattern. */
  channels?: Channel[];
  /** The lyrics notebook (absent in old projects = no pages). */
  notebook?: Notebook;
}

/** One notebook page: each line of `text` is a bar counted from `startBar` (1-based); `[…]` lines are headers. */
export interface LyricPage { id: Id; name: string; startBar: number; text: string }
export interface Notebook { pages: LyricPage[] }

export type Op =
  | { op: 'putClips'; clips: Clip[] }
  | { op: 'removeClips'; ids: Id[] }
  | { op: 'putTrack'; track: Track }
  | { op: 'putInsert'; insert: Insert }
  | { op: 'removeTrack'; id: Id }
  | { op: 'removeInsert'; id: Id }
  | { op: 'putFx'; insertId: Id; fx: Fx; index?: number }
  | { op: 'removeFx'; fxId: Id }
  | { op: 'moveFx'; fxId: Id; to: number }
  | { op: 'putPattern'; pattern: Pattern }
  | { op: 'putChannel'; channel: Channel; index?: number | null }
  | { op: 'removeChannel'; id: Id }
  | { op: 'removePattern'; id: Id }
  | { op: 'putSource'; source: Source }
  | { op: 'removeSource'; id: Id }
  | { op: 'setBpm'; bpm: number }
  | { op: 'setLoop'; loop: LoopRange | null }
  | { op: 'putAutomation'; automation: Automation }
  | { op: 'removeAutomation'; id: Id }
  | { op: 'putComp'; comp: Comp }
  | { op: 'removeComp'; id: Id }
  | { op: 'putNotebook'; notebook: Notebook };

export interface TransportDto {
  playing: boolean; rec: boolean; monitor: boolean; metro: boolean; countIn: boolean; startBeat: number;
}

export interface StateDto {
  project: Project; revision: number; canUndo: boolean; canRedo: boolean;
  transport: TransportDto; path: string | null; dirty: boolean;
  /** Comp id (as a string key) → take played instead of the comp. Live only, not undone. */
  audition?: Record<string, Id>;
}

/** `options`: a discrete choice (value = index), shown as buttons instead of a knob. */
export interface ParamDef { id: string; label: string; min: number; max: number; def: number; unit: string; log: boolean; options?: string[] }
export interface TypeDef { name: string; params: ParamDef[] }
export interface Preset { name: string; params: Record<string, number>; bands?: EqBand[]; beats?: number }
/** One panel section of the synth (ids in order: osc1, osc2, subnoise, unison, filter, ampenv, modenv, lfo1, lfo2, matrix, voice, fx). */
export interface SynthSection { id: string; name: string; params: ParamDef[] }
/** A factory synth preset: `params` is the complete parameter set (every id). */
export interface SynthPreset { name: string; category: string; params: Record<string, number> }
export interface SynthCatalog { sections: SynthSection[]; categories: string[]; presets: SynthPreset[] }
export interface Catalog {
  types: Partial<Record<FxKind, TypeDef>>;
  presets: Partial<Record<FxKind, Preset[]>>;
  /** The built-in synth (Channel Rack channels of kind `synth`). */
  synth: SynthCatalog;
}

export interface AudioHost { id: string; name: string }
export interface AudioDevices { outputs: string[]; inputs: string[] }
export interface AudioConfig {
  host: string; output: string | null; input: string | null; sampleRate: number | null; bufferSize: number | null;
  /** Input channel recorded and monitored (0-based), mono in both sides; null = the first. */
  inputChannel?: number | null;
  /** Recording calibration in ms (signed), added to the measured round trip: positive moves
   *  takes earlier. Omitted/null when sent = keep the saved value; saved with the audio
   *  config. Changing only this does not restart the audio device. */
  recOffsetMs?: number | null;
}
export interface AudioStatus extends AudioConfig {
  running: boolean; actualRate: number; actualBuffer: number; latencyMs: number;
  inputChannels: number; error: string | null; note: string | null; asioCompiled: boolean;
  /** Input dropouts since the input opened (each a short faded gap in a take): "przerwy w wejściu: N". */
  inputUnderruns: number;
  /** Times input piled up (the output stalled) and was skipped, faded. */
  inputOverruns: number;
  /** Input clock vs output clock in ppm as followed by the jitter buffer (≈0 for one device). */
  inputDriftPpm: number;
  /** Times the output reopened by itself after the device or driver stopped it (reason in `note`). */
  restarts: number;
}
/** Result of `audioMeasureLatency`. */
export interface LatencyReport {
  /** Measured round trip: a sound played reaches the recording this much later. */
  roundTripMs: number;
  /** What the driver timestamps report (what takes use without an offset). */
  reportedMs: number;
  /** roundTripMs − reportedMs: the `recOffsetMs` that makes takes land on the beat. */
  suggestedOffsetMs: number;
  /** 0..1 — share of clicks agreeing within 1 ms × how clearly the peak beats echoes/noise; below ~0.5 measure again. */
  confidence: number;
}

/** A MIDI input port. `id` is stable per port and is what `midiSelectDevice` takes. */
export interface MidiDevice { id: string; name: string }
/** The last key event, for an activity light. `velocity` is 1..127 (0 on a release). */
export interface MidiLastNote { key: number; velocity: number; on: boolean }
export interface MidiStatus {
  /** The device that is open and feeding notes right now. */
  connected: MidiDevice | null;
  /** The device the user chose, open or not (it may be unplugged; it reopens when it returns). */
  selected: MidiDevice | null;
  /** Why the chosen device is not open (unplugged, busy), or the last error. */
  error: string | null;
  lastNote: MidiLastNote | null;
  /** Key events received since the app started: a change means new activity (poll this). */
  noteCount: number;
  /** Channel (instrument) the keyboard plays; null = the first pitched channel. */
  target: Id | null;
}
/** Event `midi-note`: one key down/up as it happens. */
export interface MidiNoteEvent { note: MidiLastNote; count: number }

export interface Kit { id: string; name: string; description: string; bytes: number; count: number; installed: boolean }
export interface LibraryList { kits: Kit[]; folders: string[] }
/** `root`: the MIDI key at which the file sounds at its own pitch, when the library knows it. */
export interface LibFile { name: string; path: string; group: string; root?: number }
export interface LibraryProgress { id: string; done: number; total: number; error?: string }

/** Level stats returned by an offline master bounce. */
export interface ExportStats {
  seconds: number; sampleRate: number; peakDbfs: number; rmsDbfs: number;
  crestDb: number; clippedSamples: number; silent: boolean;
}
/** Result of `exportAudio`; `{ cancelled: true }` when the save dialog or render was aborted. */
export type ExportResult =
  | { cancelled: true }
  | { cancelled?: false; out: string; fromBeat: number; toBeat: number; tail: number; stats: ExportStats };

/** Result of `projectOpen`; `{ cancelled: true }` when the file dialog was dismissed. */
export type OpenResult =
  | { cancelled: true }
  | { cancelled?: false; state: StateDto; warnings: string[] };
/** Result of `projectSave`; `{ cancelled: true }` when the save dialog was dismissed. */
export type SaveResult = { cancelled: true } | { cancelled?: false; path: string };
/** An unsaved project a WAVE that crashed or was killed left behind (`recovery_list`). */
export interface RecoveredProject { id: string; savedMs: number }

export interface ImportResult { sourceIds: Id[]; clipIds: Id[] }

export interface Vst3Info { path: string; cid: string; name: string; vendor: string; category: string }
export interface Vst3Param { id: number; name: string; value: number; display: string; def: number }

export interface CliStatus { installed: boolean; command: string; path: string | null; note?: string | null }
