//! The audio thread. Everything here runs inside the device callback (or the offline renderer):
//! no allocation, no blocking locks, no I/O. Structures arrive pre-built through [`Cmd`] and
//! leave through [`Garbage`] so they are freed elsewhere.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

use crate::model::{Id, SamplerState};
use crate::processor::{ParamBlock, ProcessCtx, Processor, METERS};
use crate::sources::Audio;
use crate::poly::{Patch, SynthBank, SynthInst};
use crate::synth::{Dest, VoiceKind, VoicePool};

pub const MAX_INSERTS: usize = 128;
pub const WATCH_SLOTS: usize = 8;
pub const TAP_CAPACITY: usize = 16384;

// ------------------------------------------------------------------ shared state

/// Lock-free numbers the audio thread publishes for the UI.
pub struct Shared {
    beat: AtomicU64,
    /// The beat being heard now (f64 bits): `beat` minus the mixer's playback latency.
    heard: AtomicU64,
    pub playing: AtomicBool,
    pub recording: AtomicBool,
    pub stopped_at_end: AtomicBool,
    in_level: AtomicU32,
    cpu: AtomicU32,
    preview_pos: AtomicU32,
    /// Beats into the previewed pattern (f64 bits); negative when no pattern preview runs.
    pat_beat: AtomicU64,
    /// Per insert *index*: (L, R) peak since last read.
    meters: Vec<(AtomicU32, AtomicU32)>,
    gr: Vec<AtomicU32>,
    fx_meters: Vec<[AtomicU32; METERS]>,
    pub sample_rate: AtomicU32,
    /// Round-trip latency of the audio device in frames (what is heard → when it is
    /// captured), published by the device layer. Recordings are moved earlier by it.
    io_latency: AtomicU32,
    /// Round-trip samples the device layer measured while the current take was capturing
    /// (sum, count): the take is aligned with their mean, not with the estimate at its start.
    take_lat_sum: AtomicU64,
    take_lat_n: AtomicU32,
    /// Beat where input capture began (f64 bits; NaN until the audio thread has started it).
    rec_start: AtomicU64,
}

fn fset(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Relaxed)
}
fn fget(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}
fn fmax(a: &AtomicU32, v: f32) {
    if v > fget(a) {
        fset(a, v)
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            beat: AtomicU64::new(0f64.to_bits()),
            heard: AtomicU64::new(0f64.to_bits()),
            playing: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            stopped_at_end: AtomicBool::new(false),
            in_level: AtomicU32::new(0),
            cpu: AtomicU32::new(0),
            preview_pos: AtomicU32::new((-1f32).to_bits()),
            pat_beat: AtomicU64::new((-1f64).to_bits()),
            meters: (0..MAX_INSERTS).map(|_| (AtomicU32::new(0), AtomicU32::new(0))).collect(),
            gr: (0..WATCH_SLOTS).map(|_| AtomicU32::new(0)).collect(),
            fx_meters: (0..WATCH_SLOTS).map(|_| std::array::from_fn(|_| AtomicU32::new(0))).collect(),
            sample_rate: AtomicU32::new(48000),
            io_latency: AtomicU32::new(0),
            take_lat_sum: AtomicU64::new(0),
            take_lat_n: AtomicU32::new(0),
            rec_start: AtomicU64::new(f64::NAN.to_bits()),
        }
    }
    /// The beat the audio thread is rendering (the timeline position of the next block).
    pub fn beat(&self) -> f64 {
        f64::from_bits(self.beat.load(Relaxed))
    }
    /// The beat coming out of the mixer now: [`Shared::beat`] minus the playback latency
    /// (plugin delay compensation + master chain), never before where playback started.
    /// This is what the playhead shows.
    pub fn heard_beat(&self) -> f64 {
        f64::from_bits(self.heard.load(Relaxed))
    }
    pub fn take_meter(&self, idx: usize) -> (f32, f32) {
        let (l, r) = &self.meters[idx];
        (f32::from_bits(l.swap(0, Relaxed)), f32::from_bits(r.swap(0, Relaxed)))
    }
    pub fn take_in_level(&self) -> f32 {
        f32::from_bits(self.in_level.swap(0, Relaxed))
    }
    pub fn cpu(&self) -> f32 {
        fget(&self.cpu)
    }
    pub fn preview_pos(&self) -> f32 {
        fget(&self.preview_pos)
    }
    /// Beats into the pattern the loop preview plays, or a negative number when it is stopped.
    pub fn pat_beat(&self) -> f64 {
        f64::from_bits(self.pat_beat.load(Relaxed))
    }
    pub fn fx_meters(&self, slot: usize) -> [f32; METERS] {
        std::array::from_fn(|i| fget(&self.fx_meters[slot][i]))
    }
    pub fn gr(&self, slot: usize) -> f32 {
        fget(&self.gr[slot])
    }
    /// Device round trip in frames: output latency + input latency (+ any input backlog).
    pub fn set_io_latency(&self, frames: u32) {
        self.io_latency.store(frames, Relaxed)
    }
    pub fn io_latency(&self) -> u32 {
        self.io_latency.load(Relaxed)
    }
    /// One round trip (frames, calibration offset included) measured by the device layer for
    /// the input consumed in this callback. Kept only while a take is capturing (call it
    /// before [`Rt::process`]); the take is aligned with the mean of these samples, so an
    /// estimate that had not settled when the take started (input just opened) does not
    /// shift it. Lock-free; no allocation.
    pub fn add_take_latency(&self, frames: u32) {
        if self.recording.load(Relaxed) {
            self.take_lat_sum.fetch_add(frames as u64, Relaxed);
            self.take_lat_n.fetch_add(1, Relaxed);
        }
    }
    /// Forgets the samples of the previous take (a new take is about to start).
    pub fn reset_take_latency(&self) {
        self.take_lat_n.store(0, Relaxed);
        self.take_lat_sum.store(0, Relaxed);
    }
    /// Mean round trip measured while the current take captured, if anything was measured.
    pub fn take_latency(&self) -> Option<u32> {
        let n = self.take_lat_n.load(Relaxed);
        (n > 0).then(|| (self.take_lat_sum.load(Relaxed) as f64 / n as f64).round() as u32)
    }
    /// Beat where the audio thread began capturing the current take (NaN = not yet).
    pub fn rec_start(&self) -> f64 {
        f64::from_bits(self.rec_start.load(Relaxed))
    }
    pub(crate) fn set_rec_start(&self, beat: f64) {
        self.rec_start.store(beat.to_bits(), Relaxed)
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ graph

pub struct FxSlot {
    pub id: Id,
    pub bypass: bool,
    /// Dry/wet 0..1.
    pub mix: f32,
    /// `None` = "take the running processor with this id from the current graph".
    pub proc: Option<Box<dyn Processor>>,
    pub params: Option<ParamBlock>,
    /// Delays the dry path by the effect's latency, so dry/wet blends line up.
    dly_l: Vec<f32>,
    dly_r: Vec<f32>,
    dly_w: usize,
}

impl FxSlot {
    /// `latency` in samples (allocates; call off the audio thread).
    pub fn new(id: Id, bypass: bool, mix: f32, proc: Option<Box<dyn Processor>>, params: Option<ParamBlock>, latency: usize) -> Self {
        Self { id, bypass, mix, proc, params, dly_l: vec![0.0; latency], dly_r: vec![0.0; latency], dly_w: 0 }
    }

    fn delay_dry(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = self.dly_l.len();
        for (x, y) in l.iter_mut().zip(r.iter_mut()) {
            let w = self.dly_w;
            *x = std::mem::replace(&mut self.dly_l[w], *x);
            *y = std::mem::replace(&mut self.dly_r[w], *y);
            self.dly_w = if w + 1 == n { 0 } else { w + 1 };
        }
    }
}

pub struct InsertRt {
    pub id: Id,
    pub gain: f32,
    pub pan: f32,
    /// Stereo width (M/S side gain): 0 mono, 1 unchanged, 2 extra wide.
    pub width: f32,
    w_cur: f32,
    pub on: bool,
    pub fx: Vec<FxSlot>,
    g_cur: f32,
    pub(crate) l: Vec<f32>,
    pub(crate) r: Vec<f32>,
    /// Latency compensation: extra delay so this insert lines up with the slowest one.
    pdc_l: Vec<f32>,
    pdc_r: Vec<f32>,
    pdc_w: usize,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    /// Samples the insert's input has been exact digital silence (see [`gate`]).
    quiet: usize,
    /// The chain has been silent longer than its tail and its state is settled: it is skipped.
    parked: bool,
    /// `quiet` at which even the PDC delay line holds only zeros (valid while `parked`).
    idle_at: usize,
    /// Set for the block just processed when the insert contributed nothing.
    idle: bool,
    /// Low-latency monitoring: this insert carries the live input of the armed track. Its
    /// effects with latency are skipped, its PDC delay runs *before* the chain (on the
    /// playback only, see `Graph::mon`) and the live input goes through undelayed.
    low_lat: bool,
}

impl InsertRt {
    pub fn new(id: Id, gain: f32, pan: f32, width: f32, on: bool, fx: Vec<FxSlot>, max_block: usize) -> Self {
        Self { id, gain, pan, width, w_cur: width, on, fx, g_cur: if on { gain } else { 0.0 }, l: vec![0.0; max_block], r: vec![0.0; max_block], pdc_l: vec![], pdc_r: vec![], pdc_w: 0, dry_l: vec![0.0; max_block], dry_r: vec![0.0; max_block], quiet: 0, parked: false, idle_at: usize::MAX, idle: false, low_lat: false }
    }

    /// Delays this insert's output by `samples` (allocates; call off the audio thread).
    pub fn with_delay(mut self, samples: usize) -> Self {
        if samples > 0 {
            self.pdc_l = vec![0.0; samples];
            self.pdc_r = vec![0.0; samples];
        }
        self
    }
}

#[derive(Clone, Copy)]
pub struct TrackRt {
    pub insert: usize,
    pub gain: f32,
    pub on: bool,
}

pub struct Graph {
    pub inserts: Vec<InsertRt>,
    pub tracks: Vec<TrackRt>,
    track_bufs: Vec<(Vec<f32>, Vec<f32>)>,
    /// Track index whose insert receives the live input when monitoring.
    pub monitor_track: Option<usize>,
    /// Pre-chain PDC line of the low-latency monitored insert (`pdc` frames, see `InsertRt::low_lat`).
    mon: (Ring, Ring),
    /// Insert index currently in low-latency monitoring.
    mon_ins: Option<usize>,
    /// Delays the metronome/count-in click by the playback latency, so it is heard with the music.
    click: Ring,
}

impl Graph {
    pub fn new(inserts: Vec<InsertRt>, tracks: Vec<TrackRt>, monitor_track: Option<usize>, max_block: usize) -> Self {
        let track_bufs = tracks.iter().map(|_| (vec![0.0; max_block], vec![0.0; max_block])).collect();
        Self { inserts, tracks, track_bufs, monitor_track, mon: (Ring::default(), Ring::default()), mon_ins: None, click: Ring::default() }
    }

    /// Allocates the latency lines (call off the audio thread): `pdc` = the delay every
    /// non-master insert is compensated to, `master` = the master chain's latency.
    pub fn with_latency(mut self, pdc: usize, master: usize) -> Self {
        self.mon = (Ring::new(pdc), Ring::new(pdc));
        self.click = Ring::new(pdc + master);
        self
    }
}

/// A fixed delay line; an empty one passes audio through.
#[derive(Default)]
struct Ring {
    buf: Vec<f32>,
    w: usize,
}

impl Ring {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len], w: 0 }
    }
    fn run(&mut self, x: &mut [f32]) {
        let n = self.buf.len();
        if n == 0 {
            return;
        }
        for v in x {
            let w = self.w;
            *v = std::mem::replace(&mut self.buf[w], *v);
            self.w = if w + 1 == n { 0 } else { w + 1 };
        }
    }
    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
    /// Takes over what `old` still has to play (see [`carry_line`]).
    fn carry(&mut self, old: &mut Ring) {
        carry_line(&mut self.buf, &mut self.w, &mut old.buf, old.w);
    }
}

/// Moves the samples a delay line (`src`, next read at `src_w`) still has to play into a new
/// line `dst` (fresh, all zeros, write position `*dst_w` = 0). Same length: the buffers are
/// swapped (O(1)). Shorter: the newest samples are kept (the oldest are skipped). Longer: the
/// old samples come after a gap of the extra delay. Runs on the audio thread: no allocation.
fn carry_line(dst: &mut Vec<f32>, dst_w: &mut usize, src: &mut Vec<f32>, src_w: usize) {
    let (dn, d) = (dst.len(), src.len());
    if dn == 0 || d == 0 {
        return;
    }
    if dn == d {
        std::mem::swap(dst, src);
        *dst_w = src_w;
        return;
    }
    *dst_w = 0;
    if dn < d {
        let start = (src_w + d - dn) % d;
        let first = (d - start).min(dn);
        dst[..first].copy_from_slice(&src[start..start + first]);
        dst[first..].copy_from_slice(&src[..dn - first]);
    } else {
        let pad = dn - d;
        let first = d - src_w;
        dst[pad..pad + first].copy_from_slice(&src[src_w..]);
        dst[pad + first..].copy_from_slice(&src[..src_w]);
    }
}

/// Carries an insert's PDC lines over a graph swap. False when there was nothing to carry
/// (no line, a line known to hold only zeros, or the line was off for low-latency monitoring).
fn carry_pdc(new: &mut InsertRt, old: &mut InsertRt) -> bool {
    if new.pdc_l.is_empty() || old.pdc_l.is_empty() || old.low_lat || (old.parked && old.quiet >= old.idle_at) {
        return false;
    }
    let mut w = 0;
    carry_line(&mut new.pdc_l, &mut w, &mut old.pdc_l, old.pdc_w);
    carry_line(&mut new.pdc_r, &mut new.pdc_w, &mut old.pdc_r, old.pdc_w);
    true
}

/// Fader/pan/mute for everything, sent when only those changed.
pub struct MixState {
    pub inserts: Vec<(Id, f32, f32, f32, bool)>,
    pub tracks: Vec<(f32, bool)>,
    pub monitor_track: Option<usize>,
    /// Channel (gain, pan, on), in the timeline's channel order.
    pub chans: Vec<(f32, f32, bool)>,
}

// ------------------------------------------------------------------ timeline

pub enum ClipSrc {
    Pat(usize),
    Audio(Arc<Audio>),
    Auto(usize),
    /// A comp, resolved off-thread (shared by every clip that plays it).
    Comp(Arc<crate::comp::CompRt>),
}

/// A resolved automation target (indices into the graph built from the same project).
#[derive(Clone, Copy, Debug)]
pub enum TargetRt {
    Param { fx: Id, idx: usize, min: f32, max: f32, log: bool },
    /// EQ band field: 0 freq, 1 gain, 2 Q.
    Band { fx: Id, band: usize, field: u8 },
    Mix { fx: Id },
    InsVol(usize),
    InsPan(usize),
    TrackVol(usize),
    /// Channel Rack channel (index into `Timeline::chans`): fader position, pan.
    ChanVol(usize),
    ChanPan(usize),
    /// A synth channel's patch parameter (`poly::params` index), glided by the synth.
    Synth { chan: Id, idx: usize, min: f32, max: f32, log: bool, choice: bool },
    /// A hosted plugin parameter (`fx` is the effect id, `param_id` the
    /// plugin's own parameter id); the envelope value goes through as the
    /// plugin's normalized 0..1.
    PluginParam { fx: Id, param_id: u32 },
    /// WAVE Keys channel (index into `Timeline::chans`): release time, dynamics.
    KeysRelease(usize),
    KeysDynamics(usize),
    /// An audio clip (index into `Timeline::clips`; `usize::MAX` while it does not play).
    ClipGain(usize),
    ClipPan(usize),
}

pub struct AutoRt {
    pub target: TargetRt,
    pub env: crate::model::Automation,
}

pub struct ClipRt {
    pub id: Id,
    pub track: usize,
    pub start: f64,
    pub end: f64,
    pub offset: f64,
    pub gain: f32,
    pub pan: f32,
    pub fade_in: f32,
    pub fade_out: f32,
    pub src: ClipSrc,
    /// `gain` (dB) and `pan` as the previous block played them: an automated change glides
    /// across the block instead of stepping (a zipper at every block edge).
    pub gain0: f32,
    pub pan0: f32,
}

/// A pattern note, ready to schedule: swing applied, channel resolved to an index.
pub struct NoteRt {
    pub beat: f64,
    pub len: f64,
    pub chan: usize,
    pub key: u8,
    pub vel: f32,
}

pub struct PatRt {
    pub id: Id,
    pub len: f64,
    /// Only the notes that start before `len` (later ones are kept in the project, hidden).
    pub notes: Vec<NoteRt>,
    /// Insert index of the track of the first clip that plays this pattern (where the
    /// loop preview and auditions of track-routed channels go).
    pub home: Option<usize>,
}

/// A sampler channel's settings, resolved for the audio thread (see `SamplerState`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmpRt {
    /// The part that plays, as fractions of the file.
    pub start: f32,
    pub end: f32,
    pub reverse: bool,
    /// Playback-rate factor of the transpose and fine tune.
    pub rate: f64,
    /// Level of "normalise" (1 when off).
    pub gain: f32,
    pub gate: bool,
    /// Seconds.
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    pub cut: u8,
}

impl Default for SmpRt {
    fn default() -> Self {
        Self::new(&SamplerState::default(), None)
    }
}

impl SmpRt {
    /// `peak_db`: the file's peak, for "normalise".
    pub fn new(s: &SamplerState, peak_db: Option<f32>) -> Self {
        let gain = match peak_db {
            Some(pk) if s.norm && pk.is_finite() => crate::dsp::db_to_gain(-0.3 - pk),
            _ => 1.0,
        };
        Self {
            start: s.start.clamp(0.0, 1.0),
            end: s.end.clamp(0.0, 1.0),
            reverse: s.reverse,
            rate: 2f64.powf((s.semi as f64 + s.fine as f64 / 100.0) / 12.0),
            gain,
            gate: s.gate,
            attack: s.attack.max(0.0) / 1000.0,
            decay: s.decay.max(0.0) / 1000.0,
            sustain: s.sustain.clamp(0.0, 1.0),
            release: s.release.max(1.0) / 1000.0,
            cut: s.cut,
        }
    }
}

/// A Channel Rack channel, resolved for the audio thread.
pub struct ChanRt {
    pub id: Id,
    pub kind: VoiceKind,
    /// Sampler channels: the audio (`None` when the file is missing: the channel is silent)
    /// and how it is played.
    pub sample: Option<Arc<Audio>>,
    pub smp: SmpRt,
    /// WAVE Keys channels: the piano (`None` until it is loaded: the channel is silent),
    /// the release time in seconds and the velocity → level amount 0..1.
    pub keys: Option<Arc<crate::keys::KeysBank>>,
    pub keys_release: f32,
    pub keys_dynamics: f32,
    pub root: u8,
    /// Channel gain (1 at unity), pan −1..1, and audible (mute/solo).
    pub gain: f32,
    pub pan: f32,
    /// Gain and pan as the previous block played them (voices glide from these to `gain`/`pan`).
    pub gain0: f32,
    pub pan0: f32,
    pub on: bool,
    /// Insert index the channel plays into; `None` = the clip's track.
    pub insert: Option<usize>,
    /// Insert of the track that plays a pattern with this channel's notes (where the song
    /// hears it), if any.
    pub heard: Option<usize>,
    /// Where auditions go when nothing else decides: `insert`, else the insert of a track
    /// that plays a pattern with this channel's notes, else the master.
    pub home: usize,
}

pub struct Timeline {
    pub bpm: f64,
    pub clips: Vec<ClipRt>,
    pub pats: Vec<PatRt>,
    pub chans: Vec<ChanRt>,
    pub autos: Vec<AutoRt>,
    pub loop_range: Option<(f64, f64)>,
    pub song_end: f64,
}

pub struct Preview {
    pub audio: Arc<Audio>,
    pub pos: f64,
    pub end: f64,
    pub gain: f32,
    pub fade_in: f64,
    pub fade_out: f64,
    pub start: f64,
}

// ------------------------------------------------------------------ messages

pub enum Cmd {
    Graph(Box<Graph>),
    Timeline(Box<Timeline>),
    Mix(Box<MixState>),
    Params { fx: Id, params: Option<ParamBlock>, mix: f32 },
    /// `rec_loop`: record in passes over the loop range (one take per pass).
    Play { from: f64, count_in: f64, rec: bool, rec_loop: bool },
    /// Stops capturing input but keeps the transport running.
    PunchOut,
    /// Starts capturing input at the current beat while playing. `rec_loop`: wrap at the
    /// loop end in passes (one take per pass), like loop recording.
    PunchIn { rec_loop: bool },
    Stop,
    Flags { metro: bool, monitor: bool },
    Watch { ids: [Id; WATCH_SLOTS], n: u8 },
    Preview(Option<Box<Preview>>),
    /// Play one note of a channel now (step clicks, the piano-roll keyboard). `pattern`
    /// decides where a track-routed channel is heard (that pattern's track).
    Audition { chan: Id, key: u8, vel: f32, len: f32, pattern: Option<Id> },
    /// Loop one pattern on its own (`None` stops). Exclusive with the song transport.
    PatPlay(Option<Id>),
    /// Live MIDI key down/up on a channel. It plays where the song plays the channel (its own
    /// insert, else the track of its pattern); `ins` (the armed track's insert) only for a
    /// channel the song does not play yet, else the master.
    MidiNote { chan: Id, key: u8, vel: f32, on: bool, ins: Option<usize> },
    /// Live MIDI sustain pedal.
    MidiSustain(bool),
    /// Live MIDI pitch bend in semitones.
    MidiBend(f32),
    /// Release every live note and the pedal.
    MidiOff,
    /// Live MIDI mod wheel (CC 1), 0..1.
    MidiMod(f32),
    /// A synth channel's instance, made off the audio thread (replaces one of the same channel).
    SynthAdd(Box<SynthInst>),
    /// New parameter values of a synth channel.
    SynthPatch { chan: Id, patch: Box<Patch> },
    /// A synth channel is gone.
    SynthRemove(Id),
}

pub enum Garbage {
    Graph(Box<Graph>),
    Timeline(Box<Timeline>),
    Mix(Box<MixState>),
    Preview(Box<Preview>),
    Synth(Box<SynthInst>),
    Patch(Box<Patch>),
}

pub struct Taps {
    pub input: Vec<rtrb::Producer<f32>>,
    pub output: Vec<rtrb::Producer<f32>>,
}

// ------------------------------------------------------------------ the engine

pub struct Rt {
    sr: f32,
    max_block: usize,
    cmds: rtrb::Consumer<Cmd>,
    garbage: rtrb::Producer<Garbage>,
    shared: Arc<Shared>,
    graph: Box<Graph>,
    timeline: Box<Timeline>,
    voices: VoicePool,
    /// Synth channel instances.
    synths: SynthBank,
    click: Vec<f32>,
    tmp_l: Vec<f32>,
    tmp_r: Vec<f32>,
    comp_l: Vec<f32>,
    comp_r: Vec<f32>,
    playing: bool,
    beat: f64,
    rec: bool,
    rec_loop: bool,
    rec_start: f64,
    count_end: f64,
    /// Where this playback run began (count-in included) and the loop it last wrapped
    /// around: the heard position is clamped to / wrapped by them.
    run_start: f64,
    wrapped: Option<(f64, f64)>,
    metro: bool,
    monitor: bool,
    watch: [Id; WATCH_SLOTS],
    watch_n: usize,
    taps: Taps,
    record: rtrb::Producer<f32>,
    preview: Option<Box<Preview>>,
    /// Pattern loop preview: pattern id and beats into it.
    pat_play: Option<Id>,
    pat_beat: f64,
    /// Skip effect chains that are silent past their tails (see [`gate`]).
    skip_silent: bool,
    /// Live MIDI events (see `Engine::midi_event`).
    midi: Option<rtrb::Consumer<Cmd>>,
    /// Worker threads for the insert chains (offline renders only; 0/1 = this thread).
    workers: usize,
    /// Instant start (live streams, see [`Rt::set_instant_start`]): the share of a callback
    /// period the audio thread may fill while pre-rolling, `None` = off (offline renders
    /// drop the latency themselves).
    instant: Option<f32>,
    /// Frames still to render muted before the music at the start position is heard: the
    /// playback latency at the last Play, rendered ahead of the device clock (pre-roll).
    preroll: usize,
    /// Seconds one frame of pre-roll took at most (budget estimate), 0 = not measured yet.
    pre_cost: f64,
    /// Scratch output of pre-roll blocks.
    pre_l: Vec<f32>,
    pre_r: Vec<f32>,
    /// Declick of a transport jump: the last frame heard before it ramps to zero over
    /// [`DECLICK_S`] while the new music fades in.
    last: (f32, f32),
    dc: (f32, f32),
    dc_pos: usize,
    fade_pos: usize,
}

/// Declick ramp of a transport jump while sound is coming out.
const DECLICK_S: f32 = 0.003;

impl Rt {
    pub fn new(sr: f32, max_block: usize, cmds: rtrb::Consumer<Cmd>, garbage: rtrb::Producer<Garbage>, shared: Arc<Shared>, taps: Taps, record: rtrb::Producer<f32>) -> Self {
        shared.sample_rate.store(sr as u32, Relaxed);
        Self {
            sr,
            max_block,
            cmds,
            garbage,
            shared,
            graph: Box::new(Graph::new(vec![], vec![], None, max_block)),
            timeline: Box::new(Timeline { bpm: 120.0, clips: vec![], pats: vec![], chans: vec![], autos: vec![], loop_range: None, song_end: 4.0 }),
            voices: VoicePool::new(sr),
            synths: SynthBank::default(),
            click: vec![0.0; max_block],
            tmp_l: vec![0.0; max_block],
            tmp_r: vec![0.0; max_block],
            comp_l: vec![0.0; max_block],
            comp_r: vec![0.0; max_block],
            playing: false,
            beat: 0.0,
            rec: false,
            rec_loop: false,
            rec_start: 0.0,
            count_end: f64::NEG_INFINITY,
            run_start: 0.0,
            wrapped: None,
            metro: false,
            monitor: false,
            watch: [u64::MAX; WATCH_SLOTS],
            watch_n: 0,
            taps,
            record,
            preview: None,
            pat_play: None,
            pat_beat: 0.0,
            skip_silent: true,
            midi: None,
            workers: 0,
            instant: None,
            preroll: 0,
            pre_cost: 0.0,
            pre_l: vec![0.0; max_block],
            pre_r: vec![0.0; max_block],
            last: (0.0, 0.0),
            dc: (0.0, 0.0),
            dc_pos: usize::MAX,
            fade_pos: usize::MAX,
        }
    }

    /// Instant start for a live stream: after Play (a start, a seek, a jump of the start
    /// marker) the playback latency (plugin delay compensation: up to seconds with a
    /// look-ahead Rider) is rendered muted ahead of the device clock, as fast as the
    /// callback deadline allows, so the music at the start position is heard at once
    /// instead of after the latency, still on the timeline. `budget` is the share of each
    /// callback period the audio thread may spend in total while pre-rolling (its own
    /// frames included); `f32::INFINITY` renders the whole pre-roll in the first callback
    /// (tests). `None` (the default, offline renders) keeps silence for the latency.
    ///
    /// Bound: with a mix costing `c` of real time, a pre-roll of `L` seconds is heard after
    /// about `L · c / budget` seconds, plus at most one callback.
    pub fn set_instant_start(&mut self, budget: Option<f32>) {
        self.instant = budget;
    }

    /// Frames of the latency still being pre-rolled (0 = the music is heard).
    pub fn preroll(&self) -> usize {
        self.preroll
    }

    /// Starts the pre-roll at a transport jump. Not while recording: a take's alignment
    /// assumes the delayed start (`RecBuf::skip` drops the playback latency).
    fn jump(&mut self) {
        if self.instant.is_none() || self.rec {
            return;
        }
        self.preroll = self.graph.click.buf.len();
        if self.last.0.abs() > 1e-6 || self.last.1.abs() > 1e-6 {
            // sound was coming out: ramp it away and fade the new music in
            self.dc = self.last;
            self.dc_pos = 0;
            self.fade_pos = 0;
        }
    }

    /// Lets this Rt process insert chains on `n` worker threads. Never for a live stream:
    /// it spawns threads and allocates. Output is identical to the single-threaded one.
    pub fn set_workers(&mut self, n: usize) {
        self.workers = n;
    }

    /// The ring live keyboard events arrive on, drained together with the command ring.
    pub fn set_midi_ring(&mut self, ring: rtrb::Consumer<Cmd>) {
        self.midi = Some(ring);
    }

    /// Inserts whose chain is parked right now (silent past its tails).
    pub fn parked_inserts(&self) -> usize {
        self.graph.inserts.iter().filter(|i| i.parked).count()
    }

    /// Turns silence skipping on (the default) or off. Output is the same either way (to
    /// well below -120 dB); off is for A/B tests.
    pub fn set_skip_silent(&mut self, on: bool) {
        self.skip_silent = on;
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }
    pub fn max_block(&self) -> usize {
        self.max_block
    }

    fn trash(&mut self, g: Garbage) {
        // The ring is sized generously; if it is ever full we leak rather than free here.
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            std::mem::forget(g);
        }
    }

    fn drain(&mut self) {
        while let Ok(cmd) = self.cmds.pop() {
            self.apply(cmd);
        }
        // live keyboard events arrive on their own ring (the UI side never waits for a lock)
        if let Some(mut ring) = self.midi.take() {
            while let Ok(cmd) = ring.pop() {
                self.apply(cmd);
            }
            self.midi = Some(ring);
        }
    }

    fn apply(&mut self, cmd: Cmd) {
        {
            match cmd {
                Cmd::Graph(mut g) => {
                    // carry running processors (and their tails) over by effect id
                    for ins in g.inserts.iter_mut() {
                        // silence bookkeeping follows the processors that are carried over;
                        // an insert made only of fresh processors starts settled
                        let (mut carried, mut quiet, mut parked) = (false, usize::MAX, true);
                        for slot in ins.fx.iter_mut() {
                            let found = self.graph.inserts.iter_mut().find_map(|i| {
                                let st = (i.quiet, i.parked);
                                i.fx.iter_mut().find(|s| s.id == slot.id).map(|s| (st, s))
                            });
                            if let Some((st, old)) = found {
                                let mut took = false;
                                if slot.proc.is_none() {
                                    slot.proc = old.proc.take();
                                    took = true;
                                }
                                if old.dly_l.len() == slot.dly_l.len() {
                                    std::mem::swap(&mut slot.dly_l, &mut old.dly_l);
                                    std::mem::swap(&mut slot.dly_r, &mut old.dly_r);
                                    slot.dly_w = old.dly_w;
                                    took = took || !slot.dly_l.is_empty();
                                }
                                if took {
                                    carried = true;
                                    quiet = quiet.min(st.0);
                                    parked &= st.1;
                                }
                            }
                            if let (Some(p), Some(pb)) = (slot.proc.as_mut(), slot.params.as_ref()) {
                                p.set_params(pb);
                            }
                        }
                        // the audio in flight in the PDC line goes on playing: rebuilding the graph
                        // (an effect added or bypassed anywhere) must not drop up to the whole
                        // compensation delay (seconds with a long look-ahead Rider) on every insert
                        let mut pdc_carried = false;
                        if let Some(old) = self.graph.inserts.iter_mut().find(|i| i.id == ins.id) {
                            ins.g_cur = old.g_cur;
                            pdc_carried = carry_pdc(ins, old);
                        }
                        (ins.quiet, ins.parked) = if carried { (quiet, parked) } else { (usize::MAX / 2, true) };
                        // a parked chain is idle once its PDC line has played out (at once if it is empty)
                        ins.idle_at = if pdc_carried { ins.quiet.saturating_add(ins.pdc_l.len()) } else { 0 };
                    }
                    g.click.carry(&mut self.graph.click);
                    let old = std::mem::replace(&mut self.graph, g);
                    self.trash(Garbage::Graph(old));
                }
                Cmd::Timeline(mut t) => {
                    self.voices.remap(&t.chans);
                    carry_automated(&mut t, &self.timeline);
                    let old = std::mem::replace(&mut self.timeline, t);
                    self.trash(Garbage::Timeline(old));
                }
                Cmd::Mix(m) => {
                    for (id, gain, pan, width, on) in m.inserts.iter().copied() {
                        if let Some(i) = self.graph.inserts.iter_mut().find(|i| i.id == id) {
                            i.gain = gain;
                            i.pan = pan;
                            i.width = width;
                            i.on = on;
                        }
                    }
                    for (t, &(gain, on)) in self.graph.tracks.iter_mut().zip(m.tracks.iter()) {
                        t.gain = gain;
                        t.on = on;
                    }
                    self.graph.monitor_track = m.monitor_track;
                    if m.chans.len() == self.timeline.chans.len() {
                        for (c, &(gain, pan, on)) in self.timeline.chans.iter_mut().zip(m.chans.iter()) {
                            c.gain = gain;
                            c.pan = pan;
                            c.on = on;
                        }
                    }
                    self.trash(Garbage::Mix(m));
                }
                Cmd::Params { fx, params, mix } => {
                    if let Some(s) = self.graph.inserts.iter_mut().flat_map(|i| i.fx.iter_mut()).find(|s| s.id == fx) {
                        s.mix = mix;
                        if let Some(params) = params {
                            s.params = Some(params);
                            if let Some(p) = s.proc.as_mut() {
                                p.set_params(&params);
                            }
                        }
                    }
                }
                Cmd::Play { from, count_in, rec, rec_loop } => {
                    self.voices.clear();
                    self.synths.stop_all();
                    // the song and the pattern preview are exclusive (FL's SONG / PAT)
                    self.set_pat_play(None);
                    self.beat = from - count_in;
                    self.count_end = if count_in > 0.0 { from } else { f64::NEG_INFINITY };
                    self.run_start = self.beat;
                    self.wrapped = None;
                    self.rec = rec;
                    self.rec_loop = rec && rec_loop;
                    self.rec_start = from;
                    if rec {
                        self.shared.set_rec_start(from);
                    }
                    self.playing = true;
                    self.shared.playing.store(true, Relaxed);
                    self.shared.stopped_at_end.store(false, Relaxed);
                    self.jump();
                }
                Cmd::PunchOut => {
                    // `rec` stays set so the run keeps its open-ended extent instead of
                    // wrapping at a song end that the new take has not extended yet
                    self.rec_start = f64::INFINITY;
                    self.shared.recording.store(false, Relaxed);
                }
                Cmd::PunchIn { rec_loop } => {
                    if self.playing {
                        self.rec = true;
                        self.rec_loop = rec_loop;
                        self.rec_start = self.beat;
                        self.shared.set_rec_start(self.beat);
                    }
                }
                Cmd::Stop => self.stop(false),
                Cmd::Flags { metro, monitor } => {
                    self.metro = metro;
                    self.monitor = monitor;
                }
                Cmd::Watch { ids, n } => {
                    self.watch = ids;
                    self.watch_n = n as usize;
                }
                Cmd::Preview(p) => {
                    if let Some(old) = std::mem::replace(&mut self.preview, p) {
                        self.trash(Garbage::Preview(old));
                    }
                    if self.preview.is_none() {
                        fset(&self.shared.preview_pos, -1.0);
                    }
                }
                Cmd::Audition { chan, key, vel, len, pattern } => {
                    let tl = &*self.timeline;
                    if let Some(i) = tl.chans.iter().position(|c| c.id == chan) {
                        let ch = &tl.chans[i];
                        let home = pattern.and_then(|id| tl.pats.iter().find(|p| p.id == id)).and_then(|p| p.home);
                        let ins = ch.insert.or(home).unwrap_or(ch.home);
                        play(&mut self.voices, &mut self.synths, ch, i, Dest::Insert(ins), 0, len, key, vel, 1.0, self.sr);
                    }
                }
                Cmd::MidiNote { chan, key, vel, on, ins } => {
                    let tl = &*self.timeline;
                    if on {
                        if let Some(i) = tl.chans.iter().position(|c| c.id == chan) {
                            let ch = &tl.chans[i];
                            // never through an unrelated armed track: a vocal chain (compressor, pitch
                            // correction) mangles chords that sound right in the piano roll
                            let ins = ch.insert.or(ch.heard).or(ins).unwrap_or(ch.home);
                            if ch.kind == VoiceKind::Synth {
                                if let Some(s) = self.synths.get(chan) {
                                    s.key_down(key, vel, Dest::Insert(ins));
                                }
                            } else {
                                self.voices.note_on(ch, i, Dest::Insert(ins), key, vel);
                            }
                        }
                    } else {
                        self.voices.note_off(chan, key);
                        self.synths.key_up(chan, key);
                    }
                }
                Cmd::MidiSustain(down) => {
                    self.voices.sustain(down);
                    self.synths.sustain(down);
                }
                Cmd::MidiBend(st) => {
                    self.voices.set_bend(st);
                    self.synths.set_bend(st);
                }
                Cmd::MidiOff => {
                    self.voices.all_notes_off();
                    self.synths.all_notes_off();
                }
                Cmd::MidiMod(v) => self.synths.set_wheel(v),
                Cmd::SynthAdd(s) => {
                    if let Some(old) = self.synths.add(s) {
                        self.trash(Garbage::Synth(old));
                    }
                }
                Cmd::SynthPatch { chan, patch } => {
                    if let Some(s) = self.synths.get(chan) {
                        s.set_patch(&patch);
                    }
                    self.trash(Garbage::Patch(patch));
                }
                Cmd::SynthRemove(id) => {
                    if let Some(old) = self.synths.remove(id) {
                        self.trash(Garbage::Synth(old));
                    }
                }
                Cmd::PatPlay(id) => {
                    if id.is_some() && self.playing {
                        self.stop(false);
                    }
                    self.set_pat_play(id);
                    if id.is_some() {
                        self.jump();
                    }
                }
            }
        }
    }

    fn set_pat_play(&mut self, id: Option<Id>) {
        self.pat_play = id;
        self.pat_beat = 0.0;
        self.shared.pat_beat.store((if id.is_some() { 0f64 } else { -1f64 }).to_bits(), Relaxed);
    }

    /// The pattern loop preview: schedules the notes of `[pat_beat, +n samples)` and wraps.
    fn schedule_pattern(&mut self, n: usize, bps: f64, spb: f64) {
        let Some(pid) = self.pat_play else { return };
        let tl = &*self.timeline;
        let Some(pat) = tl.pats.iter().find(|p| p.id == pid) else {
            self.set_pat_play(None);
            return;
        };
        if pat.len <= 0.0 {
            return;
        }
        let b0 = self.pat_beat;
        let b1 = b0 + n as f64 * bps;
        let (r0, r1) = ((b0 / pat.len).floor() as i64, (b1 / pat.len).floor() as i64);
        for r in r0..=r1 {
            for nt in &pat.notes {
                let nb = r as f64 * pat.len + nt.beat;
                if nb < b0 || nb >= b1 {
                    continue;
                }
                let Some(ch) = tl.chans.get(nt.chan) else { continue };
                if !ch.on {
                    continue;
                }
                let ins = ch.insert.or(pat.home).unwrap_or(ch.home);
                let delay = ((nb - b0) / bps) as u32;
                play(&mut self.voices, &mut self.synths, ch, nt.chan, Dest::Insert(ins), delay, (nt.len * spb) as f32, nt.key, nt.vel, 1.0, self.sr);
            }
        }
        self.pat_beat = b1.rem_euclid(pat.len);
        self.shared.pat_beat.store(self.pat_beat.to_bits(), Relaxed);
    }

    fn stop(&mut self, at_end: bool) {
        self.playing = false;
        self.rec = false;
        self.rec_loop = false;
        self.shared.playing.store(false, Relaxed);
        self.shared.recording.store(false, Relaxed);
        if at_end {
            self.shared.stopped_at_end.store(true, Relaxed);
        }
        // tails of instruments keep ringing; effect tails too
    }

    /// Renders `out_l.len()` frames. `input` is the device input for the same period, if any.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32], input: Option<(&[f32], &[f32])>) {
        let t0 = Instant::now();
        // device callback and offline render alike: decaying filters must not crawl through denormals
        let _no_denormals = crate::processor::NoDenormals::new();
        self.drain();
        let n = out_l.len();
        let period = n as f32 / self.sr;
        let pre = self.pre_roll(t0, n, period as f64);
        let mut pos = 0;
        while pos < n {
            let m = (n - pos).min(self.max_block);
            let inp = input.map(|(l, r)| (&l[pos.min(l.len())..(pos + m).min(l.len())], &r[pos.min(r.len())..(pos + m).min(r.len())]));
            let mute = self.preroll.min(m);
            self.block(pos, m, out_l, out_r, inp, mute, false);
            self.preroll -= mute;
            pos += m;
        }
        // the pre-roll is not this callback's load (the meter would only show its spike)
        let load = (t0.elapsed().as_secs_f32() - pre).max(0.0) / period.max(1e-6);
        let prev = fget(&self.shared.cpu);
        fset(&self.shared.cpu, if load > prev { load } else { prev * 0.95 + load * 0.05 });
    }

    /// Renders pre-roll frames before this callback's own `n` while the deadline allows:
    /// another block only if it and the callback's own frames still fit the budget at the
    /// slowest per-frame cost seen in this pre-roll (bounded: at most one block over the
    /// estimate). Allocation-free; the blocks go to scratch and are not heard. Returns the
    /// seconds spent.
    fn pre_roll(&mut self, t0: Instant, n: usize, period: f64) -> f32 {
        let Some(budget) = self.instant else {
            return 0.0;
        };
        if self.preroll == 0 {
            return 0.0;
        }
        let tp = Instant::now();
        let limit = period * budget as f64;
        // the first block is small: it measures what a frame costs
        let full = n.min(self.max_block);
        let mut m = if self.pre_cost > 0.0 || !limit.is_finite() { full } else { 64.min(full) };
        while self.preroll > 0 {
            let m_now = m.min(self.preroll);
            if limit.is_finite() && t0.elapsed().as_secs_f64() + self.pre_cost * (m_now + n) as f64 > limit {
                break;
            }
            let tb = Instant::now();
            let (mut l, mut r) = (std::mem::take(&mut self.pre_l), std::mem::take(&mut self.pre_r));
            self.block(0, m_now, &mut l, &mut r, None, m_now, true);
            (self.pre_l, self.pre_r) = (l, r);
            self.pre_cost = self.pre_cost.max(tb.elapsed().as_secs_f64() / m_now as f64);
            self.preroll -= m_now;
            m = full;
        }
        if self.preroll == 0 {
            self.pre_cost = 0.0; // the next pre-roll measures afresh
        }
        tp.elapsed().as_secs_f32()
    }

    /// One block of `n` frames into `out_*[at..]`. The first `mute` frames of the music are
    /// not heard (pre-roll); `ahead`: a pre-roll block rendered ahead of the device clock
    /// (no device input, no preview: those follow the real clock).
    #[allow(clippy::too_many_arguments)]
    fn block(&mut self, at: usize, n: usize, out_l: &mut [f32], out_r: &mut [f32], input: Option<(&[f32], &[f32])>, mute: usize, ahead: bool) {
        let sr = self.sr;
        for (l, r) in self.graph.track_bufs.iter_mut() {
            l[..n].fill(0.0);
            r[..n].fill(0.0);
        }
        for ins in self.graph.inserts.iter_mut() {
            ins.l[..n].fill(0.0);
            ins.r[..n].fill(0.0);
        }
        self.click[..n].fill(0.0);

        let bpm = self.timeline.bpm;
        let spb = 60.0 / bpm; // seconds per beat
        let bps = 1.0 / (spb * sr as f64); // beats per sample
        let block_beat = self.beat;

        // ---- schedule & render the timeline
        if self.playing {
            let mut p = 0usize;
            let mut guard = 0;
            while p < n && self.playing && guard < 8 {
                guard += 1;
                let (ls, le) = if self.rec {
                    match self.timeline.loop_range {
                        Some(lr) if self.rec_loop && lr.1 > lr.0 => lr,
                        _ => (0.0, crate::model::TOTAL_BEATS),
                    }
                } else {
                    self.timeline.loop_range.unwrap_or((0.0, self.timeline.song_end))
                };
                let left = ((le - self.beat) / bps).ceil().max(1.0) as usize;
                let seg = left.min(n - p);
                let b0 = self.beat;
                let b1 = b0 + seg as f64 * bps;
                self.schedule(b0, b1, p, seg, bps, spb);
                // record input
                if self.rec && b1 > self.rec_start {
                    self.shared.recording.store(true, Relaxed);
                    if let Some((il, ir)) = input {
                        let first = if b0 >= self.rec_start { 0 } else { (((self.rec_start - b0) / bps).ceil() as usize).min(seg) };
                        for i in p + first..(p + seg).min(il.len()) {
                            if self.record.slots() >= 2 {
                                // NaN is the pass marker (below): never let it in from the device
                                let fin = |x: f32| if x.is_finite() { x } else { 0.0 };
                                let _ = self.record.push(fin(il[i]));
                                let _ = self.record.push(fin(ir[i]));
                            }
                        }
                    }
                }
                self.beat = b1;
                p += seg;
                if self.beat >= le - 1e-9 {
                    if self.rec && !self.rec_loop {
                        self.stop(true);
                    } else {
                        self.beat = ls;
                        self.wrapped = Some((ls, le));
                        // loop recording: a NaN frame tells the engine a new pass (take) begins
                        if self.rec && self.rec_start.is_finite() && self.record.slots() >= 2 {
                            let _ = self.record.push(f32::NAN);
                            let _ = self.record.push(f32::NAN);
                        }
                    }
                }
            }
        }
        // the clips of this block are mixed: the next one glides from what they played
        for c in self.timeline.clips.iter_mut() {
            c.gain0 = c.gain;
            c.pan0 = c.pan;
        }
        if self.playing {
            apply_automation(&mut self.timeline, &mut self.graph, &mut self.synths, block_beat);
        }
        let shared_beat = if self.playing { self.beat } else { block_beat };
        self.shared.beat.store(shared_beat.to_bits(), Relaxed);
        let heard = if self.playing { self.heard(shared_beat, bps) } else { shared_beat };
        self.shared.heard.store(heard.to_bits(), Relaxed);
        // tempo-locked effects follow the pattern loop preview like the song
        let pat_ctx = if !self.playing && self.pat_play.is_some() { Some(self.pat_beat) } else { None };
        self.schedule_pattern(n, bps, spb);
        self.voices.render(n, &mut self.graph.track_bufs, &mut self.graph.inserts, &mut self.click[..n], &self.timeline.chans);
        self.synths.render(n, &mut self.graph.track_bufs, &mut self.graph.inserts, &self.timeline.chans, bpm);
        // what this block played is where the next one glides from
        for c in self.timeline.chans.iter_mut() {
            c.gain0 = c.gain;
            c.pan0 = c.pan;
        }

        // ---- input level + monitoring
        // low-latency monitoring: the armed track's own insert (not the master) takes the
        // live input past its PDC delay and its look-ahead effects (see `InsertRt::low_lat`)
        let mon_ins = match input {
            // a pre-roll block has no device input: the monitoring set-up stays as it is
            _ if ahead => self.graph.mon_ins,
            Some(_) if self.monitor => self.graph.monitor_track.and_then(|t| self.graph.tracks.get(t)).map(|t| t.insert).filter(|&k| k > 0 && k < self.graph.inserts.len()),
            _ => None,
        };
        if mon_ins != self.graph.mon_ins {
            // the delay lines change place: start them empty (a short gap, never stale audio)
            let g = &mut *self.graph;
            for k in [g.mon_ins, mon_ins].into_iter().flatten() {
                if let Some(ins) = g.inserts.get_mut(k) {
                    ins.pdc_l.fill(0.0);
                    ins.pdc_r.fill(0.0);
                    ins.low_lat = mon_ins == Some(k);
                }
            }
            g.mon.0.clear();
            g.mon.1.clear();
            g.mon_ins = mon_ins;
        }
        if let Some((il, ir)) = input {
            let pk = il.iter().chain(ir.iter()).fold(0f32, |a, b| a.max(b.abs()));
            fmax(&self.shared.in_level, pk);
            if self.monitor && mon_ins.is_none() {
                if let Some(t) = self.graph.monitor_track {
                    if let Some((l, r)) = self.graph.track_bufs.get_mut(t) {
                        for i in 0..n.min(il.len()) {
                            l[i] += il[i];
                            r[i] += ir[i];
                        }
                    }
                }
            }
        }

        // ---- tracks → inserts
        let g = &mut *self.graph;
        for (ti, t) in g.tracks.iter().enumerate() {
            let Some(ins) = g.inserts.get_mut(t.insert) else { continue };
            if !t.on {
                continue;
            }
            let (tl, tr) = &g.track_bufs[ti];
            for i in 0..n {
                ins.l[i] += tl[i] * t.gain;
                ins.r[i] += tr[i] * t.gain;
            }
        }

        if let Some(k) = mon_ins {
            // playback on the monitored insert is compensated before the chain; the live
            // input joins after the delay, scaled and muted by its track like the playback
            let ins = &mut g.inserts[k];
            g.mon.0.run(&mut ins.l[..n]);
            g.mon.1.run(&mut ins.r[..n]);
            if let (Some(t), Some((il, ir))) = (g.monitor_track.and_then(|t| g.tracks.get(t)).filter(|t| t.on), input) {
                for i in 0..n.min(il.len()) {
                    ins.l[i] += il[i] * t.gain;
                    ins.r[i] += ir[i] * t.gain;
                }
            }
        }

        // ---- inserts (non-master first), then master
        let ctx = match pat_ctx {
            Some(b) => ProcessCtx { sample_rate: sr, bpm, beat: b, playing: true },
            None => ProcessCtx { sample_rate: sr, bpm, beat: block_beat, playing: self.playing },
        };
        let count = g.inserts.len();
        if count > 0 {
            let (enabled, watch) = (self.skip_silent, &self.watch[..self.watch_n]);
            if self.workers > 1 && count > 4 && watch.is_empty() {
                // offline: the effect chains of the non-master inserts are independent, so
                // worker threads take them one by one (the sum into the master stays in order)
                let next = std::sync::atomic::AtomicUsize::new(0);
                let cells: Vec<std::sync::Mutex<&mut InsertRt>> = g.inserts[1..].iter_mut().map(std::sync::Mutex::new).collect();
                let shared = &*self.shared;
                std::thread::scope(|sc| {
                    for _ in 0..self.workers.min(cells.len()) {
                        sc.spawn(|| {
                            let mut taps = Taps { input: vec![], output: vec![] };
                            loop {
                                let j = next.fetch_add(1, Relaxed);
                                let Some(cell) = cells.get(j) else { break };
                                let mut ins = cell.lock().unwrap();
                                process_insert(&mut ins, j + 1, n, &ctx, enabled, &[], &mut taps, shared);
                            }
                        });
                    }
                });
            } else {
                for k in 1..count {
                    process_insert(&mut g.inserts[k], k, n, &ctx, enabled, watch, &mut self.taps, &self.shared);
                }
            }
            let (head, tail) = g.inserts.split_at_mut(1);
            let master = &mut head[0];
            for ins in tail.iter().filter(|i| !i.idle) {
                for i in 0..n {
                    master.l[i] += ins.l[i];
                    master.r[i] += ins.r[i];
                }
            }
            process_insert(master, 0, n, &ctx, enabled, watch, &mut self.taps, &self.shared);
        }

        // ---- out: master + click (delayed like the music) + preview
        g.click.run(&mut self.click[..n]);
        let (ml, mr) = match g.inserts.first() {
            Some(m) => (&m.l[..n], &m.r[..n]),
            None => (&self.tmp_l[..n], &self.tmp_r[..n]),
        };
        if ahead {
            return;
        }
        let ramp = ((DECLICK_S * sr) as usize).max(1);
        for i in 0..n {
            let (mut a, mut b) = if i < mute { (0.0, 0.0) } else { (ml[i] + self.click[i] * 0.6, mr[i] + self.click[i] * 0.6) };
            if i >= mute && self.fade_pos < ramp {
                let w = self.fade_pos as f32 / ramp as f32;
                a *= w;
                b *= w;
                self.fade_pos += 1;
            }
            if self.dc_pos < ramp {
                let w = 1.0 - self.dc_pos as f32 / ramp as f32;
                a += self.dc.0 * w;
                b += self.dc.1 * w;
                self.dc_pos += 1;
            }
            out_l[at + i] = a;
            out_r[at + i] = b;
        }
        if n > 0 {
            self.last = (out_l[at + n - 1], out_r[at + n - 1]);
        }
        if let Some(pv) = self.preview.as_mut() {
            let rate = pv.audio.sr as f64 / sr as f64;
            for i in 0..n {
                if pv.pos >= pv.end {
                    break;
                }
                let t = (pv.pos - pv.start) / pv.audio.sr as f64;
                let tl = (pv.end - pv.pos) / pv.audio.sr as f64;
                let mut env = pv.gain as f64;
                if pv.fade_in > 0.0 && t < pv.fade_in {
                    env *= t / pv.fade_in;
                }
                if pv.fade_out > 0.0 && tl < pv.fade_out {
                    env *= tl / pv.fade_out;
                }
                let (a, b) = pv.audio.sample(pv.pos);
                out_l[at + i] += a * env as f32;
                out_r[at + i] += b * env as f32;
                pv.pos += rate;
            }
            fset(&self.shared.preview_pos, (pv.pos / pv.audio.sr as f64) as f32);
            if pv.pos >= pv.end {
                let done = self.preview.take().unwrap();
                fset(&self.shared.preview_pos, -1.0);
                self.trash(Garbage::Preview(done));
            }
        }
    }

    /// The beat leaving the mixer while `beat` is rendered: the playback latency earlier,
    /// wrapped back into the loop after a wrap, never before the run's start.
    fn heard(&self, beat: f64, bps: f64) -> f64 {
        let h = beat - self.graph.click.buf.len() as f64 * bps;
        match self.wrapped {
            Some((ls, le)) if h < ls => (h + (le - ls)).max(ls),
            Some(_) => h,
            None => h.max(self.run_start),
        }
    }

    fn schedule(&mut self, b0: f64, b1: f64, at: usize, n: usize, bps: f64, spb: f64) {
        // metronome & count-in clicks
        let mut b = b0.ceil();
        while b < b1 {
            if self.metro || b < self.count_end {
                let delay = ((b - b0) / bps) as u32 + at as u32;
                let accent = (b as i64).rem_euclid(4) == 0;
                self.voices.click(accent, delay);
            }
            b += 1.0;
        }
        let tl = &*self.timeline;
        for c in tl.clips.iter() {
            if c.end <= b0 || c.start >= b1 {
                continue;
            }
            match &c.src {
                ClipSrc::Pat(pi) => {
                    let Some(pat) = tl.pats.get(*pi) else { continue };
                    if pat.len <= 0.0 {
                        continue;
                    }
                    let base = c.start - c.offset;
                    let lo = b0.max(c.start);
                    let hi = b1.min(c.end);
                    let r0 = ((lo - base) / pat.len).floor() as i64 - 1;
                    let r1 = ((hi - base) / pat.len).floor() as i64;
                    for r in r0..=r1 {
                        for nt in &pat.notes {
                            let nb = base + r as f64 * pat.len + nt.beat;
                            if nb >= lo && nb < hi {
                                let Some(ch) = tl.chans.get(nt.chan) else { continue };
                                if !ch.on {
                                    continue;
                                }
                                let delay = ((nb - b0) / bps) as u32 + at as u32;
                                let len_beats = nt.len.min(c.end - nb);
                                let (dest, gain) = match ch.insert {
                                    None => (Dest::Track(c.track), 1.0),
                                    // routed past the track: the track still mutes and scales it
                                    Some(k) => match self.graph.tracks.get(c.track) {
                                        Some(t) if t.on => (Dest::Insert(k), t.gain),
                                        _ => continue,
                                    },
                                };
                                play(&mut self.voices, &mut self.synths, ch, nt.chan, dest, delay, (len_beats * spb) as f32, nt.key, nt.vel, gain, self.sr);
                            }
                        }
                    }
                }
                ClipSrc::Auto(_) => {}
                ClipSrc::Comp(cp) => {
                    let Some((tl_l, tl_r)) = self.graph.track_bufs.get_mut(c.track) else { continue };
                    // samples of this block inside the clip
                    // (same per-sample test as audio clips: start ≤ beat < end)
                    let mut i0 = (((c.start - b0) / bps).ceil().max(0.0) as usize).min(n);
                    let mut i1 = (((c.end - b0) / bps).ceil().max(0.0) as usize).min(n);
                    while i1 > i0 && b0 + (i1 - 1) as f64 * bps >= c.end {
                        i1 -= 1;
                    }
                    while i0 < i1 && b0 + i0 as f64 * bps < c.start {
                        i0 += 1;
                    }
                    if i1 <= i0 {
                        continue;
                    }
                    let m = i1 - i0;
                    let (bl, br) = (&mut self.comp_l[..m], &mut self.comp_r[..m]);
                    bl.fill(0.0);
                    br.fill(0.0);
                    let bt0 = b0 + i0 as f64 * bps;
                    cp.render_add(c.offset + bt0 - c.start, bps, bl, br, spb);
                    let dur = (c.end - c.start) * spb;
                    let g = 10f32.powf(c.gain / 20.0);
                    let (pl, pr) = (if c.pan > 0.0 { 1.0 - c.pan } else { 1.0 }, if c.pan < 0.0 { 1.0 + c.pan } else { 1.0 });
                    for k in 0..m {
                        let t = (bt0 + k as f64 * bps - c.start) * spb;
                        let mut env = g;
                        if c.fade_in > 0.0 && t < c.fade_in as f64 {
                            env *= (t / c.fade_in as f64).max(0.0) as f32;
                        }
                        let left = dur - t;
                        if c.fade_out > 0.0 && left < c.fade_out as f64 {
                            env *= (left / c.fade_out as f64).max(0.0) as f32;
                        }
                        tl_l[at + i0 + k] += bl[k] * env * pl;
                        tl_r[at + i0 + k] += br[k] * env * pr;
                    }
                }
                ClipSrc::Audio(a) => {
                    let Some((tl_l, tl_r)) = self.graph.track_bufs.get_mut(c.track) else { continue };
                    crate::clipmix::mix_audio_clip(c, a, b0, bps, spb, &mut tl_l[at..at + n], &mut tl_r[at..at + n]);
                }
            }
        }
    }
}

/// Starts a note on a channel: the voice pool for built-ins and samplers, the channel's
/// synth instance for synth channels. `len_s` in seconds; `gain` is level on top of the
/// velocity (a track fader the channel's routing skips).
#[allow(clippy::too_many_arguments)]
fn play(voices: &mut VoicePool, synths: &mut SynthBank, ch: &ChanRt, idx: usize, dest: Dest, delay: u32, len_s: f32, key: u8, vel: f32, gain: f32, sr: f32) {
    if ch.kind == VoiceKind::Synth {
        if let Some(s) = synths.get(ch.id) {
            s.note(delay, (len_s.max(0.0) * sr) as u32, key, vel * gain, dest);
        }
    } else {
        voices.note(ch, idx, dest, delay, len_s, key, vel, gain);
    }
}

/// One insert's turn in a block: silence gate, effect chain, fader/pan, PDC delay. Leaves the
/// insert's output in `ins.l`/`ins.r` (or `ins.idle` set: it contributes nothing).
#[allow(clippy::too_many_arguments)]
fn process_insert(ins: &mut InsertRt, k: usize, n: usize, ctx: &ProcessCtx, enabled: bool, watch: &[Id], taps: &mut Taps, shared: &Shared) {
    // also set here: offline renders run chains on worker threads, each with its own float mode
    let _no_denormals = crate::processor::NoDenormals::new();
    let gate = gate(ins, n, enabled, watch);
    ins.idle = gate == Gate::Idle;
    if ins.idle {
        // no signal in, nothing left in the effects or the delay line: nothing out
        ins.g_cur = if ins.on { ins.gain } else { 0.0 };
        return;
    }
    if gate == Gate::Run {
        run_chain(ins, n, ctx, watch, taps, shared);
    }
    // stereo width: M/S side gain, ramped across the block; exactly 1 is a bit-exact bypass
    if gate == Gate::Run && (ins.width != 1.0 || ins.w_cur != 1.0) {
        let w0 = ins.w_cur;
        let step = (ins.width - w0) / n as f32;
        for i in 0..n {
            let w = if i + 1 == n { ins.width } else { w0 + step * (i + 1) as f32 };
            let (m, s) = ((ins.l[i] + ins.r[i]) * 0.5, (ins.l[i] - ins.r[i]) * 0.5 * w);
            ins.l[i] = m + s;
            ins.r[i] = m - s;
        }
    }
    ins.w_cur = ins.width;
    // fader + pan, smoothed across the block (a skipped chain outputs zeros)
    let target = if ins.on { ins.gain } else { 0.0 };
    if gate == Gate::Run {
        let (gl, gr) = (if ins.pan > 0.0 { 1.0 - ins.pan } else { 1.0 }, if ins.pan < 0.0 { 1.0 + ins.pan } else { 1.0 });
        let g0 = ins.g_cur;
        let step = (target - g0) / n as f32;
        let (mut pl, mut pr) = (0f32, 0f32);
        for i in 0..n {
            let gg = g0 + step * (i + 1) as f32;
            ins.l[i] *= gg * gl;
            ins.r[i] *= gg * gr;
            pl = pl.max(ins.l[i].abs());
            pr = pr.max(ins.r[i].abs());
        }
        if k < MAX_INSERTS {
            fmax(&shared.meters[k].0, pl);
            fmax(&shared.meters[k].1, pr);
        }
    }
    ins.g_cur = target;
    if !ins.pdc_l.is_empty() && !ins.low_lat {
        let d = ins.pdc_l.len();
        for i in 0..n {
            let w = ins.pdc_w;
            let (a, b) = (ins.pdc_l[w], ins.pdc_r[w]);
            ins.pdc_l[w] = ins.l[i];
            ins.pdc_r[w] = ins.r[i];
            ins.l[i] = a;
            ins.r[i] = b;
            ins.pdc_w = if w + 1 == d { 0 } else { w + 1 };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    /// Process the chain.
    Run,
    /// The chain is parked (silent input, tails over): skip it, the PDC line may still be draining.
    Skip,
    /// Parked and the PDC line is empty too: the insert contributes nothing.
    Idle,
}

/// Silence skipping. An insert whose input is exact digital silence for longer than the
/// tail of its chain (every effect's ring-down, summed, plus the dry-path delays) puts out
/// silence whether or not the effects run, so they are settled once (`Processor::park`)
/// and then skipped until the input is non-silent again. Chains with an effect of unknown
/// tail (`tail_samples() == None`, e.g. hosted plugins) or a watched effect never skip.
fn gate(ins: &mut InsertRt, n: usize, enabled: bool, watch: &[Id]) -> Gate {
    if !enabled {
        return Gate::Run;
    }
    let silent = ins.l[..n].iter().all(|&x| x == 0.0) && ins.r[..n].iter().all(|&x| x == 0.0);
    if !silent {
        ins.quiet = 0;
        ins.parked = false;
        return Gate::Run;
    }
    // silence that has already passed *before* this block: whatever rings on into the block
    // (an effect tail, a PDC line) still has to be rendered
    let before = ins.quiet;
    ins.quiet = ins.quiet.saturating_add(n);
    if !ins.parked {
        let mut tail = 0usize;
        for s in ins.fx.iter() {
            if !watch.is_empty() && watch.contains(&s.id) {
                return Gate::Run;
            }
            if s.bypass {
                continue;
            }
            tail = tail.saturating_add(s.dly_l.len());
            if s.mix > 0.0005 {
                if let Some(p) = s.proc.as_ref() {
                    match p.tail_samples() {
                        Some(t) => tail = tail.saturating_add(t),
                        None => return Gate::Run,
                    }
                }
            }
        }
        if before < tail {
            return Gate::Run;
        }
        for s in ins.fx.iter_mut() {
            if let Some(p) = s.proc.as_mut() {
                p.park();
            }
        }
        ins.parked = true;
        ins.idle_at = tail.saturating_add(ins.pdc_l.len());
    }
    if before >= ins.idle_at {
        Gate::Idle
    } else {
        Gate::Skip
    }
}

fn run_chain(ins: &mut InsertRt, n: usize, ctx: &ProcessCtx, watch: &[Id], taps: &mut Taps, shared: &Shared) {
    let low_lat = ins.low_lat;
    for slot in ins.fx.iter_mut() {
        let delayed = !slot.dly_l.is_empty();
        if low_lat && delayed {
            continue; // low-latency monitoring: look-ahead effects are bypassed
        }
        let w = watch.iter().position(|&id| id == slot.id);
        if let Some(w) = w {
            push_tap(&mut taps.input[w], &ins.l[..n], &ins.r[..n]);
        }
        if !slot.bypass && slot.mix <= 0.0005 && delayed {
            // fully dry: still late by the latency PDC is compensating for
            slot.delay_dry(&mut ins.l[..n], &mut ins.r[..n]);
        } else if !slot.bypass && slot.mix > 0.0005 {
            if let Some(p) = slot.proc.as_mut() {
                let mix = slot.mix.min(1.0);
                if mix < 0.9995 || delayed {
                    ins.dry_l[..n].copy_from_slice(&ins.l[..n]);
                    ins.dry_r[..n].copy_from_slice(&ins.r[..n]);
                }
                p.process(&mut ins.l[..n], &mut ins.r[..n], ctx);
                if delayed {
                    // the line runs at every Mix so lowering it never plays stale audio
                    slot.delay_dry(&mut ins.dry_l[..n], &mut ins.dry_r[..n]);
                }
                if mix < 0.9995 {
                    for i in 0..n {
                        ins.l[i] = ins.dry_l[i] + (ins.l[i] - ins.dry_l[i]) * mix;
                        ins.r[i] = ins.dry_r[i] + (ins.r[i] - ins.dry_r[i]) * mix;
                    }
                }
            }
        }
        if let Some(w) = w {
            let wet = if slot.bypass { None } else { slot.proc.as_ref().and_then(|p| p.wet_tap()) };
            match wet {
                Some(wet) if !wet.is_empty() => push_mono(&mut taps.output[w], wet),
                _ => push_tap(&mut taps.output[w], &ins.l[..n], &ins.r[..n]),
            }
            let gr = if slot.bypass { 0.0 } else { slot.proc.as_ref().map(|p| p.gain_reduction_db()).unwrap_or(0.0) };
            fset(&shared.gr[w], gr);
            let mut m = [0f32; METERS];
            if let Some(p) = slot.proc.as_ref() {
                p.meters(&mut m);
            }
            for (a, v) in shared.fx_meters[w].iter().zip(m) {
                fset(a, v);
            }
        }
    }
}

fn push_tap(p: &mut rtrb::Producer<f32>, l: &[f32], r: &[f32]) {
    if p.slots() < l.len() {
        return;
    }
    for i in 0..l.len() {
        let _ = p.push((l[i] + r[i]) * 0.5);
    }
}

fn push_mono(p: &mut rtrb::Producer<f32>, x: &[f32]) {
    if p.slots() < x.len() {
        return;
    }
    for &v in x {
        let _ = p.push(v);
    }
}

fn slot(g: &mut Graph, fx: Id) -> Option<&mut FxSlot> {
    g.inserts.iter_mut().flat_map(|i| i.fx.iter_mut()).find(|s| s.id == fx)
}

/// A new timeline is built from the project's static values: the channels and clips an
/// automation drives keep the value they were playing (the next block glides from there to the
/// automated one; jumping back to the static value for a block was an audible blip on every edit
/// during playback). Allocation-free; quadratic only in the number of automations.
fn carry_automated(new: &mut Timeline, old: &Timeline) {
    for a in new.autos.iter() {
        match a.target {
            TargetRt::ChanVol(i) | TargetRt::ChanPan(i) | TargetRt::KeysRelease(i) | TargetRt::KeysDynamics(i) => {
                let Some(c) = new.chans.get_mut(i) else { continue };
                let Some(o) = old.chans.iter().find(|o| o.id == c.id) else { continue };
                match a.target {
                    TargetRt::ChanVol(_) => (c.gain, c.gain0) = (o.gain, o.gain0),
                    TargetRt::ChanPan(_) => (c.pan, c.pan0) = (o.pan, o.pan0),
                    TargetRt::KeysRelease(_) => c.keys_release = o.keys_release,
                    _ => c.keys_dynamics = o.keys_dynamics,
                }
            }
            TargetRt::ClipGain(k) | TargetRt::ClipPan(k) => {
                let Some(id) = new.clips.get(k).map(|c| c.id) else { continue };
                let found = old.autos.iter().find_map(|oa| match oa.target {
                    TargetRt::ClipGain(j) | TargetRt::ClipPan(j) => old.clips.get(j).filter(|c| c.id == id),
                    _ => None,
                });
                let Some(o) = found else { continue };
                let c = &mut new.clips[k];
                if matches!(a.target, TargetRt::ClipGain(_)) {
                    (c.gain, c.gain0) = (o.gain, o.gain0);
                } else {
                    (c.pan, c.pan0) = (o.pan, o.pan0);
                }
            }
            _ => {}
        }
    }
}

/// Moves automated parameters to their value at `beat`. Allocation-free.
fn apply_automation(tl: &mut Timeline, g: &mut Graph, synths: &mut crate::poly::SynthBank, beat: f64) {
    for ci in 0..tl.clips.len() {
        let c = &tl.clips[ci];
        let ClipSrc::Auto(ai) = c.src else { continue };
        if beat < c.start || beat >= c.end {
            continue;
        }
        let Some(a) = tl.autos.get(ai) else { continue };
        let Some(v) = a.env.value_at(beat - c.start + c.offset) else { continue };
        let target = a.target;
        match target {
            TargetRt::Param { fx, idx, min, max, log } => {
                if let Some(s) = slot(g, fx) {
                    let x = if log { min * (max / min).powf(v) } else { min + (max - min) * v };
                    if let Some(pb) = s.params.as_mut() {
                        if (pb.values[idx] - x).abs() > 1e-6 * x.abs().max(1.0) {
                            pb.values[idx] = x;
                            let pb = *pb;
                            if let Some(p) = s.proc.as_mut() {
                                p.set_params(&pb);
                            }
                        }
                    }
                }
            }
            TargetRt::Band { fx, band, field } => {
                if let Some(s) = slot(g, fx) {
                    if let Some(pb) = s.params.as_mut() {
                        let b = &mut pb.bands[band];
                        let x = match field {
                            0 => 20.0 * 1000f32.powf(v),
                            1 => -30.0 + 60.0 * v,
                            _ => 0.1 * 180f32.powf(v),
                        };
                        let cur = match field {
                            0 => &mut b.f,
                            1 => &mut b.g,
                            _ => &mut b.q,
                        };
                        if (*cur - x).abs() > 1e-4 {
                            *cur = x;
                            let pb = *pb;
                            if let Some(p) = s.proc.as_mut() {
                                p.set_params(&pb);
                            }
                        }
                    }
                }
            }
            TargetRt::Mix { fx } => {
                if let Some(s) = slot(g, fx) {
                    s.mix = v;
                }
            }
            TargetRt::PluginParam { fx, param_id } => {
                if let Some(s) = slot(g, fx) {
                    if let Some(p) = s.proc.as_mut() {
                        p.set_plugin_param(param_id, v.clamp(0.0, 1.0));
                    }
                }
            }
            TargetRt::InsVol(i) => {
                if let Some(ins) = g.inserts.get_mut(i) {
                    ins.gain = crate::model::fader_gain(v);
                }
            }
            TargetRt::InsPan(i) => {
                if let Some(ins) = g.inserts.get_mut(i) {
                    ins.pan = v * 2.0 - 1.0;
                }
            }
            TargetRt::TrackVol(i) => {
                if let Some(t) = g.tracks.get_mut(i) {
                    t.gain = crate::model::fader_gain(v);
                }
            }
            TargetRt::ChanVol(i) => {
                if let Some(ch) = tl.chans.get_mut(i) {
                    ch.gain = crate::model::fader_gain(v) / crate::model::fader_gain(crate::model::VOL_UNITY);
                }
            }
            TargetRt::ChanPan(i) => {
                if let Some(ch) = tl.chans.get_mut(i) {
                    ch.pan = v * 2.0 - 1.0;
                }
            }
            TargetRt::Synth { chan, idx, min, max, log, choice } => {
                if let Some(s) = synths.get(chan) {
                    let x = if log { min * (max / min).powf(v) } else { min + (max - min) * v };
                    s.set_param(idx, if choice { x.round() } else { x });
                }
            }
            TargetRt::KeysRelease(i) => {
                if let Some(ch) = tl.chans.get_mut(i) {
                    let (lo, hi, _) = crate::keys::RELEASE_MS;
                    ch.keys_release = lo * (hi / lo).powf(v) / 1000.0;
                }
            }
            TargetRt::KeysDynamics(i) => {
                if let Some(ch) = tl.chans.get_mut(i) {
                    ch.keys_dynamics = v;
                }
            }
            TargetRt::ClipGain(k) => {
                if let Some(cl) = tl.clips.get_mut(k) {
                    cl.gain = CLIP_GAIN_DB.0 + (CLIP_GAIN_DB.1 - CLIP_GAIN_DB.0) * v;
                }
            }
            TargetRt::ClipPan(k) => {
                if let Some(cl) = tl.clips.get_mut(k) {
                    cl.pan = v * 2.0 - 1.0;
                }
            }
        }
    }
}

/// The range an audio clip's gain automation spans, dB (the sample editor's VOL knob).
pub const CLIP_GAIN_DB: (f32, f32) = (-24.0, 12.0);
