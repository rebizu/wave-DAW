//! VST3 hosting glue: the engine's `PluginFactory`, per-effect handles, editors.
//!
//! Threading (see `wave_vst3` docs): instantiate/params/state block on a UI-thread
//! job, so they are only ever reached from async commands or worker threads —
//! never from the UI thread itself.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::Manager;
use wave_engine::{Id, PluginFactory, PluginRef, Processor};

pub use wave_vst3::ParamInfo;

pub struct Plugins {
    handles: Mutex<HashMap<Id, wave_vst3::Vst3Handle>>,
    known: Mutex<Vec<wave_vst3::PluginInfo>>,
}

struct Factory(Arc<Plugins>);

impl PluginFactory for Factory {
    fn create(&self, fx_id: Id, p: &PluginRef, sr: f32, max_block: usize, live: bool) -> Result<Box<dyn Processor>, String> {
        let state = p.state.as_deref().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
        let (proc, handle) = wave_vst3::instantiate(&p.path, &p.cid, state.as_deref(), sr, max_block).map_err(|e| format!("{}: {e}", p.name))?;
        if live {
            self.0.handles.lock().insert(fx_id, handle);
        }
        Ok(proc)
    }
    fn release(&self, fx_id: Id) {
        self.0.handles.lock().remove(&fx_id);
    }
    fn state(&self, fx_id: Id) -> Option<String> {
        let h = self.0.handles.lock().get(&fx_id).cloned()?;
        h.get_state().ok().map(|b| base64::engine::general_purpose::STANDARD.encode(b))
    }
    fn has_plugin_param(&self, fx_id: Id, param_id: u32) -> bool {
        self.0.params(fx_id).ok().is_some_and(|ps| ps.iter().any(|p| p.id == param_id))
    }
}

impl Plugins {
    pub fn new() -> Arc<Self> {
        Arc::new(Plugins { handles: Mutex::new(HashMap::new()), known: Mutex::new(vec![]) })
    }

    pub fn factory(self: &Arc<Self>) -> Arc<dyn PluginFactory> {
        Arc::new(Factory(self.clone()))
    }

    pub fn scan(&self, rescan: bool) -> Vec<Value> {
        let list = wave_vst3::scan(rescan);
        *self.known.lock() = list.clone();
        list.into_iter().map(|p| json!(p)).collect()
    }

    /// (name, vendor) of a scanned plugin.
    pub fn info(&self, path: &str, cid: &str) -> Option<(String, String)> {
        let find = |l: &[wave_vst3::PluginInfo]| l.iter().find(|p| p.path == path && p.cid.eq_ignore_ascii_case(cid)).map(|p| (p.name.clone(), p.vendor.clone()));
        if let Some(x) = find(&self.known.lock()) {
            return Some(x);
        }
        let list = wave_vst3::scan(false);
        let r = find(&list);
        *self.known.lock() = list;
        r
    }

    fn handle(&self, fx: Id) -> Result<wave_vst3::Vst3Handle, String> {
        self.handles.lock().get(&fx).cloned().ok_or_else(|| format!("effect {fx} is not a loaded VST3 plugin"))
    }

    pub fn params(&self, fx: Id) -> Result<Vec<ParamInfo>, String> {
        Ok(self.handle(fx)?.params())
    }

    pub fn set_param(&self, fx: Id, id: u32, v: f64) -> Result<(), String> {
        if !(0.0..=1.0).contains(&v) {
            return Err("VST3 parameter values are normalised 0..1".into());
        }
        self.handle(fx)?.set_param(id, v).map_err(|e| e.to_string())
    }

    pub fn editor(&self, app: &tauri::AppHandle, fx: Id, open: bool) -> Result<(), String> {
        let h = self.handle(fx)?;
        if open && !h.has_editor() {
            return Err("this plugin has no editor; use the parameter list".into());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let a = app.clone();
        app.run_on_main_thread(move || {
            let r = if open {
                let w = a.state::<wave_plugin_windows::Service>();
                h.open_editor(&w)
            } else {
                h.close_editor()
            };
            let _ = tx.send(r.map_err(|e| e.to_string()));
        })
        .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
}
