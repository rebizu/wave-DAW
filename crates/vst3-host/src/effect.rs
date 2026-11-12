//! One hosted VST3 effect: its UI-thread instance, the RT [`Vst3Effect`] and
//! the non-RT [`Vst3Handle`].
//!
//! # Instantiation (UI thread)
//!
//! ```text
//!  factory->setHostContext(host)                   [IPluginFactory3 only]
//!  factory->createInstance(cid, IComponent)
//!  component->initialize(host)
//!  component->getControllerClassId(&ccid)
//!      ccid == cid or none  -> single component: query IEditController off it
//!      otherwise            -> createInstance(ccid) + initialize + connect the two IConnectionPoints
//!  controller->setComponentHandler(host)
//!  state given?   component->setState, controller->setComponentState + setState
//!  otherwise      component->getState -> controller->setComponentState   (editor shows real values)
//!  check: IAudioProcessor, 32-bit float, stereo main in + out
//!  setBusArrangements(stereo) -> setupProcessing -> activateBus -> setActive(1) -> setProcessing(1)
//! ```
//!
//! # Threading
//!
//! The [`Instance`] (every COM pointer, the editor) lives behind a mutex in
//! [`Shared`] and is only ever touched from jobs run by
//! [`main_thread`](crate::main_thread). The audio thread owns the
//! [`Vst3Effect`]: its own reference to the `IAudioProcessor`, preallocated
//! buffers and parameter lists, and the consumer side of the parameter
//! mailbox. It never locks. When the last `Vst3Effect`/`Vst3Handle` goes away,
//! the instance is torn down on the UI thread (posted), in the order VST3
//! wants: editor detached, `setProcessing(0)`, `setActive(0)`, halves
//! disconnected, `terminate`, release.

#![allow(clippy::unnecessary_cast)] // enum constants are c_int on Windows, c_uint elsewhere

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use serde::Serialize;
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, BusInfo, IAudioProcessor,
    IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait, IConnectionPoint,
    IConnectionPointTrait, IEditController, IEditControllerTrait, IEventList, IParameterChanges,
    MediaTypes_, ParamID, ParameterInfo, ParameterInfo_::ParameterFlags_, ProcessContext,
    ProcessContext_, ProcessData, ProcessModes_, ProcessSetup, SpeakerArr, SpeakerArrangement,
    String128, SymbolicSampleSizes_,
};
use vst3::Steinberg::{
    kNotImplemented, kResultOk, kResultTrue, IBStream, IPluginBaseTrait, IPluginFactory3,
    IPluginFactory3Trait, IPluginFactoryTrait, TUID,
};
use vst3::{ComPtr, ComWrapper};
use wave_engine::{ProcessCtx, Processor};

use crate::editor::EditorSession;
use crate::host::{
    host_context_ptr, Counters, EmptyEventList, GuiRequests, HostContext, HostFlags, MemoryStream,
    ParamMailbox, ParameterChanges,
};
use crate::module::{utf16_to_string, ClassId, Module};
use crate::scan::{read_classes, PluginInfo};
use crate::{main_thread, Vst3Error};

/// Audio buses of one direction a plugin may declare (sidechains etc.); only
/// bus 0 carries audio, the others get silence / a scratch sink.
const MAX_BUSES: usize = 8;
/// Channels one of those extra buses may declare.
const MAX_BUS_CHANNELS: u32 = 32;
/// Parameters the controller may list.
const MAX_PARAMS: i32 = 1 << 16;

/// One parameter of a hosted plugin, as the UI shows it. All values are VST3
/// *normalized* (0..=1); `display` is the plugin's own text for the current
/// value, units included. Hidden and read-only parameters are not listed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub display: String,
    pub def: f64,
}

// ---------------------------------------------------------------------------
// Bus layout
// ---------------------------------------------------------------------------

/// Channel count of every audio bus, per direction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BusLayout {
    pub(crate) inputs: Vec<u32>,
    pub(crate) outputs: Vec<u32>,
}

impl BusLayout {
    /// `Ok` for the shape of a stereo insert effect: a stereo main input and
    /// a stereo main output, plus at most a few extra (side-chain) buses.
    pub(crate) fn check_stereo_effect(&self) -> Result<(), String> {
        let (Some(&main_in), Some(&main_out)) = (self.inputs.first(), self.outputs.first()) else {
            return Err(if self.inputs.is_empty() {
                "it has no audio input (an instrument or a generator)".to_owned()
            } else {
                "it has no audio output".to_owned()
            });
        };
        if main_in != 2 || main_out != 2 {
            return Err(format!(
                "its main buses are {main_in} channel(s) in and {main_out} out, not stereo"
            ));
        }
        if self.inputs.len() > MAX_BUSES
            || self.outputs.len() > MAX_BUSES
            || self
                .inputs
                .iter()
                .chain(&self.outputs)
                .any(|count| *count > MAX_BUS_CHANNELS)
        {
            return Err(
                "it declares more buses or channels than an insert slot supports".to_owned(),
            );
        }
        Ok(())
    }
}

fn read_bus_layout(component: &ComPtr<IComponent>) -> BusLayout {
    // SAFETY: live, initialised component; scalar arguments and a live
    // out-parameter. Nonsense answers produce a layout that is then refused.
    unsafe {
        let read = |direction: i32| -> Vec<u32> {
            let count = component
                .getBusCount(MediaTypes_::kAudio as i32, direction)
                .clamp(0, 64);
            (0..count)
                .map(|index| {
                    let mut info: BusInfo = std::mem::zeroed();
                    if component.getBusInfo(MediaTypes_::kAudio as i32, direction, index, &mut info)
                        == kResultOk
                    {
                        u32::try_from(info.channelCount).unwrap_or(0)
                    } else {
                        0
                    }
                })
                .collect()
        };
        BusLayout {
            inputs: read(BusDirections_::kInput as i32),
            outputs: read(BusDirections_::kOutput as i32),
        }
    }
}

// ---------------------------------------------------------------------------
// The UI-thread instance
// ---------------------------------------------------------------------------

/// Every COM object of one plugin instance. UI thread only.
///
/// Field order is the release order: editor, then the plugin's interfaces,
/// then the host context they hold raw pointers to, then the module.
pub(crate) struct Instance {
    editor: Option<EditorSession>,
    processor: Option<ComPtr<IAudioProcessor>>,
    controller: Option<ComPtr<IEditController>>,
    component_connection: Option<ComPtr<IConnectionPoint>>,
    controller_connection: Option<ComPtr<IConnectionPoint>>,
    component: Option<ComPtr<IComponent>>,
    context: ComWrapper<HostContext>,
    component_initialized: bool,
    controller_initialized: bool,
    single_component: bool,
    active: bool,
    layout: BusLayout,
    name: String,
    _module: Arc<Module>,
}

// SAFETY: an `Instance` holds COM pointers (already `Send`) and, while an
// editor is open, a `wave_plugin_windows::Window` (not `Send`: its handle is
// only meaningful on the UI thread). It is moved between threads only as an
// inert value -- created inside a UI-thread job and handed back, or handed to
// the UI thread for teardown -- and every method is called from a job run by
// `main_thread`, i.e. on the UI thread. Only the editor's window enforces that
// (it re-checks the thread on every call), which is enough: nothing else in
// here is thread-affine at the Rust level.
unsafe impl Send for Instance {}

impl Instance {
    fn component(&self) -> &ComPtr<IComponent> {
        self.component.as_ref().expect("live until Drop")
    }

    fn processor(&self) -> &ComPtr<IAudioProcessor> {
        self.processor.as_ref().expect("checked at creation")
    }

    /// Create and initialise the plugin class `cid` of the module at `path`,
    /// restore `state`, and check it is a stereo effect. Not yet active.
    fn create(
        path: &Path,
        cid: ClassId,
        state: Option<&[u8]>,
        flags: Arc<HostFlags>,
    ) -> Result<(Self, PluginInfo), Vst3Error> {
        let module = Module::shared(path)?;
        let class = read_classes(module.factory(), path)
            .into_iter()
            .find(|class| class.cid == cid)
            .ok_or_else(|| Vst3Error::ClassNotFound {
                path: path.to_path_buf(),
                cid: cid.to_hex(),
            })?;
        let name = class.name.clone();
        let context = HostContext::new(flags);
        let context_ptr = host_context_ptr(&context);
        let factory = module.factory();

        // SAFETY (all plugin calls below): every pointer handed over outlives
        // the call (`context` is owned by the instance being built, ids are
        // locals), out-parameters are live locals, and every returned COM
        // pointer is adopted exactly once by `ComPtr::from_raw`.
        let component = unsafe {
            if let Some(factory3) = factory.cast::<IPluginFactory3>() {
                let _ = factory3.setHostContext(context_ptr);
            }
            let tuid: TUID = cid.to_tuid();
            let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
            let result = factory.createInstance(
                tuid.as_ptr(),
                vst3::Steinberg::Vst::IComponent_iid.as_ptr(),
                &mut raw,
            );
            ComPtr::<IComponent>::from_raw(raw.cast())
                .filter(|_| result == kResultOk)
                .ok_or(Vst3Error::CreateInstance {
                    cid: cid.to_hex(),
                    result,
                })?
        };
        let mut instance = Self {
            editor: None,
            processor: None,
            controller: None,
            component_connection: None,
            controller_connection: None,
            component: Some(component),
            context,
            component_initialized: false,
            controller_initialized: false,
            single_component: true,
            active: false,
            layout: BusLayout::default(),
            name: name.clone(),
            _module: Arc::clone(&module),
        };
        // From here on an early return drops `instance`, which terminates
        // whatever was initialised.
        unsafe {
            let component = instance.component().clone();
            let result = component.initialize(context_ptr);
            if result != kResultOk {
                return Err(Vst3Error::Initialize { name, result });
            }
            instance.component_initialized = true;

            let mut controller_tuid: TUID = [0; 16];
            let separate = component.getControllerClassId(&mut controller_tuid) == kResultOk
                && controller_tuid != [0; 16]
                && ClassId::from_tuid(&controller_tuid) != cid;
            if separate {
                instance.single_component = false;
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                let result = factory.createInstance(
                    controller_tuid.as_ptr(),
                    vst3::Steinberg::Vst::IEditController_iid.as_ptr(),
                    &mut raw,
                );
                if let Some(controller) =
                    ComPtr::<IEditController>::from_raw(raw.cast()).filter(|_| result == kResultOk)
                {
                    if controller.initialize(context_ptr) == kResultOk {
                        instance.controller_initialized = true;
                        instance.controller = Some(controller);
                    } else {
                        // Costs the editor and the parameter list, not the audio.
                        log::warn!("VST3 plugin '{name}': its controller refused to initialise");
                    }
                }
                if let (Some(from_component), Some(from_controller)) = (
                    component.cast::<IConnectionPoint>(),
                    instance
                        .controller
                        .as_ref()
                        .and_then(ComPtr::cast::<IConnectionPoint>),
                ) {
                    let _ = from_component.connect(from_controller.as_ptr());
                    let _ = from_controller.connect(from_component.as_ptr());
                    instance.component_connection = Some(from_component);
                    instance.controller_connection = Some(from_controller);
                }
            } else {
                // The controller *is* the component (already initialised).
                instance.controller = component.cast::<IEditController>();
            }
            if let Some(controller) = instance.controller.as_ref() {
                if let Some(handler) = instance.context.as_com_ref::<IComponentHandler>() {
                    let _ = controller.setComponentHandler(handler.as_ptr());
                }
            }
        }

        let processor = component_processor(&instance, &name)?;
        instance.processor = Some(processor);
        instance.layout = read_bus_layout(instance.component());
        instance
            .layout
            .check_stereo_effect()
            .map_err(|message| Vst3Error::NotAStereoEffect {
                name: name.clone(),
                message,
            })?;
        match state {
            Some(bytes) => instance.restore_state(bytes)?,
            None => instance.sync_controller_from_component(),
        }
        Ok((instance, class.to_info()))
    }

    /// The controller's parameter ids, for the mailboxes.
    fn param_ids(&self) -> Vec<ParamID> {
        let Some(controller) = self.controller.as_ref() else {
            return Vec::new();
        };
        // SAFETY: live controller, live out-parameter.
        unsafe {
            let count = controller.getParameterCount().clamp(0, MAX_PARAMS);
            (0..count)
                .filter_map(|index| {
                    let mut info: ParameterInfo = std::mem::zeroed();
                    (controller.getParameterInfo(index, &mut info) == kResultOk).then_some(info.id)
                })
                .collect()
        }
    }

    /// Copy the component's state into the controller, so the editor opens
    /// showing what the processor holds rather than class defaults.
    fn sync_controller_from_component(&self) {
        let Some(controller) = self.controller.as_ref() else {
            return;
        };
        let stream = MemoryStream::empty();
        let Some(bstream) = stream.as_com_ref::<IBStream>() else {
            return;
        };
        // SAFETY: live objects; the stream outlives both calls.
        unsafe {
            if self.component().getState(bstream.as_ptr()) == kResultOk && !stream.is_empty() {
                stream.rewind();
                let _ = controller.setComponentState(bstream.as_ptr());
            }
        }
    }

    fn restore_state(&self, bytes: &[u8]) -> Result<(), Vst3Error> {
        let (component_bytes, controller_bytes) = crate::state::decode(bytes)?;
        let stream = MemoryStream::reading(component_bytes);
        let bstream = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::InvalidState("no state stream".to_owned()))?;
        // SAFETY: live objects; each stream outlives the calls reading it. The
        // rewind matters: `setState` leaves the cursor at the end.
        unsafe {
            let result = self.component().setState(bstream.as_ptr());
            if result != kResultOk {
                return Err(Vst3Error::State {
                    name: self.name.clone(),
                    direction: "restore",
                    result,
                });
            }
            if let Some(controller) = self.controller.as_ref() {
                stream.rewind();
                let _ = controller.setComponentState(bstream.as_ptr());
                if !controller_bytes.is_empty() {
                    let editor_stream = MemoryStream::reading(controller_bytes);
                    if let Some(editor_stream) = editor_stream.as_com_ref::<IBStream>() {
                        let _ = controller.setState(editor_stream.as_ptr());
                    }
                }
            }
        }
        Ok(())
    }

    fn save_state(&self) -> Result<Vec<u8>, Vst3Error> {
        let stream = MemoryStream::empty();
        let bstream = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::InvalidState("no state stream".to_owned()))?;
        // SAFETY: live objects; streams outlive the calls writing them.
        let result = unsafe { self.component().getState(bstream.as_ptr()) };
        if result != kResultOk {
            return Err(Vst3Error::State {
                name: self.name.clone(),
                direction: "save",
                result,
            });
        }
        // The controller's own stream (editor-only settings). A separate stream
        // for single-component plugins too.
        let controller_state = match self.controller.as_ref() {
            Some(controller) => {
                let editor_stream = MemoryStream::empty();
                match editor_stream.as_com_ref::<IBStream>() {
                    // SAFETY: as above.
                    Some(bstream)
                        if unsafe { controller.getState(bstream.as_ptr()) } == kResultOk =>
                    {
                        editor_stream.contents()
                    }
                    _ => Vec::new(),
                }
            }
            None => Vec::new(),
        };
        crate::state::encode(&stream.contents(), &controller_state)
    }

    /// (Re)activate for `sample_rate` and blocks of at most `max_block`.
    /// Returns the bus layout the plugin settled on.
    fn activate(&mut self, sample_rate: f64, max_block: usize) -> Result<BusLayout, Vst3Error> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(Vst3Error::InvalidAudioConfig(format!(
                "sample rate {sample_rate} is not a positive number"
            )));
        }
        let max_samples = i32::try_from(max_block)
            .ok()
            .filter(|max| *max > 0)
            .ok_or_else(|| Vst3Error::InvalidAudioConfig(format!("block size {max_block}")))?;
        self.deactivate();

        let component = self.component().clone();
        let processor = self.processor().clone();
        let name = self.name.clone();
        let refused = |stage: &'static str, result: i32| Vst3Error::Activate {
            name: name.clone(),
            stage,
            result,
        };
        // SAFETY: live objects, the component is inactive, every pointer is to
        // a live local. This is the activation order VST3 requires.
        unsafe {
            let arrangements = |direction: i32, count: usize| -> Vec<SpeakerArrangement> {
                (0..count)
                    .map(|index| {
                        if index == 0 {
                            return SpeakerArr::kStereo;
                        }
                        let mut current: SpeakerArrangement = 0;
                        let _ = processor.getBusArrangement(direction, index as i32, &mut current);
                        current
                    })
                    .collect()
            };
            let mut inputs = arrangements(BusDirections_::kInput as i32, self.layout.inputs.len());
            let mut outputs =
                arrangements(BusDirections_::kOutput as i32, self.layout.outputs.len());
            let result = processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            );
            if result != kResultTrue {
                // Not fatal: the layout check below decides.
                log::debug!("VST3 plugin '{name}' declined the stereo arrangement ({result:#x})");
            }
            let layout = read_bus_layout(&component);
            layout
                .check_stereo_effect()
                .map_err(|message| Vst3Error::NotAStereoEffect {
                    name: name.clone(),
                    message,
                })?;

            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: max_samples,
                sampleRate: sample_rate,
            };
            let result = processor.setupProcessing(&mut setup);
            if result != kResultOk {
                return Err(refused("setting up processing", result));
            }
            // Buses default to inactive unless flagged otherwise; only the
            // main pair carries audio.
            for (direction, count) in [
                (BusDirections_::kInput as i32, layout.inputs.len()),
                (BusDirections_::kOutput as i32, layout.outputs.len()),
            ] {
                for index in 0..count {
                    let _ = component.activateBus(
                        MediaTypes_::kAudio as i32,
                        direction,
                        index as i32,
                        u8::from(index == 0),
                    );
                }
            }
            let result = component.setActive(1);
            if result != kResultOk {
                return Err(refused("being activated", result));
            }
            let result = processor.setProcessing(1);
            if result != kResultOk && result != kNotImplemented {
                let _ = component.setActive(0);
                return Err(refused("starting processing", result));
            }
            self.active = true;
            self.layout = layout.clone();
            Ok(layout)
        }
    }

    fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // SAFETY: live objects; the counterparts of `activate`'s calls. No
        // `process` runs concurrently: callers are `prepare` (contract: not
        // while processing) and teardown (the effect is gone).
        unsafe {
            let _ = self.processor().setProcessing(0);
            let _ = self.component().setActive(0);
        }
    }

    fn params(&self) -> Vec<ParamInfo> {
        let Some(controller) = self.controller.as_ref() else {
            return Vec::new();
        };
        // SAFETY: live controller; live out-parameters.
        unsafe {
            let count = controller.getParameterCount().clamp(0, MAX_PARAMS);
            let mut list = Vec::with_capacity(count as usize);
            for index in 0..count {
                let mut info: ParameterInfo = std::mem::zeroed();
                if controller.getParameterInfo(index, &mut info) != kResultOk {
                    continue;
                }
                let hidden =
                    ParameterFlags_::kIsHidden as i32 | ParameterFlags_::kIsReadOnly as i32;
                if info.flags & hidden != 0 {
                    continue;
                }
                let value = controller.getParamNormalized(info.id);
                let mut text: String128 = [0; 128];
                let mut display =
                    if controller.getParamStringByValue(info.id, value, &mut text) == kResultOk {
                        utf16_to_string(&text)
                    } else {
                        format!("{value:.3}")
                    };
                let units = utf16_to_string(&info.units);
                if !units.is_empty() && !display.ends_with(&units) {
                    display = format!("{display} {units}");
                }
                let mut name = utf16_to_string(&info.title);
                if name.is_empty() {
                    name = utf16_to_string(&info.shortTitle);
                }
                list.push(ParamInfo {
                    id: info.id,
                    name,
                    value,
                    display,
                    def: info.defaultNormalizedValue,
                });
            }
            list
        }
    }

    fn set_controller_param(&self, id: ParamID, value: f64) {
        if let Some(controller) = self.controller.as_ref() {
            // SAFETY: live controller, scalar arguments.
            let _ = unsafe { controller.setParamNormalized(id, value) };
        }
    }
}

fn component_processor(
    instance: &Instance,
    name: &str,
) -> Result<ComPtr<IAudioProcessor>, Vst3Error> {
    let processor = instance
        .component()
        .cast::<IAudioProcessor>()
        .ok_or_else(|| Vst3Error::NoAudioProcessor {
            name: name.to_owned(),
        })?;
    // SAFETY: live processor, scalar argument.
    if unsafe { processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) }
        != kResultTrue
    {
        return Err(Vst3Error::NoFloat32 {
            name: name.to_owned(),
        });
    }
    Ok(processor)
}

impl Drop for Instance {
    fn drop(&mut self) {
        // The editor's child window belongs to the plugin: it must go first.
        drop(self.editor.take());
        if self.processor.is_some() && self.component.is_some() {
            self.deactivate();
        }
        // SAFETY: live handles, each terminated once (a single-component
        // plugin's controller *is* its component).
        unsafe {
            if let (Some(from_component), Some(from_controller)) = (
                self.component_connection.as_ref(),
                self.controller_connection.as_ref(),
            ) {
                let _ = from_component.disconnect(from_controller.as_ptr());
                let _ = from_controller.disconnect(from_component.as_ptr());
            }
            if let Some(controller) = self.controller.as_ref() {
                let _ = controller.setComponentHandler(std::ptr::null_mut());
                if !self.single_component && self.controller_initialized {
                    let _ = controller.terminate();
                }
            }
            if self.component_initialized {
                if let Some(component) = self.component.as_ref() {
                    let _ = component.terminate();
                }
            }
        }
        drop(self.controller_connection.take());
        drop(self.component_connection.take());
        drop(self.controller.take());
        drop(self.processor.take());
        drop(self.component.take());
    }
}

// ---------------------------------------------------------------------------
// Shared between the RT effect and the handles
// ---------------------------------------------------------------------------

pub(crate) struct Shared {
    info: PluginInfo,
    has_editor: bool,
    instance: Mutex<Option<Instance>>,
    /// Host -> processor.
    inbox: Arc<ParamMailbox>,
    /// Processor -> host (output parameter changes, e.g. meters, automation
    /// the plugin generates). Drained into the controller on the UI thread.
    feedback: ParamMailbox,
    flags: Arc<HostFlags>,
    gui: Arc<GuiRequests>,
    counters: Counters,
    editor_open: AtomicBool,
}

impl Shared {
    /// Hand the processor's output parameter values to the controller. UI
    /// thread, with the instance locked.
    fn apply_feedback(&self, instance: &Instance) {
        self.feedback.drain(usize::MAX, |id, value| {
            instance.set_controller_param(id, value)
        });
    }

    /// The editor's per-tick work. UI thread.
    fn tick_editor(&self) {
        // `try_lock`: a plugin running a modal loop inside a call we made can
        // deliver a nested tick; skip it rather than deadlock.
        let Some(mut guard) = self.instance.try_lock() else {
            return;
        };
        let Some(instance) = guard.as_mut() else {
            return;
        };
        self.apply_feedback(instance);
        let closed = instance.editor.as_mut().is_some_and(EditorSession::pump);
        if closed {
            drop(instance.editor.take());
            self.editor_open.store(false, Ordering::Release);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.get_mut().take() {
            main_thread::post("being torn down", move || drop(instance));
        }
    }
}

// ---------------------------------------------------------------------------
// The RT side
// ---------------------------------------------------------------------------

/// Preallocated audio I/O for one activation.
struct Io {
    main_in: [Vec<f32>; 2],
    main_out: [Vec<f32>; 2],
    /// What extra input buses read (re-zeroed each block: a plugin may write
    /// into its inputs).
    silence: Vec<f32>,
    /// Where extra output buses write; discarded.
    sink: Vec<f32>,
    in_channels: Vec<u32>,
    out_channels: Vec<u32>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    in_buses: Vec<AudioBusBuffers>,
    out_buses: Vec<AudioBusBuffers>,
}

impl Io {
    fn new(layout: &BusLayout, max_block: usize) -> Self {
        let total = |channels: &[u32]| channels.iter().map(|count| *count as usize).sum::<usize>();
        let bus = |channels: u32| AudioBusBuffers {
            numChannels: channels as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: std::ptr::null_mut(),
            },
        };
        Self {
            main_in: [vec![0.0; max_block], vec![0.0; max_block]],
            main_out: [vec![0.0; max_block], vec![0.0; max_block]],
            silence: vec![0.0; max_block],
            sink: vec![0.0; max_block],
            in_ptrs: vec![std::ptr::null_mut(); total(&layout.inputs)],
            out_ptrs: vec![std::ptr::null_mut(); total(&layout.outputs)],
            in_buses: layout.inputs.iter().map(|count| bus(*count)).collect(),
            out_buses: layout.outputs.iter().map(|count| bus(*count)).collect(),
            in_channels: layout.inputs.clone(),
            out_channels: layout.outputs.clone(),
        }
    }

    /// Point every bus at its buffers. Rewritten each block (a few stores):
    /// nothing a plugin did to the arrays last block can leak into this one.
    fn wire(&mut self) {
        let mut cursor = 0;
        for (index, (bus, channels)) in self.in_buses.iter_mut().zip(&self.in_channels).enumerate()
        {
            let start = cursor;
            for channel in 0..*channels as usize {
                self.in_ptrs[cursor] = match (index, channel) {
                    (0, 0 | 1) => self.main_in[channel].as_mut_ptr(),
                    _ => self.silence.as_mut_ptr(),
                };
                cursor += 1;
            }
            bus.numChannels = *channels as i32;
            bus.silenceFlags = 0;
            bus.__field0.channelBuffers32 = self.in_ptrs[start..].as_mut_ptr();
        }
        let mut cursor = 0;
        for (index, (bus, channels)) in self
            .out_buses
            .iter_mut()
            .zip(&self.out_channels)
            .enumerate()
        {
            let start = cursor;
            for channel in 0..*channels as usize {
                self.out_ptrs[cursor] = match (index, channel) {
                    (0, 0 | 1) => self.main_out[channel].as_mut_ptr(),
                    _ => self.sink.as_mut_ptr(),
                };
                cursor += 1;
            }
            bus.numChannels = *channels as i32;
            bus.silenceFlags = 0;
            bus.__field0.channelBuffers32 = self.out_ptrs[start..].as_mut_ptr();
        }
    }

    fn has_extra_inputs(&self) -> bool {
        self.in_channels.len() > 1
    }
}

/// A hosted VST3 effect on the audio thread. Implements
/// [`wave_engine::Processor`]; get one from [`instantiate`].
///
/// [`process`](Processor::process) is real-time safe: it copies the block into
/// preallocated buffers, drains the parameter mailbox (atomics) into
/// preallocated `IParameterChanges`, calls the plugin, copies the result back
/// and posts the plugin's output parameter values (atomics). No allocation, no
/// lock, no syscall, no logging. Blocks longer than the activated maximum are
/// processed in chunks. If the plugin reports an error for a block, that
/// block passes through unprocessed.
///
/// [`prepare`](Processor::prepare) with a new sample rate or a larger block
/// re-activates the plugin on the UI thread (blocking); with the same
/// configuration it does nothing. [`reset`](Processor::reset) is a no-op:
/// VST3 has no real-time-safe reset (only `setActive` cycling, which is UI
/// thread work).
///
/// Drop it off the audio thread (the engine's contract already does); the
/// plugin itself is torn down on the UI thread once every handle is gone too.
pub struct Vst3Effect {
    /// Released before `shared`, i.e. before the instance can be torn down.
    processor: ComPtr<IAudioProcessor>,
    io: Io,
    input_changes: ComWrapper<ParameterChanges>,
    output_changes: ComWrapper<ParameterChanges>,
    events: ComWrapper<EmptyEventList>,
    /// Consumer side of the inbox's overflow ring.
    unlisted: rtrb::Consumer<(ParamID, f64)>,
    sample_rate: f64,
    max_block: usize,
    continuous_samples: i64,
    /// Re-activation failed: pass audio through untouched.
    failed: bool,
    shared: Arc<Shared>,
}

// SAFETY: the raw channel pointers in `io` point into `io`'s own heap buffers,
// which do not move when the effect moves, and they are rewritten before every
// use. The `ComWrapper`s are owned exclusively by this effect (their `Send` /
// `Sync` assertions are documented in `host`). The effect is moved to the audio
// thread as a whole and used there by one thread at a time.
unsafe impl Send for Vst3Effect {}

impl Vst3Effect {
    fn run_block(&mut self, l: &mut [f32], r: &mut [f32], ctx: &ProcessCtx, offset: usize) {
        let frames = l.len();
        self.io.main_in[0][..frames].copy_from_slice(l);
        self.io.main_in[1][..frames].copy_from_slice(r);
        if self.io.has_extra_inputs() {
            self.io.silence[..frames].fill(0.0);
        }
        self.io.wire();
        self.input_changes
            .fill(&self.shared.inbox, &mut self.unlisted);
        self.output_changes.clear();

        let mut context = self.process_context(ctx, offset);
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.io.in_buses.len() as i32,
            numOutputs: self.io.out_buses.len() as i32,
            inputs: self.io.in_buses.as_mut_ptr(),
            outputs: self.io.out_buses.as_mut_ptr(),
            inputParameterChanges: self
                .input_changes
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |changes| changes.as_ptr()),
            outputParameterChanges: self
                .output_changes
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |changes| changes.as_ptr()),
            inputEvents: self
                .events
                .as_com_ref::<IEventList>()
                .map_or(std::ptr::null_mut(), |events| events.as_ptr()),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut context,
        };
        // SAFETY: the plugin is active and processing (activation happened on
        // the UI thread before this effect could be handed to the audio thread,
        // and only `prepare` -- never concurrent with `process` -- changes it).
        // Every pointer in `data` is to a live local or this effect's own
        // buffers; each channel buffer holds at least `frames <= max_block`
        // samples, the maximum given to `setupProcessing`.
        let result = unsafe { self.processor.process(&mut data) };
        self.continuous_samples += frames as i64;
        if result == kResultOk {
            if std::env::var_os("WAVE_DIAG").is_some() {
                let calls = self.shared.counters.ok_calls.fetch_add(1, Ordering::Relaxed);
                if calls < 30 {
                    let outp = self.io.main_out[0][..frames].iter().fold(0f32, |a, v| a.max(v.abs()));
                    let nan = self.io.main_out[0][..frames].iter().filter(|v| v.is_nan()).count();
                    eprintln!("[diag-vst3] call {calls} frames {frames} out_peak {outp:.4} nan {nan} this {:p}", self as *const _);
                }
            }
            l.copy_from_slice(&self.io.main_out[0][..frames]);
            r.copy_from_slice(&self.io.main_out[1][..frames]);
        } else {
            self.shared
                .counters
                .process_errors
                .fetch_add(1, Ordering::Relaxed);
        }
        let shared = &self.shared;
        self.output_changes.for_each_last(|id, value| {
            if !shared.feedback.post_listed(id, value) {
                shared
                    .counters
                    .feedback_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    /// Only what WAVE actually knows is marked valid: a plugin syncing to a
    /// field the host left at zero is worse off than one told nothing.
    fn process_context(&self, ctx: &ProcessCtx, offset: usize) -> ProcessContext {
        // SAFETY: a plain aggregate of scalars; all-zero is a valid value.
        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        let sample_rate = self.sample_rate;
        context.sampleRate = sample_rate;
        context.continousTimeSamples = self.continuous_samples;
        context.state = ProcessContext_::StatesAndFlags_::kContTimeValid as u32;
        if ctx.bpm.is_finite() && ctx.bpm > 0.0 {
            let beat = ctx.beat + offset as f64 / sample_rate * ctx.bpm / 60.0;
            context.tempo = ctx.bpm;
            context.projectTimeMusic = beat;
            context.projectTimeSamples = (beat * 60.0 / ctx.bpm * sample_rate).round() as i64;
            context.timeSigNumerator = 4;
            context.timeSigDenominator = 4;
            context.state |= ProcessContext_::StatesAndFlags_::kTempoValid as u32
                | ProcessContext_::StatesAndFlags_::kProjectTimeMusicValid as u32
                | ProcessContext_::StatesAndFlags_::kTimeSigValid as u32;
        }
        if ctx.playing {
            context.state |= ProcessContext_::StatesAndFlags_::kPlaying as u32;
        }
        context
    }
}

impl Processor for Vst3Effect {
    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        let sample_rate = f64::from(sample_rate);
        if (sample_rate - self.sample_rate).abs() < 1e-6
            && max_block <= self.max_block
            && !self.failed
        {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let activated = main_thread::run("being re-activated", move || {
            let mut guard = shared.instance.lock();
            let instance = guard.as_mut().ok_or(Vst3Error::Closed)?;
            instance.activate(sample_rate, max_block)
        })
        .and_then(|result| result);
        match activated {
            Ok(layout) => {
                self.io = Io::new(&layout, max_block);
                self.sample_rate = sample_rate;
                self.max_block = max_block;
                self.failed = false;
            }
            Err(err) => {
                log::error!(
                    "VST3 plugin '{}' could not be re-activated at {sample_rate} Hz / {max_block}: {err}; it is bypassed",
                    self.shared.info.name
                );
                self.failed = true;
            }
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32], ctx: &ProcessCtx) {
        if self.failed || self.max_block == 0 {
            return;
        }
        let frames = l.len().min(r.len());
        let mut done = 0;
        while done < frames {
            let chunk = (frames - done).min(self.max_block);
            let (l_chunk, r_chunk) = (&mut l[done..done + chunk], &mut r[done..done + chunk]);
            self.run_block(l_chunk, r_chunk, ctx, done);
            done += chunk;
        }
    }

    fn set_plugin_param(&mut self, param_id: u32, normalized: f32) {
        let v = f64::from(normalized);
        // the mailbox is the same lock-free path `set_param` uses: atomics only,
        // drained by the next `run_block`. Automation values are already
        // normalized 0..1; clamp finite strays, drop NaN.
        if v.is_finite() {
            self.shared.inbox.post(param_id, v.clamp(0.0, 1.0));
        }
    }
}

impl std::fmt::Debug for Vst3Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Effect")
            .field("plugin", &self.shared.info.name)
            .field("sample_rate", &self.sample_rate)
            .field("max_block", &self.max_block)
            .field("failed", &self.failed)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The non-RT handle
// ---------------------------------------------------------------------------

/// The non-real-time side of a hosted effect: parameters, state, editor.
///
/// `Send + Sync` and cheap to clone (an `Arc`). Every method that talks to the
/// plugin runs on the UI thread through the installed executor (see
/// [`set_main_thread_executor`](crate::set_main_thread_executor)) and blocks
/// until done, except [`set_param`](Self::set_param), which never blocks. The
/// editor methods must be *called* on the UI thread.
#[derive(Clone)]
pub struct Vst3Handle {
    shared: Arc<Shared>,
}

impl Vst3Handle {
    /// What was instantiated.
    pub fn info(&self) -> &PluginInfo {
        &self.shared.info
    }

    /// The plugin's parameters with their current values and display text.
    /// Empty for a plugin without an edit controller, or after an error
    /// (logged).
    pub fn params(&self) -> Vec<ParamInfo> {
        let shared = Arc::clone(&self.shared);
        main_thread::run("listing its parameters", move || {
            let guard = shared.instance.lock();
            guard.as_ref().map_or_else(Vec::new, |instance| {
                shared.apply_feedback(instance);
                instance.params()
            })
        })
        .unwrap_or_else(|err| {
            log::warn!("VST3 plugin '{}': {err}", self.shared.info.name);
            Vec::new()
        })
    }

    /// Set a parameter to a normalized value (0..=1). Never blocks: the audio
    /// thread picks it up at its next block (coalesced with any newer move of
    /// the same parameter), and the controller -- hence an open editor -- is
    /// told on the UI thread.
    ///
    /// # Errors
    /// [`Vst3Error::InvalidParamValue`] outside 0..=1; [`Vst3Error::ParamQueueFull`]
    /// only for an id the plugin did not list and a full overflow ring.
    pub fn set_param(&self, id: u32, value: f64) -> Result<(), Vst3Error> {
        if !(value.is_finite() && (0.0..=1.0).contains(&value)) {
            return Err(Vst3Error::InvalidParamValue(value));
        }
        if !self.shared.inbox.post(id, value) {
            return Err(Vst3Error::ParamQueueFull);
        }
        self.shared.flags.dirty.store(true, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        main_thread::post("taking a parameter value", move || {
            if let Some(instance) = shared.instance.lock().as_ref() {
                instance.set_controller_param(id, value);
            }
        });
        Ok(())
    }

    /// The plugin's complete state (processor + controller), as an opaque blob
    /// to store in the project and pass back to [`instantiate`].
    pub fn get_state(&self) -> Result<Vec<u8>, Vst3Error> {
        let shared = Arc::clone(&self.shared);
        main_thread::run("saving its state", move || {
            shared
                .instance
                .lock()
                .as_ref()
                .ok_or(Vst3Error::Closed)?
                .save_state()
        })?
    }

    /// Whether the plugin's state may have changed since the last call (an
    /// edit in its editor, a parameter set through this handle, a plugin
    /// restart request). Clears the flag. Lets the app re-save the state and
    /// mark the project dirty only when something happened.
    pub fn take_state_changed(&self) -> bool {
        self.shared.flags.dirty.swap(false, Ordering::AcqRel)
    }

    /// Whether the plugin has an edit controller, and so *may* have an editor
    /// (`createView` can still decline).
    pub fn has_editor(&self) -> bool {
        self.shared.has_editor
    }

    /// Whether its editor window is open. Turns `false` by itself when the
    /// user closes the window.
    pub fn is_editor_open(&self) -> bool {
        self.shared.editor_open.load(Ordering::Acquire)
    }

    /// Open the plugin's editor in its own native window, or bring it to the
    /// front if already open. **UI thread only** (the thread that created
    /// `windows`); from Tauri, call it inside `AppHandle::run_on_main_thread`.
    ///
    /// After this the editor services itself from its window's timer (resize
    /// requests, user drags, DPI changes, the close button): no per-frame call
    /// is needed.
    ///
    /// # Errors
    /// [`Vst3Error::WrongThread`] off the UI thread, [`Vst3Error::NoEditor`]
    /// for a plugin without one, [`Vst3Error::Window`] where the platform has
    /// no window backend (everything but Windows), and the plugin's refusals.
    pub fn open_editor(&self, windows: &wave_plugin_windows::Service) -> Result<(), Vst3Error> {
        if !windows.on_owner_thread() || !main_thread::on_main_thread() {
            return Err(Vst3Error::WrongThread);
        }
        crate::module::enter_com_apartment();
        let mut guard = self.shared.instance.lock();
        let instance = guard.as_mut().ok_or(Vst3Error::Closed)?;
        if let Some(editor) = instance.editor.as_ref() {
            editor.raise();
            return Ok(());
        }
        let controller = instance.controller.clone().ok_or(Vst3Error::NoEditor)?;
        let session = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            EditorSession::open(
                &controller,
                windows,
                &self.shared.info.name,
                Arc::clone(&self.shared.gui),
            )
        }))
        .map_err(|_| Vst3Error::Panicked {
            stage: "opening its editor",
        })??;
        let weak: Weak<Shared> = Arc::downgrade(&self.shared);
        let tick: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(shared) = weak.upgrade() {
                shared.tick_editor();
            }
        });
        session.window().set_tick_handler(Some(tick))?;
        instance.editor = Some(session);
        self.shared.editor_open.store(true, Ordering::Release);
        Ok(())
    }

    /// Close the editor (no-op when closed). **UI thread only.**
    pub fn close_editor(&self) -> Result<(), Vst3Error> {
        if !main_thread::on_main_thread() {
            return Err(Vst3Error::WrongThread);
        }
        let session = self
            .shared
            .instance
            .lock()
            .as_mut()
            .and_then(|instance| instance.editor.take());
        // Dropped outside the lock: closing may run the window's handlers.
        drop(session);
        self.shared.editor_open.store(false, Ordering::Release);
        Ok(())
    }
}

impl std::fmt::Debug for Vst3Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Handle")
            .field("plugin", &self.shared.info)
            .field("editor_open", &self.is_editor_open())
            .finish()
    }
}

/// Load the effect `cid` from the module at `path`, restore `state` (a blob
/// from [`Vst3Handle::get_state`]; `None` for a fresh instance) and activate it
/// for `sample_rate` and blocks of at most `max_block` frames.
///
/// Call it off the audio thread; it blocks while the UI thread does the work
/// (see [`set_main_thread_executor`](crate::set_main_thread_executor)), so do
/// not hold a lock the UI thread may wait for. The returned processor is
/// already prepared for (`sample_rate`, `max_block`).
///
/// Runs third-party code in this process: a module that the scanner
/// block-listed should not be passed here.
///
/// # Errors
/// Everything in [`Vst3Error`] that loading, creating, restoring and
/// activating can produce -- notably [`Vst3Error::NotAStereoEffect`] for an
/// instrument or a mono/surround-only plugin.
pub fn instantiate(
    path: &str,
    cid: &str,
    state: Option<&[u8]>,
    sample_rate: f32,
    max_block: usize,
) -> Result<(Box<dyn Processor>, Vst3Handle), Vst3Error> {
    let (effect, handle) = instantiate_effect(path, cid, state, sample_rate, max_block)?;
    Ok((Box::new(effect), handle))
}

/// [`instantiate`] returning the concrete type.
pub(crate) fn instantiate_effect(
    path: &str,
    cid: &str,
    state: Option<&[u8]>,
    sample_rate: f32,
    max_block: usize,
) -> Result<(Vst3Effect, Vst3Handle), Vst3Error> {
    let class_id =
        ClassId::from_hex(cid).ok_or_else(|| Vst3Error::InvalidClassId(cid.to_owned()))?;
    let path = std::path::PathBuf::from(path);
    let state = state.map(<[u8]>::to_vec);
    let flags = Arc::new(HostFlags::default());
    let sample_rate = f64::from(sample_rate);

    let job_flags = Arc::clone(&flags);
    let created = main_thread::run("being instantiated", move || {
        let (mut instance, info) = Instance::create(&path, class_id, state.as_deref(), job_flags)?;
        let layout = instance.activate(sample_rate, max_block)?;
        let (inbox, unlisted) = ParamMailbox::new(&instance.param_ids());
        let inbox = Arc::new(inbox);
        instance.context.connect_inbox(Arc::clone(&inbox));
        let (feedback, _) = ParamMailbox::new(&instance.param_ids());
        let processor = instance.processor().clone();
        Ok::<_, Vst3Error>((instance, info, layout, inbox, unlisted, feedback, processor))
    })??;
    let (instance, info, layout, inbox, unlisted, feedback, processor) = created;

    let has_editor = instance.controller.is_some();
    let shared = Arc::new(Shared {
        info,
        has_editor,
        instance: Mutex::new(Some(instance)),
        inbox,
        feedback,
        flags,
        gui: Arc::new(GuiRequests::default()),
        counters: Counters::default(),
        editor_open: AtomicBool::new(false),
    });
    // Restoring a state is not an edit.
    shared.flags.dirty.store(false, Ordering::Release);
    let effect = Vst3Effect {
        processor,
        io: Io::new(&layout, max_block),
        input_changes: ParameterChanges::new(),
        output_changes: ParameterChanges::new(),
        events: EmptyEventList::new(),
        unlisted,
        sample_rate,
        max_block,
        continuous_samples: 0,
        failed: false,
        shared: Arc::clone(&shared),
    };
    Ok((effect, Vst3Handle { shared }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo() -> BusLayout {
        BusLayout {
            inputs: vec![2],
            outputs: vec![2],
        }
    }

    #[test]
    fn only_a_stereo_in_stereo_out_layout_is_an_insert_effect() {
        assert!(stereo().check_stereo_effect().is_ok());
        let sidechain = BusLayout {
            inputs: vec![2, 2],
            outputs: vec![2],
        };
        assert!(
            sidechain.check_stereo_effect().is_ok(),
            "a side-chain bus is fed silence"
        );

        let instrument = BusLayout {
            inputs: vec![],
            outputs: vec![2],
        };
        let err = instrument.check_stereo_effect().unwrap_err();
        assert!(err.contains("no audio input"), "{err}");
        let mono = BusLayout {
            inputs: vec![1],
            outputs: vec![1],
        };
        assert!(mono
            .check_stereo_effect()
            .unwrap_err()
            .contains("1 channel"));
        let sink = BusLayout {
            inputs: vec![2],
            outputs: vec![],
        };
        assert!(sink.check_stereo_effect().is_err());
        let absurd = BusLayout {
            inputs: vec![2; MAX_BUSES + 1],
            outputs: vec![2],
        };
        assert!(absurd.check_stereo_effect().is_err());
    }

    #[test]
    fn io_wires_the_main_pair_to_real_buffers_and_extras_to_silence_and_a_sink() {
        let layout = BusLayout {
            inputs: vec![2, 1],
            outputs: vec![2, 2],
        };
        let mut io = Io::new(&layout, 64);
        io.wire();
        assert_eq!(io.in_buses.len(), 2);
        assert_eq!(io.out_buses.len(), 2);
        // SAFETY: `wire` pointed every bus at `io`'s own pointer arrays.
        unsafe {
            let main_in = io.in_buses[0].__field0.channelBuffers32;
            assert_eq!(*main_in, io.main_in[0].as_mut_ptr());
            assert_eq!(*main_in.add(1), io.main_in[1].as_mut_ptr());
            assert_eq!(
                *io.in_buses[1].__field0.channelBuffers32,
                io.silence.as_mut_ptr()
            );
            let main_out = io.out_buses[0].__field0.channelBuffers32;
            assert_eq!(*main_out.add(1), io.main_out[1].as_mut_ptr());
            let extra_out = io.out_buses[1].__field0.channelBuffers32;
            assert_eq!(*extra_out, io.sink.as_mut_ptr());
            assert_eq!(*extra_out.add(1), io.sink.as_mut_ptr());
        }
        assert!(io.has_extra_inputs());
    }

    #[test]
    fn instantiating_garbage_is_a_typed_error_not_a_panic() {
        let err = instantiate("/nowhere/x.vst3", "not-a-cid", None, 48_000.0, 256)
            .err()
            .expect("must fail");
        assert!(matches!(err, Vst3Error::InvalidClassId(_)), "got {err:?}");

        let dir = crate::module::scratch_dir("instantiate");
        let bogus = dir.join("garbage.vst3");
        std::fs::write(&bogus, b"not a library").unwrap();
        let err = instantiate(
            bogus.to_str().unwrap(),
            &"00".repeat(16),
            None,
            48_000.0,
            256,
        )
        .err()
        .expect("must fail");
        assert!(
            matches!(err, Vst3Error::Load { .. } | Vst3Error::UnsupportedPlatform),
            "got {err:?}"
        );
    }

    #[test]
    fn param_info_serialises_in_the_shape_the_ui_expects() {
        let info = ParamInfo {
            id: 3,
            name: "Gain".to_owned(),
            value: 0.5,
            display: "-6.0 dB".to_owned(),
            def: 1.0,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": 3, "name": "Gain", "value": 0.5, "display": "-6.0 dB", "def": 1.0})
        );
    }
}
