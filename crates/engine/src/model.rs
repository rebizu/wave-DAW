//! The project: everything that is saved, undone, and shared between the GUI and the CLI.
//!
//! Plain data. The engine thread owns the one true copy; the UI and the CLI see
//! JSON snapshots of it and change it only through [`crate::ops::Op`]s.
//!
//! Units, so nobody has to guess:
//! * time on the playlist is in **beats** (quarter notes), `f64`;
//! * pattern notes are in **steps** (sixteenths), `f32`;
//! * fades are in **seconds** (they belong to the audio, not the grid);
//! * `vol` is a **fader position** 0..1, gain = 2·t³ (0.7937 ≈ 0 dB, 1.0 ≈ +6 dB);
//! * clip `gain` is in **dB**, `pan` is -1..1.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type Id = u64;

/// Fader position that gives unity gain.
pub const VOL_UNITY: f32 = 0.7937;
/// Beats shown on the playlist. The arrangement may not go past this: 512 bars of 4/4,
/// 11:46 at 174 BPM, 17 minutes at 120. Nothing is allocated per beat, so the limit
/// only bounds the playlist's scroll width (≈200k px at the deepest zoom).
pub const TOTAL_BEATS: f64 = 2048.0;

pub fn fader_gain(t: f32) -> f32 {
    2.0 * t * t * t
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub version: u32,
    pub bpm: f64,
    pub tracks: Vec<Track>,
    /// `inserts[0]` is always the master (id 0).
    pub inserts: Vec<Insert>,
    pub patterns: Vec<Pattern>,
    pub sources: Vec<Source>,
    pub clips: Vec<Clip>,
    #[serde(rename = "loop")]
    pub loop_range: Option<LoopRange>,
    /// Automation envelopes; played by clips of kind `auto`.
    #[serde(default)]
    pub automations: Vec<Automation>,
    /// Take groups with their comps; played by clips of kind `comp`.
    #[serde(default)]
    pub comps: Vec<Comp>,
    /// Channel Rack rows, shared by every pattern (notes refer to them by `ch`).
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// The lyrics notebook ("Notatnik"): pages of text where one line is one bar.
    #[serde(default)]
    pub notebook: Notebook,
    /// Next id handed out by [`Project::alloc_id`]. Ids are never reused.
    pub next_id: Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopRange {
    pub a: f64,
    pub b: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: Id,
    pub name: String,
    pub color: String,
    pub muted: bool,
    pub solo: bool,
    pub arm: bool,
    pub vol: f32,
    /// Lane height in px. UI state, but it is saved with the song like in FL.
    pub h: f32,
    /// Mixer insert id this track plays into.
    pub insert: Id,
    /// Instrument icon id shown in the lane header (UI only); older songs have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

fn width_unity() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Insert {
    pub id: Id,
    pub name: String,
    pub color: String,
    pub vol: f32,
    pub pan: f32,
    /// Stereo width, 0 mono .. 1 unchanged .. 2 extra wide (old projects: 1).
    #[serde(default = "width_unity")]
    pub width: f32,
    pub muted: bool,
    pub solo: bool,
    pub fx: Vec<Fx>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FxKind {
    Eq,
    Comp,
    Sat,
    Delay,
    Reverb,
    Limit,
    Fet,
    Deess,
    Tune,
    Denoise,
    Voice,
    Rider,
    /// Tempo-locked slow-down (`dsp::halftime`).
    Half,
    Vst3,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fx {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: FxKind,
    pub bypass: bool,
    /// Dry/wet: 1 = fully processed (default), 0 = dry.
    #[serde(default = "one")]
    pub mix: f32,
    /// Index into the built-in preset list this was last loaded from (UI shows `*` when edited).
    #[serde(default)]
    pub preset: Option<u32>,
    /// Built-in effects: parameter id -> value in the parameter's own unit (see [`crate::fx::params`]).
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
    /// EQ only: up to 24 bands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bands: Option<Vec<EqBand>>,
    /// VST3 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandType {
    Lowcut,
    Lowshelf,
    Peak,
    Highshelf,
    Highcut,
    Notch,
    Bandpass,
    Tilt,
}

/// Which part of the stereo image a band works on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    #[default]
    Stereo,
    Left,
    Right,
    Mid,
    Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub on: bool,
    #[serde(rename = "type")]
    pub kind: BandType,
    pub f: f64,
    pub g: f64,
    pub q: f64,
    /// Cut filters: 12, 24, 36 or 48 dB/oct.
    #[serde(default = "default_slope")]
    pub slope: u8,
    #[serde(default)]
    pub place: Placement,
    /// Dynamic EQ: when set, the band's gain moves by up to `range` dB
    /// as the band's own level goes over `thr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynBand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DynBand {
    /// dB, −30..30 (negative = duck when loud).
    pub range: f64,
    /// dBFS.
    pub thr: f64,
}

fn one() -> f32 {
    1.0
}

fn default_slope() -> u8 {
    12
}

/// Maximum EQ bands.
pub const MAX_BANDS: usize = 24;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRef {
    pub format: String,
    pub path: String,
    /// VST3 class id (hex) as reported by the scan.
    pub cid: String,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    /// Opaque plugin state (base64), refreshed on save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// MIDI key at which a sampler channel plays its sample unpitched (C5 in FL's naming).
pub const SAMPLE_ROOT: u8 = 60;

/// Standard-library files of a new project's channels, and the channel names.
pub const STARTER_KIT: [(&str, &str); 4] = [("kick-808-short.flac", "Kick"), ("clap-808.flac", "Clap"), ("hat-808-closed.flac", "Hat"), ("snare-808.flac", "Snare")];

/// What a channel plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChannelSrc {
    /// The sampler: plays a project source, pitched by the note (`root` = original pitch).
    Sample {
        source: Id,
        #[serde(default)]
        sampler: SamplerState,
    },
    /// The built-in polyphonic synthesizer (see `crate::poly`).
    Synth { #[serde(default)] synth: SynthState },
    /// WAVE Keys: the sampled grand piano (see `crate::keys`).
    Keys { #[serde(default)] keys: KeysState },
}

/// How a sampler channel plays its sample (FL Studio's Sampler, Ableton's Simpler).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SamplerState {
    /// The part of the file that plays, as fractions of its length (`start` < `end`).
    #[serde(default)]
    pub start: f32,
    #[serde(default = "one")]
    pub end: f32,
    /// Plays the part backwards.
    #[serde(default)]
    pub reverse: bool,
    /// Brings the file's peak to −0.3 dBFS.
    #[serde(default)]
    pub norm: bool,
    /// Transpose in semitones (−24..24) and fine tune in cents (−100..100), on top of the note.
    #[serde(default)]
    pub semi: i8,
    #[serde(default)]
    pub fine: f32,
    /// `false` = one-shot: a note plays the sample to its end, whatever its length (drums).
    /// `true` = gate: the sample plays while the note is held, then fades over `release`.
    #[serde(default)]
    pub gate: bool,
    /// Volume envelope: `attack` and `decay` in ms, `sustain` 0..1 (the level after the
    /// decay; 0 makes the decay the whole sound), `release` in ms (gate mode).
    #[serde(default)]
    pub attack: f32,
    #[serde(default)]
    pub decay: f32,
    #[serde(default = "one")]
    pub sustain: f32,
    #[serde(default = "sampler_release")]
    pub release: f32,
    /// Cut group 1..8 (0 = none): a new note silences every sounding note of the channels in
    /// the same group, itself included. A closed hat that cuts the open one.
    #[serde(default)]
    pub cut: u8,
}

fn sampler_release() -> f32 {
    SamplerState::RELEASE_DEFAULT
}

impl SamplerState {
    pub const SEMI: (i8, i8) = (-24, 24);
    pub const FINE: (f32, f32) = (-100.0, 100.0);
    pub const ATTACK_MS: (f32, f32) = (0.0, 2000.0);
    pub const DECAY_MS: (f32, f32) = (0.0, 5000.0);
    pub const RELEASE_MS: (f32, f32) = (1.0, 5000.0);
    pub const RELEASE_DEFAULT: f32 = 20.0;
    pub const CUT_GROUPS: u8 = 8;
    /// The shortest part that may be selected, as a fraction of the file.
    pub const MIN_SPAN: f32 = 0.001;

    /// Why these settings cannot be used, if they cannot.
    pub fn check(&self) -> Result<(), String> {
        let within = |v: f32, (lo, hi): (f32, f32)| v.is_finite() && (lo..=hi).contains(&v);
        if !(within(self.start, (0.0, 1.0)) && within(self.end, (0.0, 1.0)) && self.end - self.start >= Self::MIN_SPAN - 1e-6) {
            return Err("sampler start and end are fractions 0..1 of the file, start before end".into());
        }
        if !(Self::SEMI.0..=Self::SEMI.1).contains(&self.semi) || !within(self.fine, Self::FINE) {
            return Err("sampler pitch is -24..24 semitones and -100..100 cents".into());
        }
        if !within(self.attack, Self::ATTACK_MS) || !within(self.decay, Self::DECAY_MS) || !within(self.sustain, (0.0, 1.0)) || !within(self.release, Self::RELEASE_MS) {
            return Err("sampler envelope: attack 0..2000 ms, decay 0..5000 ms, sustain 0..1, release 1..5000 ms".into());
        }
        if self.cut > Self::CUT_GROUPS {
            return Err("sampler cut group is 0 (none) or 1..8".into());
        }
        Ok(())
    }
}

impl Default for SamplerState {
    fn default() -> Self {
        Self { start: 0.0, end: 1.0, reverse: false, norm: false, semi: 0, fine: 0.0, gate: false, attack: 0.0, decay: 0.0, sustain: 1.0, release: Self::RELEASE_DEFAULT, cut: 0 }
    }
}

/// A WAVE Keys channel's settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeysState {
    /// How long a note rings after its key is let go, in milliseconds.
    #[serde(default = "keys_release")]
    pub release: f32,
    /// 0..100 %: how much the velocity changes the level (the timbre always follows it).
    #[serde(default = "keys_dynamics")]
    pub dynamics: f32,
}

fn keys_release() -> f32 {
    crate::keys::RELEASE_MS.2
}
fn keys_dynamics() -> f32 {
    crate::keys::DYNAMICS.2
}

impl Default for KeysState {
    fn default() -> Self {
        Self { release: keys_release(), dynamics: keys_dynamics() }
    }
}

/// A synth channel's sound: parameter values by catalog id (`fx_catalog().synth`). Missing ids
/// play their defaults, unknown ids are ignored. `preset` names the factory preset the values
/// came from (display only; `None` once edited or for a blank patch).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SynthState {
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
}

/// A Channel Rack row. Channels are global to the project (like FL Studio): every pattern
/// can have notes for every channel, and a note says which channel it plays with `ch`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub color: String,
    #[serde(flatten)]
    pub src: ChannelSrc,
    /// Fader position (0.7937 = unity, see [`fader_gain`]).
    #[serde(default = "vol_unity")]
    pub vol: f32,
    /// -1..1.
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    /// Mixer insert the channel plays into. `None`: the insert of the playlist track
    /// the pattern clip is on (how every channel behaved before channels existed).
    #[serde(default)]
    pub insert: Option<Id>,
    /// Key a step plays when the note has no `p`; for a sampler also the key at which
    /// the sample plays at its original pitch.
    #[serde(default = "sample_root")]
    pub root: u8,
}

fn vol_unity() -> f32 {
    VOL_UNITY
}
fn sample_root() -> u8 {
    SAMPLE_ROOT
}

impl Channel {
    /// A channel at unity, centred, following its clip's track, rooted at C5.
    pub fn new(id: Id, name: &str, color: &str, src: ChannelSrc) -> Self {
        Self { id, name: name.into(), color: color.into(), src, vol: VOL_UNITY, pan: 0.0, mute: false, solo: false, insert: None, root: SAMPLE_ROOT }
    }
    /// A sampler channel playing `source` with the default settings.
    pub fn sampler(id: Id, name: &str, color: &str, source: Id) -> Self {
        Self::new(id, name, color, ChannelSrc::Sample { source, sampler: SamplerState::default() })
    }
    /// The source a sampler channel plays.
    pub fn sample_source(&self) -> Option<Id> {
        match self.src {
            ChannelSrc::Sample { source, .. } => Some(source),
            _ => None,
        }
    }
    /// Channel gain: the fader curve normalised so the unity position is exactly 1.
    pub fn gain(&self) -> f32 {
        fader_gain(self.vol) / fader_gain(VOL_UNITY)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Channel id.
    pub ch: Id,
    /// Start in steps (1/16) from the pattern start.
    pub step: f32,
    /// Length in steps.
    pub len: f32,
    /// MIDI key; `None` = the channel's `root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<u8>,
    /// Velocity 0..1 (gain).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub v: f32,
}

fn is_one(v: &f32) -> bool {
    *v == 1.0
}

impl Note {
    pub fn new(ch: Id, step: f32, len: f32, p: Option<u8>) -> Self {
        Self { ch, step, len, p, v: 1.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    pub id: Id,
    pub name: String,
    pub color: String,
    /// Length in beats, always whole bars: derived, never chosen directly. It is the end of
    /// the last note rounded up to a bar, but at least `min_len` (FL Studio's automatic
    /// pattern length). [`Pattern::fit_len`] sets it; every `putPattern` and file load does.
    #[serde(default = "bar")]
    pub len: f64,
    /// The shortest the pattern may be, in beats (whole bars, at least one): bars the user
    /// added on purpose ("+ bar") stay even while empty, e.g. a crash every second bar.
    #[serde(default = "bar", rename = "minLen")]
    pub min_len: f64,
    pub notes: Vec<Note>,
    /// 0..1: how far every second 16th is delayed (see [`swing_offset`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub swing: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn bar() -> f64 {
    4.0
}

/// Beats in one bar (the project is in 4/4).
pub const BAR_BEATS: f64 = 4.0;

impl Pattern {
    /// The length its notes need: the end of the last note rounded up to a whole bar, at
    /// least one bar, at most [`TOTAL_BEATS`].
    pub fn content_len(notes: &[Note]) -> f64 {
        let end = notes.iter().map(|n| (n.step + n.len) as f64 / 4.0).fold(0.0, f64::max);
        ((end / BAR_BEATS - 1e-6).ceil().max(1.0) * BAR_BEATS).min(TOTAL_BEATS)
    }

    /// Sets `len` from the notes and `min_len` (whole bars), and normalises `min_len`.
    pub fn fit_len(&mut self) {
        let min = if self.min_len.is_finite() { (self.min_len / BAR_BEATS - 1e-6).ceil().max(1.0) * BAR_BEATS } else { BAR_BEATS };
        self.min_len = min.min(TOTAL_BEATS);
        self.len = Self::content_len(&self.notes).max(self.min_len);
    }
}

/// Swing, in steps, added to a note that starts on an odd 16th (0-based 1, 3, 5 …).
/// `swing` 1 (100 %) delays it by half a step: the off-beat 16th lands at 75 % of its
/// 8th (MPC 75 %); 0.67 is close to a triplet feel (MPC 66 %). Notes off the 16th grid
/// and on even 16ths do not move.
pub fn swing_offset(step: f32, swing: f32) -> f32 {
    let r = step.round();
    if swing <= 0.0 || (step - r).abs() > 1e-3 || (r as i64).rem_euclid(2) != 1 {
        return 0.0;
    }
    0.5 * swing.clamp(0.0, 1.0)
}

/// An audio file the project uses. Samples live in the engine, not here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub id: Id,
    pub name: String,
    pub color: String,
    /// Where the audio is on disk. Recordings and edits are written next to the project.
    pub path: String,
    /// Seconds.
    pub dur: f64,
    /// Sample rate of the file (the engine resamples on load).
    pub sr: u32,
    pub ch: u16,
    pub peak_db: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Pat,
    Audio,
    Auto,
    /// Plays a [`Comp`]: `ref` is the comp id, `offset` is in the comp's own beats.
    Comp,
}

/// What an automation moves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AutoTarget {
    /// A built-in effect parameter (catalog id), or `band:<i>:f|g|q` of an EQ.
    Fx { fx_id: Id, param: String },
    /// An effect's dry/wet.
    Mix { fx_id: Id },
    InsertVol { id: Id },
    InsertPan { id: Id },
    TrackVol { id: Id },
    /// A Channel Rack channel's volume (fader position 0..1, like `vol`) and pan.
    ChanVol { id: Id },
    ChanPan { id: Id },
    /// A synth channel's patch parameter (catalog id, `poly::params`).
    Synth { chan_id: Id, param: String },
    /// A WAVE Keys channel's `release` (ms, logarithmic) or `dynamics` (%).
    Keys { chan_id: Id, param: String },
    /// A hosted VST3 parameter (`param_id` is the plugin's own id, as
    /// `vst3_params` reports it). The envelope value is the plugin's
    /// normalized 0..1.
    Plugin { fx_id: Id, param_id: u32 },
    /// An audio clip's own gain (−24..+12 dB) and pan.
    ClipGain { id: Id },
    ClipPan { id: Id },
}

/// How an automation segment travels from the previous point to this one (FL's curve types).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CurveShape {
    /// One curve, bent by the tension (straight at 0).
    #[default]
    Curve,
    /// An S: slow at both ends, fast in the middle; the tension sharpens or flattens it.
    Double,
    /// Holds the previous value until this point, then jumps.
    Hold,
    /// Even steps from the previous value to this one; the tension sets how many (2..16).
    Stairs,
}

impl CurveShape {
    fn is_curve(&self) -> bool {
        *self == CurveShape::Curve
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AutoPoint {
    /// Beats from the start of the automation.
    pub t: f64,
    /// Normalised 0..1 (log parameters are normalised logarithmically).
    pub v: f32,
    /// Bend of the segment that ends at this point, -1..1 (0 = straight). The first point's is unused.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tension: f32,
    /// Shape of the segment that ends at this point.
    #[serde(default, skip_serializing_if = "CurveShape::is_curve")]
    pub shape: CurveShape,
}

impl AutoPoint {
    /// A point ending a straight segment.
    pub fn at(t: f64, v: f32) -> Self {
        Self { t, v, ..Self::default() }
    }
}

/// `(e^(s·x) − 1) / (e^s − 1)`: 0 at 0, 1 at 1, slow start for s > 0, straight at s = 0.
fn bend(x: f64, s: f64) -> f64 {
    if s.abs() < 1e-6 {
        x
    } else {
        (s * x).exp_m1() / s.exp_m1()
    }
}

/// Progress 0..1 along a segment of `shape` and `tension` at `f` (0..1 of its time).
/// The UI (`ui/src/lib/automation.ts` `curveAt`) uses the same formulas; both are checked
/// against `tests/fixtures/curves.json`.
pub fn curve_at(shape: CurveShape, tension: f32, f: f64) -> f64 {
    let k = (tension as f64).clamp(-1.0, 1.0);
    let f = f.clamp(0.0, 1.0);
    match shape {
        CurveShape::Curve => bend(f, 8.0 * k),
        CurveShape::Double => {
            let s = 4.0 + 6.0 * k;
            if f < 0.5 {
                0.5 * bend(2.0 * f, s)
            } else {
                1.0 - 0.5 * bend(2.0 - 2.0 * f, s)
            }
        }
        CurveShape::Hold => 0.0,
        CurveShape::Stairs => {
            let n = stairs_of(tension) as f64;
            ((f * n).floor() / (n - 1.0)).min(1.0)
        }
    }
}

/// Steps of a `Stairs` segment for `tension` -1..1: 2..16.
pub fn stairs_of(tension: f32) -> u32 {
    2 + (((tension as f64).clamp(-1.0, 1.0) + 1.0) / 2.0 * 14.0).round() as u32
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Automation {
    pub id: Id,
    pub name: String,
    pub color: String,
    pub target: AutoTarget,
    pub points: Vec<AutoPoint>,
}

impl Automation {
    /// Value at `t` beats (each segment shaped by its end point, held at the ends).
    pub fn value_at(&self, t: f64) -> Option<f32> {
        let p = &self.points;
        let first = p.first()?;
        if t <= first.t {
            return Some(first.v);
        }
        let i = p.partition_point(|q| q.t <= t);
        if i >= p.len() {
            return Some(p[p.len() - 1].v);
        }
        let (a, b) = (p[i - 1], p[i]);
        let f = (t - a.t) / (b.t - a.t).max(1e-9);
        let g = curve_at(b.shape, b.tension, f);
        Some((a.v as f64 + (b.v as f64 - a.v as f64) * g) as f32)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: Id,
    /// Track index (lane).
    pub track: u32,
    pub start: f64,
    pub len: f64,
    /// Beats into the source/pattern where the clip begins.
    pub offset: f64,
    pub kind: ClipKind,
    #[serde(rename = "ref")]
    pub source: Id,
    pub muted: bool,
    /// dB.
    pub gain: f32,
    #[serde(default)]
    pub fade_in: f32,
    #[serde(default)]
    pub fade_out: f32,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub norm: bool,
    #[serde(default)]
    pub rev: bool,
    /// Note-level pitch edits (the pitch editor). Rendered offline, latency-free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune: Option<ClipTune>,
}

/// Pitch edits of an audio clip. `notes` starts as the analysis' segmentation and
/// is then the user's: split, merge, shift. Times are seconds into the source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipTune {
    pub notes: Vec<NoteEdit>,
    /// Hand-drawn pitch (pencil): `[seconds, midi]` points, sorted. Within a
    /// drawn run it replaces the note's pitch; gaps > 30 ms end a run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pencil: Vec<[f32; 2]>,
    /// Tune to Reference: the base pitch comes from another performance of the
    /// same melody; note edits and pencil then apply on top of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<RefTune>,
}

impl ClipTune {
    /// True when rendering would change the source.
    pub fn is_active(&self) -> bool {
        !self.notes.is_empty() || !self.pencil.is_empty() || self.reference.is_some()
    }
}

/// Parameters of Tune to Reference. The target pitch is derived from them
/// deterministically (`crate::reference`), so only these are stored.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefTune {
    /// Audio source holding the reference vocal (a cappella).
    pub source: Id,
    /// 0..1: how far the pitch moves from the sung one toward the target.
    pub correction: f32,
    /// 0..1: how much of the singer's own vibrato and note transitions is kept.
    pub vibrato: f32,
    /// 0..1: note centres from the reference (1) or from the nearest scale note (0).
    pub influence: f32,
    /// Reference transposition in semitones (a multiple of 12); `None` = automatic.
    #[serde(default)]
    pub octave: Option<i32>,
    /// Key (0 = C) and scale (`pitch::scale_mask`) for the scale part of `influence`.
    #[serde(default)]
    pub key: u32,
    #[serde(default)]
    pub scale: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteEdit {
    pub start: f64,
    pub end: f64,
    /// Pitch move in cents.
    #[serde(default)]
    pub shift: f32,
    /// 0..1: how much of the pitch drift/vibrato inside the note is flattened toward its centre.
    #[serde(default)]
    pub drift: f32,
    /// Formant move in cents (0 = natural).
    #[serde(default)]
    pub formant: f32,
    /// Level change in dB.
    #[serde(default)]
    pub gain: f32,
}

/// Default crossfade at comp region boundaries, seconds.
pub const DEFAULT_XFADE: f32 = 0.010;
/// Longest crossfade a comp boundary may have, seconds.
pub const MAX_XFADE: f32 = 0.5;

/// One recorded pass. Takes are never cut: the comp only chooses which parts play.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Take {
    pub id: Id,
    pub name: String,
    /// Source with the audio.
    #[serde(rename = "ref")]
    pub source: Id,
    /// Beats from the comp's origin (comp time, see [`Comp`]).
    pub start: f64,
    /// Length in beats.
    pub len: f64,
    /// Beats into the source where the take begins.
    #[serde(default)]
    pub offset: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl Take {
    pub fn end(&self) -> f64 {
        self.start + self.len
    }
}

/// A piece of the comp: `take` plays over `[from, to)` (comp beats).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompRegion {
    pub take: Id,
    pub from: f64,
    pub to: f64,
    /// Crossfade at `from` in seconds (the comp default when absent). A region whose
    /// end is followed by silence uses it for its fade-out as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xfade: Option<f32>,
}

/// The project's lyrics notebook. Text only: nothing here plays or renders, it is saved,
/// undone and shared with the CLI like the rest of the song.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notebook {
    /// Tabs in display order.
    #[serde(default)]
    pub pages: Vec<LyricPage>,
}

/// One notebook page (a verse, a hook, …). Each line of `text` is one bar, counted from
/// `start_bar` (1-based like the playlist ruler); an empty line is a bar of rest, a line
/// wrapped in square brackets (`[Refren]`) is a header and takes no bar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricPage {
    pub id: Id,
    pub name: String,
    #[serde(default = "first_bar")]
    pub start_bar: u32,
    #[serde(default)]
    pub text: String,
}

fn first_bar() -> u32 {
    1
}

/// Limits of a notebook (a bad CLI script must not bloat every undo snapshot).
pub const NOTEBOOK_MAX_PAGES: usize = 64;
pub const NOTEBOOK_MAX_TEXT: usize = 100_000;
pub const NOTEBOOK_MAX_BAR: u32 = 9_999;

/// A take group and its comp. Times are in **comp beats**: a clip of kind `comp`
/// maps them to the timeline like an audio clip maps its source (`offset` = comp
/// beat at the clip's start), so moving, trimming or splitting the clip just works.
/// `regions` are sorted and do not overlap; gaps are silent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comp {
    pub id: Id,
    pub name: String,
    pub takes: Vec<Take>,
    pub regions: Vec<CompRegion>,
    /// Default crossfade at region boundaries, seconds.
    #[serde(default = "default_xfade")]
    pub xfade: f32,
}

fn default_xfade() -> f32 {
    DEFAULT_XFADE
}

impl Comp {
    pub fn take(&self, id: Id) -> Option<&Take> {
        self.takes.iter().find(|t| t.id == id)
    }
    /// Earliest take start and latest take end (comp beats).
    pub fn extent(&self) -> (f64, f64) {
        let a = self.takes.iter().map(|t| t.start).fold(f64::INFINITY, f64::min);
        let b = self.takes.iter().map(|t| t.end()).fold(f64::NEG_INFINITY, f64::max);
        if a.is_finite() {
            (a, b)
        } else {
            (0.0, 0.0)
        }
    }
    /// Puts `take` over `[from, to)`, replacing whatever played there, and merges
    /// neighbouring pieces of the same take. The range is clipped to the take.
    pub fn write(&mut self, take: Id, from: f64, to: f64) {
        let Some(t) = self.take(take) else { return };
        let (from, to) = (from.max(t.start), to.min(t.end()));
        if to - from <= 1e-9 {
            return;
        }
        let mut out = Vec::with_capacity(self.regions.len() + 2);
        for r in &self.regions {
            if r.to <= from || r.from >= to {
                out.push(*r);
                continue;
            }
            if r.from < from {
                out.push(CompRegion { to: from, ..*r });
            }
            if r.to > to {
                out.push(CompRegion { from: to, xfade: None, ..*r });
            }
        }
        out.push(CompRegion { take, from, to, xfade: None });
        out.sort_by(|a, b| a.from.total_cmp(&b.from));
        self.regions = merge_regions(out);
    }
    /// Removes a take and every region that plays it.
    pub fn remove_take(&mut self, id: Id) {
        self.takes.retain(|t| t.id != id);
        self.regions.retain(|r| r.take != id);
    }
}

/// Joins touching regions of the same take (input sorted by `from`).
pub fn merge_regions(sorted: Vec<CompRegion>) -> Vec<CompRegion> {
    let mut merged: Vec<CompRegion> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match merged.last_mut() {
            Some(m) if m.take == r.take && (r.from - m.to).abs() < 1e-9 => m.to = m.to.max(r.to),
            _ => merged.push(r),
        }
    }
    merged
}

impl Project {
    pub fn alloc_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// The sampler channel that plays a standard-library file: the first one there is, or a
    /// new one (appended, named and coloured after the file, rooted at the file's pitch).
    pub fn factory_channel(&mut self, file: &str) -> Option<Id> {
        let f = crate::factory::file(file)?;
        let source = crate::factory::source(self, file)?;
        if let Some(c) = self.channels.iter().find(|c| c.sample_source() == Some(source)) {
            return Some(c.id);
        }
        let id = self.alloc_id();
        let mut ch = Channel::sampler(id, f.name, crate::factory::color(f), source);
        ch.root = f.root;
        self.channels.push(ch);
        Some(id)
    }

    /// The kit a new project starts with, as in FL Studio's template: four samplers from the
    /// standard library. Returns their ids (kick, clap, hat, snare).
    pub fn add_starter_kit(&mut self) -> [Id; 4] {
        STARTER_KIT.map(|(file, name)| {
            let id = self.factory_channel(file).expect("the starter kit is in the standard library");
            if let Some(c) = self.channels.iter_mut().find(|c| c.id == id) {
                c.name = name.into();
            }
            id
        })
    }

    /// Brings a project up to date and makes it consistent: pattern lengths are derived, a
    /// project without channels gets the starter kit so the Channel Rack is never empty.
    /// Idempotent; a project that is already current is unchanged.
    pub fn migrate(&mut self) {
        for pt in self.patterns.iter_mut() {
            pt.fit_len();
        }
        if self.channels.is_empty() {
            self.add_starter_kit();
        }
        // saturator parameters from before the character saturator
        for ins in &mut self.inserts {
            for fx in ins.fx.iter_mut().filter(|f| f.kind == FxKind::Sat) {
                if crate::dsp::saturator::migrate_params(&mut fx.params) {
                    fx.preset = None;
                }
            }
        }
        // ids the file used must stay taken
        let max = self.channels.iter().map(|c| c.id).chain(self.sources.iter().map(|s| s.id)).max().unwrap_or(0);
        if max >= self.next_id {
            self.next_id = max + 1;
        }
    }

    /// Reads a project from a file's JSON. Files from before the standard library named
    /// built-in synthesised drums (`"kind":"inst"` channels, or `inst` on notes before there
    /// were channels); those now play the closest standard-library sample.
    pub fn from_json(mut v: serde_json::Value) -> Result<Self, serde_json::Error> {
        use serde_json::{json, Value};
        const LEGACY: [(&str, &str); 4] = [("kick", "kick-808-short.flac"), ("clap", "clap-808.flac"), ("hat", "hat-808-closed.flac"), ("bass", "bass-808-short.flac")];
        let file_of = |inst: &Value| LEGACY.iter().find(|(k, _)| inst.as_str() == Some(k)).map(|(_, f)| *f);
        // (channel index or None for "notes of this instrument", file)
        let mut chans: Vec<(usize, &str)> = vec![];
        let mut loose: Vec<&str> = vec![];
        if let Some(list) = v.get("channels").and_then(Value::as_array) {
            for (i, c) in list.iter().enumerate() {
                if c.get("kind").and_then(Value::as_str) == Some("inst") {
                    chans.push((i, file_of(&c["inst"]).unwrap_or(LEGACY[0].1)));
                }
            }
        }
        for pt in v.get("patterns").and_then(Value::as_array).into_iter().flatten() {
            for n in pt.get("notes").and_then(Value::as_array).into_iter().flatten() {
                if n.get("ch").is_none() {
                    if let Some(f) = n.get("inst").and_then(file_of) {
                        if !loose.contains(&f) {
                            loose.push(f);
                        }
                    }
                }
            }
        }
        if chans.is_empty() && loose.is_empty() {
            return serde_json::from_value(v);
        }
        // park the legacy channels as samplers of source 0, read, then give them their sample
        for (i, _) in &chans {
            let c = &mut v["channels"][*i];
            c["kind"] = json!("sample");
            c["source"] = json!(0);
        }
        for pt in v.get_mut("patterns").and_then(Value::as_array_mut).into_iter().flatten() {
            for n in pt.get_mut("notes").and_then(Value::as_array_mut).into_iter().flatten() {
                if n.get("ch").is_none() {
                    // a negative-free marker: 1-based index into `loose`, resolved below
                    let k = n.get("inst").and_then(file_of).and_then(|f| loose.iter().position(|x| *x == f));
                    n["ch"] = json!(k.map(|k| k as u64 + 1).unwrap_or(0));
                    n["legacy"] = json!(true);
                }
            }
        }
        let legacy_notes: Vec<Vec<bool>> = v
            .get("patterns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|pt| pt.get("notes").and_then(Value::as_array).into_iter().flatten().map(|n| n.get("legacy").is_some()).collect())
            .collect();
        let mut p: Project = serde_json::from_value(v)?;
        let top = p.channels.iter().map(|c| c.id).chain(p.sources.iter().map(|s| s.id)).max().unwrap_or(0);
        p.next_id = p.next_id.max(top + 1);
        for (i, file) in chans {
            let (source, f) = (crate::factory::source(&mut p, file), crate::factory::file(file));
            if let (Some(source), Some(f), Some(c)) = (source, f, p.channels.get_mut(i)) {
                // the synthesised bass held its note; the drums rang out
                let sampler = SamplerState { gate: file == LEGACY[3].1, ..Default::default() };
                c.src = ChannelSrc::Sample { source, sampler };
                c.root = f.root;
            }
        }
        let ids: Vec<Id> = loose.iter().map(|f| p.factory_channel(f).unwrap_or(0)).collect();
        for (id, file) in ids.iter().zip(&loose) {
            if let Some(ChannelSrc::Sample { sampler, .. }) = p.channels.iter_mut().find(|c| c.id == *id).map(|c| &mut c.src) {
                sampler.gate = *file == LEGACY[3].1;
            }
        }
        for (pt, flags) in p.patterns.iter_mut().zip(legacy_notes) {
            for (n, legacy) in pt.notes.iter_mut().zip(flags) {
                if legacy {
                    n.ch = (n.ch as usize).checked_sub(1).and_then(|k| ids.get(k)).copied().unwrap_or(0);
                }
            }
            pt.notes.retain(|n| n.ch != 0);
        }
        Ok(p)
    }

    pub fn comp(&self, id: Id) -> Option<&Comp> {
        self.comps.iter().find(|c| c.id == id)
    }

    pub fn insert(&self, id: Id) -> Option<&Insert> {
        self.inserts.iter().find(|m| m.id == id)
    }

    pub fn find_fx(&self, fx_id: Id) -> Option<(&Insert, &Fx)> {
        self.inserts
            .iter()
            .find_map(|m| m.fx.iter().find(|f| f.id == fx_id).map(|f| (m, f)))
    }

    /// End of the last clip, rounded up to a bar; at least one bar.
    pub fn song_end(&self) -> f64 {
        let end = self.clips.iter().map(|c| c.start + c.len).fold(4.0, f64::max);
        (end / 4.0).ceil() * 4.0
    }

    /// The small fixed song tests and benchmarks build on.
    pub fn sketch() -> Self {
        crate::demo::sketch_project()
    }

    pub fn empty() -> Self {
        crate::demo::empty_project()
    }

    /// The empty project with track n sent to mixer insert n (tests and benchmarks).
    pub fn routed() -> Self {
        crate::demo::routed_project()
    }
}
