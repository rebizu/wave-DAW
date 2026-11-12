//! `wave` — the command line for a running WAVE app (and `tune-file`, which runs offline).
//!
//! stdout is only the JSON result; diagnostics go to stderr.
//! Exit codes: 0 ok, 1 usage, 2 no session / connection, 3 the app refused the request.

use std::process::ExitCode;

use serde_json::{json, Value};
use wave_session as ws;

const HELP: &str = r#"wave — control the open WAVE project

USAGE
  wave <command> [options]                 (add --session ID when several apps are open)

READ
  sessions                                 list running WAVE apps
  status                                   transport, revision, file, audio device
  context [--full]                         project summary (--full: every clip, note and parameter)
  catalog                                  effect types, parameters and presets
  schema                                   every op `apply` accepts, with an example
  analyze [--clip ID] [--exclude-fx ID]    detect tempo (BPM) and musical key

EDIT  (all edits are one undo step each; --if-revision N refuses to act if the project moved on)
  apply --file ops.json | --json '[...]'   apply a batch of ops atomically   [--label TEXT]
  undo | redo
  bpm <value>
  add-fx --insert ID --type eq|comp|sat|delay|reverb|limit [--preset N]
  set-param --fx ID --param NAME --value V
  preset --fx ID --index N
  import <file>... [--track N --beat B]    load audio, optionally place it

PATTERNS & CHANNELS  (steps: x = on, 1-9 = velocity 10-90 %, . = off; spaces and | are ignored)
  channels                                 Channel Rack: ids, instrument/sample, volume, pan, insert
  channels --add kick-808|snare|marimba…   add a sampler of a standard-library sample (--sample SOURCE_ID: of a project source)
  channels --keys                          add WAVE Keys, the sampled grand piano
  channels --synth "PRESET"                add a synth channel ("" = blank patch); --ch C --preset NAME loads one
  channels --ch C --param ID --value V     one knob of a synth (ids in `catalog`) or of WAVE Keys (release ms, dynamics %)
  channels --remove ID|NAME                remove a channel and its notes
  pattern [--id N]                         a pattern as step strings per channel
  pattern --id N --ch C --steps "x... x... x... x..." [--vel 0.8]   replace channel C's notes
  pattern --id N --ch C --notes '[{"step":0,"len":4,"p":36,"v":0.9}]'  piano-roll notes (p = MIDI key)
  pattern --id N --ch C --clear            remove channel C's notes from the pattern
  pattern --id N [--bars N (minimum)] [--swing 0..100] [--name TEXT]      (--new instead of --id: a new pattern)
  pattern --id N --play | --stop           loop the pattern on its own (stops the song)

TAKES & COMPING  (times in timeline beats; add --clip ID when a track has several comps)
  takes --track N                          takes, comp regions and crossfades of the track
  comp --track N --set '[{"take":ID,"from":B,"to":B,"xfadeMs":20},...]'   replace the comp
  comp --track N --take ID                 play one whole take
  comp --track N --xfade MS                default crossfade at region boundaries
  comp --track N --remove-unused           drop takes the comp does not play
  comp --track N --flatten                 render the comp to one audio clip (removes the takes)
  comp --track N --audition ID|off         play one take instead of the comp (not an edit)

TRANSPORT
  play [--from BEAT] | stop

FILES
  render --out file.wav|.mp3 [--from B --to B --tail SEC --rate HZ --format wav|mp3 --kbps 128|192|256|320]
                                           bounce offline (mp3 chosen by .mp3 extension); prints level stats
  save [--path FILE] | open <FILE>

PITCH  (Tune to Reference: pull a vocal take onto another performance of the same melody)
  tune-ref --clip ID --reference SOURCE [--correction 0..100 --vibrato 0..100 --influence 0..100
           --octave auto|-12|0|12 --key 0..11 --scale 0..4]     switch a clip to it (--off: manual)
  pitch-target --clip ID [--full]          the computed target: score, octave, notes (--full: curves)
  f0-model                                 pitch detector in use; RMVPE model install state
  tune-file --take u.wav --reference r.wav --out o.wav [--json target.json]
            [--model rmvpe.onnx | --yin] [same options as tune-ref]   headless; needs no running app

PLUGINS
  vst3 [--rescan]                          installed VST3 effects
  vst3-add --insert ID --path P --cid C    put a VST3 on an insert
  vst3-params --fx ID [--query TEXT]       parameters of a VST3 instance
  vst3-set --fx ID --id PARAM --value 0..1
  auto-add --fx ID --param ID [--points "0:0.2,4:0.8" | --value V] [--track N --name TEXT]
                                           automate a hosted plugin parameter (adds the playlist clip too)

  call <command> [--args JSON]             raw request (see `catalog`)
  --pretty                                 indent JSON output
"#;

struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Self {
        let mut pos = vec![];
        let mut opts = vec![];
        let mut it = raw.into_iter().peekable();
        while let Some(a) = it.next() {
            if let Some(k) = a.strip_prefix("--") {
                let v = match it.peek() {
                    Some(n) if !n.starts_with("--") || n.parse::<f64>().is_ok() => it.next(),
                    _ => None,
                };
                opts.push((k.to_string(), v));
            } else {
                pos.push(a);
            }
        }
        Self { pos, opts }
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.opts.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, k: &str) -> bool {
        self.opts.iter().any(|(n, _)| n == k)
    }
    fn num(&self, k: &str) -> Result<Option<f64>, String> {
        self.get(k).map(|v| v.parse::<f64>().map_err(|_| format!("--{k} must be a number, got {v}"))).transpose()
    }
    fn need(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or_else(|| format!("missing --{k}"))
    }
}

enum Fail {
    Usage(String),
    Conn(String),
    App(String),
}

/// Runs the CLI with `raw` arguments (without the program name).
pub fn run_cli(raw: Vec<String>) -> ExitCode {
    let a = Args::parse(raw);
    let pretty = a.has("pretty");
    let Some(cmd) = a.pos.first().cloned() else {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    };
    if cmd == "help" || a.has("help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match run(&cmd, &a) {
        Ok(v) => {
            let s = if pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) };
            println!("{}", s.unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(Fail::Usage(m)) => {
            eprintln!("wave: {m}\n(run `wave help`)");
            ExitCode::from(1)
        }
        Err(Fail::Conn(m)) => {
            eprintln!("wave: {m}");
            ExitCode::from(2)
        }
        Err(Fail::App(m)) => {
            eprintln!("wave: {m}");
            ExitCode::from(3)
        }
    }
}

fn session(a: &Args) -> Result<ws::Endpoint, Fail> {
    let all = ws::sessions();
    match a.get("session") {
        Some(id) => all.into_iter().find(|e| e.session_id.starts_with(id)).ok_or_else(|| Fail::Conn(format!("no running WAVE session {id}"))),
        None => match all.len() {
            0 => Err(Fail::Conn("WAVE is not running (start the app, then try again)".into())),
            1 => Ok(all.into_iter().next().unwrap()),
            n => Err(Fail::Conn(format!("{n} WAVE apps are open; pick one with --session ID (see `wave sessions`)"))),
        },
    }
}

fn send(a: &Args, command: &str, args: Value) -> Result<Value, Fail> {
    let ep = session(a)?;
    let rev = a.num("if-revision").map_err(Fail::Usage)?.map(|v| v as u64);
    let rep = ws::call(&ep, command, args, rev, a.get("label").map(str::to_owned)).map_err(|e| Fail::Conn(format!("could not reach WAVE: {e}")))?;
    if rep.ok {
        let mut d = rep.data;
        if let Value::Object(m) = &mut d {
            m.entry("revision").or_insert(json!(rep.revision));
        }
        Ok(d)
    } else {
        let e = rep.error.unwrap_or_else(|| ws::ErrorBody::new("ERROR", "unknown error"));
        Err(Fail::App(format!("{}: {}", e.code, e.message)))
    }
}

/// `12` → 12, `kick` → "kick".
fn id_or_name(s: &str) -> Value {
    s.parse::<u64>().map(|n| json!(n)).unwrap_or_else(|_| json!(s))
}

fn run(cmd: &str, a: &Args) -> Result<Value, Fail> {
    let u = Fail::Usage;
    match cmd {
        "sessions" => Ok(json!(ws::sessions().into_iter().map(|e| json!({"sessionId": e.session_id, "pid": e.pid, "project": e.project})).collect::<Vec<_>>())),
        "status" | "catalog" | "schema" | "undo" | "redo" | "stop" => send(a, cmd, json!({})),
        "context" => send(a, "context", json!({"full": a.has("full")})),
        "analyze" => send(a, "music_analyze", json!({"clipId": a.num("clip").map_err(u)?.map(|v| v as u64), "excludeFx": a.num("exclude-fx").map_err(u)?.map(|v| v as u64)})),
        "apply" => {
            let text = match (a.get("file"), a.get("json")) {
                (Some(f), _) => std::fs::read_to_string(f).map_err(|e| u(format!("{f}: {e}")))?,
                (None, Some(j)) => j.to_string(),
                _ => return Err(u("apply needs --file ops.json or --json '[...]'".into())),
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| u(format!("ops are not valid JSON: {e}")))?;
            let ops = match v {
                Value::Array(_) => v,
                Value::Object(ref m) if m.contains_key("ops") => m["ops"].clone(),
                Value::Object(_) => json!([v]),
                _ => return Err(u("ops must be an array of op objects".into())),
            };
            send(a, "apply", json!({"ops": ops}))
        }
        "bpm" => {
            let v: f64 = a.pos.get(1).and_then(|s| s.parse().ok()).ok_or_else(|| u("bpm <value>".into()))?;
            send(a, "apply", json!({"ops": [{"op": "setBpm", "bpm": v}]}))
        }
        "add-fx" => send(a, "add_fx", json!({"insertId": a.num("insert").map_err(u)?.ok_or_else(|| u("missing --insert".into()))? as u64, "type": a.need("type").map_err(u)?, "preset": a.num("preset").map_err(u)?})),
        "set-param" => send(a, "set_param", json!({"fxId": a.num("fx").map_err(u)?.ok_or_else(|| u("missing --fx".into()))? as u64, "param": a.need("param").map_err(u)?, "value": a.num("value").map_err(u)?.ok_or_else(|| u("missing --value".into()))?})),
        "preset" => send(a, "load_preset", json!({"fxId": a.num("fx").map_err(u)?.ok_or_else(|| u("missing --fx".into()))? as u64, "index": a.num("index").map_err(u)?.ok_or_else(|| u("missing --index".into()))? as u64})),
        "import" => {
            let files: Vec<String> = a.pos[1..].iter().map(|p| std::fs::canonicalize(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.clone())).collect();
            if files.is_empty() {
                return Err(u("import <file>...".into()));
            }
            send(a, "import", json!({"paths": files, "track": a.num("track").map_err(u)?, "beat": a.num("beat").map_err(u)?}))
        }
        "channels" => {
            let mut args = json!({});
            if let Some(k) = a.get("add") {
                args["add"] = json!(k);
            }
            if let Some(s) = a.num("sample").map_err(u)? {
                args["sample"] = json!(s as u64);
            }
            if let Some(r) = a.get("remove") {
                args["remove"] = id_or_name(r);
            }
            if a.has("keys") {
                args["keys"] = json!(true);
            }
            if a.has("synth") {
                args["synth"] = json!(a.get("synth").unwrap_or(""));
            }
            if let Some(c) = a.get("ch") {
                args["ch"] = id_or_name(c);
            }
            for k in ["preset", "param"] {
                if let Some(v) = a.get(k) {
                    args[k] = json!(v);
                }
            }
            if let Some(v) = a.num("value").map_err(u)? {
                args["value"] = json!(v);
            }
            send(a, "channels", args)
        }
        "pattern" => {
            if a.has("play") || a.has("stop") {
                let id = if a.has("stop") { None } else { Some(a.num("id").map_err(u)?.ok_or_else(|| u("--play needs --id".into()))? as u64) };
                return send(a, "pattern_play", json!({ "id": id }));
            }
            let mut args = json!({ "new": a.has("new"), "clear": a.has("clear") });
            if let Some(id) = a.num("id").map_err(u)? {
                args["id"] = json!(id as u64);
            }
            for k in ["name", "color", "steps"] {
                if let Some(v) = a.get(k) {
                    args[k] = json!(v);
                }
            }
            for k in ["bars", "swing", "vel"] {
                if let Some(v) = a.num(k).map_err(u)? {
                    args[k] = json!(v);
                }
            }
            if let Some(c) = a.get("ch") {
                args["ch"] = id_or_name(c);
            }
            if let Some(n) = a.get("notes") {
                args["notes"] = serde_json::from_str(n).map_err(|e| u(format!("--notes is not JSON: {e}")))?;
            }
            send(a, "pattern", args)
        }
        "takes" => send(a, "takes", json!({"track": a.num("track").map_err(u)?.ok_or_else(|| u("missing --track".into()))? as u64})),
        "comp" => {
            let mut args = json!({"track": a.num("track").map_err(u)?.map(|v| v as u64), "clip": a.num("clip").map_err(u)?.map(|v| v as u64)});
            if args["track"].is_null() && args["clip"].is_null() {
                return Err(u("comp needs --track N (or --clip ID)".into()));
            }
            if a.has("flatten") {
                return send(a, "flatten_comp", args);
            }
            if a.has("audition") {
                let v = a.need("audition").map_err(u)?;
                args["take"] = if v == "off" { Value::Null } else { json!(v.parse::<u64>().map_err(|_| u(format!("--audition takes a take id or `off`, got {v}")))?) };
                return send(a, "comp_audition", args);
            }
            if let Some(s) = a.get("set") {
                args["regions"] = serde_json::from_str(s).map_err(|e| u(format!("--set is not JSON: {e}")))?;
            }
            if let Some(t) = a.num("take").map_err(u)? {
                args["take"] = json!(t as u64);
            }
            if let Some(x) = a.num("xfade").map_err(u)? {
                args["xfadeMs"] = json!(x);
            }
            if a.has("remove-unused") {
                args["removeUnused"] = json!(true);
            }
            send(a, "comp", args)
        }
        "play" => send(a, "play", json!({"from": a.num("from").map_err(u)?})),
        "render" => {
            let out = a.need("out").map_err(u)?;
            let out = std::path::absolute(out).map(|p| p.display().to_string()).unwrap_or_else(|_| out.to_string());
            send(a, "render", json!({"out": out, "from": a.num("from").map_err(u)?, "to": a.num("to").map_err(u)?, "tail": a.num("tail").map_err(u)?, "rate": a.num("rate").map_err(u)?, "format": a.get("format"), "mp3Kbps": a.num("kbps").map_err(u)?}))
        }
        "save" => send(a, "save", json!({"path": a.get("path").map(|p| std::path::absolute(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.to_string()))})),
        "open" => {
            let p = a.pos.get(1).ok_or_else(|| u("open <file>".into()))?;
            send(a, "open", json!({"path": std::path::absolute(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.clone())}))
        }
        "vst3" => send(a, "vst3_scan", json!({"rescan": a.has("rescan")})),
        "vst3-add" => send(a, "vst3_add", json!({"insertId": a.num("insert").map_err(u)?.ok_or_else(|| u("missing --insert".into()))? as u64, "path": a.need("path").map_err(u)?, "cid": a.need("cid").map_err(u)?})),
        "vst3-params" => send(a, "vst3_params", json!({"fxId": a.num("fx").map_err(u)?.ok_or_else(|| u("missing --fx".into()))? as u64, "query": a.get("query")})),
        "vst3-set" => send(a, "vst3_set_param", json!({"fxId": a.num("fx").map_err(u)?.ok_or_else(|| u("missing --fx".into()))? as u64, "id": a.num("id").map_err(u)?.ok_or_else(|| u("missing --id".into()))? as u64, "value": a.num("value").map_err(u)?.ok_or_else(|| u("missing --value".into()))?})),
        "auto-add" => {
            // an automation envelope on a hosted VST3 parameter, plus the playlist clip that plays it
            let fx = a.num("fx").map_err(u)?.ok_or_else(|| u("missing --fx".into()))? as u64;
            let param = a.num("param").map_err(u)?.ok_or_else(|| u("missing --param".into()))? as u64;
            let mut points: Vec<Value> = vec![];
            if let Some(p) = a.get("points") {
                for pair in p.split(',') {
                    let (t, v) = pair.split_once(':').ok_or_else(|| u("--points is t:v pairs, e.g. 0:0.2,4:0.8".into()))?;
                    let t: f64 = t.trim().parse().map_err(|_| u(format!("bad time `{t}`")))?;
                    let v: f64 = v.trim().parse().map_err(|_| u(format!("bad value `{v}`")))?;
                    if !(0.0..=1.0).contains(&v) {
                        return Err(u(format!("value {v} outside 0..1")));
                    }
                    points.push(json!({"t": t, "v": v}));
                }
            } else {
                let v = a.num("value").map_err(u)?.ok_or_else(|| u("needs --points \"t:v,...\" or --value 0..1".into()))?;
                if !(0.0..=1.0).contains(&v) {
                    return Err(u(format!("value {v} outside 0..1")));
                }
                points.push(json!({"t": 0.0, "v": v}));
            }
            let ctx = send(a, "context", json!({}))?;
            let next = ctx.get("nextId").and_then(Value::as_u64).ok_or_else(|| u("no nextId in context".into()))?;
            let (auto_id, clip_id) = (next, next + 1);
            let track = a.num("track").map_err(u)?.unwrap_or(0.0) as u64;
            let name = a.get("name").unwrap_or("Automation").to_string();
            let len = points.iter().filter_map(|p| p["t"].as_f64()).fold(1.0, f64::max) + 1.0;
            send(a, "apply", json!({"ops": [
                {"op": "putAutomation", "automation": {"id": auto_id, "name": name, "color": "#e0a44a", "target": {"kind": "plugin", "fxId": fx, "paramId": param}, "points": points}},
                {"op": "putClips", "clips": [{"id": clip_id, "track": track, "start": 0.0, "len": len, "offset": 0.0, "kind": "auto", "ref": auto_id, "muted": false, "gain": 0.0, "fadeIn": 0.0, "fadeOut": 0.0, "pan": 0.0, "norm": false, "rev": false}]}
            ]}))
        }
        "call" => {
            let c = a.pos.get(1).ok_or_else(|| u("call <command> [--args JSON]".into()))?;
            let args = match a.get("args") {
                Some(t) => serde_json::from_str(t).map_err(|e| u(format!("--args is not JSON: {e}")))?,
                None => json!({}),
            };
            send(a, c, args)
        }
        "pitch-target" => send(a, "pitch_target", json!({"clipId": a.num("clip").map_err(u)?.ok_or_else(|| u("missing --clip".into()))? as u64, "full": a.has("full")})),
        "tune-ref" => {
            let mut args = json!({"clipId": a.num("clip").map_err(u)?.ok_or_else(|| u("missing --clip".into()))? as u64, "off": a.has("off")});
            for k in ["reference", "correction", "vibrato", "influence", "key", "scale"] {
                if let Some(v) = a.num(k).map_err(u)? {
                    args[k] = json!(v);
                }
            }
            match a.get("octave") {
                Some("auto") => args["octave"] = json!("auto"),
                Some(o) => args["octave"] = json!(o.parse::<i64>().map_err(|_| u("--octave is auto or semitones (-12, 0, 12…)".into()))?),
                None => {}
            }
            send(a, "tune_ref", args)
        }
        "f0-model" => send(a, "f0_model", json!({})),
        "tune-file" => tune_file(a),
        other => Err(u(format!("unknown command `{other}`"))),
    }
}

/// Tune to Reference on two files, offline: no app, no project.
fn tune_file(a: &Args) -> Result<Value, Fail> {
    use wave_engine::sources::{write_wav, Audio};
    let u = Fail::Usage;
    let (take_p, ref_p, out_p) = (a.need("take").map_err(u)?, a.need("reference").map_err(u)?, a.need("out").map_err(u)?);
    let take = Audio::load(std::path::Path::new(take_p)).map_err(|e| u(format!("{take_p}: {e}")))?;
    let reference = Audio::load(std::path::Path::new(ref_p)).map_err(|e| u(format!("{ref_p}: {e}")))?;
    let pct = |k: &str| -> Result<f32, Fail> {
        match a.num(k).map_err(u)? {
            Some(v) if (0.0..=100.0).contains(&v) => Ok(v as f32 / 100.0),
            Some(_) => Err(u(format!("--{k} is a percentage 0..100"))),
            None => Ok(1.0),
        }
    };
    let octave = match a.get("octave") {
        None | Some("auto") => None,
        Some(o) => Some(o.parse::<i32>().ok().filter(|o| o % 12 == 0 && o.abs() <= 36).ok_or_else(|| u("--octave is auto or a multiple of 12 within ±36".into()))?),
    };
    let params = wave_engine::RefTune {
        source: 0,
        correction: pct("correction")?,
        vibrato: pct("vibrato")?,
        influence: pct("influence")?,
        octave,
        key: a.num("key").map_err(u)?.map(|k| k as u32).unwrap_or(0).min(11),
        scale: a.num("scale").map_err(u)?.map(|k| k as u32).unwrap_or(0).min(4),
    };
    let model = if a.has("yin") {
        None
    } else {
        let explicit = a.get("model");
        let path = explicit.map(std::path::PathBuf::from).unwrap_or_else(wave_rmvpe::model_path);
        match wave_rmvpe::Rmvpe::load(&path) {
            Ok(m) => Some(m),
            Err(e) if explicit.is_some() => return Err(Fail::App(e)),
            Err(_) => None,
        }
    };
    let est = model.as_ref().map(|m| m as &dyn wave_engine::pitch::F0Estimator);
    let (out, target, analysis) = wave_engine::reference::tune_audio(&take, &reference, &params, est);
    write_wav(std::path::Path::new(out_p), out.content(), out.sr).map_err(|e| Fail::App(format!("{out_p}: {e}")))?;
    if let Some(j) = a.get("json") {
        let text = serde_json::to_string(&target).map_err(|e| Fail::App(e.to_string()))?;
        std::fs::write(j, text).map_err(|e| Fail::App(format!("{j}: {e}")))?;
    }
    Ok(json!({ "out": out_p, "backend": analysis.backend, "corrected": target.reason.is_none(), "reason": target.reason, "score": target.score, "octave": target.octave, "notes": target.notes.len(), "seconds": out.seconds() }))
}
