//! Plugin hosting glue: the engine's `PluginFactory` for every hosted format (VST3
//! everywhere, LV2 on Linux), per-effect and per-instrument-channel handles (by id: effect
//! and channel ids never collide), scanning, editors.
//!
//! Threading: VST3 instantiate/params/state block on a UI-thread job, so they are only
//! ever reached from async commands or worker threads — never from the UI thread itself.
//! LV2 work runs on the calling thread.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::Manager;
use wave_engine::{Id, Instrument, PluginFactory, PluginRef, Processor};

/// A live hosted effect's non-real-time side.
#[derive(Clone)]
enum Handle {
    Vst3(wave_vst3::Vst3Handle),
    Lv2(wave_lv2::Lv2Handle),
}

impl Handle {
    fn params(&self) -> Vec<ParamInfo> {
        match self {
            Handle::Vst3(h) => h.params().into_iter().map(ParamInfo::Vst3).collect(),
            Handle::Lv2(h) => h.params().into_iter().map(ParamInfo::Lv2).collect(),
        }
    }
    fn set_param(&self, id: u32, v: f64) -> Result<(), String> {
        match self {
            Handle::Vst3(h) => h.set_param(id, v).map_err(|e| e.to_string()),
            Handle::Lv2(h) => h.set_param(id, v),
        }
    }
    fn state(&self) -> Result<Vec<u8>, String> {
        match self {
            Handle::Vst3(h) => h.get_state().map_err(|e| e.to_string()),
            Handle::Lv2(h) => h.get_state(),
        }
    }
    fn latency(&self) -> usize {
        match self {
            Handle::Vst3(h) => h.latency(),
            Handle::Lv2(h) => h.latency(),
        }
    }
}

/// One parameter of a hosted plugin as `plugin_params` reports it: `{id, name, value,
/// display, def}` for every format (values normalized 0..1); LV2 adds the port's real
/// range and kind (`symbol, real, min, max, unit, steps, toggle, integer, log, options`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ParamInfo {
    Vst3(wave_vst3::ParamInfo),
    Lv2(wave_lv2::ParamInfo),
}

impl ParamInfo {
    pub fn id(&self) -> u32 {
        match self {
            ParamInfo::Vst3(p) => p.id,
            ParamInfo::Lv2(p) => p.id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            ParamInfo::Vst3(p) => &p.name,
            ParamInfo::Lv2(p) => &p.name,
        }
    }
}

/// One installed effect, as `plugin_scan` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanEntry {
    /// `vst3` or `lv2`.
    pub format: String,
    pub path: String,
    /// VST3 class id, LV2 plugin URI.
    pub cid: String,
    pub name: String,
    pub vendor: String,
    pub category: String,
    /// Why it cannot be used here (LV2: a host feature it needs); absent when it can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
}

/// What `plugin_scan` returns: the effects, the instruments, and per format why it is
/// unavailable here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub plugins: Vec<ScanEntry>,
    /// Plugin instruments for the Channel Rack (VST3).
    pub instruments: Vec<ScanEntry>,
    /// Format → the sentence explaining why it is not available (LV2 without liblilv).
    pub unavailable: HashMap<String, String>,
}

/// Tells the VST3 scan where the plugins that ship with WAVE are (`plugins/` among the app's
/// resources: WAVE Demo Synth). `resource_dir`: the app's, when a Tauri app runs; otherwise
/// (the CLI on a file) next to the executable, or the Linux packages' `/usr/lib/WAVE`.
pub fn register_bundled(resource_dir: Option<std::path::PathBuf>) {
    let dirs: Vec<std::path::PathBuf> = match resource_dir {
        Some(dir) => vec![dir],
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .map(|dir| vec![dir.clone(), dir.join("..").join("lib").join("WAVE")])
            .unwrap_or_default(),
    };
    for dir in dirs {
        wave_vst3::add_bundled_dir(dir.join("plugins"));
    }
}

/// The error of a plugin that has no window of its own (WAVE Demo Synth): the UI matches its
/// prefix (`wave_engine::errors::ERR_NO_EDITOR`, pinned by `src/ui_prefixes_tests.rs`).
pub const NO_EDITOR: &str = "NO_EDITOR: this plugin has no window of its own; its parameters are listed below";

fn vst3_editor_error(e: wave_vst3::Vst3Error) -> String {
    match e {
        // the UI recognises this one: it turns the button into a note instead of an error
        wave_vst3::Vst3Error::NoEditor => NO_EDITOR.to_string(),
        e => e.to_string(),
    }
}

pub struct Plugins {
    handles: Mutex<HashMap<Id, Handle>>,
    known: Mutex<Option<ScanResult>>,
    /// This session's live plugins, mirrored into the marker file (`live_loaded`).
    live: Mutex<HashMap<Id, CrashSuspect>>,
    /// Plugins still live when the last session ended: the suspects of a crash (a session
    /// that exits normally leaves the marker file empty).
    crash_suspects: Mutex<Vec<CrashSuspect>>,
    /// Where the live marker file is (`None`: nowhere — a machine without WAVE's data
    /// folder, and every test).
    marker: Option<std::path::PathBuf>,
}

/// A plugin to ask the user about: it was loaded when WAVE last died without exiting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashSuspect {
    pub format: String,
    /// VST3 class id, LV2 plugin URI.
    pub cid: String,
    pub name: String,
    /// The module (bundle) path, as the scan block list knows it.
    pub path: String,
}

/// The file naming every live plugin; its absence or emptiness means a normal exit.
#[cfg(not(test))]
fn live_marker() -> Option<std::path::PathBuf> {
    wave_plugin_scan::data_file("live-plugins.json")
}

fn read_marker_at(path: &std::path::Path) -> Vec<CrashSuspect> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_marker_at(path: &std::path::Path, live: &[CrashSuspect]) {
    if let Ok(t) = serde_json::to_string(live) {
        let _ = std::fs::write(path, t);
    }
}

struct Factory(Arc<Plugins>);

impl PluginFactory for Factory {
    fn create(&self, fx_id: Id, p: &PluginRef, sr: f32, max_block: usize, live: bool) -> Result<Box<dyn Processor>, String> {
        let state = p.state.as_deref().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
        let (proc, handle) = match p.format.as_str() {
            PluginRef::LV2 => {
                let (proc, h) =
                    wave_lv2::instantiate(&p.cid, state.as_deref(), sr, max_block).map_err(|e| format!("{} (LV2): {e}", p.name))?;
                (proc, Handle::Lv2(h))
            }
            PluginRef::VST3 => {
                let path = self.0.vst3_path(p);
                let (proc, h) =
                    wave_vst3::instantiate(&path, &p.cid, state.as_deref(), sr, max_block).map_err(|e| format!("{}: {e}", p.name))?;
                (proc, Handle::Vst3(h))
            }
            other => return Err(format!("{}: WAVE does not host `{other}` plugins", p.name)),
        };
        if live {
            self.0.handles.lock().insert(fx_id, handle);
            self.0.live_loaded(fx_id, p);
        }
        Ok(proc)
    }
    fn create_instrument(&self, chan_id: Id, p: &PluginRef, sr: f32, max_block: usize, live: bool) -> Result<Box<dyn Instrument>, String> {
        if p.format != PluginRef::VST3 {
            return Err(format!("{}: {} instruments are not supported (VST3 only)", p.name, p.format_label()));
        }
        let state = p.state.as_deref().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
        let path = self.0.vst3_path(p);
        let (inst, h) = wave_vst3::instantiate_instrument(&path, &p.cid, state.as_deref(), sr, max_block, !live)
            .map_err(|e| format!("{}: {e}", p.name))?;
        if live {
            self.0.handles.lock().insert(chan_id, Handle::Vst3(h));
            self.0.live_loaded(chan_id, p);
        }
        Ok(inst)
    }
    fn release(&self, fx_id: Id) {
        self.0.handles.lock().remove(&fx_id);
        self.0.live_released(fx_id);
    }
    fn state(&self, fx_id: Id) -> Option<String> {
        let h = self.0.handles.lock().get(&fx_id).cloned()?;
        match h.state() {
            Ok(b) => Some(base64::engine::general_purpose::STANDARD.encode(b)),
            Err(e) => {
                log::warn!("plugin {fx_id}: its state could not be saved ({e}); the last saved state is kept");
                None
            }
        }
    }
    fn has_plugin_param(&self, fx_id: Id, param_id: u32) -> bool {
        self.0.params(fx_id).ok().is_some_and(|ps| ps.iter().any(|p| p.id() == param_id))
    }
    fn latency(&self, fx_id: Id) -> Option<usize> {
        let h = self.0.handles.lock().get(&fx_id).cloned()?;
        Some(h.latency())
    }
}

impl Plugins {
    pub fn new() -> Arc<Self> {
        // Tests must not read or replace the user's marker (a running app's crash evidence).
        #[cfg(test)]
        let marker = None;
        #[cfg(not(test))]
        let marker = live_marker();
        Arc::new(Self::open(marker))
    }

    /// Whatever the last session left in the marker file names its crash suspects; this
    /// session starts clean (entries come back as plugins load).
    fn open(marker: Option<std::path::PathBuf>) -> Self {
        let crash_suspects = marker.as_ref().map(|p| read_marker_at(p)).unwrap_or_default();
        if let Some(p) = &marker {
            write_marker_at(p, &[]);
        }
        Plugins {
            handles: Mutex::new(HashMap::new()),
            known: Mutex::new(None),
            live: Mutex::new(HashMap::new()),
            crash_suspects: Mutex::new(crash_suspects),
            marker,
        }
    }

    /// A plugin loaded for real: it goes into the live marker, so a crash while it is
    /// loaded points here the next time WAVE starts.
    fn live_loaded(&self, id: Id, p: &PluginRef) {
        let path = match p.format.as_str() {
            PluginRef::VST3 => self.vst3_path(p),
            _ => p.path.clone(),
        };
        let suspect = CrashSuspect { format: p.format.clone(), cid: p.cid.clone(), name: p.name.clone(), path };
        let mut live = self.live.lock();
        live.insert(id, suspect);
        self.write_live(&live);
    }

    /// A plugin's instance went away: out of the live marker with it.
    fn live_released(&self, id: Id) {
        let mut live = self.live.lock();
        if live.remove(&id).is_some() {
            self.write_live(&live);
        }
    }

    /// The marker file follows the live set.
    fn write_live(&self, live: &HashMap<Id, CrashSuspect>) {
        if let Some(path) = &self.marker {
            let mut all: Vec<CrashSuspect> = live.values().cloned().collect();
            all.sort_by(|a, b| a.cid.cmp(&b.cid).then_with(|| a.path.cmp(&b.path)));
            write_marker_at(path, &all);
        }
    }

    /// The plugins that were live when the last session ended without exiting.
    pub fn crash_suspects(&self) -> Vec<CrashSuspect> {
        self.crash_suspects.lock().clone()
    }

    /// A clean exit: nothing is live (the instances are dropped with the engine), nothing
    /// is suspected, and the marker file says so. An abnormal exit never reaches this, so
    /// the next start still sees what was loaded.
    pub fn session_ended(&self) {
        let mut live = self.live.lock();
        live.clear();
        self.write_live(&live);
        drop(live);
        self.crash_suspects.lock().clear();
    }

    /// Keep a plugin's module out of future scans (the user asked, after a crash).
    pub fn block(&self, format: &str, path: &str) -> Result<(), String> {
        let file = wave_plugin_scan::data_file(&format!("{format}-blocklist.txt")).ok_or("WAVE has no data folder for the block list")?;
        self.block_at(file, path)
    }

    /// The block list's whole read-modify-write, at an explicit file (the production one
    /// comes from `data_file`). Every call reads the whole file and rewrites it, so the
    /// calls are serialized: two overlapping requests would each write from a stale copy,
    /// and the loser's entry would vanish.
    fn block_at(&self, file: std::path::PathBuf, path: &str) -> Result<(), String> {
        static BLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = BLOCK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let module = std::path::Path::new(path);
        wave_plugin_scan::BlockList::with_file(&file).block(module);
        // `BlockList::block` persists best-effort: confirm the entry really landed, or the
        // UI would report a plugin as blocked that the next scan opens again.
        if wave_plugin_scan::BlockList::with_file(&file).contains(module) {
            Ok(())
        } else {
            Err(format!("could not write the block list ({}): {} stays scannable", file.display(), path))
        }
    }

    pub fn factory(self: &Arc<Self>) -> Arc<dyn PluginFactory> {
        Arc::new(Factory(self.clone()))
    }

    /// Every installed effect of every format this platform hosts (VST3; LV2 on Linux).
    /// A cold `rescan` is slow: call it from a worker thread.
    pub fn scan(&self, rescan: bool) -> ScanResult {
        let vst3 = |p: wave_vst3::PluginInfo| ScanEntry {
            format: PluginRef::VST3.into(),
            path: p.path,
            cid: p.cid,
            name: p.name,
            vendor: p.vendor,
            category: p.category,
            unsupported: None,
        };
        let mut plugins: Vec<ScanEntry> = wave_vst3::scan(rescan).into_iter().map(vst3).collect();
        // the effects' scan opened whatever was new: this reads the same cache
        let instruments: Vec<ScanEntry> = wave_vst3::scan_instruments(false).into_iter().map(vst3).collect();
        let mut unavailable = HashMap::new();
        if cfg!(target_os = "linux") {
            match wave_lv2::scan(rescan) {
                Ok(list) => plugins.extend(list.into_iter().map(|p| ScanEntry {
                    format: PluginRef::LV2.into(),
                    path: p.path,
                    cid: p.cid,
                    name: p.name,
                    vendor: p.vendor,
                    category: p.category,
                    unsupported: p.unsupported,
                })),
                Err(e) => {
                    unavailable.insert(PluginRef::LV2.to_owned(), e);
                }
            }
        }
        let r = ScanResult { plugins, instruments, unavailable };
        *self.known.lock() = Some(r.clone());
        r
    }

    /// A scanned effect: from the last scan, else a scan that opens nothing new.
    pub fn info(&self, format: &str, path: &str, cid: &str) -> Option<ScanEntry> {
        self.find(format, path, cid, |r| &r.plugins)
    }

    /// A scanned instrument (VST3): from the last scan, else a scan that opens nothing new.
    pub fn instrument(&self, path: &str, cid: &str) -> Option<ScanEntry> {
        self.find(PluginRef::VST3, path, cid, |r| &r.instruments)
    }

    fn find(&self, format: &str, path: &str, cid: &str, list: fn(&ScanResult) -> &Vec<ScanEntry>) -> Option<ScanEntry> {
        let find = |r: &ScanResult| {
            list(r)
                .iter()
                .find(|p| {
                    p.format == format
                        && match format {
                            PluginRef::LV2 => p.cid == cid,
                            _ => p.path == path && p.cid.eq_ignore_ascii_case(cid),
                        }
                })
                .cloned()
        };
        if let Some(x) = self.known.lock().as_ref().and_then(find) {
            return Some(x);
        }
        find(&self.scan(false))
    }

    /// Where to load a project's VST3 from: its saved path, or, when nothing is there any
    /// more, the installed plugin with the same class id. A plugin that comes with WAVE moves
    /// with the installation (another user or computer, a new AppImage mount on every start),
    /// and a project must not lose it for that.
    fn vst3_path(&self, p: &PluginRef) -> String {
        if std::path::Path::new(&p.path).exists() {
            return p.path.clone();
        }
        let find = |r: &ScanResult| {
            r.instruments
                .iter()
                .chain(&r.plugins)
                .find(|e| e.format == PluginRef::VST3 && e.cid.eq_ignore_ascii_case(&p.cid) && std::path::Path::new(&e.path).exists())
                .map(|e| e.path.clone())
        };
        let known = self.known.lock().as_ref().map(find);
        known.unwrap_or_else(|| find(&self.scan(false))).unwrap_or_else(|| p.path.clone())
    }

    fn handle(&self, fx: Id) -> Result<Handle, String> {
        self.handles.lock().get(&fx).cloned().ok_or_else(|| format!("{fx} is not a loaded plugin effect or instrument"))
    }

    pub fn params(&self, fx: Id) -> Result<Vec<ParamInfo>, String> {
        Ok(self.handle(fx)?.params())
    }

    /// A live plugin's current state (base64, as a project stores it): what a copy of the
    /// effect or instrument channel needs to sound the same before the project is saved.
    pub fn state(&self, fx: Id) -> Result<String, String> {
        let b = self.handle(fx)?.state()?;
        Ok(base64::engine::general_purpose::STANDARD.encode(b))
    }

    pub fn set_param(&self, fx: Id, id: u32, v: f64) -> Result<(), String> {
        if !(0.0..=1.0).contains(&v) {
            return Err("plugin parameter values are normalised 0..1".into());
        }
        self.handle(fx)?.set_param(id, v)
    }

    /// Whether the plugin's own window is open (the user may have closed it with its X).
    pub fn editor_open(&self, fx: Id) -> Result<bool, String> {
        Ok(match self.handle(fx)? {
            Handle::Vst3(h) => h.is_editor_open(),
            Handle::Lv2(h) => h.is_editor_open(),
        })
    }

    /// Open (or bring forward) or close the plugin's own window, under WAVE's header showing
    /// `label`, pinned on top of the app when `pinned`. The window lives on the UI thread; this
    /// waits for it.
    pub fn editor(
        &self,
        app: &tauri::AppHandle,
        fx: Id,
        open: bool,
        label: wave_plugin_windows::Label,
        pinned: bool,
    ) -> Result<(), String> {
        let h = self.handle(fx)?;
        if open {
            let has = match &h {
                Handle::Vst3(h) => h.has_editor(),
                Handle::Lv2(h) => h.has_editor(),
            };
            if !has {
                return Err(NO_EDITOR.into());
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let a = app.clone();
        app.run_on_main_thread(move || {
            let w = a.state::<wave_plugin_windows::Service>();
            let r = match (&h, open) {
                (Handle::Vst3(h), true) => h.open_editor(&w, &label).map_err(vst3_editor_error),
                (Handle::Vst3(h), false) => h.close_editor().map_err(vst3_editor_error),
                (Handle::Lv2(h), true) => h.open_editor(&w, &label),
                (Handle::Lv2(h), false) => h.close_editor(),
            };
            if open && r.is_ok() {
                match &h {
                    Handle::Vst3(h) => {
                        let _ = h.set_editor_pinned(pinned);
                    }
                    Handle::Lv2(h) => h.set_editor_pinned(pinned),
                }
            }
            let _ = tx.send(r);
        })
        .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plugins that were live when the last session died are the next session's crash
    /// suspects; a clean exit names none, and a released plugin leaves the marker.
    #[test]
    fn crash_suspects_come_from_the_marker_a_crashed_session_left_behind() {
        let dir = std::env::temp_dir().join(format!("wave-suspects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("live-plugins.json");
        let vitalium = CrashSuspect {
            format: PluginRef::VST3.into(),
            cid: "Vitalium".repeat(8)[..32].to_owned(),
            name: "Vitalium".into(),
            path: "/vst3/Vitalium.vst3".into(),
        };
        // the "crashed" session left Vitalium in the marker
        write_marker_at(&marker, std::slice::from_ref(&vitalium));
        let plugins = Plugins::open(Some(marker.clone()));
        assert_eq!(plugins.crash_suspects(), vec![vitalium.clone()]);
        // this session starts clean: the marker no longer names anybody
        assert_eq!(read_marker_at(&marker), vec![]);
        // a live load lands in the marker, and its release takes it out again
        let p = PluginRef {
            format: PluginRef::VST3.into(),
            path: "/vst3/Vitalium.vst3".into(),
            cid: vitalium.cid.clone(),
            name: "Vitalium".into(),
            vendor: String::new(),
            state: None,
        };
        plugins.live_loaded(7, &p);
        assert_eq!(read_marker_at(&marker), vec![vitalium.clone()]);
        plugins.live_released(7);
        assert_eq!(read_marker_at(&marker), vec![]);
        // a normal exit empties the marker even while plugins are still live (the instances
        // die with the process): the next start must suspect nobody
        plugins.live_loaded(8, &p);
        assert_eq!(read_marker_at(&marker), vec![vitalium.clone()]);
        plugins.session_ended();
        assert_eq!(read_marker_at(&marker), vec![]);
        assert!(plugins.crash_suspects().is_empty());
        // a refused load (the factory's error path) never entered the marker
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A block-list write that could not land is reported, not pretended (the UI would say
    /// "Blocked" while the next scan opens the plugin again).
    #[test]
    fn block_reports_a_persistence_failure_instead_of_ok() {
        let plugins = Plugins::open(None);
        // a directory in the list's place: nothing can be written there
        let dir = std::env::temp_dir().join(format!("wave-block-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vst3-blocklist.txt")).unwrap();
        let err = plugins.block_at(dir.join("vst3-blocklist.txt"), "/vst3/BadActor.vst3");
        assert!(err.is_err(), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn block_lands_in_the_list_and_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!("wave-block-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plugins = Plugins::open(None);
        plugins.block_at(dir.join("vst3-blocklist.txt"), "/vst3/BadActor.vst3").unwrap();
        let module = std::path::Path::new("/vst3/BadActor.vst3");
        assert!(wave_plugin_scan::BlockList::with_file(dir.join("vst3-blocklist.txt")).contains(module));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_entries_and_params_serialise_in_the_shapes_the_ui_expects() {
        let e = ScanEntry {
            format: "lv2".into(),
            path: "/usr/lib/lv2/amp.lv2".into(),
            cid: "http://example.org/amp".into(),
            name: "Amp".into(),
            vendor: "Ex".into(),
            category: "Amplifier".into(),
            unsupported: None,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["format"], "lv2");
        assert!(j.get("unsupported").is_none());
        let v = ParamInfo::Vst3(wave_vst3::ParamInfo { id: 3, name: "Gain".into(), value: 0.5, display: "-6 dB".into(), def: 1.0 });
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            serde_json::json!({"id": 3, "name": "Gain", "value": 0.5, "display": "-6 dB", "def": 1.0})
        );
        let spec = wave_lv2::ParamSpec::new(-24.0, 12.0, 0.0, false, false, false, false, vec![], "dB".into());
        let l = ParamInfo::Lv2(wave_lv2::ParamInfo::new(4, "gain", "Gain".into(), &spec, -6.0));
        let j = serde_json::to_value(&l).unwrap();
        assert_eq!((j["id"].as_u64(), j["unit"].as_str(), j["min"].as_f64()), (Some(4), Some("dB"), Some(-24.0)));
        assert_eq!(l.id(), 4);
        assert_eq!(l.name(), "Gain");
    }

    /// The whole path with a real LV2 plugin (Linux, liblilv and `swh-lv2` installed; CI
    /// sets `WAVE_REQUIRE_LV2=1`): scan → slot → live instance → parameter → offline render
    /// at the set gain → save → reopen in a fresh engine → the same sound; and plugin
    /// delay compensation from a plugin that reports latency.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_lv2_effect_renders_saves_and_reopens_with_its_settings() {
        use wave_engine::{Engine, Op, Project};
        const AMP: &str = "http://plugin.org.uk/swh-plugins/amp";
        let required = std::env::var_os("WAVE_REQUIRE_LV2").is_some_and(|v| v == "1");
        let plugins = Plugins::new();
        let scan = plugins.scan(false);
        let Some(amp) = scan.plugins.iter().find(|p| p.cid == AMP).cloned() else {
            assert!(!required, "WAVE_REQUIRE_LV2=1 but {AMP} is not available: {:?}", scan.unavailable);
            eprintln!("skipping: {AMP} not installed ({:?})", scan.unavailable);
            return;
        };
        let dir = std::env::temp_dir().join(format!("wave-lv2-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Project::routed();
        for m in p.inserts.iter_mut() {
            m.fx.clear();
        }
        let e = Engine::new(p, plugins.factory());
        e.save(&dir.join("song.wave")).unwrap();
        let wav = dir.join("sine.wav");
        let sine: Vec<f32> = (0..48_000).map(|i| 0.25 * (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin()).collect();
        wave_engine::sources::write_wav(&wav, &sine, 48_000).unwrap();
        e.import_audio(&[wav], Some((0, 0.0))).unwrap();
        let rms = |x: &[f32]| (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let dry = rms(&e.render(0.0, 1.0, 0.0, 48_000).unwrap());
        assert!(dry > 0.05, "the clip plays ({dry})");

        let ins = e.project().tracks[0].insert;
        let fx = wave_engine::catalog::new_fx(7000, wave_engine::FxKind::Plugin, 120.0);
        let fx = wave_engine::Fx {
            plugin: Some(PluginRef {
                format: amp.format.clone(),
                path: amp.path.clone(),
                cid: amp.cid.clone(),
                name: amp.name.clone(),
                vendor: amp.vendor.clone(),
                state: None,
            }),
            ..fx
        };
        e.apply(&[Op::PutFx { insert_id: ins, fx, index: None }], false, None).unwrap();
        let _rt = e.create_rt(48_000.0, 256);
        assert!(e.state().fx_errors.is_empty(), "{:?}", e.state().fx_errors);
        let gain = match plugins.params(7000).unwrap().into_iter().find(|p| p.name().to_lowercase().contains("gain")) {
            Some(ParamInfo::Lv2(p)) => p,
            other => panic!("no LV2 gain parameter: {other:?}"),
        };
        plugins.set_param(7000, gain.id, (-6.0 - gain.min) / (gain.max - gain.min)).unwrap();
        let want = 10f64.powf(-6.0 / 20.0);
        let wet = rms(&e.render(0.0, 1.0, 0.0, 48_000).unwrap());
        assert!((wet / dry - want).abs() < 0.01, "rendered gain {} want {want}", wet / dry);

        // the setting lives in the project file
        let saved = dir.join("saved.wave");
        e.save(&saved).unwrap();
        let st = e.project().find_fx(7000).unwrap().1.plugin.clone().unwrap().state;
        assert!(st.is_some_and(|s| !s.is_empty()), "no state saved");
        let plugins2 = Plugins::new();
        let e2 = Engine::new(Project::empty(), plugins2.factory());
        e2.open(&saved).unwrap();
        let again = rms(&e2.render(0.0, 1.0, 0.0, 48_000).unwrap());
        assert!((again / dry - want).abs() < 0.01, "reopened gain {} want {want}", again / dry);

        // a plugin that reports latency is compensated (x42 dpl has look-ahead)
        if let Some(dpl) = scan.plugins.iter().find(|p| p.cid.starts_with("http://gareus.org/oss/lv2/dpl#stereo")) {
            let ins1 = e.project().tracks[1].insert;
            let fx = wave_engine::Fx {
                plugin: Some(PluginRef {
                    format: dpl.format.clone(),
                    path: dpl.path.clone(),
                    cid: dpl.cid.clone(),
                    name: dpl.name.clone(),
                    vendor: dpl.vendor.clone(),
                    state: None,
                }),
                ..wave_engine::catalog::new_fx(7001, wave_engine::FxKind::Plugin, 120.0)
            };
            e.apply(&[Op::PutFx { insert_id: ins1, fx, index: None }], false, None).unwrap();
            let lat = Factory(plugins.clone()).latency(7001).unwrap();
            eprintln!("x42 dpl reports {lat} samples");
            assert!(lat > 0, "dpl should report its look-ahead");
            assert_eq!(e.playback_latency(), lat, "the compensation follows the plugin");
        } else {
            assert!(!required, "WAVE_REQUIRE_LV2=1 but x42 dpl is not installed");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plugin_of_an_unknown_format_is_a_load_error_not_a_panic() {
        let f = Plugins::new().factory();
        let p =
            PluginRef { format: "clap".into(), path: "x".into(), cid: "y".into(), name: "Z".into(), vendor: String::new(), state: None };
        let err = f.create(1, &p, 48_000.0, 256, true).err().unwrap();
        assert!(err.contains("does not host `clap`"), "{err}");
        // and an LV2 that is not installed (or a platform without LV2) is a sentence too
        let p = PluginRef {
            format: "lv2".into(),
            path: String::new(),
            cid: "urn:wave:not-installed".into(),
            name: "Gone".into(),
            vendor: String::new(),
            state: None,
        };
        let err = f.create(2, &p, 48_000.0, 256, true).err().unwrap();
        assert!(err.starts_with("Gone (LV2): "), "{err}");
        assert!(f.latency(2).is_none() && f.state(2).is_none());
    }
}
