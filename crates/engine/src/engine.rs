//! The non-real-time side of the engine: owns the project, undo history and the
//! loaded audio, turns edits into ready-built structures for the audio thread,
//! finalises recordings, and publishes telemetry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::analysis::Analyzer;
use crate::catalog::param_block;
use crate::midi::{self, MidiMsg, MidiRecorder};
use crate::model::*;
use crate::ops::{self, Dirty, Op};
use crate::processor::{ParamBlock, Processor};
use crate::rt::*;
use crate::sources::{write_wav, Audio, SourceStore};

/// Creates processors the engine cannot build itself (hosted plugins).
pub trait PluginFactory: Send + Sync {
    /// `live` is false for offline renders: the factory must not replace the live instance's handle.
    fn create(&self, fx_id: Id, plugin: &PluginRef, sample_rate: f32, max_block: usize, live: bool) -> Result<Box<dyn Processor>, String>;
    /// The effect left the project (or the engine is rebuilding everything).
    fn release(&self, _fx_id: Id) {}
    /// Current opaque state of a live instance (base64), for saving.
    fn state(&self, _fx_id: Id) -> Option<String> {
        None
    }
    /// Whether hosted plugin `fx_id` currently lists parameter `param_id`.
    /// Automation targets (`AutoTarget::Plugin`) are validated against this at
    /// `apply` time, so a stale or unknown id is an error, not a dead envelope.
    fn has_plugin_param(&self, _fx_id: Id, _param_id: u32) -> bool {
        false
    }
}

pub struct NoPlugins;
impl PluginFactory for NoPlugins {
    fn create(&self, _: Id, p: &PluginRef, _: f32, _: usize, _: bool) -> Result<Box<dyn Processor>, String> {
        Err(format!("{} cannot be loaded: this build has no plugin host", p.name))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransportDto {
    pub playing: bool,
    pub rec: bool,
    pub monitor: bool,
    pub metro: bool,
    pub count_in: bool,
    pub start_beat: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateDto {
    pub project: Project,
    pub revision: u64,
    pub can_undo: bool,
    pub can_redo: bool,
    pub transport: TransportDto,
    pub path: Option<String>,
    pub dirty: bool,
    /// Effects that failed to load (plugin missing etc.): fx id → message.
    pub fx_errors: HashMap<Id, String>,
    /// Comp id → take being auditioned (played instead of the comp). Live only.
    pub audition: HashMap<Id, Id>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Op(#[from] ops::OpError),
    #[error("{0}")]
    Msg(String),
    /// Offline render was aborted by the caller before a file was written.
    #[error("cancelled")]
    Cancelled,
}

impl From<String> for EngineError {
    fn from(s: String) -> Self {
        EngineError::Msg(s)
    }
}

type Listener = Box<dyn Fn(&StateDto) + Send + Sync>;
type FrameSink = Box<dyn Fn(Vec<u8>) + Send + Sync>;

/// Input being recorded. With loop recording it holds several passes; `marks` are the
/// frames where each later pass begins (the audio thread sends a NaN frame at the wrap).
struct RecBuf {
    track: usize,
    /// Beat where the first pass starts (NaN until the audio thread has punched in).
    start: f64,
    /// Frames of input to drop from the front so the take lines up with what the
    /// performer heard: device round trip + plugin delay compensation of the playback.
    /// Set at the start from the device estimate then; replaced in [`RecBuf::resolve`] by the
    /// mean round trip measured while the take was capturing, when there is one.
    skip: usize,
    /// The mixer's playback latency `skip` was taken with: the graph keeps it until the take ends.
    pdc: usize,
    /// Loop range when recording in passes.
    lap: Option<(f64, f64)>,
    sr: u32,
    data: Vec<f32>,
    marks: Vec<usize>,
    nans: u32,
    /// Peaks of the current pass only (the live clip restarts every pass).
    peaks: Vec<f32>,
    peaks_sent: usize,
    acc: (f32, f32, usize),
}

impl RecBuf {
    fn frames(&self) -> usize {
        self.data.len() / 2
    }
    fn pass_start_frame(&self) -> usize {
        self.marks.last().copied().unwrap_or(0)
    }
    fn pass_start_beat(&self) -> f64 {
        match (self.lap, self.marks.is_empty()) {
            (Some((a, _)), false) => a,
            _ => self.start,
        }
    }
    /// Reads what the audio thread learned during the take (before its `Shared` goes away):
    /// a punch-in take starts where capturing began, and the device round trip is the one
    /// measured while it captured (the estimate at the start may not have settled).
    fn resolve(&mut self, shared: &Shared) {
        if self.start.is_nan() {
            self.start = shared.rec_start();
        }
        if let Some(io) = shared.take_latency() {
            self.skip = io as usize + self.pdc;
        }
    }
    fn push(&mut self, x: f32) {
        if x.is_nan() {
            self.nans += 1;
            if self.nans.is_multiple_of(2) {
                self.marks.push(self.frames());
                self.peaks.clear();
                self.peaks_sent = 0;
                self.acc = (0.0, 0.0, 0);
            }
            return;
        }
        self.data.push(x);
        if self.data.len() % 2 == 1 {
            return;
        }
        let f = self.frames() - 1;
        let v = (self.data[f * 2] + self.data[f * 2 + 1]) * 0.5;
        self.acc.0 = self.acc.0.min(v);
        self.acc.1 = self.acc.1.max(v);
        self.acc.2 += 1;
        if self.acc.2.is_multiple_of((self.sr as usize / 100).max(1)) {
            self.peaks.push(self.acc.0);
            self.peaks.push(self.acc.1);
            self.acc.0 = 0.0;
            self.acc.1 = 0.0;
        }
    }
}

struct Wires {
    cmds: rtrb::Producer<Cmd>,
    garbage: rtrb::Consumer<Garbage>,
    record: rtrb::Consumer<f32>,
    analyzer: Analyzer,
    sr: f32,
    max_block: usize,
}

struct Core {
    project: Project,
    undo: Vec<Project>,
    redo: Vec<Project>,
    revision: u64,
    gesture: Option<Project>,
    /// A transient edit inside the open gesture was not reported to listeners yet.
    gesture_unreported: bool,
    sources: SourceStore,
    path: Option<PathBuf>,
    saved_revision: u64,
    transport: TransportDto,
    /// Effect ids the audio thread currently holds a processor for.
    live_fx: HashSet<Id>,
    sent_params: HashMap<Id, (Option<ParamBlock>, f32)>,
    fx_errors: HashMap<Id, String>,
    wires: Option<Wires>,
    take: Option<RecBuf>,
    /// A mixer change that would alter the playback latency arrived during a take: the
    /// audio thread keeps the old graph until the take ends (its `skip` stays right).
    graph_deferred: bool,
    watch: Vec<Id>,
    /// Comp id → take played instead of the comp (live only: not saved, not undone).
    audition: HashMap<Id, Id>,
    midi: MidiState,
    /// Synth channels the audio thread holds an instance for, with the patch it was last sent.
    synths: HashMap<Id, crate::poly::Patch>,
    /// The WAVE Keys piano, once the app has loaded it.
    keys: Option<Arc<crate::keys::KeysBank>>,
}

/// Live MIDI input state kept under the core lock (see [`Engine::midi_configure`]).
#[derive(Default)]
struct MidiState {
    /// Channel the keyboard plays; `None` = the first pitched channel.
    target: Option<Id>,
    /// A device is connected: notes play, and an armed recording captures them.
    live: bool,
}

/// The MIDI take being recorded; its own lock, so keyboard events never wait for the core.
#[derive(Default)]
struct MidiTake {
    /// The take being played in: recorder, armed track index, target channel.
    rec: Option<(MidiRecorder, usize, Id)>,
    /// The running take received notes (then its audio take is dropped).
    played: bool,
}

/// What a keyboard event needs, readable without the core lock: whether a device is live,
/// the channel and insert the keys play (refreshed whenever the project changes) and the
/// audio thread's MIDI ring (separate from the command ring, so it has its own producer).
struct MidiFast {
    live: AtomicBool,
    chan: std::sync::atomic::AtomicU64,
    ins: std::sync::atomic::AtomicUsize,
    tx: Mutex<Option<rtrb::Producer<Cmd>>>,
}

impl MidiFast {
    fn send(&self, cmd: Cmd) {
        if let Some(tx) = self.tx.lock().as_mut() {
            if tx.push(cmd).is_err() {
                log::warn!("audio MIDI queue full; dropping an event");
            }
        }
    }
}

pub struct Engine {
    core: Mutex<Core>,
    shared: Mutex<Arc<Shared>>,
    factory: Arc<dyn PluginFactory>,
    listeners: Mutex<Vec<Listener>>,
    sink: Mutex<Option<FrameSink>>,
    /// Effect chains that are silent past their tails are not processed (on by default).
    skip_silent: std::sync::atomic::AtomicBool,
    midi_fast: MidiFast,
    midi_take: Mutex<MidiTake>,
    /// Threads an offline render may use for the insert chains.
    render_workers: std::sync::atomic::AtomicUsize,
}

const UNDO_LIMIT: usize = 200;

impl Engine {
    pub fn new(mut project: Project, factory: Arc<dyn PluginFactory>) -> Arc<Self> {
        project.migrate();
        let mut sources = SourceStore::default();
        load_library_sources(&project, &mut sources);
        let e = Arc::new(Self {
            core: Mutex::new(Core {
                project,
                undo: vec![],
                redo: vec![],
                revision: 1,
                gesture: None,
                gesture_unreported: false,
                sources,
                path: None,
                saved_revision: 1,
                transport: TransportDto { playing: false, rec: false, monitor: false, metro: false, count_in: true, start_beat: 0.0 },
                live_fx: HashSet::new(),
                sent_params: HashMap::new(),
                fx_errors: HashMap::new(),
                wires: None,
                take: None,
                graph_deferred: false,
                watch: vec![],
                audition: HashMap::new(),
                midi: MidiState::default(),
                synths: HashMap::new(),
                keys: None,
            }),
            shared: Mutex::new(Arc::new(Shared::new())),
            factory,
            listeners: Mutex::new(vec![]),
            sink: Mutex::new(None),
            skip_silent: std::sync::atomic::AtomicBool::new(true),
            midi_fast: MidiFast { live: AtomicBool::new(false), chan: std::sync::atomic::AtomicU64::new(u64::MAX), ins: std::sync::atomic::AtomicUsize::new(usize::MAX), tx: Mutex::new(None) },
            midi_take: Mutex::new(MidiTake::default()),
            render_workers: std::sync::atomic::AtomicUsize::new(std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).clamp(1, 8)).unwrap_or(1)),
        });
        let weak = Arc::downgrade(&e);
        thread::Builder::new()
            .name("wave-housekeeping".into())
            .spawn(move || {
                // fixed 60 Hz cadence (sleeping a fixed time *after* the work made frames uneven)
                let period = Duration::from_micros(16_667);
                let mut next = std::time::Instant::now();
                let mut tick = 0u32;
                while let Some(e) = weak.upgrade() {
                    e.housekeeping(tick);
                    drop(e);
                    tick = tick.wrapping_add(1);
                    next += period;
                    let now = std::time::Instant::now();
                    if next > now {
                        thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })
            .expect("spawn housekeeping thread");
        e
    }

    /// Turns silence skipping of effect chains on/off for Rts created from now on and for
    /// offline renders. The output is the same (below -120 dB); off is for A/B tests.
    pub fn set_silence_skip(&self, on: bool) {
        self.skip_silent.store(on, Relaxed);
    }

    /// Threads an offline render may use for the insert chains (1 = single-threaded). The
    /// default leaves two cores to the rest of the machine, at most 8. The render is identical
    /// whatever the count.
    pub fn set_render_workers(&self, n: usize) {
        self.render_workers.store(n.max(1), Relaxed);
    }

    // ------------------------------------------------------------ device attach

    /// Builds the audio-thread half for a device running at `sample_rate` with
    /// blocks of at most `max_block`. Any previous `Rt` becomes inert: drop it
    /// (off the audio thread) once its stream has stopped.
    pub fn create_rt(&self, sample_rate: f32, max_block: usize) -> Rt {
        let max_block = max_block.clamp(16, 8192);
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(4096);
        let (midi_tx, midi_rx) = rtrb::RingBuffer::<Cmd>::new(1024);
        let (gb_tx, gb_rx) = rtrb::RingBuffer::<Garbage>::new(4096);
        let (rec_tx, rec_rx) = rtrb::RingBuffer::<f32>::new(sample_rate as usize * 2 * 20);
        let mut inputs = vec![];
        let mut outputs = vec![];
        let mut ain = vec![];
        let mut aout = vec![];
        for _ in 0..WATCH_SLOTS {
            let (a, b) = rtrb::RingBuffer::<f32>::new(TAP_CAPACITY);
            inputs.push(a);
            ain.push(b);
            let (a, b) = rtrb::RingBuffer::<f32>::new(TAP_CAPACITY);
            outputs.push(a);
            aout.push(b);
        }
        let shared = Arc::new(Shared::new());
        {
            // a punched-in take learns its start beat from the stream that is going away
            let mut c = self.core.lock();
            let old = self.shared.lock().clone();
            if let Some(t) = c.take.as_mut() {
                t.resolve(&old);
            }
        }
        *self.shared.lock() = shared.clone();
        let mut rt = Rt::new(sample_rate, max_block, cmd_rx, gb_tx, shared, Taps { input: inputs, output: outputs }, rec_tx);
        rt.set_skip_silent(self.skip_silent.load(Relaxed));
        rt.set_midi_ring(midi_rx);
        *self.midi_fast.tx.lock() = Some(midi_tx);
        let mut c = self.core.lock();
        // everything is rebuilt for the new rate: hosted plugins keep their state through the project
        self.snapshot_plugin_states(&mut c);
        for id in c.live_fx.drain().collect::<Vec<_>>() {
            self.factory.release(id);
        }
        c.sent_params.clear();
        c.synths.clear();
        // the old stream is gone: keep what it captured instead of orphaning the take
        self.finalize_take(&mut c);
        c.wires = Some(Wires { cmds: cmd_tx, garbage: gb_rx, record: rec_rx, analyzer: Analyzer::new(ain, aout, sample_rate), sr: sample_rate, max_block });
        self.sync(&mut c, Dirty { timeline: true, mixer: true, params: true });
        let flags = Cmd::Flags { metro: c.transport.metro, monitor: c.transport.monitor };
        let watch = watch_cmd(&c.watch);
        send(&mut c, flags);
        send(&mut c, watch);
        c.transport.playing = false;
        self.changed(c);
        rt
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.lock().clone()
    }

    // ------------------------------------------------------------ state & listeners

    pub fn on_change(&self, f: impl Fn(&StateDto) + Send + Sync + 'static) {
        self.listeners.lock().push(Box::new(f));
    }

    pub fn set_frame_sink(&self, f: Option<FrameSink>) {
        *self.sink.lock() = f;
    }

    fn dto(&self, c: &Core) -> StateDto {
        StateDto {
            project: c.project.clone(),
            revision: c.revision,
            can_undo: !c.undo.is_empty(),
            can_redo: !c.redo.is_empty(),
            transport: c.transport.clone(),
            path: c.path.as_ref().map(|p| p.display().to_string()),
            dirty: c.revision != c.saved_revision,
            fx_errors: c.fx_errors.clone(),
            audition: live_audition(c),
        }
    }

    pub fn state(&self) -> StateDto {
        let c = self.core.lock();
        self.dto(&c)
    }

    pub fn project(&self) -> Project {
        self.core.lock().project.clone()
    }

    pub fn revision(&self) -> u64 {
        self.core.lock().revision
    }

    fn notify(&self, dto: StateDto) {
        for l in self.listeners.lock().iter() {
            l(&dto);
        }
    }

    fn changed(&self, c: parking_lot::MutexGuard<'_, Core>) {
        let dto = self.dto(&c);
        drop(c);
        self.notify(dto);
    }

    // ------------------------------------------------------------ edits

    /// Applies a batch. `transient` edits (inside a gesture) do not create undo steps.
    /// With `if_revision`, fails if the project changed since that revision.
    pub fn apply(&self, ops: &[Op], transient: bool, if_revision: Option<u64>) -> Result<u64, EngineError> {
        let mut c = self.core.lock();
        if let Some(r) = if_revision {
            if r != c.revision {
                return Err(EngineError::Msg(format!("REVISION_CONFLICT: project is at revision {}, not {r}", c.revision)));
            }
        }
        let before = c.project.clone();
        // plugin automation targets are validated here, not in `ops::apply_all`:
        // whether a parameter id exists is knowledge the plugin factory holds
        for op in ops {
            if let Op::PutAutomation { automation } = op {
                if let AutoTarget::Plugin { fx_id, param_id } = &automation.target {
                    let in_project = c.project.find_fx(*fx_id).is_some_and(|(_, f)| f.kind == FxKind::Vst3);
                    if !in_project || !self.factory.has_plugin_param(*fx_id, *param_id) {
                        return Err(EngineError::Msg(format!(
                            "effect {fx_id} has no VST3 parameter {param_id} to automate"
                        )));
                    }
                }
            }
        }
        let d = ops::apply_all(&mut c.project, ops)?;
        if c.project == before {
            return Ok(c.revision);
        }
        let in_gesture = transient && c.gesture.is_some();
        if !in_gesture {
            push_undo(&mut c, before);
        }
        c.revision += 1;
        self.sync(&mut c, d);
        let rev = c.revision;
        // inside a gesture (a drag sends one batch per frame) listeners are not sent the whole
        // project each time: the UI holds those states back anyway, and `end_gesture` reports
        // the result. On a big song that was ~1 MB serialised, sent and parsed per frame.
        if in_gesture {
            c.gesture_unreported = true;
        } else {
            self.changed(c);
        }
        Ok(rev)
    }

    pub fn begin_gesture(&self) {
        let mut c = self.core.lock();
        if c.gesture.is_none() {
            c.gesture = Some(c.project.clone());
        }
    }

    pub fn end_gesture(&self) {
        let mut c = self.core.lock();
        let unreported = std::mem::take(&mut c.gesture_unreported);
        if let Some(g) = c.gesture.take() {
            if g != c.project {
                push_undo(&mut c, g);
                // renders skipped during the drag (pitch edits) happen now
                self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
                self.changed(c);
            } else if unreported {
                // dragged and back: nothing to undo, but listeners have not seen the revisions
                self.changed(c);
            }
        }
    }

    pub fn undo(&self) -> bool {
        let mut c = self.core.lock();
        c.gesture = None;
        c.gesture_unreported = false;
        let Some(p) = c.undo.pop() else { return false };
        let cur = std::mem::replace(&mut c.project, p);
        c.redo.push(cur);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, mixer: true, params: true });
        self.changed(c);
        true
    }

    pub fn redo(&self) -> bool {
        let mut c = self.core.lock();
        c.gesture = None;
        c.gesture_unreported = false;
        let Some(p) = c.redo.pop() else { return false };
        let cur = std::mem::replace(&mut c.project, p);
        c.undo.push(cur);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, mixer: true, params: true });
        self.changed(c);
        true
    }

    // ------------------------------------------------------------ transport

    pub fn play(&self, from: f64) {
        let mut c = self.core.lock();
        if c.take.is_some() || self.midi_take.lock().rec.is_some() {
            return;
        }
        let from = from.max(0.0);
        let p = &c.project;
        let rec_track = if c.transport.rec { p.tracks.iter().position(|t| t.arm) } else { None };
        let (ls, le) = p.loop_range.map(|l| (l.a, l.b)).unwrap_or((0.0, p.song_end()));
        // with a loop set, recording cycles over it: one take per pass
        let lap = p.loop_range.filter(|l| rec_track.is_some() && l.b > l.a).map(|l| (l.a, l.b));
        let from = match lap {
            Some((a, b)) if from < a || from >= b => a,
            Some(_) => from,
            None if rec_track.is_none() && (from >= le || from < ls && p.loop_range.is_some()) => ls,
            None => from,
        };
        let count_in = if rec_track.is_some() && c.transport.count_in { 4.0 } else { 0.0 };
        if let Some(track) = rec_track {
            c.take = self.new_take(&c, track, from, lap);
        }
        if let Some(track) = rec_track {
            self.begin_midi_take(&mut c, track, from);
        }
        c.transport.start_beat = from;
        c.transport.playing = true;
        send(&mut c, Cmd::Play { from, count_in, rec: rec_track.is_some(), rec_loop: lap.is_some() });
        self.changed(c);
    }

    pub fn stop(&self) {
        let mut c = self.core.lock();
        send(&mut c, Cmd::Stop);
        c.transport.playing = false;
        drop(c);
        // give the callback a moment to flush the last input
        thread::sleep(Duration::from_millis(30));
        self.finish_take();
        self.finish_midi_take();
        let c = self.core.lock();
        self.changed(c);
    }

    pub fn set_start(&self, beat: f64) {
        let mut c = self.core.lock();
        c.transport.start_beat = beat.max(0.0);
        self.changed(c);
    }

    pub fn set_transport(&self, metro: Option<bool>, count_in: Option<bool>, rec: Option<bool>, monitor: Option<bool>) -> TransportDto {
        let mut c = self.core.lock();
        if let Some(v) = metro {
            c.transport.metro = v;
        }
        if let Some(v) = count_in {
            c.transport.count_in = v;
        }
        if let Some(v) = rec {
            c.transport.rec = v;
        }
        if let Some(v) = monitor {
            c.transport.monitor = v;
        }
        let punch_out = !c.transport.rec && (c.take.is_some() || self.midi_take.lock().rec.is_some());
        if punch_out {
            send(&mut c, Cmd::PunchOut);
        }
        // arming while the song plays punches in right away (on the first armed track)
        if c.transport.rec && c.transport.playing && c.take.is_none() {
            if let Some(track) = c.project.tracks.iter().position(|t| t.arm) {
                let lap = c.project.loop_range.filter(|l| l.b > l.a).map(|l| (l.a, l.b));
                if let Some(t) = self.new_take(&c, track, f64::NAN, lap) {
                    self.shared().set_rec_start(f64::NAN);
                    c.take = Some(t);
                    send(&mut c, Cmd::PunchIn { rec_loop: lap.is_some() });
                }
            }
        }
        if c.transport.rec && c.transport.playing && self.midi_take.lock().rec.is_none() {
            if let Some(track) = c.project.tracks.iter().position(|t| t.arm) {
                let at = self.shared().beat();
                self.begin_midi_take(&mut c, track, at);
            }
        }
        let f = Cmd::Flags { metro: c.transport.metro, monitor: c.transport.monitor };
        send(&mut c, f);
        let t = c.transport.clone();
        if punch_out {
            drop(c);
            thread::sleep(Duration::from_millis(30));
            self.finish_take();
            self.finish_midi_take();
            self.changed(self.core.lock());
        } else {
            self.changed(c);
        }
        t
    }

    /// An empty take buffer for the live device (`None` without one). Its start is `start`
    /// (NaN = learned from the audio thread at punch-in).
    fn new_take(&self, c: &Core, track: usize, start: f64, lap: Option<(f64, f64)>) -> Option<RecBuf> {
        let w = c.wires.as_ref()?;
        let sr = w.sr as u32;
        let pdc = playback_latency(&c.project, w.sr);
        let shared = self.shared();
        let skip = shared.io_latency() as usize + pdc;
        shared.reset_take_latency();
        Some(RecBuf { track, start, skip, pdc, lap, sr, data: Vec::with_capacity(sr as usize * 2 * 60), marks: vec![], nans: 0, peaks: vec![], peaks_sent: 0, acc: (0.0, 0.0, 0) })
    }

    pub fn watch_fx(&self, ids: &[Id]) {
        let mut c = self.core.lock();
        c.watch = ids.iter().copied().take(WATCH_SLOTS).collect();
        let w = watch_cmd(&c.watch);
        send(&mut c, w);
    }

    // ------------------------------------------------------------ audio sources

    /// Decodes files (slow; call off the UI thread) and adds them as sources.
    /// With `place`, also puts one clip per file on consecutive tracks starting there.
    pub fn import_audio(&self, paths: &[PathBuf], place: Option<(u32, f64)>) -> Result<(Vec<Id>, Vec<Id>), EngineError> {
        let mut loaded = vec![];
        let mut errors = vec![];
        // A file of the sample library is one source however often it is used (a sampler
        // channel and audio clips share it), stored by its portable `lib:` path.
        let known: HashMap<String, Id> = {
            let c = self.core.lock();
            c.project.sources.iter().filter(|s| s.path.starts_with(crate::factory::SCHEME) && c.sources.get(s.id).is_some()).map(|s| (s.path.clone(), s.id)).collect()
        };
        for p in paths {
            let stored = match p.to_str() {
                Some(s) if s.starts_with(crate::factory::SCHEME) => s.to_string(),
                _ => crate::factory::portable(p),
            };
            if known.contains_key(&stored) {
                loaded.push((p.clone(), stored, None));
                continue;
            }
            match Audio::load_path(&stored, Path::new(".")) {
                Ok(a) => loaded.push((p.clone(), stored, Some(a))),
                Err(e) => errors.push(e),
            }
        }
        if loaded.is_empty() {
            return Err(EngineError::Msg(errors.join("; ")));
        }
        let mut c = self.core.lock();
        let before = c.project.clone();
        let mut src_ids = vec![];
        let mut clip_ids = vec![];
        let bpm = c.project.bpm;
        for (k, (path, stored, audio)) in loaded.into_iter().enumerate() {
            let reuse = c.project.sources.iter().find(|s| s.path == stored && s.path.starts_with(crate::factory::SCHEME)).map(|s| (s.id, s.dur));
            let (id, seconds) = match (reuse, audio) {
                (Some(r), _) if c.sources.get(r.0).is_some() => r,
                (_, None) => continue,
                (_, Some(audio)) => {
                    let id = c.project.alloc_id();
                    let factory = crate::factory::embedded(&stored);
                    let name = match factory {
                        Some(f) => f.name.to_string(),
                        None => path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Audio".into()),
                    };
                    let colors = ["#69c3d6", "#47b8a8", "#5fbf8a", "#7c8cf8", "#e0a44a", "#9a8cf0", "#d9678f", "#5aa9e6"];
                    let color = factory.map(crate::factory::color).unwrap_or(colors[c.project.sources.len() % colors.len()]).to_string();
                    let seconds = audio.seconds();
                    let src = Source { id, name, color, path: stored, dur: seconds, sr: audio.sr, ch: audio.channels, peak_db: crate::dsp::gain_to_db(audio.peak()) };
                    c.sources.insert(id, Arc::new(audio));
                    c.project.sources.push(src);
                    (id, seconds)
                }
            };
            let len_beats = seconds / (60.0 / bpm);
            src_ids.push(id);
            if let Some((track, beat)) = place {
                let ntracks = c.project.tracks.len() as u32;
                if ntracks == 0 {
                    continue;
                }
                let track = (track + k as u32).min(ntracks - 1);
                let cid = c.project.alloc_id();
                let start = beat.max(0.0);
                c.project.clips.push(Clip {
                    id: cid,
                    track,
                    start,
                    len: len_beats.min(TOTAL_BEATS - start).max(1.0 / 16.0),
                    offset: 0.0,
                    kind: ClipKind::Audio,
                    source: id,
                    muted: false,
                    gain: 0.0,
                    fade_in: 0.0,
                    fade_out: 0.0,
                    pan: 0.0,
                    norm: false,
                    rev: false,
                    tune: None,
                });
                clip_ids.push(cid);
            }
        }
        push_undo(&mut c, before);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
        self.changed(c);
        Ok((src_ids, clip_ids))
    }

    /// Pitch curve and notes of an audio clip's source (for the note editor).
    pub fn pitch_analysis(&self, clip_id: Id) -> Result<Arc<crate::pitch::PitchAnalysis>, EngineError> {
        let clip = self.audio_clip(clip_id)?;
        self.source_analysis(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))
    }

    fn audio_clip(&self, clip_id: Id) -> Result<Clip, EngineError> {
        let c = self.core.lock();
        let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
        if clip.kind != ClipKind::Audio {
            return Err(EngineError::Msg("only audio clips have pitch".into()));
        }
        Ok(clip)
    }

    /// Analysis of a source variant; slow detectors run without holding the project lock.
    fn source_analysis(&self, id: Id, norm: bool, rev: bool) -> Option<Arc<crate::pitch::PitchAnalysis>> {
        let (audio, est) = {
            let mut c = self.core.lock();
            if let Some(a) = c.sources.cached_analysis(id, norm, rev) {
                return Some(a);
            }
            (c.sources.variant(id, norm, rev)?, c.sources.estimator())
        };
        let a = Arc::new(crate::pitch::analyze_with(&crate::sources::mono_of(&audio), audio.sr as f32, est.as_deref()));
        let mut c = self.core.lock();
        c.sources.put_analysis(id, norm, rev, a.clone());
        Some(c.sources.cached_analysis(id, norm, rev).unwrap_or(a))
    }

    fn source_content(&self, id: Id, norm: bool, rev: bool) -> Option<Arc<crate::content::ContentFeatures>> {
        let audio = {
            let mut c = self.core.lock();
            if let Some(x) = c.sources.cached_content(id, norm, rev) {
                return Some(x);
            }
            c.sources.variant(id, norm, rev)?
        };
        let x = Arc::new(crate::content::mfcc(&crate::sources::mono_of(&audio), audio.sr as f32));
        self.core.lock().sources.put_content(id, norm, rev, x.clone());
        Some(x)
    }

    /// Tune to Reference target of an audio clip with `params` (the clip's own when `None`).
    /// The slow parts (analysis, features, alignment) run without the project lock and are
    /// cached, so committing the same reference afterwards only renders.
    pub fn pitch_target(&self, clip_id: Id, params: Option<RefTune>) -> Result<Arc<crate::reference::PitchTarget>, EngineError> {
        let clip = self.audio_clip(clip_id)?;
        let p = params.or_else(|| clip.tune.as_ref().and_then(|t| t.reference)).ok_or(EngineError::Msg("the clip has no reference".into()))?;
        let key = {
            let mut c = self.core.lock();
            if !c.project.sources.iter().any(|s| s.id == p.source) {
                return Err(EngineError::Msg("the reference source is not in the project".into()));
            }
            let bpm = c.project.bpm;
            c.sources.align_key(&clip, bpm, p.source).ok_or(EngineError::Msg("audio not loaded".into()))?
        };
        let take = self.source_analysis(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))?;
        let cached = self.core.lock().sources.cached_alignment(&key);
        let al = match cached {
            Some(a) => a,
            None => {
                let reference = self.source_analysis(p.source, false, false).ok_or(EngineError::Msg("reference audio not loaded".into()))?;
                let tc = self.source_content(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))?;
                let rc = self.source_content(p.source, false, false).ok_or(EngineError::Msg("reference audio not loaded".into()))?;
                let r = crate::pitch::ANALYSIS_RATE as f64;
                let al = Arc::new(crate::reference::align(
                    &crate::reference::Performance { pitch: &take, content: &tc },
                    (key.2 as f64 / r, key.3 as f64 / r),
                    &crate::reference::Performance { pitch: &reference, content: &rc },
                ));
                self.core.lock().sources.put_alignment(key, al.clone());
                al
            }
        };
        Ok(Arc::new(crate::sources::target_for(&clip, &take, &al, &p)))
    }

    /// Uses `est` for pitch analysis from now on (`None` = built-in YIN). Pitch-edited
    /// clips are re-rendered with the new analysis.
    pub fn set_f0_estimator(&self, est: Option<Arc<dyn crate::pitch::F0Estimator>>) {
        let mut c = self.core.lock();
        c.sources.set_estimator(est);
        self.sync(&mut c, Dirty { timeline: true, mixer: false, params: false });
    }

    /// Name of the pitch detector in use ("yin" or the estimator's).
    pub fn f0_backend(&self) -> &'static str {
        self.core.lock().sources.backend()
    }

    fn source_music(&self, id: Id, norm: bool, rev: bool) -> Option<Arc<crate::music::SourceMusic>> {
        if let Some(m) = self.core.lock().sources.cached_music(id, norm, rev) {
            return Some(m);
        }
        let audio = self.core.lock().sources.variant(id, norm, rev)?;
        let m = crate::music::analyze_audio(&audio);
        self.core.lock().sources.put_music(id, norm, rev, m.clone());
        Some(m)
    }

    /// Tempo and key of an audio clip, or key of the project's audio clips
    /// (excluding muted tracks/clips and tracks routed to `exclude_fx`'s insert,
    /// so key detection for Pitch Correct listens to the backing rather than the vocal).
    pub fn music_analysis(&self, clip_id: Option<Id>, exclude_fx: Option<Id>) -> Result<crate::music::MusicAnalysis, EngineError> {
        self.music_analysis_ex(clip_id, exclude_fx, None)
    }

    /// [`Engine::music_analysis`], also excluding the tracks routed to insert `exclude_insert`.
    fn music_analysis_ex(&self, clip_id: Option<Id>, exclude_fx: Option<Id>, exclude_insert: Option<Id>) -> Result<crate::music::MusicAnalysis, EngineError> {
        if let Some(cid) = clip_id {
            let (clip, bpm) = {
                let c = self.core.lock();
                let clip = c.project.clips.iter().find(|x| x.id == cid).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
                (clip, c.project.bpm)
            };
            if clip.kind != ClipKind::Audio {
                return Err(EngineError::Msg("only audio clips have tempo and key".into()));
            }
            let m = self.source_music(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))?;
            let spb = 60.0 / bpm;
            let (a, b) = (clip.offset * spb, (clip.offset + clip.len) * spb);
            let (mut cs, n) = m.chroma_sum(a, b);
            if n == 0 {
                (cs, _) = m.chroma_sum(0.0, 1e9);
            }
            let key = crate::music::estimate_key(&cs, m.tuning_cents);
            return Ok(crate::music::MusicAnalysis { tempo: m.tempo.clone(), key });
        }

        let (all_clips, excluded_insert, bpm) = {
            let c = self.core.lock();
            let p = &c.project;
            let ex_ins = exclude_insert.or_else(|| exclude_fx.and_then(|fid| p.find_fx(fid).map(|(ins, _)| ins.id)));
            let any_solo = p.tracks.iter().any(|t| t.solo);
            let active: Vec<(Clip, bool)> = p
                .clips
                .iter()
                .filter(|cl| cl.kind == ClipKind::Audio && !cl.muted)
                .filter_map(|cl| {
                    let tr = p.tracks.get(cl.track as usize)?;
                    if tr.muted || (any_solo && !tr.solo) {
                        return None;
                    }
                    let is_excluded = ex_ins == Some(tr.insert);
                    Some((cl.clone(), is_excluded))
                })
                .collect();
            (active, ex_ins, p.bpm)
        };
        let spb = 60.0 / bpm;
        let use_non_excluded = excluded_insert.is_some() && all_clips.iter().any(|(_, ex)| !*ex);
        let chosen: Vec<Clip> = all_clips.into_iter().filter(|(_, ex)| !use_non_excluded || !*ex).map(|(cl, _)| cl).collect();
        let mut total_chroma = [0f32; 12];
        let mut tuning_sum = 0f32;
        let mut weight_sum = 0f32;
        let mut best_tempo: Option<(f64, crate::music::TempoEstimate)> = None;
        for cl in chosen {
            let Some(m) = self.source_music(cl.source, cl.norm, cl.rev) else { continue };
            let (a, b) = (cl.offset * spb, (cl.offset + cl.len) * spb);
            let (cs, n) = m.chroma_sum(a, b);
            if n > 0 {
                for k in 0..12 {
                    total_chroma[k] += cs[k];
                }
                tuning_sum += m.tuning_cents * n as f32;
                weight_sum += n as f32;
            }
            if let Some(ref t) = m.tempo {
                let score = cl.len * t.confidence.max(0.1) as f64;
                if best_tempo.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    best_tempo = Some((score, t.clone()));
                }
            }
        }
        let tuning = if weight_sum > 0.0 { tuning_sum / weight_sum } else { 0.0 };
        let key = crate::music::estimate_key(&total_chroma, tuning);
        Ok(crate::music::MusicAnalysis { tempo: best_tempo.map(|(_, t)| t), key })
    }

    /// "Auto vocal": measures the vocal clip (what it plays, its region only) and returns
    /// the calibrated insert chain for its track. The key comes from `beat_clip` when given,
    /// else from the project's other audio (tracks not routed to the vocal's insert); the
    /// level suggestion needs `beat_clip`. Slow (seconds): call off the UI thread.
    ///
    /// `bake_rider`: the plan's rider comes out of the chain into `rider_bake`, for
    /// [`Self::bake_rider`] (no live look-ahead on the insert, so no playback delay).
    pub fn auto_vocal_plan(&self, clip_id: Id, beat_clip: Option<Id>, style: crate::autovocal::Style, bake_rider: bool) -> Result<crate::autovocal::AutoVocalPlan, EngineError> {
        let clip = self.audio_clip(clip_id)?;
        let region = |cl: &Clip| -> Result<(Vec<f32>, f32), EngineError> {
            let (audio, bpm) = {
                let mut c = self.core.lock();
                let bpm = c.project.bpm;
                (c.sources.clip_audio(cl, false, bpm).ok_or(EngineError::Msg("audio not loaded".into()))?, bpm)
            };
            let mono = crate::sources::mono_of(&audio);
            let sr = audio.sr as f64;
            let (a, b) = crate::sources::clip_region(cl, bpm, mono.len() as f64 / sr);
            let (i0, i1) = ((a * sr) as usize, ((b * sr) as usize).min(mono.len()));
            let gain = crate::dsp::db_to_gain(cl.gain);
            Ok((mono[i0.min(i1)..i1].iter().map(|v| v * gain).collect(), audio.sr as f32))
        };
        let (vocal, sr) = region(&clip)?;
        if vocal.len() < (sr * 1.0) as usize {
            return Err(EngineError::Msg("the clip is too short to analyse (under 1 s)".into()));
        }
        let vocal_insert = {
            let c = self.core.lock();
            c.project.tracks.get(clip.track as usize).map(|t| t.insert)
        };
        let key = match beat_clip {
            Some(b) => self.music_analysis(Some(b), None)?.key,
            None => self.music_analysis_ex(None, None, vocal_insert).ok().and_then(|m| m.key),
        };
        let pitch = self.pitch_analysis(clip_id).ok();
        let beat = match beat_clip {
            Some(b) => {
                let bc = {
                    let c = self.core.lock();
                    c.project.clips.iter().find(|x| x.id == b).cloned().ok_or(EngineError::Msg("no such beat clip".into()))?
                };
                Some(region(&bc)?)
            }
            None => None,
        };
        let plan = crate::autovocal::plan(&crate::autovocal::PlanInput {
            vocal: &vocal,
            sr,
            style,
            key,
            pitch: pitch.as_deref(),
            beat: beat.as_ref().map(|(x, s)| (x.as_slice(), *s)),
        });
        Ok(if bake_rider { plan.baked() } else { plan })
    }

    /// Renders what an audio clip plays (its region, clip gain, normalise/reverse and pitch
    /// edits; not its fades, which stay on the clip) through the Vocal Rider offline, as
    /// `autovocal::render` does — the rider's latency removed, sample-aligned, any
    /// look-ahead — into a new source, and points the clip at it (gain 0 dB, no
    /// norm/rev/pitch edits, the region's sub-sample start kept as the offset). The original
    /// source stays in the project. One undo step. `params` are Rider params (catalog ids;
    /// missing ones take the catalog defaults), e.g. an auto-vocal plan's `rider_bake`.
    /// Returns (new source id, clip id). Slow (the whole region is rendered): call it off
    /// the UI thread.
    pub fn bake_rider(&self, clip_id: Id, params: &std::collections::BTreeMap<String, f64>) -> Result<(Id, Id), EngineError> {
        let (clip, audio, bpm, src_name, color) = {
            let mut c = self.core.lock();
            let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
            if clip.kind != ClipKind::Audio {
                return Err(EngineError::Msg("only audio clips can be baked".into()));
            }
            let bpm = c.project.bpm;
            let audio = c.sources.clip_audio(&clip, false, bpm).ok_or(EngineError::Msg("audio not loaded".into()))?;
            let src = c.project.sources.iter().find(|s| s.id == clip.source);
            let (name, color) = src.map(|s| (s.name.clone(), s.color.clone())).unwrap_or_else(|| ("Vocal".into(), "#e05263".into()));
            (clip, audio, bpm, name, color)
        };
        let sr = audio.sr;
        let (a, b) = crate::sources::clip_region(&clip, bpm, audio.seconds());
        // the same frames `auto_vocal_plan` measures (truncated start); the fraction of a
        // frame the region starts past it stays on the clip as its offset
        let pos = a * sr as f64;
        let i0 = (pos as usize).min(audio.frames);
        let i1 = ((b * sr as f64) as usize).clamp(i0, audio.frames);
        if i1 - i0 < 16 {
            return Err(EngineError::Msg("the clip is empty".into()));
        }
        // two guard frames (as in `flatten_comp`): playback reads a frame or two past the
        // region's last whole frame (interpolation, the clip end landing between frames)
        let i1 = (i1 + 2).min(audio.frames);
        let g = crate::dsp::db_to_gain(clip.gain);
        let content = audio.content();
        let l: Vec<f32> = (i0..i1).map(|i| content[2 * i] * g).collect();
        let r: Vec<f32> = (i0..i1).map(|i| content[2 * i + 1] * g).collect();
        let (yl, yr) = crate::autovocal::render_stereo(FxKind::Rider, params, &l, &r, sr as f32);
        let data: Vec<f32> = yl.iter().zip(&yr).flat_map(|(a, b)| [*a, *b]).collect();
        let spb = 60.0 / bpm;
        let offset = (pos - i0 as f64) / sr as f64 / spb;

        let mut c = self.core.lock();
        let now = c.project.clips.iter().find(|x| x.id == clip_id).cloned();
        if now.as_ref() != Some(&clip) || c.project.bpm != bpm {
            return Err(EngineError::Msg("the clip changed while the rider was rendering; try again".into()));
        }
        let id = c.project.alloc_id();
        let name = format!("{src_name} (rider)");
        let path = fresh_path(&audio_dir(&c), &format!("{} rider {}", sanitize(&src_name), id));
        write_wav(&path, &data, sr)?;
        let baked = Audio::from_stereo(data, sr, audio.channels);
        let before = c.project.clone();
        c.project.sources.push(Source { id, name, color, path: path.display().to_string(), dur: baked.seconds(), sr, ch: audio.channels, peak_db: crate::dsp::gain_to_db(baked.peak()) });
        c.sources.insert(id, Arc::new(baked));
        if let Some(x) = c.project.clips.iter_mut().find(|x| x.id == clip_id) {
            *x = Clip { source: id, offset, gain: 0.0, norm: false, rev: false, tune: None, ..clip };
        }
        push_undo(&mut c, before);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
        self.changed(c);
        Ok((id, clip_id))
    }

    /// Peaks of what the clip actually plays (including pitch edits), for drawing.
    pub fn clip_peaks(&self, clip_id: Id) -> Option<Vec<f32>> {
        let mut c = self.core.lock();
        let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned()?;
        let bpm = c.project.bpm;
        c.sources.clip_audio(&clip, true, bpm).map(|a| a.peaks.clone())
    }

    /// [min,max] pairs at 100/s for drawing.
    pub fn source_peaks(&self, id: Id, norm: bool, rev: bool) -> Option<Vec<f32>> {
        let mut c = self.core.lock();
        c.sources.variant(id, norm, rev).map(|a| a.peaks.clone())
    }

    /// Replaces the clip's source by a new file holding only the clip's region.
    pub fn trim_clip(&self, clip_id: Id) -> Result<Id, EngineError> {
        let mut c = self.core.lock();
        let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
        if clip.kind != ClipKind::Audio {
            return Err(EngineError::Msg("only audio clips can be trimmed".into()));
        }
        let audio = c.sources.variant(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))?;
        let spb = 60.0 / c.project.bpm;
        let sr = audio.sr as f64;
        let a = ((clip.offset * spb * sr) as usize).min(audio.frames);
        let b = (((clip.offset + clip.len) * spb * sr) as usize).min(audio.frames);
        if b <= a + 16 {
            return Err(EngineError::Msg("the region is empty".into()));
        }
        let data = audio.content()[a * 2..b * 2].to_vec();
        let old = c.project.sources.iter().find(|s| s.id == clip.source).cloned().unwrap();
        let id = c.project.alloc_id();
        let path = fresh_path(&audio_dir(&c), &format!("{} trim {}", sanitize(&old.name), id));
        write_wav(&path, &data, audio.sr)?;
        let new = Audio::from_stereo(data, audio.sr, audio.channels);
        let before = c.project.clone();
        c.project.sources.push(Source { id, path: path.display().to_string(), dur: new.seconds(), peak_db: crate::dsp::gain_to_db(new.peak()), ..old });
        c.sources.insert(id, Arc::new(new));
        if let Some(x) = c.project.clips.iter_mut().find(|x| x.id == clip_id) {
            x.source = id;
            x.offset = 0.0;
            x.norm = false;
            x.rev = false;
        }
        push_undo(&mut c, before);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
        self.changed(c);
        Ok(id)
    }

    pub fn preview_clip(&self, clip_id: Id) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
        let audio = c.sources.variant(clip.source, clip.norm, clip.rev).ok_or(EngineError::Msg("audio not loaded".into()))?;
        let spb = 60.0 / c.project.bpm;
        let sr = audio.sr as f64;
        let pv = Preview {
            pos: clip.offset * spb * sr,
            start: clip.offset * spb * sr,
            end: ((clip.offset + clip.len) * spb * sr).min(audio.frames as f64),
            gain: 10f32.powf(clip.gain / 20.0),
            fade_in: clip.fade_in as f64,
            fade_out: clip.fade_out as f64,
            audio,
        };
        send(&mut c, Cmd::Preview(Some(Box::new(pv))));
        Ok(())
    }

    pub fn preview_file(&self, path: &Path) -> Result<(), EngineError> {
        let audio = Arc::new(match path.to_str() {
            Some(s) if s.starts_with(crate::factory::SCHEME) => Audio::load_path(s, Path::new("."))?,
            _ => Audio::load(path)?,
        });
        let mut c = self.core.lock();
        let end = audio.frames as f64;
        send(&mut c, Cmd::Preview(Some(Box::new(Preview { audio, pos: 0.0, start: 0.0, end, gain: 0.8, fade_in: 0.0, fade_out: 0.0 }))));
        Ok(())
    }

    pub fn preview_stop(&self) {
        let mut c = self.core.lock();
        send(&mut c, Cmd::Preview(None));
    }

    /// Plays one note of a channel right away (step clicks, the piano-roll keyboard, Ctrl+click
    /// on a channel). `pattern`: the pattern being edited, so a channel that follows its clip's
    /// track is heard through that track. Not an edit.
    pub fn note_audition(&self, chan: Id, key: Option<u8>, vel: f32, pattern: Option<Id>) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        let ch = c.project.channel(chan).ok_or(EngineError::Msg(format!("no channel {chan}")))?;
        let key = key.unwrap_or(ch.root).min(127);
        // a one-shot rings out by itself; held sounds get a short note
        let len = 0.35;
        send(&mut c, Cmd::Audition { chan, key, vel: vel.clamp(0.0, 1.0), len, pattern });
        Ok(())
    }

    /// Loops one pattern on its own (`None` stops). Starting it stops the song; starting the
    /// song stops it (they are exclusive, like FL Studio's PAT / SONG). Not an edit.
    pub fn pattern_play(&self, pattern: Option<Id>) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        if let Some(id) = pattern {
            if !c.project.patterns.iter().any(|p| p.id == id) {
                return Err(EngineError::Msg(format!("no pattern {id}")));
            }
            if c.take.is_some() {
                return Err(EngineError::Msg("recording: stop it first".into()));
            }
        }
        send(&mut c, Cmd::PatPlay(pattern));
        if pattern.is_some() && c.transport.playing {
            c.transport.playing = false;
            self.changed(c);
        }
        Ok(())
    }

    // ------------------------------------------------------------ WAVE Keys

    /// Hands the engine the loaded piano (or takes it away). Keys channels are silent
    /// without it; with it they play at once, also notes that are sounding.
    pub fn set_keys_bank(&self, bank: Option<Arc<crate::keys::KeysBank>>) {
        let mut c = self.core.lock();
        c.keys = bank;
        self.sync(&mut c, Dirty { timeline: true, mixer: false, params: false });
    }

    pub fn has_keys_bank(&self) -> bool {
        self.core.lock().keys.is_some()
    }

    /// True when the project has a WAVE Keys channel.
    pub fn uses_keys(&self) -> bool {
        self.core.lock().project.channels.iter().any(|c| matches!(c.src, ChannelSrc::Keys { .. }))
    }

    // ------------------------------------------------------------ project files

    pub fn new_project(&self, project: Project) {
        let mut c = self.core.lock();
        if c.take.is_some() {
            return;
        }
        send(&mut c, Cmd::Stop);
        send(&mut c, Cmd::PatPlay(None));
        c.transport.playing = false;
        let mut project = project;
        project.migrate();
        // ids start over in a new project: audio of the previous one must not answer to them
        let mut sources = SourceStore::default();
        load_library_sources(&project, &mut sources);
        c.sources = sources;
        c.project = project;
        c.undo.clear();
        c.redo.clear();
        c.audition.clear();
        c.path = None;
        c.revision += 1;
        c.saved_revision = c.revision;
        c.fx_errors.clear();
        self.sync(&mut c, Dirty { timeline: true, mixer: true, params: true });
        self.changed(c);
    }

    pub fn save(&self, path: &Path) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        self.snapshot_plugin_states(&mut c);
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut p = c.project.clone();
        // move recordings that live in the scratch folder next to the project
        let audio_dir = dir.join(format!("{} Audio", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
        let scratch = scratch_dir();
        for s in p.sources.iter_mut().filter(|s| !s.path.starts_with(crate::factory::SCHEME)) {
            let sp = PathBuf::from(&s.path);
            if sp.starts_with(&scratch) {
                let dst = audio_dir.join(sp.file_name().unwrap_or_default());
                std::fs::create_dir_all(&audio_dir).map_err(|e| e.to_string())?;
                std::fs::copy(&sp, &dst).map_err(|e| format!("copying {}: {e}", sp.display()))?;
                s.path = dst.display().to_string();
            }
            if let Ok(rel) = Path::new(&s.path).strip_prefix(dir) {
                s.path = rel.display().to_string();
            }
        }
        write_project_file(&p, path, true)?;
        // keep absolute paths in memory
        for s in c.project.sources.iter_mut() {
            let sp = PathBuf::from(&s.path);
            if sp.starts_with(&scratch) {
                s.path = audio_dir.join(sp.file_name().unwrap_or_default()).display().to_string();
            }
        }
        c.path = Some(path.to_path_buf());
        c.saved_revision = c.revision;
        self.changed(c);
        Ok(())
    }

    /// Writes the current project to `path` without making it the project's file: the
    /// path, the dirty flag and the sources stay as they are (recordings stay in the
    /// scratch folder, paths are absolute). Crash recovery uses it for projects that
    /// were never saved.
    pub fn save_copy(&self, path: &Path) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        self.snapshot_plugin_states(&mut c);
        let p = c.project.clone();
        drop(c);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        write_project_file(&p, path, false)
    }

    /// Opens a copy written by [`Engine::save_copy`] as an unsaved project with unsaved
    /// changes: Save asks where to put it instead of overwriting the copy.
    pub fn open_recovered(&self, path: &Path) -> Result<Vec<String>, EngineError> {
        let warnings = self.open(path)?;
        let mut c = self.core.lock();
        c.path = None;
        c.revision += 1;
        self.changed(c);
        Ok(warnings)
    }

    /// Loads a project file. Missing audio is reported, not fatal.
    pub fn open(&self, path: &Path) -> Result<Vec<String>, EngineError> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("not a WAVE project: {e}"))?;
        let mut p = Project::from_json(doc.get("project").cloned().unwrap_or(doc)).map_err(|e| format!("not a WAVE project: {e}"))?;
        p.migrate();
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut warnings = vec![];
        let mut store = SourceStore::default();
        for s in p.sources.iter_mut() {
            // library paths stay portable; everything else is absolute in memory
            if !s.path.starts_with(crate::factory::SCHEME) {
                let sp = PathBuf::from(&s.path);
                let abs = if sp.is_absolute() { sp } else { dir.join(sp) };
                s.path = abs.display().to_string();
            }
            match Audio::load_path(&s.path, dir) {
                Ok(a) => store.insert(s.id, Arc::new(a)),
                Err(e) => warnings.push(e),
            }
        }
        if p.inserts.first().map(|m| m.id) != Some(0) {
            return Err(EngineError::Msg("the project has no master insert".into()));
        }
        let mut c = self.core.lock();
        send(&mut c, Cmd::Stop);
        send(&mut c, Cmd::PatPlay(None));
        c.transport.playing = false;
        c.sources = store;
        c.project = p;
        c.audition.clear();
        c.undo.clear();
        c.redo.clear();
        c.path = Some(path.to_path_buf());
        c.revision += 1;
        c.saved_revision = c.revision;
        c.fx_errors.clear();
        self.sync(&mut c, Dirty { timeline: true, mixer: true, params: true });
        self.changed(c);
        Ok(warnings)
    }

    // ------------------------------------------------------------ offline render

    /// Beat range the UI and CLI use for a song or loop bounce.
    ///
    /// `song` is always `0..song_end`. `loop` uses the project's loop when one
    /// exists; otherwise it falls back to the song so a missing loop never yields
    /// an empty range.
    pub fn export_range(project: &Project, use_loop: bool) -> (f64, f64) {
        if use_loop {
            if let Some(l) = project.loop_range {
                if l.b > l.a {
                    return (l.a, l.b);
                }
            }
        }
        (0.0, project.song_end())
    }

    /// Renders `[from, to)` beats (plus `tail` seconds) of the current project to
    /// interleaved stereo at `sample_rate`, independently of the live device.
    pub fn render(&self, from: f64, to: f64, tail: f64, sample_rate: u32) -> Result<Vec<f32>, EngineError> {
        self.render_cancelable(from, to, tail, sample_rate, &AtomicBool::new(false))
    }

    /// Same as [`Self::render`], but returns [`EngineError::Cancelled`] as soon as
    /// `cancel` is set. Checked between DSP blocks so a long bounce can stop without
    /// waiting for the whole file.
    pub fn render_cancelable(
        &self,
        from: f64,
        to: f64,
        tail: f64,
        sample_rate: u32,
        cancel: &AtomicBool,
    ) -> Result<Vec<f32>, EngineError> {
        if to <= from {
            return Err(EngineError::Msg("`to` must be after `from`".into()));
        }
        if sample_rate < 8000 {
            return Err(EngineError::Msg("sample rate must be at least 8000 Hz".into()));
        }
        if cancel.load(Relaxed) {
            return Err(EngineError::Cancelled);
        }
        let (project, store_snapshot, takes, keys) = {
            let mut guard = self.core.lock();
            let c = &mut *guard;
            self.snapshot_plugin_states(c);
            let mut map = HashMap::new();
            let bpm = c.project.bpm;
            for clip in &c.project.clips {
                if clip.kind == ClipKind::Audio {
                    if let Some(a) = c.sources.clip_audio(clip, false, bpm) {
                        map.insert(clip.id, a);
                    }
                }
            }
            let mut audio = comp_audio(&c.sources, &c.project.comps);
            audio.extend(sample_audio(&c.sources, &c.project.channels));
            (c.project.clone(), map, audio, c.keys.clone())
        };
        let sr = sample_rate as f32;
        let block = 512;
        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::<Cmd>::new(64 + project.channels.len());
        let (gb_tx, mut gb_rx) = rtrb::RingBuffer::<Garbage>::new(64);
        let (rec_tx, _rec_rx) = rtrb::RingBuffer::<f32>::new(16);
        let mut taps = Taps { input: vec![], output: vec![] };
        let mut keep = vec![];
        for _ in 0..WATCH_SLOTS {
            let (a, b) = rtrb::RingBuffer::<f32>::new(16);
            let (c2, d) = rtrb::RingBuffer::<f32>::new(16);
            taps.input.push(a);
            taps.output.push(c2);
            keep.push((b, d));
        }
        let mut rt = Rt::new(sr, block, cmd_rx, gb_tx, Arc::new(Shared::new()), taps, rec_tx);
        rt.set_skip_silent(self.skip_silent.load(Relaxed));
        rt.set_workers(self.render_workers.load(Relaxed));
        let mut errs = HashMap::new();
        let graph = build_graph(&project, &HashSet::new(), sr, block, &*self.factory, false, &mut errs);
        let mut tl = build_timeline(&project, &mut |clip| store_snapshot.get(&clip.id).cloned(), &takes, &HashMap::new(), keys.as_ref());
        tl.loop_range = None;
        tl.song_end = TOTAL_BEATS;
        let _ = cmd_tx.push(Cmd::Graph(Box::new(graph)));
        for ch in &project.channels {
            if let ChannelSrc::Synth { synth } = &ch.src {
                let _ = cmd_tx.push(Cmd::SynthAdd(Box::new(crate::poly::SynthInst::new(ch.id, crate::poly::params::resolve(synth), sr))));
            }
        }
        let _ = cmd_tx.push(Cmd::Timeline(Box::new(tl)));
        let _ = cmd_tx.push(Cmd::Play { from, count_in: 0.0, rec: false, rec_loop: false });
        let spb = 60.0 / project.bpm;
        let frames = (((to - from).max(0.0) * spb + tail.max(0.0)) * sample_rate as f64) as usize;
        let play_frames = ((to - from).max(0.0) * spb * sample_rate as f64) as usize;
        // plugin delay compensation delays everything by `pdc` frames: render that much
        // longer and drop it from the front, so the file starts on `from` and the last
        // `pdc` frames of the song are not cut off
        let pdc = playback_latency(&project, sr);
        let total = frames + pdc;
        let mut out = Vec::with_capacity(frames * 2);
        let mut l = vec![0f32; block];
        let mut r = vec![0f32; block];
        let mut done = 0;
        let mut stopped = false;
        let keep = |out: &mut Vec<f32>, at: usize, l: &[f32], r: &[f32]| {
            for i in at.max(pdc).saturating_sub(at)..l.len() {
                out.push(l[i]);
                out.push(r[i]);
            }
        };
        while done < total {
            if cancel.load(Relaxed) {
                return Err(EngineError::Cancelled);
            }
            let n = block.min(total - done);
            if !stopped && done + n > play_frames {
                // stop exactly at `to`: render up to it, then let tails ring
                let k = play_frames.saturating_sub(done);
                if k > 0 {
                    rt.process(&mut l[..k], &mut r[..k], None);
                    keep(&mut out, done, &l[..k], &r[..k]);
                    done += k;
                }
                let _ = cmd_tx.push(Cmd::Stop);
                stopped = true;
                continue;
            }
            rt.process(&mut l[..n], &mut r[..n], None);
            keep(&mut out, done, &l[..n], &r[..n]);
            done += n;
            while gb_rx.pop().is_ok() {}
        }
        Ok(out)
    }

    // ------------------------------------------------------------ internals

    fn snapshot_plugin_states(&self, c: &mut Core) {
        let factory = &self.factory;
        for m in c.project.inserts.iter_mut() {
            for f in m.fx.iter_mut() {
                if let Some(pl) = f.plugin.as_mut() {
                    if let Some(st) = factory.state(f.id) {
                        pl.state = Some(st);
                    }
                }
            }
        }
    }

    /// Pushes the project to the audio thread.
    fn sync(&self, c: &mut Core, d: Dirty) {
        self.refresh_midi(c);
        let Some(w) = c.wires.as_ref() else { return };
        let (sr, max_block) = (w.sr, w.max_block);
        // synth instances first, so a new timeline never plays a channel without one
        sync_synths(c, sr);
        // a take's alignment assumes the latency at its start: hold latency changes back
        let defer = d.mixer && c.take.as_ref().is_some_and(|t| t.pdc != playback_latency(&c.project, sr));
        c.graph_deferred |= defer;
        if d.mixer && !defer {
            c.graph_deferred = false;
            let now: HashSet<Id> = c.project.inserts.iter().flat_map(|m| m.fx.iter().map(|f| f.id)).collect();
            for gone in c.live_fx.difference(&now).copied().collect::<Vec<_>>() {
                self.factory.release(gone);
                c.fx_errors.remove(&gone);
            }
            let mut errs = std::mem::take(&mut c.fx_errors);
            let graph = build_graph(&c.project, &c.live_fx, sr, max_block, &*self.factory, true, &mut errs);
            c.fx_errors = errs;
            c.live_fx = graph.inserts.iter().flat_map(|i| i.fx.iter().map(|s| s.id)).collect();
            c.sent_params = c.project.inserts.iter().flat_map(|m| m.fx.iter()).map(|f| (f.id, fx_send(f))).collect();
            send(c, Cmd::Graph(Box::new(graph)));
        } else if d.params || defer {
            send(c, Cmd::Mix(Box::new(mix_state(&c.project))));
            let changed: Vec<(Id, (Option<ParamBlock>, f32))> = c
                .project
                .inserts
                .iter()
                .flat_map(|m| m.fx.iter())
                .map(|f| (f.id, fx_send(f)))
                .filter(|(id, pb)| c.sent_params.get(id) != Some(pb))
                .collect();
            for (id, (params, mix)) in changed {
                c.sent_params.insert(id, (params, mix));
                send(c, Cmd::Params { fx: id, params, mix });
            }
        }
        if d.timeline {
            let gesture_open = c.gesture.is_some();
            let audition = live_audition(c);
            let Core { project, sources, keys, .. } = c;
            let stale = gesture_open;
            let mut takes = comp_audio(sources, &project.comps);
            takes.extend(sample_audio(sources, &project.channels));
            let bpm = project.bpm;
            let tl = build_timeline(project, &mut |clip| sources.clip_audio(clip, stale, bpm), &takes, &audition, keys.as_ref());
            send(c, Cmd::Timeline(Box::new(tl)));
        }
    }

    fn housekeeping(&self, tick: u32) {
        let shared = self.shared();
        let frame_parts;
        {
            let mut guard = self.core.lock();
            let c = &mut *guard;
            let Some(w) = c.wires.as_mut() else { return };
            while w.garbage.pop().is_ok() {}
            let analysis = if c.watch.is_empty() { None } else { Some(w.analyzer.analyze(&c_watch(&c.watch), &shared)) };
            let w = c.wires.as_mut().unwrap();
            let sr = w.sr as u32;
            // drain recorded input
            let n = w.record.slots();
            if n > 0 {
                if let Ok(chunk) = w.record.read_chunk(n) {
                    let (a, b) = chunk.as_slices();
                    if let Some(t) = c.take.as_mut() {
                        for &x in a.iter().chain(b.iter()) {
                            t.push(x);
                        }
                    }
                    chunk.commit_all();
                }
            }
            let _ = sr;
            let ids: Vec<Id> = c.project.inserts.iter().map(|m| m.id).collect();
            if let Some(t) = c.take.as_mut() {
                t.resolve(&shared);
            }
            let rec = c.take.as_mut().filter(|t| !t.start.is_nan()).map(|t| {
                let new = t.peaks[t.peaks_sent..].to_vec();
                let off = t.peaks_sent / 2;
                t.peaks_sent = t.peaks.len();
                (t.track as u32, t.pass_start_beat(), (t.frames() - t.pass_start_frame()) as f32 / t.sr as f32, off as u32, new)
            });
            frame_parts = Some((ids, analysis, rec));
        }
        if shared.stopped_at_end.swap(false, Relaxed) {
            {
                let mut c = self.core.lock();
                c.transport.playing = false;
            }
            self.finish_take();
            self.finish_midi_take();
            let c = self.core.lock();
            self.changed(c);
        }
        if let Some((ids, analysis, rec)) = frame_parts {
            let sink = self.sink.lock();
            if let Some(sink) = sink.as_ref() {
                let frame = crate::telemetry::frame(tick, &shared, &ids, analysis.as_deref(), rec.as_ref());
                sink(frame);
            } else {
                // still consume meters so the first frame after subscribing is fresh
                for i in 0..ids.len().min(MAX_INSERTS) {
                    shared.take_meter(i);
                }
            }
        }
    }

    fn finish_take(&self) {
        let shared = self.shared();
        let mut c = self.core.lock();
        if let Some(t) = c.take.as_mut() {
            t.resolve(&shared);
        }
        self.finalize_take(&mut c);
        if c.graph_deferred {
            self.sync(&mut c, Dirty { timeline: false, mixer: true, params: true });
        }
    }

    fn finalize_take(&self, c: &mut Core) {
        let Some(mut take) = c.take.take() else { return };
        // whatever is still in the ring belongs to this recording
        if let Some(w) = c.wires.as_mut() {
            while let Ok(x) = w.record.pop() {
                take.push(x);
            }
        }
        if take.start.is_nan() {
            return; // punched in, but the audio thread never started capturing
        }
        if self.midi_take.lock().played {
            return; // the armed track was played on a MIDI keyboard: this is a MIDI take
        }
        let spb = 60.0 / c.project.bpm;
        let sr = take.sr as f64;
        // Latency compensation: input captured at frame `j` was sung to what was heard
        // `skip` frames earlier. Dropping the first `skip` frames moves the whole take
        // earlier; pass marks (written when the audio thread wrapped) then land exactly
        // on the loop start of what was heard, so they stay where they are.
        let skip = take.skip.min(take.frames());
        let data = &take.data[skip * 2..];
        let total = data.len() / 2;
        // one pass per loop cycle; a last pass cut off within a beat of the wrap is not a take
        let mut bounds = vec![0];
        bounds.extend(take.marks.iter().copied().filter(|&m| m < total));
        bounds.push(total);
        let mut passes: Vec<(f64, &[f32])> = vec![];
        for k in 0..bounds.len() - 1 {
            let (f0, f1) = (bounds[k], bounds[k + 1]);
            let secs = f1.saturating_sub(f0) as f64 / sr;
            let cut_short = k > 0 && k == bounds.len() - 2;
            if secs < if cut_short { spb } else { 0.05 } {
                continue;
            }
            let start = if k == 0 { take.start } else { take.lap.map(|l| l.0).unwrap_or(take.start) };
            passes.push((start, &data[f0 * 2..f1 * 2]));
        }
        if passes.is_empty() {
            return;
        }
        let before = c.project.clone();
        let dir = audio_dir(c);
        let mut landed = vec![];
        for (start, data) in passes {
            let secs = data.len() as f64 / 2.0 / sr;
            let id = c.project.alloc_id();
            let n = c.project.sources.iter().filter(|s| s.name.starts_with("Take")).count() + 1;
            let name = format!("Take {n}");
            let path = fresh_path(&dir, &format!("{} {}", name, id));
            if let Err(e) = write_wav(&path, data, take.sr) {
                log::error!("could not write recording: {e}");
            }
            let audio = Audio::from_stereo(data.to_vec(), take.sr, 1);
            c.project.sources.push(Source { id, name, color: "#e05263".into(), path: path.display().to_string(), dur: secs, sr: take.sr, ch: 1, peak_db: crate::dsp::gain_to_db(audio.peak()) });
            c.sources.insert(id, Arc::new(audio));
            landed.push((id, start, (secs / spb).min(TOTAL_BEATS - start)));
        }
        crate::comp::land_takes(&mut c.project, take.track as u32, &landed);
        push_undo(c, before);
        c.revision += 1;
        self.sync(c, Dirty { timeline: true, ..Default::default() });
    }

    // ------------------------------------------------------------ live MIDI

    /// Sets what the MIDI keyboard plays. `target`: the channel (instrument) to play, `None` =
    /// the first pitched channel. `live`: a device is connected; without one nothing is
    /// recorded and playing stops. Anything still sounding is released when this changes.
    pub fn midi_configure(&self, target: Option<Id>, live: bool) {
        let mut c = self.core.lock();
        let changed = c.midi.target != target || c.midi.live != live;
        c.midi.target = target;
        c.midi.live = live;
        self.refresh_midi(&c);
        self.midi_fast.live.store(live, Release);
        if changed {
            self.midi_fast.send(Cmd::MidiOff);
        }
    }

    /// The channel the keyboard plays right now (the chosen one, else the first pitched
    /// channel, else the first channel).
    pub fn midi_target(&self) -> Option<Id> {
        midi_channel(&self.core.lock())
    }

    /// A note from the computer keyboard used as a piano (FL "typing keyboard"): plays `chan`
    /// like a MIDI key, through the same lock-free ring, whether or not a MIDI device is connected.
    pub fn typing_note(&self, chan: Id, key: u8, vel: f32, on: bool) {
        let ins = match self.midi_fast.ins.load(Relaxed) {
            usize::MAX => None,
            i => Some(i),
        };
        let vel = if on { vel.clamp(0.0, 1.0) } else { 0.0 };
        self.midi_fast.send(Cmd::MidiNote { chan, key: key.min(127), vel, on, ins: if on { ins } else { None } });
    }

    /// Releases every sounding keyboard note (device unplugged).
    pub fn midi_panic(&self) {
        self.midi_fast.send(Cmd::MidiOff);
    }

    /// Publishes the channel and insert the keyboard plays for the lock-free event path.
    fn refresh_midi(&self, c: &Core) {
        self.midi_fast.chan.store(midi_channel(c).unwrap_or(u64::MAX), Relaxed);
        self.midi_fast.ins.store(armed_insert(c).unwrap_or(usize::MAX), Relaxed);
    }

    /// One decoded message from the keyboard. Plays through the audio thread's MIDI ring
    /// (no allocation there) and, while a take is armed, goes into the recorder at the
    /// playhead. Cheap and never waits for the core lock (the project being edited, saved
    /// or rebuilt); safe to call from the MIDI input thread.
    pub fn midi_event(&self, m: MidiMsg) {
        let mf = &self.midi_fast;
        if !mf.live.load(Acquire) {
            return;
        }
        let chan = match mf.chan.load(Relaxed) {
            u64::MAX => None,
            id => Some(id),
        };
        let ins = match mf.ins.load(Relaxed) {
            usize::MAX => None,
            i => Some(i),
        };
        let beat = self.shared().beat();
        match m {
            MidiMsg::NoteOn { key, vel, .. } => {
                let Some(chan) = chan else { return };
                mf.send(Cmd::MidiNote { chan, key, vel: MidiMsg::vel_gain(vel), on: true, ins });
                let mut t = self.midi_take.lock();
                if let Some((r, _, _)) = t.rec.as_mut() {
                    r.note_on(beat, key, vel);
                    t.played = true;
                }
            }
            MidiMsg::NoteOff { key, .. } => {
                let Some(chan) = chan else { return };
                mf.send(Cmd::MidiNote { chan, key, vel: 0.0, on: false, ins: None });
                if let Some((r, _, _)) = self.midi_take.lock().rec.as_mut() {
                    r.note_off(beat, key);
                }
            }
            MidiMsg::Sustain { down, .. } => {
                mf.send(Cmd::MidiSustain(down));
                if let Some((r, _, _)) = self.midi_take.lock().rec.as_mut() {
                    r.sustain(beat, down);
                }
            }
            MidiMsg::PitchBend { value, .. } => mf.send(Cmd::MidiBend(MidiMsg::bend_semitones(value))),
            MidiMsg::ModWheel { value, .. } => mf.send(Cmd::MidiMod(value as f32 / 127.0)),
            MidiMsg::AllNotesOff { .. } => mf.send(Cmd::MidiOff),
        }
    }

    /// Starts collecting keyboard notes for a take on armed track `track` from beat `from`.
    fn begin_midi_take(&self, c: &mut Core, track: usize, from: f64) {
        let mut t = self.midi_take.lock();
        t.played = false;
        if !c.midi.live {
            return;
        }
        if let Some(ch) = midi_channel(c) {
            t.rec = Some((MidiRecorder::new(from), track, ch));
        }
    }

    /// Ends the MIDI take at the playhead: the played notes become a pattern and a pattern
    /// clip on the armed track (one undo step). Nothing played = nothing added.
    fn finish_midi_take(&self) {
        let end = self.shared().beat();
        let mut c = self.core.lock();
        let Some((rec, track, ch)) = ({
            let mut t = self.midi_take.lock();
            t.played = false;
            t.rec.take()
        }) else {
            return;
        };
        let notes = rec.finish(end);
        let Some(laid) = midi::layout(&notes, ch, 4.0) else { return };
        let color = c.project.channel(ch).map(|x| x.color.clone()).unwrap_or_else(|| "#69c3d6".into());
        let n = c.project.patterns.iter().filter(|p| p.name.starts_with("MIDI take")).count() + 1;
        let pid = c.project.alloc_id();
        let cid = c.project.alloc_id();
        let pattern = Pattern { id: pid, name: format!("MIDI take {n}"), color, len: laid.len, min_len: laid.len, notes: laid.notes, swing: 0.0 };
        let clip = Clip {
            id: cid,
            track: track as u32,
            start: laid.start,
            len: laid.len,
            offset: 0.0,
            kind: ClipKind::Pat,
            source: pid,
            muted: false,
            gain: 0.0,
            fade_in: 0.0,
            fade_out: 0.0,
            pan: 0.0,
            norm: false,
            rev: false,
            tune: None,
        };
        drop(c);
        if let Err(e) = self.apply(&[Op::PutPattern { pattern }, Op::PutClips { clips: vec![clip] }], false, None) {
            log::error!("could not place the MIDI take: {e}");
        }
    }

    // ------------------------------------------------------------ comps

    /// Plays one whole take instead of the comp (`None` = back to the comp). Live only.
    pub fn audition(&self, comp: Id, take: Option<Id>) -> Result<(), EngineError> {
        let mut c = self.core.lock();
        let cp = c.project.comp(comp).ok_or(EngineError::Msg(format!("no comp {comp}")))?;
        match take {
            Some(t) if cp.take(t).is_none() => return Err(EngineError::Msg(format!("comp {comp} has no take {t}"))),
            Some(t) => {
                c.audition.insert(comp, t);
            }
            None => {
                c.audition.remove(&comp);
            }
        }
        self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
        self.changed(c);
        Ok(())
    }

    /// Renders what a comp clip plays into a new source and replaces the clip by an
    /// ordinary audio clip (gain, fades and pan stay on the clip). The take group goes
    /// when no other clip plays it. One undo step.
    pub fn flatten_comp(&self, clip_id: Id) -> Result<Id, EngineError> {
        let mut c = self.core.lock();
        let clip = c.project.clips.iter().find(|x| x.id == clip_id).cloned().ok_or(EngineError::Msg("no such clip".into()))?;
        if clip.kind != ClipKind::Comp {
            return Err(EngineError::Msg("only comp clips can be flattened".into()));
        }
        let comp = c.project.comp(clip.source).cloned().ok_or(EngineError::Msg("the clip's comp is missing".into()))?;
        let map = comp_audio(&c.sources, std::slice::from_ref(&comp));
        let first = comp.takes.iter().find_map(|t| map.get(&t.source)).ok_or(EngineError::Msg("the takes' audio is not loaded".into()))?;
        let (sr, ch) = (first.sr, comp.takes.iter().filter_map(|t| map.get(&t.source)).map(|a| a.channels).max().unwrap_or(1));
        let spb = 60.0 / c.project.bpm;
        let frames = (clip.len * spb * sr as f64).round() as usize;
        if frames < 16 {
            return Err(EngineError::Msg("the clip is empty".into()));
        }
        // two guard frames: the playhead's beat may land a hair before the clip end
        let frames = frames + 2;
        let rt = crate::comp::build(&comp, spb, None, &map);
        let (mut l, mut r) = (vec![0f32; frames], vec![0f32; frames]);
        rt.render_add(clip.offset, 1.0 / (spb * sr as f64), &mut l, &mut r, spb);
        let data: Vec<f32> = l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]).collect();
        let id = c.project.alloc_id();
        let name = format!("{} (flat)", comp.name);
        let path = fresh_path(&audio_dir(&c), &format!("{} flat {}", sanitize(&comp.name), id));
        write_wav(&path, &data, sr)?;
        let audio = Audio::from_stereo(data, sr, ch);
        let before = c.project.clone();
        c.project.sources.push(Source { id, name, color: "#e05263".into(), path: path.display().to_string(), dur: audio.seconds(), sr, ch, peak_db: crate::dsp::gain_to_db(audio.peak()) });
        c.sources.insert(id, Arc::new(audio));
        if let Some(x) = c.project.clips.iter_mut().find(|x| x.id == clip_id) {
            *x = Clip { kind: ClipKind::Audio, source: id, offset: 0.0, norm: false, rev: false, tune: None, ..clip.clone() };
        }
        if !c.project.clips.iter().any(|x| x.kind == ClipKind::Comp && x.source == comp.id) {
            c.project.comps.retain(|x| x.id != comp.id);
            c.audition.remove(&comp.id);
        }
        push_undo(&mut c, before);
        c.revision += 1;
        self.sync(&mut c, Dirty { timeline: true, ..Default::default() });
        self.changed(c);
        Ok(id)
    }
}

/// Loaded audio of every take source of `comps`.
fn comp_audio(store: &SourceStore, comps: &[Comp]) -> HashMap<Id, Arc<Audio>> {
    comps.iter().flat_map(|cp| cp.takes.iter()).filter_map(|t| store.get(t.source).map(|a| (t.source, a.clone()))).collect()
}

/// Loaded audio of every sampler channel.
fn sample_audio(store: &SourceStore, chans: &[Channel]) -> HashMap<Id, Arc<Audio>> {
    chans
        .iter()
        .filter_map(|c| match c.src {
            ChannelSrc::Sample { source, .. } => store.get(source).map(|a| (source, a.clone())),
            ChannelSrc::Synth { .. } | ChannelSrc::Keys { .. } => None,
        })
        .collect()
}

/// Decodes the sample-library sources of a project that did not come from a file (a new
/// project's starter kit). A kit that is not installed leaves its channels silent.
fn load_library_sources(p: &Project, store: &mut SourceStore) {
    for s in p.sources.iter().filter(|s| s.path.starts_with(crate::factory::SCHEME)) {
        if let Ok(a) = Audio::load_path(&s.path, Path::new(".")) {
            store.insert(s.id, Arc::new(a));
        }
    }
}

/// Auditions that still point at an existing take.
fn live_audition(c: &Core) -> HashMap<Id, Id> {
    c.audition.iter().filter(|(cp, t)| c.project.comp(**cp).is_some_and(|x| x.take(**t).is_some())).map(|(a, b)| (*a, *b)).collect()
}

fn fx_send(f: &Fx) -> (Option<ParamBlock>, f32) {
    (if f.kind == FxKind::Vst3 { None } else { Some(param_block(f)) }, f.mix.clamp(0.0, 1.0))
}

fn c_watch(w: &[Id]) -> Vec<Id> {
    w.to_vec()
}

fn push_undo(c: &mut Core, before: Project) {
    c.undo.push(before);
    if c.undo.len() > UNDO_LIMIT {
        c.undo.remove(0);
    }
    c.redo.clear();
}

/// The channel the keyboard plays: the chosen one if it exists, else the first pitched
/// channel, else the first channel.
fn midi_channel(c: &Core) -> Option<Id> {
    let p = &c.project;
    c.midi.target.filter(|id| p.channel(*id).is_some()).or_else(|| p.channels.first().map(|x| x.id))
}

/// Mixer insert index of the first armed track (where the keyboard is heard through).
fn armed_insert(c: &Core) -> Option<usize> {
    let p = &c.project;
    let t = p.tracks.iter().find(|t| t.arm)?;
    p.inserts.iter().position(|m| m.id == t.insert)
}

/// Brings the audio thread's synth instances in line with the project's synth channels:
/// new channels get an instance (built here, off the audio thread), changed ones their new
/// patch, removed ones are dropped.
fn sync_synths(c: &mut Core, sr: f32) {
    let want: Vec<(Id, crate::poly::Patch)> = c
        .project
        .channels
        .iter()
        .filter_map(|ch| match &ch.src {
            ChannelSrc::Synth { synth } => Some((ch.id, crate::poly::params::resolve(synth))),
            _ => None,
        })
        .collect();
    let gone: Vec<Id> = c.synths.keys().filter(|id| !want.iter().any(|(w, _)| w == *id)).copied().collect();
    for id in gone {
        c.synths.remove(&id);
        send(c, Cmd::SynthRemove(id));
    }
    for (id, patch) in want {
        match c.synths.get(&id) {
            None => {
                c.synths.insert(id, patch);
                send(c, Cmd::SynthAdd(Box::new(crate::poly::SynthInst::new(id, patch, sr))));
            }
            Some(old) if *old != patch => {
                c.synths.insert(id, patch);
                send(c, Cmd::SynthPatch { chan: id, patch: Box::new(patch) });
            }
            _ => {}
        }
    }
}

fn send(c: &mut Core, cmd: Cmd) {
    if let Some(w) = c.wires.as_mut() {
        if w.cmds.push(cmd).is_err() {
            log::warn!("audio command queue full; dropping a command");
        }
    }
}

fn watch_cmd(ids: &[Id]) -> Cmd {
    let mut a = [u64::MAX; WATCH_SLOTS];
    for (i, id) in ids.iter().take(WATCH_SLOTS).enumerate() {
        a[i] = *id;
    }
    Cmd::Watch { ids: a, n: ids.len().min(WATCH_SLOTS) as u8 }
}

/// `Song.wave` -> `Song (backup).wave`: the previous save, kept next to the project.
/// It is an ordinary project file, so File > Open shows and opens it.
pub fn backup_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{stem} (backup).wave"))
}

/// Serializes `p` and replaces `path` atomically (temp file + rename). With `backup`,
/// the file being replaced is kept as [`backup_path`] first; a failed backup never
/// blocks the save.
fn write_project_file(p: &Project, path: &Path, backup: bool) -> Result<(), EngineError> {
    let doc = serde_json::json!({ "format": "wave-project", "version": 2, "project": p });
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("wave.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    if backup && path.is_file() {
        if let Err(e) = std::fs::copy(path, backup_path(path)) {
            log::warn!("could not keep a backup of {}: {e}", path.display());
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn scratch_dir() -> PathBuf {
    dirs_data().join("WAVE").join("Recordings")
}

fn dirs_data() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(p);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join("Library/Application Support");
        }
    }
    if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(p);
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".local/share");
    }
    std::env::temp_dir()
}

fn audio_dir(c: &Core) -> PathBuf {
    match &c.path {
        Some(p) => p.parent().unwrap_or(Path::new(".")).join(format!("{} Audio", p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())),
        None => scratch_dir(),
    }
}

/// `dir/stem.wav`, or `dir/stem (2).wav` … when that file exists: ids restart in every new
/// project and unsaved projects share the scratch folder, so a new take must never
/// overwrite audio another project (or an older save of this one) still points at.
fn fresh_path(dir: &Path, stem: &str) -> PathBuf {
    let mut p = dir.join(format!("{stem}.wav"));
    let mut k = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({k}).wav"));
        k += 1;
    }
    p
}

fn sanitize(s: &str) -> String {
    s.chars().map(|ch| if ch.is_alphanumeric() || ch == ' ' || ch == '-' || ch == '_' { ch } else { '_' }).collect()
}

fn audible_tracks(p: &Project) -> Vec<(f32, bool)> {
    let any_solo = p.tracks.iter().any(|t| t.solo);
    p.tracks.iter().map(|t| (fader_gain(t.vol), !t.muted && (!any_solo || t.solo))).collect()
}

fn audible_inserts(p: &Project) -> Vec<(Id, f32, f32, f32, bool)> {
    let any_solo = p.inserts.iter().any(|m| m.id != 0 && m.solo);
    p.inserts.iter().map(|m| (m.id, fader_gain(m.vol), m.pan.clamp(-1.0, 1.0), if m.width.is_finite() { m.width.clamp(0.0, 2.0) } else { 1.0 }, !m.muted && (m.id == 0 || !any_solo || m.solo))).collect()
}

/// Frames between the timeline and the speakers inside the mixer: the slowest non-master
/// insert (plugin delay compensation delays every insert to match it) plus the master chain.
pub fn playback_latency(p: &Project, sr: f32) -> usize {
    let lat = |m: &crate::model::Insert| m.fx.iter().map(|f| crate::catalog::latency(f, sr)).sum::<usize>();
    let master = p.inserts.first().map(lat).unwrap_or(0);
    master + p.inserts.iter().skip(1).map(lat).max().unwrap_or(0)
}

fn monitor_track(p: &Project) -> Option<usize> {
    p.tracks.iter().position(|t| t.arm)
}

fn mix_state(p: &Project) -> MixState {
    MixState { inserts: audible_inserts(p), tracks: audible_tracks(p), monitor_track: monitor_track(p), chans: audible_channels(p) }
}

/// (gain, pan, audible) of every channel, in project order. Solo works like the mixer's:
/// with any channel soloed, only soloed channels play.
fn audible_channels(p: &Project) -> Vec<(f32, f32, bool)> {
    let any_solo = p.channels.iter().any(|c| c.solo);
    p.channels.iter().map(|c| (c.gain(), c.pan.clamp(-1.0, 1.0), !c.mute && (!any_solo || c.solo))).collect()
}

/// The channel table and the patterns for the audio thread. Swing is applied here (when
/// the timeline is built, i.e. at schedule time, never written into the notes); notes of
/// unknown channels and notes past the pattern end are left out.
fn build_patterns(p: &Project, audio: &HashMap<Id, Arc<Audio>>, keys: Option<&Arc<crate::keys::KeysBank>>) -> (Vec<ChanRt>, Vec<PatRt>) {
    let ins_idx = |id: Id| p.inserts.iter().position(|m| m.id == id);
    let track_ins = |track: u32| p.tracks.get(track as usize).and_then(|t| ins_idx(t.insert));
    // first clip (by time) of each pattern → its track's insert
    let mut first: HashMap<Id, (f64, u32)> = HashMap::new();
    for c in p.clips.iter().filter(|c| c.kind == ClipKind::Pat) {
        let e = first.entry(c.source).or_insert((c.start, c.track));
        if c.start < e.0 {
            *e = (c.start, c.track);
        }
    }
    let pat_home = |pid: Id| first.get(&pid).and_then(|&(_, t)| track_ins(t));
    let gains = audible_channels(p);
    let chans: Vec<ChanRt> = p
        .channels
        .iter()
        .zip(gains)
        .map(|(c, (gain, pan, on))| {
            let insert = c.insert.and_then(ins_idx);
            let heard = p.patterns.iter().filter(|pt| pt.notes.iter().any(|n| n.ch == c.id)).find_map(|pt| pat_home(pt.id));
            let (kind, sample) = match c.src {
                ChannelSrc::Sample { source, .. } => (crate::synth::VoiceKind::Sample, audio.get(&source).cloned()),
                ChannelSrc::Synth { .. } => (crate::synth::VoiceKind::Synth, None),
                ChannelSrc::Keys { .. } => (crate::synth::VoiceKind::Keys, None),
            };
            let ks = match c.src {
                ChannelSrc::Keys { keys: k } => Some(k),
                _ => None,
            };
            let (keys_release, keys_dynamics) = ks.map(|k| (k.release / 1000.0, k.dynamics / 100.0)).unwrap_or((0.35, 1.0));
            let smp = match c.src {
                ChannelSrc::Sample { source, sampler } => SmpRt::new(&sampler, p.sources.iter().find(|s| s.id == source).map(|s| s.peak_db)),
                _ => SmpRt::default(),
            };
            ChanRt { id: c.id, kind, sample, smp, keys: ks.and(keys.cloned()), keys_release, keys_dynamics, root: c.root, gain, pan, gain0: gain, pan0: pan, on, insert, heard, home: insert.or(heard).unwrap_or(0) }
        })
        .collect();
    let pats = p
        .patterns
        .iter()
        .map(|pt| {
            let end = (pt.len * 4.0) as f32;
            let notes = pt
                .notes
                .iter()
                .filter(|n| n.step < end - 1e-4)
                .filter_map(|n| {
                    let ci = p.channels.iter().position(|c| c.id == n.ch)?;
                    let step = n.step + swing_offset(n.step, pt.swing);
                    Some(NoteRt { beat: step as f64 / 4.0, len: n.len as f64 / 4.0, chan: ci, key: n.p.unwrap_or(p.channels[ci].root), vel: n.v.clamp(0.0, 1.0) })
                })
                .collect();
            PatRt { id: pt.id, len: pt.len, notes, home: pat_home(pt.id) }
        })
        .collect();
    (chans, pats)
}

/// Builds the mixer graph. Effects in `live` are left as `proc: None` so the audio
/// thread keeps the running instance; everything else is created and prepared here.
pub(crate) fn build_graph(p: &Project, live: &HashSet<Id>, sr: f32, max_block: usize, factory: &dyn PluginFactory, live_instances: bool, errs: &mut HashMap<Id, String>) -> Graph {
    let ins_state = audible_inserts(p);
    let lat: Vec<usize> = p.inserts.iter().map(|m| m.fx.iter().map(|f| crate::catalog::latency(f, sr)).sum()).collect();
    let max_lat = lat.iter().skip(1).copied().max().unwrap_or(0);
    let inserts = p
        .inserts
        .iter()
        .zip(ins_state)
        .map(|(m, (id, gain, pan, width, on))| {
            let fx = m
                .fx
                .iter()
                .map(|f| {
                    let proc: Option<Box<dyn Processor>> = if live.contains(&f.id) {
                        None
                    } else if f.kind == FxKind::Vst3 {
                        match factory.create(f.id, f.plugin.as_ref().unwrap(), sr, max_block, live_instances) {
                            Ok(p) => {
                                errs.remove(&f.id);
                                Some(p)
                            }
                            Err(e) => {
                                errs.insert(f.id, e);
                                None
                            }
                        }
                    } else {
                        let mut pr = crate::dsp::create(f.kind).unwrap();
                        pr.prepare(sr, max_block);
                        pr.set_params(&param_block(f));
                        Some(pr)
                    };
                    let params = if f.kind == FxKind::Vst3 { None } else { Some(param_block(f)) };
                    FxSlot::new(f.id, f.bypass, f.mix.clamp(0.0, 1.0), proc, params, crate::catalog::latency(f, sr))
                })
                .collect();
            let k = p.inserts.iter().position(|x| x.id == id).unwrap_or(0);
            let comp = if k == 0 { 0 } else { max_lat - lat[k] };
            InsertRt::new(id, gain, pan, width, on, fx, max_block).with_delay(comp)
        })
        .collect();
    let track_state = audible_tracks(p);
    let tracks = p
        .tracks
        .iter()
        .zip(track_state)
        .map(|(t, (gain, on))| TrackRt { insert: p.inserts.iter().position(|m| m.id == t.insert).unwrap_or(0), gain, on })
        .collect();
    Graph::new(inserts, tracks, monitor_track(p), max_block).with_latency(max_lat, lat.first().copied().unwrap_or(0))
}

/// `takes`: loaded audio by source id of every comp take and sampler channel.
pub(crate) fn build_timeline(p: &Project, audio: &mut dyn FnMut(&Clip) -> Option<Arc<Audio>>, takes: &HashMap<Id, Arc<Audio>>, audition: &HashMap<Id, Id>, keys: Option<&Arc<crate::keys::KeysBank>>) -> Timeline {
    // comps are resolved once each, shared by every clip that plays them
    let spb = 60.0 / p.bpm;
    let comps: HashMap<Id, Arc<crate::comp::CompRt>> = p.comps.iter().map(|cp| (cp.id, Arc::new(crate::comp::build(cp, spb, audition.get(&cp.id).copied(), takes)))).collect();
    let (chans, pats) = build_patterns(p, takes, keys);
    // automation envelopes whose target still exists
    let mut autos = vec![];
    let mut auto_idx = HashMap::new();
    for a in &p.automations {
        if let Some(target) = resolve_target(p, &a.target) {
            auto_idx.insert(a.id, autos.len());
            autos.push(AutoRt { target, env: a.clone() });
        }
    }
    let mut clips = vec![];
    for c in p.clips.iter().filter(|c| !c.muted) {
        if c.track as usize >= p.tracks.len() {
            continue;
        }
        let src = match c.kind {
            ClipKind::Pat => match p.patterns.iter().position(|x| x.id == c.source) {
                Some(i) => ClipSrc::Pat(i),
                None => continue,
            },
            ClipKind::Auto => match auto_idx.get(&c.source) {
                Some(&i) => ClipSrc::Auto(i),
                None => continue,
            },
            ClipKind::Audio => match audio(c) {
                Some(a) => ClipSrc::Audio(a),
                None => continue,
            },
            ClipKind::Comp => match comps.get(&c.source) {
                Some(cp) => ClipSrc::Comp(cp.clone()),
                None => continue,
            },
        };
        clips.push(ClipRt { id: c.id, track: c.track as usize, start: c.start, end: c.start + c.len, offset: c.offset, gain: c.gain, pan: c.pan, fade_in: c.fade_in, fade_out: c.fade_out, src, gain0: c.gain, pan0: c.pan });
    }
    clips.sort_by(|a, b| a.start.total_cmp(&b.start));
    // clip targets point at the sorted list (a muted clip is not in it: its automation rests)
    for a in autos.iter_mut() {
        if let TargetRt::ClipGain(k) | TargetRt::ClipPan(k) = &mut a.target {
            let id = *k as Id;
            *k = clips.iter().position(|c| c.id == id && matches!(c.src, ClipSrc::Audio(_))).unwrap_or(usize::MAX);
        }
    }
    Timeline { bpm: p.bpm, clips, pats, chans, autos, loop_range: p.loop_range.map(|l| (l.a, l.b)), song_end: p.song_end() }
}

/// Turns an automation target into graph indices; `None` if it no longer exists.
fn resolve_target(p: &Project, t: &AutoTarget) -> Option<TargetRt> {
    Some(match t {
        AutoTarget::Fx { fx_id, param } => {
            let (_, f) = p.find_fx(*fx_id)?;
            if let Some(rest) = param.strip_prefix("band:") {
                let mut it = rest.split(':');
                let band: usize = it.next()?.parse().ok()?;
                let field = match it.next()? {
                    "f" => 0,
                    "g" => 1,
                    "q" => 2,
                    _ => return None,
                };
                if band >= MAX_BANDS {
                    return None;
                }
                TargetRt::Band { fx: *fx_id, band, field }
            } else {
                let defs = crate::catalog::params(f.kind);
                let idx = defs.iter().position(|d| d.id == param)?;
                let d = &defs[idx];
                TargetRt::Param { fx: *fx_id, idx, min: d.min as f32, max: d.max as f32, log: d.log }
            }
        }
        AutoTarget::Mix { fx_id } => {
            p.find_fx(*fx_id)?;
            TargetRt::Mix { fx: *fx_id }
        }
        AutoTarget::InsertVol { id } => TargetRt::InsVol(p.inserts.iter().position(|m| m.id == *id)?),
        AutoTarget::InsertPan { id } => TargetRt::InsPan(p.inserts.iter().position(|m| m.id == *id)?),
        AutoTarget::TrackVol { id } => TargetRt::TrackVol(p.tracks.iter().position(|x| x.id == *id)?),
        AutoTarget::ChanVol { id } => TargetRt::ChanVol(p.channels.iter().position(|c| c.id == *id)?),
        AutoTarget::ChanPan { id } => TargetRt::ChanPan(p.channels.iter().position(|c| c.id == *id)?),
        AutoTarget::Synth { chan_id, param } => {
            let c = p.channels.iter().find(|c| c.id == *chan_id)?;
            if !matches!(c.src, ChannelSrc::Synth { .. }) {
                return None;
            }
            let idx = crate::poly::params::index_of(param)?;
            let d = &crate::poly::params::PARAMS[idx];
            TargetRt::Synth { chan: *chan_id, idx, min: d.min as f32, max: d.max as f32, log: d.log, choice: d.options.is_some() }
        }
        AutoTarget::Keys { chan_id, param } => {
            let i = p.channels.iter().position(|c| c.id == *chan_id && matches!(c.src, ChannelSrc::Keys { .. }))?;
            match param.as_str() {
                "release" => TargetRt::KeysRelease(i),
                "dynamics" => TargetRt::KeysDynamics(i),
                _ => return None,
            }
        }
        AutoTarget::Plugin { fx_id, param_id } => {
            p.find_fx(*fx_id).filter(|(_, f)| f.kind == FxKind::Vst3)?;
            TargetRt::PluginParam { fx: *fx_id, param_id: *param_id }
        }
        // the clip id for now: `build_timeline` turns it into the clip's index once sorted
        AutoTarget::ClipGain { id } => TargetRt::ClipGain(p.clips.iter().find(|c| c.id == *id && c.kind == ClipKind::Audio)?.id as usize),
        AutoTarget::ClipPan { id } => TargetRt::ClipPan(p.clips.iter().find(|c| c.id == *id && c.kind == ClipKind::Audio)?.id as usize),
    })
}
