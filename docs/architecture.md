# WAVE v2 — architecture and engine ⇄ UI contract

```
 ┌──────────── ui/ (SolidJS + canvas) ────────────┐        ┌──── wave (CLI) ────┐
 │ playlist · mixer · FX windows · library · ...  │        │ context/apply/undo │
 └───────▲───────────────┬────────────────────────┘        └────────▲───────────┘
   events│ "state"       │ invoke(...)          binary Channel       │ local socket
   ┌─────┴───────────────▼───────────────────────────────────────────┴───────────┐
 │ app/ (Tauri shell): commands, session server, CLI shim, project files        │
 └───────────────────────────┬──────────────────────────────────────────────────┘
                             │ Engine handle (Mutex<Project> + history, rtrb rings)
 ┌───────────────────────────▼──────────────────────────────────────────────────┐
 │ crates/engine: model · ops · history · sequencer · instruments · built-in FX  │
 │                mixer graph · recording · offline render · analysis thread    │
 │ crates/audio-io: cpal (WASAPI / ASIO / CoreAudio / ALSA / JACK) duplex streams│
 │ crates/vst3-host: VST3 effects as `Processor`s, scan in a child process      │
 └──────────────────────────────────────────────────────────────────────────────┘
```

Rules that keep it fast:

* **The audio callback never allocates, locks or does I/O.** Structure changes
  are built off-thread and swapped in through an `rtrb` ring; the old graph is
  sent back to be dropped off-thread. DSP state (reverb tails, delay lines) is
  carried across a swap by effect id.
* **Parameter moves do not rebuild anything.** They are a fixed-size message.
* **The UI never polls JSON at frame rate.** Meters, playhead, recording peaks
  and the analysis of *open* effect windows arrive as one binary frame on a
  Tauri `Channel` (≤60 Hz). FFTs run on an analysis thread in Rust, only for
  effects whose window is open (`watch_fx`).
* **The UI draws with canvas** for anything that moves (playlist clips,
  meters, visualisations) with a single `requestAnimationFrame` loop; static
  layers (grids, curves that depend only on parameters) are cached offscreen.

## Data model

`crates/engine/src/model.rs` is the source of truth; JSON is camelCase. A
project looks like:

```jsonc
{
  "version": 2, "bpm": 128, "nextId": 1200, "loop": null,          // or {"a":0,"b":16}
  "tracks":  [{"id":1,"name":"Drums","color":"#4f8ef7","muted":false,"solo":false,"arm":false,"vol":0.7937,"h":56,"insert":1}],
  "inserts": [{"id":0,"name":"Master","color":"#d5dae2","vol":0.7937,"pan":0,"muted":false,"solo":false,
               "fx":[{"id":900,"type":"limit","bypass":false,"preset":0,"params":{"gain":0,"ceil":-0.3}}]}],
  "channels":[{"id":1001,"name":"Kick","color":"#4f8ef7","kind":"sample","source":1000,"vol":0.7937,"pan":0,
               "mute":false,"solo":false,"insert":null,"root":60,
               "sampler":{"start":0,"end":1,"reverse":false,"norm":false,"semi":0,"fine":0,"gate":false,
                          "attack":0,"decay":0,"sustain":1,"release":20,"cut":0}}],
  "patterns":[{"id":1,"name":"Beat","color":"#4f8ef7","len":4,"minLen":4,"swing":0.5,
               "notes":[{"ch":1001,"step":0,"len":1},{"ch":1001,"step":6,"len":1,"p":55,"v":0.8}]}],
  "sources": [{"id":1000,"name":"808 Kick Short","color":"#4f8ef7","path":"lib:Standard/kick-808-short.flac","dur":0.378,"sr":44100,"ch":1,"peakDb":-1},
              {"id":1010,"name":"Pad Loop","color":"#47b8a8","path":"…/pad.wav","dur":3.75,"sr":48000,"ch":2,"peakDb":-3.1}],
  "clips":   [{"id":5,"track":0,"start":0,"len":16,"offset":0,"kind":"pat","ref":1,"muted":false,"gain":0,
               "fadeIn":0,"fadeOut":0,"pan":0,"norm":false,"rev":false}]
}
```

## Channels, patterns and notes (Channel Rack and Piano roll)

Channels are **global to the project**, like FL Studio's Channel Rack: one row per
instrument, and every pattern can hold notes for every channel. A note names its
channel with `ch`. Why global and not per pattern: a beat is usually several patterns
(verse / chorus / fill) played by the same kit; per-pattern instruments would duplicate
the kit and its mixer routing in every pattern.

* `channel.kind`: `sample` (the sampler: plays project source `source`; the note's key
  repitches it, `root` = the key at which the file plays at its own pitch, default 60 = C5 in
  FL's naming / C4 in this app's labels; `sampler` = how it plays, see below), `keys` (WAVE
  Keys, see [keys.md](keys.md)) or `synth` (the built-in polyphonic synth, `synth: {preset, params}` with
  parameter ids from `fx_catalog().synth`; see [synth.md](synth.md)). `vol` is a fader position (unity = 0.7937, gain normalised so unity is exactly
  1), `pan` −1..1 (balance law of the mixer), `mute`/`solo` (with any solo, only soloed
  channels play), `insert`: a mixer insert id, or `null` = **the insert of the playlist track
  the clip is on** (how every pattern note behaved before channels existed; the track's fader
  and mute still apply). A channel routed to an insert skips the track's insert but the
  track still mutes it and scales it by its fader.
* `channel.sampler` (kind `sample`; every field has a default, so older files load):
  `start`/`end` = the part of the file that plays, fractions 0..1; `reverse`; `norm` (peak to
  −0.3 dBFS, from the source's `peakDb`); `semi` −24..24 and `fine` −100..100 cents on top of
  the note; `gate` false = **one-shot** (the note's length is ignored: drums), true = the
  sample plays while the note is held and then fades over `release` ms (1..5000); `attack`
  0..2000 ms and `decay` 0..5000 ms to the `sustain` level 0..1 (0 = the decay is the whole
  sound); `cut` 0 = none, 1..8 = a **cut group**: a new note silences every sounding sampler
  note of the group's channels, its own included, when it starts (a closed hat cutting the
  open one). A part that starts or ends inside the waveform (trimmed or reversed) is faded
  over 1.5 ms; an untrimmed sample starts exactly as recorded, no fade softens its transient.
  `putChannel` refuses values out of range (`SamplerState::check`).
* `note`: `step`/`len` in 16ths, `p` MIDI key (absent = the channel's `root`), `v` velocity
  0..1 = linear gain (absent = 1). A step-sequencer step is simply a note of length 1 (at
  the root, or at its own key set in the rack's graph editor), so the step grid and the piano
  roll edit the same notes; a channel with other lengths, off-grid notes or chords is shown
  as a mini piano roll in the rack (a click opens the Piano roll).
* `pattern.len` in beats is **derived**, like FL Studio's automatic pattern length: the end
  of the last note rounded up to a whole bar, at least `minLen` (whole bars, ≥ 1; the bars
  the user keeps on purpose with "+ bar", e.g. a crash every second bar). The engine sets it
  on every `putPattern` and on load (`Pattern::fit_len`); a `len` sent by a client is
  ignored. Drawing past the end makes a pattern longer, deleting its last notes shorter.
  When `len` changes, pattern clips that show exactly one whole pass (`offset` 0, `len` = the
  old length) take the new length; clips sized on purpose (repeats, offsets) keep theirs. New
  clips use the pattern's length. Notes must end within the song (`TOTAL_BEATS`).
* `pattern.swing` 0..1: a note that starts exactly on an odd 16th (0-based 1, 3, 5 …) is
  delayed by `swing × ½` step. 100 % puts the off-beat 16th at 75 % of its 8th (MPC 75 %),
  ~67 % is close to triplet feel; 0 is straight. Even 16ths and off-grid notes never move.
  Swing is applied when the timeline is built (schedule time), never written into the notes.

**The UI, as in FL Studio.** The *current pattern* is global (the toolbar's pattern selector,
numpad 1–9 / + / −, F4 next empty pattern, F2 rename, Shift+Ctrl+C clone); picking a pattern
clip on the playlist or in the browser makes it current, and the playlist paints it. Two
separate floating windows show it: the **Channel Rack** (F6: a row per channel — mute LED,
pan, volume, mixer insert, name, steps — "+ / −" bars, swing, and the graph editor `G` for
per-step velocity and key of the selected channel) and the **Piano roll** (F7: the selected
channel's notes in the current pattern, any channel; the pattern's other channels as ghost
notes, double right-click one to switch). PAT mode (L) loops the current pattern and follows
it when it changes. *To new pattern* (Ctrl+Shift+X, or the button shown while notes are
selected) moves the selected notes to a new pattern at the same steps. Both patterns keep
the source's length, and every playlist clip of the source gets a clip of the new pattern
beside it, on the nearest lane below that is free over its span (a new lane when none is),
so the song sounds the same. It is one undo step (`lib/moveNotes.ts`).

**Automation clips** are edited on the playlist without an edit mode, as in FL. Points and the
tension handles halfway along each segment (shown on the clip under the pointer) are live:
- drag a point (Shift locks the value, Ctrl the time, Alt skips the snap), or drag a handle to
  bend the segment (Ctrl: finer);
- double-click: a point deletes it, a handle straightens it, empty envelope adds a point;
- right-click: empty envelope adds a point that stays under the pointer until release, a point
  opens its menu (type in a value in the control's unit, curve type, delete);
- Ctrl+drag selects points (Shift adds); a selection moves together and Delete removes it.
Each `AutoPoint` may carry `tension` (−1..1) and `shape` (`curve`, `double`, `hold`, `stairs`,
default `curve`) for the segment that ends at it. They are omitted when default, so older
projects load and play unchanged. `curve_at` (model.rs) and `curveAt` (`lib/automation.ts`) are
the same formulas, both checked against `crates/engine/tests/fixtures/curves.json`. The engine
still evaluates automation once per audio block, so a hold or a stair lands on a block boundary.

**Stacked clips.** Clips may overlap on a track, as in FL (layers, crossfades). The
playlist overlay hatches the header where clips of a track play at once, and puts "×N" on a
clip that hides N − 1 clips entirely (`lib/overlaps.ts`). Clicking such a clip says so in
the status line. The overlay is recomputed with the fades on clip edits and tile moves,
never per frame.

**The standard library** (`crates/engine/src/factory.rs`). Fifty CC0 one-shots (kicks,
snares, claps, hats, cymbals, toms, percussion, a few tonal ones) are embedded in the engine
(`crates/engine/library`, 3.2 MB of FLAC; built by `scripts/make-factory-library.py`,
which trims, fades and normalises picks from the kits pinned in `assets-manifest.json`).
There is nothing synthesised behind a channel any more: a new project's Channel Rack is four
samplers of these files (`STARTER_KIT`: Kick, Clap, Hat, Snare), and the program starts
with that empty project. A source inside the sample library is stored by a portable
path, `lib:<kit>/<file>` (in memory and in the file), never by where the library is on this
computer; `lib:Standard/…` is decoded from the embedded bytes, so it is never missing, other
kits resolve to `<data>/WAVE/Library/<kit>/`. Importing a library file twice gives the same
source: a sampler channel and audio clips of one sample share it. The app also writes the
standard library into `<data>/WAVE/Library/Standard` (`factory::install`) so the browser
lists, previews and drags it like any downloaded kit. `ui/src/api/factory.json` (the file
table and the empty project, for the in-browser mock) is generated from the engine;
`tests/sampler.rs` fails when it is stale and says how to regenerate it.

**Old files.** Files from before the standard library name synthesised instruments:
`{"kind":"inst","inst":"kick"}` channels, or `inst` on notes before channels existed.
`Project::from_json` (used by `open`) turns each into a sampler of the closest library sample
(the bass in gate mode); they do not sound the same as before. A project without channels
gets the starter kit.

**Real time.** Channels are resolved off-thread into a table in the timeline (`ChanRt`:
voice kind, `Arc` of the sample, gain/pan/on, destination insert). Voices keep a channel
index and id; a new timeline re-maps voices by id (no allocation), and knob / mute / solo
moves go through the mixer fast path (`MixState.chans`) without a timeline rebuild, so
ringing notes follow them. Auditions (`note_audition`) and the pattern loop preview
(`pattern_play`) are commands to the audio thread, not edits.

**Silence skipping.** An insert whose input is exact digital silence for longer than its
chain's tail (each `Processor::tail_samples`, summed, plus the dry-path delays) is not
processed: its effects are settled once (`Processor::park`) and skipped until sound returns;
an effect with no known tail (`None`, e.g. hosted plugins) keeps its insert running. Tune,
Voice and Denoise idle after a second of silence themselves, so a skipped chain and a
processed one give the same output (`crates/engine/tests/silence_skip.rs`, below -120 dB).
Live keyboard events do not take the core lock: they read an atomic snapshot (device live,
target channel, armed insert; refreshed on every sync) and push into a dedicated ring that
the audio thread drains with the command ring. Offline renders may run the insert chains on
worker threads (`Engine::set_render_workers`), bit-identical to one thread.

Effect types: `eq` (7 `bands`), `comp`, `sat` (character saturator, see
`docs/saturator.md`), `delay`, `reverb`, `limit`,
`vst3` (`plugin: {format,path,cid,name,vendor,state}`). Parameter ids, ranges
and presets come from `fx_catalog` — the UI must not hard-code them.

## Ops (`crates/engine/src/ops.rs`)

All edits are "put" ops, applied atomically per batch:
`putClips{clips}`, `removeClips{ids}`, `putTrack{track}`, `putInsert{insert}`,
`removeInsert{id}`, `putFx{insertId,fx,index?}`, `removeFx{fxId}`,
`moveFx{fxId,to}`, `putPattern{pattern}`, `removePattern{id}` (also removes its clips), `putChannel{channel,index?}`, `removeChannel{id}`
(also removes its notes in every pattern; `removeSource` removes sampler channels of that
source), `putSource{source}`,
`removeSource{id}`, `putAutomation{automation}`, `removeAutomation{id}`,
`putComp{comp}`, `removeComp{id}`, `putNotebook{notebook}`, `setBpm{bpm}`, `setLoop{loop}`.

Every effect has `mix` (0..1 dry/wet, default 1). Automation:
`project.automations[] = {id, name, color, target, points:[{t, v}]}`; a clip of
kind `auto` with `ref` = automation id plays it (`offset` in beats into it).
Targets: `{kind:"fx", fxId, param}` (catalog id, or `band:<i>:f|g|q` for EQ:
f log 20–20k, g −30..30 dB, q log 0.1–18), `{kind:"mix", fxId}`,
`{kind:"insertVol"|"insertPan", id}`, `{kind:"trackVol", id}`,
`{kind:"chanVol"|"chanPan", id}` (Channel Rack channel), `{kind:"synth", chanId, param}`
(synth catalog id), `{kind:"keys", chanId, param:"release"|"dynamics"}` (release log
40–3000 ms, dynamics 0–100 %), `{kind:"plugin", fxId, paramId}` (a hosted VST3
parameter, `paramId` as `vst3_params` reports it; `v` is the plugin's normalised
0..1; `apply` refuses unknown fx/param pairs), `{kind:"clipGain"|"clipPan", id}` (an audio clip; gain
−24..+12 dB). `v` is 0..1; log parameters are normalised logarithmically, vol is the
fader position, pan maps −1..1 to 0..1, choices snap to an option. While playing, the
engine applies automation every block; channel and clip gain/pan glide across the block,
synth parameters glide like an edit, and an edit during playback keeps the automated value. New ids come from the
client: take them from `project.nextId` and increase locally (the engine bumps
`nextId` past any id it sees).

## Tauri commands

| command | args | returns |
| --- | --- | --- |
| `get_state` | – | `StateDto` |
| `apply_ops` | `{ops, label?, transient?}` | `{revision}`; error string on invalid batch (nothing applied) |
| `begin_gesture` / `end_gesture` | `{label?}` | – . Ops sent with `transient:true` between them become one undo step |
| `undo` / `redo` | – | `StateDto` |
| `transport_play` | `{fromBeat}` | – |
| `transport_stop` | – | – |
| `transport_set` | `{metro?, countIn?, rec?, monitor?}` | `TransportDto` |
| `telemetry_subscribe` | `{channel}` (Tauri `Channel<ArrayBuffer>`) | – |
| `watch_fx` | `{ids: number[]}` | – (effects whose windows are open; ≤ 8) |
| `fx_catalog` | – | `{types:{[type]:{name, params:[{id,label,min,max,def,unit,log}]}}, presets:{[type]:[{name, params?, bands?, beats?}]}}` |
| `import_audio` | `{paths, track?, beat?}` | `{sourceIds, clipIds}` (placed clips only when track/beat given) |
| `source_peaks` | `{id, norm?, rev?}` | `ArrayBuffer` of f32 `[min,max]` pairs, 100 per second of audio |
| `pitch_analyze` | `{clipId}` | `{rate, curve: (midi\|null)[], levels: number[], confidence: number[], notes: [{start,end,pitch,level}], backend: "rmvpe"\|"yin"}` — pitch every 1/rate s (null = unvoiced: breath, sibilant, silence); times in seconds of the source |
| `pitch_target` | `{clipId, reference?: RefTune}` | `PitchTarget` (below) — Tune to Reference for the clip, with `reference` or the clip's own `tune.reference` (preview before applying) |
| `f0_model_status` | – | `{id, name, bytes, installed, backend, licenseNote, licenseUrl, path, error}` |
| `f0_model_install` | – | – ; downloads RMVPE, verifies size and SHA-256, activates it; progress as event `model-progress {id,done,total,error?}` |
| `music_analyze` | `{clipId?, excludeFx?}` | `{tempo: {bpm, confidence, candidates:[{bpm,score}], firstBeat}\|null, key: {root, minor, confidence, candidates:[{root,minor,score}], chroma:number[12], tuningCents}\|null}` — tempo and key of a clip, or of the project's active audio clips (excluding tracks routed to `excludeFx`'s insert) |
| `auto_vocal_plan` | `{clipId, beatClipId?, style?: "natural"\|"hard"}` | `AutoVocalPlan` (`ui/src/api/types.ts`): `{version, style, analysis, chain: [{type, params, mix, enabled, note}], trackGainDb\|null, stages, warnings}` — "Auto wokal": measures the vocal clip's region and calibrates tune → rider → fet → comp → deess → reverb → limit on it (`crates/engine/src/autovocal.rs`; key from `beatClipId`, else from the tracks not routed to the vocal's insert). Read-only, seconds per minute of audio; the UI applies the chain with `apply_ops` |
| `clip_peaks` | `{clipId}` | like `source_peaks`, but of what the clip actually plays (norm/rev/pitch edits applied) |
| `trim_clip` | `{clipId}` | `{sourceId}` — writes a new file with only the clip region, clip now points at it |
| `flatten_comp` | `{clipId}` | `{sourceId}` — renders a comp clip to a new file; the clip becomes an `audio` clip, the takes go (one undo step) |
| `comp_audition` | `{compId, takeId?}` | – ; plays one whole take instead of the comp (`null` = the comp). Live only: not saved, not undone, ignored by renders |
| `note_audition` | `{ch, key?, vel?, pattern?}` | – ; plays one note of a channel now (step clicks, piano-roll keys). `pattern`: the edited pattern, so a track-routed channel is heard through that pattern's track. Not an edit |
| `pattern_play` | `{pattern}` (`null` stops) | – ; loops one pattern on its own. Starting it stops the song, starting the song stops it (FL's PAT / SONG). Position in telemetry `patBeat` |
| `preview_clip` / `preview_file` / `preview_stop` | `{clipId}` / `{path}` / – | – |
| `audio_hosts` | – | `[{id, name}]` (ids like `"wasapi"`, `"asio"`, `"alsa"`) |
| `audio_devices` | `{host}` | `{outputs: string[], inputs: string[]}` |
| `audio_get_config` / `audio_set_config` | `– / AudioConfig` | `AudioStatus` (a config without `recOffsetMs` keeps the saved one; changing only `recOffsetMs` does not restart the device) |
| `audio_measure_latency` | – | `{roundTripMs, reportedMs, suggestedOffsetMs, confidence}` — plays 8 clicks (~5 s) instead of the mix, records them back (mic at the speaker or a loopback cable), finds them by cross-correlation off the audio thread. Transport must be stopped; opens the input for the measurement. Saves nothing |
| `asio_control_panel` | – | – (opens the driver's own panel, Windows only) |
| `library_list` | – | `{kits:[{id,name,description,bytes,count,installed}], folders:string[]}` |
| `library_install` | `{id}` | – ; progress as event `library-progress {id,done,total,error?}` |
| `library_files` | `{kit?}` or `{folder?}` | `[{name, path, group, root?}]`; kit `Standard` = the standard library (always installed); `root` = the file's own pitch when the library knows it |
| `library_add_folder` / `library_remove_folder` | – (native dialog) / `{path}` | updated `folders` |
| `project_new` / `project_open` / `project_save` | – / `{path?}` / `{saveAs?}` | `StateDto` / `{state: StateDto, warnings}` or `{cancelled}` / `{path}` or `{cancelled}`. A save first keeps the file it replaces as `<name> (backup).wave` |
| `recovery_write` | – | `bool`; the autosave tick for a project with no file: writes its crash-recovery copy (`<data>/WAVE/Recovery/<session>.wave`), or removes it once the project has a file or nothing unsaved |
| `recovery_list` / `recovery_restore` / `recovery_discard` | – / `{id}` / `{id}` | `[{id, savedMs}]` newest first (copies of sessions that did not exit normally) / `{state, warnings}` (opened as an unsaved project) / – |
| `export_audio` | `{useLoop, tail, sampleRate}` | `{out, fromBeat, toBeat, tail, stats}` or `{cancelled:true}`; native save dialog; offline master → 32-bit float stereo WAV |
| `export_cancel` | – | – (abort an in-flight `export_audio` before a file is written) |
| `vst3_scan` | `{rescan}` | `[{path,cid,name,vendor,category}]` |
| `vst3_editor` | `{fxId, open}` | – (native window) |
| `vst3_params` | `{fxId}` | `[{id,name,value,display,def}]` |
| `vst3_set_param` | `{fxId,id,value}` | – |
| `cli_status` | – | `{installed, command, path, note?}` |

Events: `state` (a `StateDto`, after every change from any origin: UI, CLI,
undo, import, recording, file open), `library-progress`, `model-progress`, `audio-status`.

```ts
type StateDto = { project: Project; revision: number; canUndo: boolean; canRedo: boolean;
                  transport: TransportDto; path: string | null; dirty: boolean;
                  audition: { [compId: string]: number } };   // comp → auditioned take
type TransportDto = { playing: boolean; rec: boolean; monitor: boolean; metro: boolean; countIn: boolean; startBeat: number };
type AudioConfig = { host: string; output: string | null; input: string | null; sampleRate: number | null; bufferSize: number | null;
                     inputChannel?: number | null; recOffsetMs?: number | null };
type AudioStatus = AudioConfig & { running: boolean; actualRate: number; actualBuffer: number; latencyMs: number;
                                   inputChannels: number; error: string | null; note: string | null; asioCompiled: boolean;
                                   inputUnderruns: number; inputOverruns: number; inputDriftPpm: number };
```

## Telemetry frame (binary, little-endian, ≤ 60 Hz)

```
u32 magic = 0x54564157 ("WAVT")   u32 seq
f64 beat            // playhead in beats as heard (Shared::heard_beat: rendered beat − playback latency); negative during count-in
u8  flags           // bit0 playing, bit1 recording, bit2 previewing
u8  pad, u16 pad
f32 inLevel         // input peak since last frame (linear)
f32 previewPos      // seconds into the previewed file, -1 when not previewing
f32 cpu             // audio callback load 0..1
u32 nIns  × { u32 insertId, f32 peakL, f32 peakR }            // linear peaks since last frame
u32 nFx   × { u32 fxId, f32 inPeak, f32 outPeak, f32 grDb,     // only watched fx (every frame, 60 Hz)
              f32 meters[32],                                   // per-type live readings, see "Effect meters"
              f32 specIn[128], f32 specOut[128],               // dBFS, 128 log bands 20 Hz..20 kHz (reverb: specOut = wet only)
              f32 scopeIn[256], f32 scopeOut[256] }            // last samples, mono
u32 rec             // 0, or 1 followed by:
    u32 track, f64 clipStart, f32 seconds, u32 peakOffset, u32 nPeaks, f32 peaks[2*nPeaks]   // new [min,max] pairs at 100/s
f64 patBeat         // beats into the pattern the loop preview plays; −1 = stopped (decoders treat a missing field as −1)
```

## Performance budget

* Callback: < 30 % of the buffer period with 8 tracks × 3 built-in effects at 48 kHz / 128 frames.
* UI: playlist pan/zoom at 60 fps with 500 clips; 6 open effect windows (incl. 3 reverbs) at 60 fps.

## Effect meters (`meters[32]` in the telemetry frame)

| type | meters |
| --- | --- |
| `eq` | `[0..24]` current dynamic gain offset of band *k* in dB (0 for static bands) |
| `fet` | `[0]` gain reduction dB, `[1]` input peak dBFS (after the Input knob), `[2]` output peak dBFS |
| `deess` | `[0]` gain reduction dB, `[1]` sidechain (high band) level dB — with Detect = Relative the high/low band energy ratio dB, `[2]` sibilance 0..1, `[3]` threshold dB |
| `rider` | `[0]` gain applied dB, `[1]` word level dB (K-weighted, ~200 ms), `[2]` phrase level dB (~2 s, confident vocal only), `[3]` vocal confidence 0..1, all at the moment being heard. Look-ahead (`look`): Live (default, value 2) adds no latency (trailing windows, forward slew only, gain applied 20 ms behind the analysis); Short ≈ 0.3 s and Long ≈ 1.6 s (the offline research windows exactly) are compensated by PDC |
| `denoise` | `[0..16]` learned noise floor dB per band, `[16..32]` gain applied per band dB (≤ 0); 16 log bands 50 Hz – 16 kHz. Total reduction in `grDb`. Adds `fft_size` (2048 at ≤ 64 kHz) samples of latency, compensated by PDC |
| `tune` | `[0]` detected input Hz (0 = unvoiced), `[1]` target note Hz, `[2]` output Hz, `[3]` confidence 0..1, `[4]` input offset from target in cents, `[5]` correction applied in cents |
| `voice` | `[0]` detected input Hz (0 = unvoiced), `[1]` output Hz, `[2]` confidence 0..1, `[3]` pitch shift applied in semitones. Same latency as `tune`, compensated by PDC; an effect's dry/wet (`fx.mix`) delays its dry path by the effect's latency, so blends line up |
| `half` | `[0]` position in the loop window 0..1, `[1]` 1 = slowing down (0 = stopped, or passing the input through until the first window after a playhead jump), `[2]`/`[3]` gains of read heads A/B (crossfade), `[4]` speed divisor (1.5 / 2 / 4), `[5]` window in beats, `[6]` crossfade ms (negative = before the window start, Percussive/FX), `[7]`/`[8]` band edges Hz (0 = that side not split). Zero latency; follows the song and the pattern loop preview (`ProcessCtx` carries the preview's beat) |

## Take lanes and comping

Recording with a loop set cycles over it and makes **one take per pass**;
recording over an existing clip adds a take instead of overwriting it. The
takes and the comp built from them are a project-level take group:

```jsonc
"comps": [{ "id": 2000, "name": "Comp 1", "xfade": 0.01,             // default boundary crossfade, s
  "takes":   [{ "id": 2001, "name": "Take 1", "ref": 1000,            // source id
                "start": 0, "len": 16, "offset": 0 }],                // comp beats; offset = beats into the source
  "regions": [{ "take": 2001, "from": 0, "to": 4, "xfade": 0.02 }] }] // sorted, non-overlapping, gaps silent
"clips":   [{ "kind": "comp", "ref": 2000, "start": 32, "len": 16, "offset": 0, … }]
```

**Why a clip kind and not takes on the track.** A comp is played by an
ordinary clip of kind `comp`, exactly like an audio clip plays a source:
comp beat = `clip.offset + (timeline beat − clip.start)`. So moving, trimming,
splitting, muting, gain, pan and fades of the comp reuse the clip machinery
and the existing timeline build; the take group itself never stores timeline
positions. Copies of a comp clip play the same group (like pattern clips).
Projects without `comps` open unchanged (`#[serde(default)]`).

* Takes are never cut. `regions` choose which take plays where; the UI's swipe
  writes a range (replacing what overlapped, merging touching pieces of the
  same take: `Comp::write`, mirrored in `ui/src/lib/comp.ts`).
* Every region boundary is an **equal-power crossfade** (cos/sin) centred on
  the boundary; its length is the region's `xfade` (at its start) or the
  comp's default, clamped so both halves fit. A region followed by silence
  fades out with the same length.
* The comp is resolved off the audio thread into segments (`comp::build`)
  when the timeline is rebuilt; the callback only mixes them (no allocation).
  Offline render and "flatten" use the same code, so they are sample-identical.
* Landing a recording (`comp::land_takes`): over a comp clip → new takes of its
  comp; over plain audio clips → those become the first takes of a new comp;
  elsewhere one pass is a plain audio clip and several passes a new comp. The
  newest take plays over its range (a last pass cut short by Stop only where
  it has audio). A last pass shorter than one beat is dropped. The audio
  thread marks each loop wrap with a NaN frame in the input ring.
* Recordings stay sources (`"Take N"`) next to the project like any audio
  (`<project> Audio/`, or the scratch folder until the first save); a new file
  never overwrites an existing one (`Take 2 1234 (2).wav`).
* **Saving and crash recovery** (`Engine::save`, `app/src/recovery.rs`). A save
  writes a temp file and renames it over the project, after copying the file it
  replaces to `<name> (backup).wave` (an ordinary project, so File > Open opens
  it). The autosave timer (Settings, default 2 min) saves a project that has a
  file; one that was never saved goes to `Engine::save_copy` in the Recovery
  folder instead (recordings stay in the scratch folder, so the copy finds them).
  Each running WAVE holds an exclusive lock on its own `<session>.lock`; a copy
  whose lock nobody holds belongs to a session that crashed or was killed, and
  the UI offers it at the next start (`Engine::open_recovered`: no path, dirty).
  New, Open, Save and a normal exit remove the session's copy.
* **Takes line up with what was heard.** Input captured at a frame was sung
  to playback heard *round trip + PDC* frames earlier. `finalize_take` drops
  that many frames from the front; pass marks stay put, since they already
  sit on the heard loop boundary. The last *latency* of a take (still in
  flight at Stop) is not captured.
  * *Round trip, per callback* (`wave-audio-io`): output playback − callback
    (cpal) + the age of the input frame played first in this callback. Each
    input packet publishes `origin` = its capture instant − frames pushed
    before it, so input frame *i* was captured at `origin + i/rate`; the
    output callback knows which frame it plays (the jitter buffer's read
    position) and its own instant, so the age needs no ring-backlog guess and
    holds for any packet phase, packet size and a separate input device. Used
    where both streams share the system clock (WASAPI QPC, CoreAudio); other
    hosts fall back to *out + backlog + input callback−capture* (one buffer
    each way when a host reports nothing).
  * *Measured during the take, not at its start.* `Shared::add_take_latency`
    collects one sample per callback while the take captures real input
    (count-in excluded, nothing before the input's first packet), and
    `RecBuf::resolve` aligns the take with their mean. The estimate published
    at the start (`Shared::io_latency`, smoothed) is only the fallback when
    nothing was measured. Before, the start value was used: with monitoring
    off the input opens only when record is armed, so a take started soon
    after (or a punch-in, which used to restart both streams and lose the
    take) was aligned with an unsettled guess — "worse with monitoring off".
    PDC stays frozen at the start (the graph keeps it until the take ends).
  * *Calibration* (`AudioConfig.recOffsetMs`, signed ms, saved with the audio
    config, default 0) is added to every sample: converters, USB and driver
    buffers that timestamps do not report. `audio_measure_latency` measures
    it: the output callback plays a click plan instead of the mix and keeps,
    per output frame, the input frame consumed with it; a click rendered at
    output frame *a* found at *a + lag* means `lag` is exactly the skip a
    take needs, so `suggestedOffsetMs = lag − reported` (reported = mean
    per-callback round trip during the measurement). Normalised
    cross-correlation of a 4 ms windowed noise burst over 8 irregularly
    spaced clicks, lags 0–400 ms; confidence = clicks agreeing within 1 ms ×
    peak prominence over echoes. The job travels to the audio thread and back
    in a preallocated `Box` through lock-free rings; analysis runs on the
    caller's thread.
* **Output watchdog** (`AudioIo::spawn_watched`). A driver error ends a cpal
  stream for good (PipeWire/ALSA restarted, a USB interface unplugged), and a
  panic in the output callback would unwind into the driver (on ALSA it ends
  cpal's thread silently). The callback runs the engine under `catch_unwind`
  (no cost unless one happens) and plays silence after a crash; the audio-io
  thread checks every 500 ms and reopens a dead output with the same config
  and a fresh `Rt` (input errors only drop the microphone). When reopening
  fails it retries after 1, 2, 4 … 30 s until it works or the user changes the
  settings. `AudioStatus.restarts` counts the reopenings, the reason is in
  `note`, and each change is pushed as an `audio-status` event that the UI
  shows in its status line. A callback that *blocks* is not reopened, because
  dropping a stream whose callback is stuck can hang the thread that drops it.
* **Input jitter buffer** (`crates/audio-io/src/jitter.rs`). The input stream
  opens and closes on its own (`AudioIo::set_input` hands the output callback
  a new ring through a lock-free swap ring; the old one is freed off the audio
  thread), so arming record or monitoring never restarts the output. Input
  packets arrive on their own schedule (WASAPI: 480-frame periods against
  512-frame output callbacks) and, with a separate input device, on their own
  clock. Before, the output took what had arrived and zero-padded the rest:
  a packet arriving a moment late put ~477 exact zeros into the take several
  times a second (clicks), and pile-ups beyond two callbacks were dropped.
  Now: *prefill* to two input periods + half a callback + a 2 ms margin (one
  period on ASIO, whose input arrives in step with the output) before
  reading; *drift steering*: after 50 callbacks the backlog it settled at
  becomes the anchor and the read step is nudged (P control, 1 ms deadband,
  ≤ ±0.1 %) through a 32-tap Kaiser-windowed sinc, which is an exact copy at
  a step of 1, so one device is never resampled and a ±200 ppm input clock
  is followed without dropping or repeating samples; *real stalls* fade the
  last sample out over 2 ms, refill with a larger margin (+¼ callback per
  underrun, ≤ 50 ms) and fade back in; *overflow* (output stalled) fades out,
  skips to the target and fades in. Counted in `AudioStatus.inputUnderruns`
  / `inputOverruns`; `inputDriftPpm` is the followed clock difference. The
  cost is a little more input latency (~2 input periods) for monitoring.
* **Latency while recording.** Plugin delay compensation (PDC) delays every
  non-master insert to the slowest one (`pdc`), so one long look-ahead effect
  (the vocal Rider, ≈ 1.6 s on Long) delays all playback. Live input must not
  wait for it: **low-latency monitoring** is on whenever monitoring is on, a
  track is armed, the device delivers input and the track's insert is not the
  master. That insert then (a) skips its effects with latency (look-ahead
  Rider, tuner, denoiser, limiter, de-esser: bypassed for monitoring only, the
  take itself is always the raw input), (b) runs its PDC delay of `pdc` frames
  *before* its chain, on the playback summed into it, and (c) adds the live
  input (× track gain, muted with the track) after that delay. So the voice
  reaches the master with no PDC and no look-ahead, while the insert's own
  clips and patterns still come out `pdc` late like every other insert. The
  master chain's own latency (a few ms of limiter look-ahead) still applies to
  the voice. Switching the monitored insert clears its delay lines (a short
  gap, never stale audio). `playback_latency` does not depend on monitoring,
  so toggling it never moves the playback or the take alignment.
* The **metronome and count-in click** run through a delay line of the
  playback latency (`Graph::with_latency`, allocated with the graph), so they
  sound on the beats of the music; the **playhead** in telemetry is
  `Shared::heard_beat` (rendered beat − playback latency, held at the run's
  start until sound comes out, wrapped back into the loop after a wrap).
* A take's `skip` is fixed at its start, so **mixer changes that would change
  the playback latency during a take are deferred** (`Core::graph_deferred`):
  the audio thread keeps its graph (fader/param changes still apply) and the
  new graph is sent when the take ends. A parameter change that alters an
  effect's latency (e.g. the Rider's look-ahead) rebuilds the graph like a
  bypass toggle, so PDC always matches the parameters.
* **Instant start** (live stream only, `Rt::set_instant_start`): after Play, a
  seek or a pattern play the audio thread renders the playback latency muted
  ahead of the device clock (pre-roll), as many blocks per callback as fit a
  budget of 70 % of the period (its own frames included, at the slowest
  per-frame cost seen), so the start position is heard at once and on the
  timeline instead of after the latency. With a mix costing `c` of real time a
  pre-roll of `L` is heard after about `L · c / 0.7` (plus a callback). A jump
  while sound plays ramps the last output to zero over 3 ms while the new
  music fades in. Not while recording (a take's `skip` assumes the delayed
  start) and not offline (renders drop the latency themselves).
* **Punch-in**: turning `rec` on while playing (with an armed track) starts a
  take at the playhead (`Cmd::PunchIn`; the audio thread reports the exact beat
  in `Shared::rec_start`); with a loop set it records in passes like loop
  recording. Turning `rec` off punches out and keeps playing.
* The device records **one input channel** (`AudioConfig.inputChannel`,
  0-based, default the first) as mono in both sides, so a mic on input 1 of a
  stereo interface is not left-only. Offline renders drop the PDC delay, so an
  export starts on its first beat and keeps its last `PDC` frames.

## Lyrics notebook ("Notatnik", F8)

```jsonc
"notebook": { "pages": [{ "id": 3000, "name": "Zwrotka 1", "startBar": 1,
                          "text": "[Zwrotka]\nidę przez miasto nocą\n\nświatła mi nie pomogą" }] }
```

Text that belongs to the song, so it is **project data**, not UI state: saved with the
project, undone, visible to the CLI (`wave context` lists the pages, `--full` has the
text, `putNotebook` through `apply`). It never touches the audio graph (the op returns an
empty `Dirty`). Old files without `notebook` open with no pages (`#[serde(default)]`).

* **One line = one bar**, counted from the page's `startBar` (1-based, 4 beats per bar,
  like the playlist ruler). An empty line is a bar of rest and still counts; a line whose
  trimmed text is `[…]` is a header and takes no bar. `ui/src/lib/notebook.ts` holds the
  mapping (`lineBars`, `lineOfBar`, `barOfBeat`), the Polish syllable counter and the rhyme
  grouping (ending from the penultimate vowel — Polish stress — with `ó`/`u`, `rz`/`ż`,
  `ch`/`h` and final devoicing folded; an ending needs ≥ 2 lines to get a colour).
* `putNotebook` replaces all pages (tab order = array order); ids are unique, `startBar`
  1..9999, ≤ 64 pages, ≤ 100 kB of text per page.
* **Typing is coalesced in the UI** (`ui/src/state/notebook.ts`): the textarea's text is a
  pending draft sent as one `putNotebook` (one undo step) after 0.9 s without typing, or on
  blur, page switch, undo/redo, save, new/open. Ctrl+Z inside the editor flushes the draft
  and runs the project's undo, never the textarea's own.
* **Draft backup.** Until the text is in a saved file, a copy sits in `localStorage`
  (`wave.notebook.backup:<path>`), together with the hash of the notebook it was based on.
  When that project is loaded again and its notebook is still exactly that base (nothing
  newer was saved), the copy is put back as one undoable edit. Saving drops the copy.
* **The window** is project-level, not part of the effect-window layer: F5 and clicks on
  the playlist never hide it. Geometry, font size (Ctrl+wheel) and the rhyme toggle are
  per-user view settings in `localStorage`. While the transport plays, the bar under the
  playhead (from telemetry) is highlighted on the current page and kept centred, unless the
  user typed or scrolled in the last 2 s. Clicking a bar number moves the playhead there.
  Rendering: a transparent-text `<textarea>` over a mirror of the same text (same font,
  fixed line height, no soft wrap), so gutter, highlight and syllable column stay aligned.

## Pitch edits on audio clips

`clip.tune = { notes: [{start, end, shift, drift, formant, gain}], pencil: [[sec, midi], …] }`
(seconds into the source; `shift`/`formant` in cents; `drift` 0..1 flattens pitch
movement inside the note toward its centre; `gain` dB). Start from
`pitch_analyze` notes, edit, send with `putClips`. Rendering is offline and
latency-free; during a gesture the previous render keeps playing and the new
one is made at `end_gesture`.

### Tune to Reference (`crates/engine/src/reference.rs`)

`clip.tune.reference = {source, correction, vibrato, influence, octave: number|null, key, scale}`
(0..1 amounts; `octave` null = automatic) makes another source the clip's
melody. Offline, deterministic, and the same code for GUI, CLI and tests:

1. **Analysis** — pitch (`F0Estimator`: RMVPE through `ort` when its model is
   installed, else YIN; 200 frames/s, MIDI, NaN = unvoiced) and content
   (MFCC, 100 frames/s; the spectral envelope c1–c6, which survives another
   singer, octave and vowel colour) of take and reference.
2. **Alignment** — coarse subsequence DTW over the whole reference finds where
   the take is; a banded DTW refines it with pitch class and contour shape.
   If that scores low, a content-only path is tried too. A jump-and-stall of
   the path (an implausible tempo map) is bridged by a straight line and
   aligned again narrowly around it, kept only if the content there vouches
   for it.
3. **Confidence** per frame — plausible tempo map (a veto, lifted where the
   content matches far better than chance), content against chance, contour
   correlation, and a gate on the take being more than 7 semitones from the
   reference (exact octave slips of the detector excepted). The median over
   sung frames is the score; below 0.3 the reference is refused and nothing
   is corrected.
4. **Register** — the multiple of 12 bringing the reference closest to the take.
5. **Target** — the reference's notes (segmented on the take's time axis) give
   centres; the take's own notes give vibrato and transitions (movement around
   a drift curve; octave slips of the detector and transitions to neighbours
   excluded from the drift). `target = take + correction·gate(confidence)·(centre + vibrato·movement − take)`,
   the interval folded to ±6 semitones so a detector octave slip still moves by
   the right amount.
6. **Render** — the existing TD-PSOLA with a continuous ratio(t), smoothed
   ±17.5 ms; manual note edits apply on top; a clip whose correction is zero
   everywhere renders bit-identical to its source.

```ts
type PitchTarget = { rate: number; start: number;            // first analysis frame of the region
  sourceF0: (number|null)[]; referenceF0: (number|null)[]; targetF0: (number|null)[];
  alignmentConfidence: number[]; refTime: number[];          // seconds into the reference
  octave: number; notes: Note[]; score: number; reason: string | null };  // reason set = refused
```

The RMVPE model is not shipped: `f0_model_install` downloads it (URL,
size, SHA-256 and licence link are constants in `crates/rmvpe`) to
`<data>/Models/rmvpe.onnx`; `WAVE_RMVPE_MODEL` overrides the path.
`crates/tune-bench` measures the whole pipeline (see `docs/cli.md`).

## Updates and release notes

The app updates itself from GitHub Releases (public repository: no token). See
[releasing.md](releasing.md) for how a release is made and signed.

* `app/src/updater.rs` wraps `tauri-plugin-updater`: `update_check` (finds a newer
  `latest.json`), `update_download` (progress on the `update-progress` event, signature
  verified, nothing restarts) and `update_install` (detaches plugin editors, stops audio, runs
  the installer, restarts; on failure audio comes back and the old version keeps running).
  `app_info` (also `pluginEditors`: VST3 editor windows exist, Windows only), `update_set_auto` and `update_mark_seen` read and write `settings.json`
  (`seenVersion`, `noAutoUpdate`). Views reach them only through `backend.ts`
  (`appInfo`, `updateCheck`, …); `api/mockUpdates.ts` is the browser mock with a fake newer release.
* `ui/src/state/updateCtl.ts` is the state machine
  (`idle → available → downloading → ready → installing`); `state/updates.ts` is its signal.
  An automatic check (8 s after start, then every 6 h, can be switched off in Settings) never
  opens a dialog, only a status-bar button; downloading happens in the background; the restart
  needs a click and the unsaved-changes question.
* **What's new** reads the `CHANGELOG.md` bundled into the UI (`lib/changelog.ts`) and opens
  once at the first start of a new version, for every version since `seenVersion`. A fresh
  install shows nothing; it can be reopened from Settings → General → Updates.
* The updater is app-shell behaviour, not a project action, so it has no `wave` CLI command.
