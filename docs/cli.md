# `wave` — the command line

The CLI talks to the **running app** and edits the same project you see,
with the same undo history. There is nothing to install: on start the app puts
a `wave` launcher on your PATH (`%LOCALAPPDATA%\Microsoft\WindowsApps\wave.cmd`
on Windows, `~/.local/bin/wave` elsewhere). Settings → CLI shows where it went.
Open a new terminal after the first start and run:

```sh
wave status
wave context --pretty
```

stdout is only JSON (`--pretty` to indent); messages go to stderr. Exit codes:
`0` ok, `1` bad usage, `2` WAVE not running / unreachable, `3` the app refused
(invalid op, revision conflict, missing plugin …).

## Reading

| command | what you get |
| --- | --- |
| `wave status` | transport, revision, file, audio device, CLI path |
| `wave context` | summary: tracks (index, insert, volume, clip count), inserts with effects and parameters, patterns, channels, sources, `nextId` |
| `wave channels` | the Channel Rack: id, name, `sample` (+ source and sampler settings)/`synth` (+ preset)/`keys`, volume, pan, mute/solo, mixer insert (`null` = the clip's track), root key, note count |
| `wave pattern [--id N]` | a pattern as one step string per channel (`x` full velocity, `1`–`9` = 10–90 %, `.` empty, a space every beat) plus `notes` for melodic rows, bars, minimum bars, swing % |
| `wave context --full` | the whole project JSON (every clip, note, parameter) |
| `wave catalog` | effect types, parameter ids/ranges/units, presets, command list |
| `wave schema` | every op `apply` accepts, with units and an example |
| `wave analyze [--clip ID] [--exclude-fx ID]` | detect tempo (BPM, ½×/2× candidates) and musical key (top 3 candidates, 12-note chroma, tuning offset) |

## Editing

Each command is one undo step. Add `--if-revision N` (the `revision` from your
last read) and the edit is refused with `REVISION_CONFLICT` if the musician
changed something meanwhile — re-read and decide, don't blindly retry.

```sh
wave bpm 140
wave add-fx --insert 6 --type reverb --preset 3          # → {"fxId":1203,…}
wave set-param --fx 1203 --param mix --value 30
wave preset --fx 1203 --index 4
wave import kick.wav loop.wav --track 3 --beat 16
wave apply --file edit.json --label "Busier hats"
wave undo
```

`edit.json` is an array of ops (see `wave schema`), applied atomically:

```json
[
  {"op": "putPattern", "pattern": {"id": 2, "name": "Hats", "color": "#e0a44a", "len": 4, "swing": 0.4,
    "notes": [{"ch": 1002, "step": 0, "len": 1}, {"ch": 1002, "step": 2, "len": 1, "v": 0.6}]}},
  {"op": "setLoop", "loop": {"a": 0, "b": 16}}
]
```

Every note names its channel with `ch` (see `wave channels`).

Units: playlist time in **beats**; pattern `step`/`len` in **sixteenths**;
fades in seconds; track/insert `vol` is a fader position (0.7937 = 0 dB);
clip `gain` in dB; pan −1..1. New ids: start at `context.nextId`.

## Patterns and channels

Channels (Channel Rack rows) are shared by all patterns; a note plays channel `ch`.
Steps are sixteenths. A pattern is as long as its notes (the end of the last note rounded
up to a bar, like FL Studio); `--bars N` sets the shortest it may be, for empty bars kept on
purpose (a crash every second bar). Swing 0–100 % (delays every odd 16th by up to half a
step). Each edit is one undo step.

```sh
wave channels                                     # ids, kinds, routing
wave channels --add snare-studio                  # a sampler of a standard-library sample (file or display name, or the start of one: "kick")
wave channels --ch "Studio Snare" --param cut --value 1   # one sampler setting: start, end, reverse, norm, semi, fine, gate, attack, decay, sustain, release, cut
wave channels --sample 1001                       # a sampler channel playing source 1001
wave channels --synth "808 Czysty"                # a synth channel with a factory preset ("" = blank patch)
wave channels --ch 1005 --preset "Pad Ciepły"     # load a synth preset (ids/names in `wave catalog` → effects.synth)
wave channels --ch 1005 --param flt_cutoff --value 900   # one synth knob
wave channels --keys                              # WAVE Keys: the sampled grand piano (see docs/keys.md)
wave channels --ch "WAVE Keys" --param release --value 600   # its knobs: release (ms), dynamics (%)
wave channels --remove 1004                       # (or a name) — also removes its notes
wave pattern --id 2                               # step strings per channel
wave pattern --id 2 --ch hat --steps "x.x. x.x. x.x5 x.x9"     # replace the hat's notes
wave pattern --id 2 --ch kick --steps "x... x... x... x..." --vel 0.9
wave pattern --id 3 --ch bass --notes '[{"step":0,"len":3,"p":33},{"step":4,"len":2,"p":40,"v":0.8}]'
wave pattern --id 2 --ch hat --clear
wave pattern --id 2 --bars 2 --swing 55 --name "Hats B"
wave pattern --new --name Fill --bars 1           # → its id in the reply
wave pattern --id 2 --play                        # loop it on its own (stops the song); --stop
```

`--ch` takes a channel id or name (`kick`, `Hats 2` …). With `apply`, the ops are
`putPattern` / `putChannel` / `removeChannel` (see `wave schema`). When a pattern's
length changes (notes added past its end or removed, `--bars`), clips that show exactly one
whole pass of it follow; clips sized on purpose keep their length.

## Takes and comping

A track you recorded with a loop set (one take per pass), or recorded over
an existing clip, has a comp clip. Times here are **timeline beats**,
crossfades **milliseconds**; add `--clip ID` when a track has several comps.

```sh
wave takes --track 5                       # takes, comp regions, crossfades
wave comp --track 5 --set '[{"take":2001,"from":32,"to":36},{"take":2002,"from":36,"to":40,"xfadeMs":20}]'
wave comp --track 5 --take 2003            # one whole take
wave comp --track 5 --xfade 10             # default crossfade at region boundaries
wave comp --track 5 --remove-unused        # drop takes the comp does not play
wave comp --track 5 --audition 2002        # hear one take instead of the comp (`off` to stop; not an edit)
wave comp --track 5 --flatten              # render to one audio clip, takes removed (undo brings them back)
```

Each edit is one undo step. Through `apply`, the ops are `putComp` /
`removeComp` with comp-beat times and crossfades in seconds (see `wave schema`).

## Lyrics (the notebook)

`wave context` lists the notebook pages (`id`, `name`, `startBar`, `lines`);
`wave context --full` includes their text. Each line of a page is one bar from
`startBar`, `[…]` lines are headers. Write them with one op that replaces every
page (one undo step):

```sh
wave apply --json '[{"op":"putNotebook","notebook":{"pages":[{"id":3000,"name":"Refren","startBar":17,"text":"[Refren]\nto jest nasz czas\nnasza noc"}]}}]'
```

## Listening without speakers

```sh
wave render --out bounce.wav --from 0 --to 32
```

renders offline (independent of the audio device, so it works while the
musician keeps playing) and prints peak/RMS/crest/clipping so an agent can
check its work, then listen to the WAV. The same bounce is available in the
app as **Eksport** (`Ctrl+Shift+R`): song or loop, a 0/2/8 s effect tail, and
44.1 or 48 kHz, written as a 32-bit float stereo WAV.

## Tune to Reference

Pulls a vocal take onto another performance of the same melody (a guide vocal,
an acapella, a better take) that is already a source in the project. Nothing is
generated and the take is not time-stretched: the reference is aligned to the
take (content, then pitch shape), brought to the take's octave, and its notes
become the take's target; the singer's vibrato and transitions are kept.
Where the alignment is not trusted (another song, a different part) the take
is left as sung.

```sh
wave tune-ref --clip 412 --reference 7                       # source 7 = the guide vocal
wave tune-ref --clip 412 --reference 7 --correction 80 --vibrato 100 --influence 100 --octave auto
wave pitch-target --clip 412            # score, octave, the reference's notes, low-confidence stretches
wave pitch-target --clip 412 --full     # plus every curve (source, reference, target, confidence)
wave tune-ref --clip 412 --off          # back to manual pitch edits
wave f0-model                           # which pitch detector runs (RMVPE or YIN) and its install state
```

`--correction` (how far toward the target), `--vibrato` (how much of the
singer's own movement is kept) and `--influence` (100 = the reference's note
centres, 0 = nearest note of `--key`/`--scale`) are percentages. Manual note
edits on the clip still apply on top.

Without a running app, a whole file:

```sh
wave tune-file --take take.wav --reference guide.wav --out tuned.wav --json target.json
```

uses the installed RMVPE model, or `--model rmvpe.onnx`, or `--yin`.

**Pitch detector.** WAVE detects pitch with YIN out of the box. The RMVPE
model (362 MB, from the RVC project; read its licence first) is better on
real voices: Pitch Editor → *Pobierz RMVPE* downloads it after showing the
licence, checks its SHA-256 and switches to it. `WAVE_RMVPE_MODEL=path`
points at a copy you already have.

**Benchmark.** `cargo run --release -p wave-tune-bench -- synth` (synthetic
takes with ±25…400 ct errors and drift against references in other voices,
octaves and tempi, plus unrelated references that must be refused),
`… -- vocalset --root DIR` (Annotated VocalSet: real singers with injected
errors, cross-singer references, mismatched references) and `… -- ab --out DIR`
(a blind A/B listening kit; `ab-score` evaluates the filled-in ratings). Add
`--rmvpe model.onnx` or `--yin`, and `--json out.json` for the full results.
Exit code 0 means every criterion passed.

## Plugins

```sh
wave vst3                       # installed VST3 effects (cached scan; --rescan)
wave vst3-add --insert 2 --path "C:\…\Pro-Q 3.vst3" --cid 72C4DB…
wave vst3-params --fx 1210 --query freq
wave vst3-set --fx 1210 --id 42 --value 0.3
wave auto-add --fx 1210 --param 42 --points "0:0.2,8:0.8" --name "Reverb decay"
                # automate a hosted plugin parameter: an envelope plus the
                # playlist clip that plays it; ids come from context.nextId
```

## Several apps open

`wave sessions` lists them; pass `--session ID` (a prefix is enough).
